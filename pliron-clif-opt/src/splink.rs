//! Splat/const sinking — clone rematerializable constants into their use
//! blocks (`PLIRON_SPLINK`).
//!
//! A `splat(iconst k)` defined before a loop and used inside it is a
//! long-lived vector value: regalloc2 does not rematerialize it, so under
//! pressure it becomes a `ldr`/`str` spill pair in the hot path (seen in
//! hashbrown probe loops). The value costs one `movi`/`dup` to recreate —
//! far cheaper than a spill slot round-trip — so copy the def into every
//! use block instead. The lowering rule for `splat (iconst)` still folds
//! to `movi`: it matches on the value's *defining instruction*, wherever
//! it sits.
//!
//! Scalar constants get the same treatment, but only for uses inside a
//! loop the def sits outside: that is the shape where the def's live
//! range spans every back edge and regalloc2 — which does not
//! rematerialize — answers with a spill slot. A `mov`/`movi` per use
//! beats the store+reload pair (and the extra store regalloc2 emits on
//! each back edge when the value is param-shaped). Outside loops a
//! shared def costs nothing a per-use clone would save.

use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Opcode, Value};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::{FxHashMap, FxHashSet};

/// Walk every reachable block once; per-block work is trivial.
const MAX_BLOCKS: usize = 16384;

/// Is `i` a `splat` whose operand is a materializable constant?
fn const_splat(func: &Function, i: Inst) -> bool {
    let InstructionData::Unary {
        opcode: Opcode::Splat,
        arg,
    } = func.dfg.insts[i]
    else {
        return false;
    };
    let arg = func.dfg.resolve_aliases(arg);
    let Some(src) = func.dfg.value_def(arg).inst() else {
        return false;
    };
    matches!(
        func.dfg.insts[src],
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            ..
        } | InstructionData::UnaryIeee32 { .. }
            | InstructionData::UnaryIeee64 { .. }
    )
}

/// Is `i` itself a scalar materializable constant (`iconst`/`f32const`/
/// `f64const`)?
fn scalar_const(func: &Function, i: Inst) -> bool {
    matches!(
        func.dfg.insts[i],
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            ..
        } | InstructionData::UnaryIeee32 { .. }
            | InstructionData::UnaryIeee64 { .. }
    )
}

/// Is `i` a rematerializable address (`tls_value`/`symbol_value`/
/// `func_addr`)? These lower to a `lea`/`movsym` (+ at most one fixed
/// TLS-segment load on COFF/ELF-IE), never trap, and are loop-invariant —
/// so a def outside a loop whose only uses sit inside it buys a
/// callee-saved register for the whole loop plus regalloc2 boundary
/// moves on every back edge (count_alpha's `mov %rbp,%rdx`/`mov %rdx,%rbp`
/// shuffles), while a per-use clone is a few cheap insts in the use
/// block. `addr_of_symbol` in jumpthread.rs already treats them as
/// deterministic. x64 only: there the clone is one RIP-relative `lea`; on
/// aarch64 it is `adrp` + `add` per use, which put two instructions in
/// pulldown-cmark's per-byte escape scan that LLVM hoists.
/// `PLIRON_SPLINK_ADDR=1` clones them on every target.
fn remat_addr(func: &Function, i: Inst) -> bool {
    matches!(
        func.dfg.insts[i],
        InstructionData::UnaryGlobalValue {
            opcode: Opcode::TlsValue | Opcode::SymbolValue | Opcode::FuncAddr,
            ..
        }
    )
}

/// `k` masked to `bits` as a single cyclic run of 1s — the element shape
/// aarch64's logical immediates encode.
fn single_run(elem: u64, e: u32) -> bool {
    let emask = if e == 64 { u64::MAX } else { (1u64 << e) - 1 };
    let elem = elem & emask;
    if elem == 0 || elem == emask {
        return false;
    }
    let wrap = ((elem << 1) | (elem >> (e - 1))) & emask;
    (elem ^ wrap).count_ones() == 2
}

/// `k` (masked to `bits`) encodes as an aarch64 logical immediate: the
/// smallest `e`-bit element (`e` a power of two) that replicates to `k`,
/// with that element a cyclic run of 1s.
fn logical_imm(k: u64, bits: u32) -> bool {
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    if k == 0 || k == mask {
        return false;
    }
    let mut e = 2;
    while e <= bits {
        let emask = if e == 64 { u64::MAX } else { (1u64 << e) - 1 };
        let elem = k & emask;
        let (mut rep, mut s) = (0u64, 0);
        while s < bits {
            rep |= elem << s;
            s += e;
        }
        if rep == k && single_run(elem, e) {
            return true;
        }
        e *= 2;
    }
    false
}

/// Does `d` rematerialize in one target instruction? On x64 any scalar
/// const is a single `mov`/`movabs`. On aarch64 an `iconst` is one inst
/// only when `movz`/`movn`-encodable (at most one 16-bit lane differs
/// from the fill) or a logical immediate; anything else is a movk chain,
/// and re-running that chain every iteration costs far more than the
/// hoisted alternative — a spill `ldr` at worst. Float consts go through
/// a literal-pool load, itself no cheaper than the spill reload sinking
/// was avoiding, so they stay hoisted too.
fn cheap_scalar(func: &Function, d: Inst, x64: bool) -> bool {
    if x64 {
        return true;
    }
    let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = func.dfg.insts[d]
    else {
        return false;
    };
    let ty = func.dfg.ctrl_typevar(d);
    let bits = ty.bits();
    if bits > 64 {
        return false;
    }
    let k = if bits == 64 {
        imm.bits() as u64
    } else {
        (imm.bits() as u64) & ((1u64 << bits) - 1)
    };
    // movz: at most one 16-bit lane nonzero; movn: at most one not full.
    let limbs = (bits / 16).max(1);
    let (mut nonzero, mut nonfull) = (0, 0);
    for i in 0..limbs {
        let l = (k >> (i * 16)) & 0xffff;
        nonzero += u32::from(l != 0);
        nonfull += u32::from(l != 0xffff);
    }
    nonzero <= 1 || nonfull <= 1 || logical_imm(k, bits)
}

pub fn run(func: &mut Function, la: &LoopAnalysis, x64: bool) -> usize {
    let addr_remat = x64 || {
        static ALL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ALL.get_or_init(|| std::env::var("PLIRON_SPLINK_ADDR").as_deref() == Ok("1"))
    };
    if func.layout.blocks().count() > MAX_BLOCKS {
        return 0;
    }
    // splat/const inst -> set of blocks (other than its def block) using it.
    let mut uses: FxHashMap<Inst, FxHashSet<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let lb = la.innermost_loop(b);
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                let Some(d) = func.dfg.value_def(v).inst() else {
                    continue;
                };
                if func.layout.inst_block(d) == Some(b) {
                    continue;
                }
                if const_splat(func, d) {
                    uses.entry(d).or_default().insert(b);
                    continue;
                }
                // Scalar consts and rematerializable addresses only move
                // when the use's innermost loop excludes the def — that's
                // the shape that costs a spill or a loop-spanning live
                // range (and its boundary moves).
                if let (Some(lp), Some(db)) = (lb, func.layout.inst_block(d))
                    && !la.is_in_loop(db, lp)
                    && ((scalar_const(func, d) && cheap_scalar(func, d, x64))
                        || (addr_remat && remat_addr(func, d)))
                {
                    uses.entry(d).or_default().insert(b);
                }
            }
        }
    }
    let mut n = 0;
    for (d, bs) in uses {
        let old = func.dfg.resolve_aliases(func.dfg.first_result(d));
        for &b in &bs {
            let data = func.dfg.insts[d];
            let ni = func.dfg.make_inst(data);
            let ctv = func.dfg.ctrl_typevar(d);
            func.set_srcloc(ni, func.srcloc(d));
            func.dfg.make_inst_results(ni, ctv);
            match func.layout.first_inst(b) {
                Some(first) => func.layout.insert_inst(ni, first),
                None => func.layout.append_inst(ni, b),
            }
            let nv = func.dfg.first_result(ni);
            // Redirect this block's uses of `old` to the clone. Raw values
            // must be captured first: `map_inst_values`'s callback cannot
            // re-enter `dfg.resolve_aliases`.
            let mut raw: FxHashSet<Value> = FxHashSet::default();
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    if func.dfg.resolve_aliases(v) == old {
                        raw.insert(v);
                    }
                }
            }
            for i in func.layout.block_insts(b).collect::<Vec<_>>() {
                func.dfg
                    .map_inst_values(i, |v| if raw.contains(&v) { nv } else { v });
            }
            n += 1;
        }
        // Uses in the def block itself were left on `old`; if none exist
        // the splat is now dead — drop it so it doesn't emit a stray
        // `movi`/`dup`.
        let mut still_used = false;
        'outer: for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                if func
                    .dfg
                    .inst_values(i)
                    .any(|v| func.dfg.resolve_aliases(v) == old)
                {
                    still_used = true;
                    break 'outer;
                }
            }
        }
        if !still_used {
            func.layout.remove_inst(d);
            func.dfg.clear_results(d);
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::Endianness;
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::{
        AbiParam, BlockArg, InstBuilder, MemFlagsData, Signature, UserFuncName, types,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(func, &flags) {
            panic!("splink produced invalid IR: {e}\n{}", func.display());
        }
    }

    fn sink(func: &mut Function) -> usize {
        sink_at(func, false)
    }

    fn sink_at(func: &mut Function, x64: bool) -> usize {
        let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(func);
        let dt = cranelift_codegen::dominator_tree::DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        run(func, &la, x64)
    }

    fn little() -> MemFlagsData {
        MemFlagsData::new().with_endianness(Endianness::Little)
    }

    /// `splat(const)` defined in a dominator and used in a loop gets cloned
    /// into the loop block; the original dies when nothing local uses it.
    #[test]
    fn sinks_into_use_block() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let lp = func.dfg.make_block();
        let out = func.dfg.make_block();
        for b in [b0, lp, out] {
            func.layout.append_block(b);
        }
        let v0 = func.dfg.append_block_param(b0, types::I64);
        let iv = func.dfg.append_block_param(lp, types::I64);
        let ov = func.dfg.append_block_param(out, types::I64);

        let sp = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
            let c = cur.ins().iconst(types::I8, -1);
            let sp = cur.ins().splat(types::I8X8, c);
            cur.ins().jump(lp, &[BlockArg::Value(v0)]);
            sp
        };
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(lp);
            let w = cur.ins().bitcast(types::I64, little(), sp);
            let n2 = cur.ins().iadd(iv, w);
            let c = cur.ins().icmp(IntCC::UnsignedLessThan, n2, v0);
            cur.ins()
                .brif(c, lp, &[BlockArg::Value(n2)], out, &[BlockArg::Value(n2)]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(out);
            cur.ins().return_(&[ov]);
        }
        verify(&func);
        assert_eq!(sink(&mut func), 1);
        verify(&func);
        // Exactly one splat left in the whole function, inside the loop.
        let mut splats = 0;
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                if let InstructionData::Unary {
                    opcode: Opcode::Splat,
                    ..
                } = func.dfg.insts[i]
                {
                    splats += 1;
                    assert_eq!(func.layout.inst_block(i), Some(lp));
                }
            }
        }
        assert_eq!(splats, 1);
    }

    /// A splat of a non-constant operand is left alone.
    #[test]
    fn skips_nonconst_splat() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let b1 = func.dfg.make_block();
        for b in [b0, b1] {
            func.layout.append_block(b);
        }
        let v0 = func.dfg.append_block_param(b0, types::I64);
        let sp = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
            let x = cur.ins().ireduce(types::I8, v0);
            let sp = cur.ins().splat(types::I8X8, x);
            cur.ins().jump(b1, &[]);
            sp
        };
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b1);
            let w = cur.ins().bitcast(types::I64, little(), sp);
            cur.ins().return_(&[w]);
        }
        verify(&func);
        assert_eq!(sink(&mut func), 0);
        verify(&func);
    }

    /// A scalar `iconst` defined before a loop and used inside it clones
    /// into the loop block; a use outside any loop leaves it alone.
    #[test]
    fn sinks_scalar_into_loop_only() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let lp = func.dfg.make_block();
        let out = func.dfg.make_block();
        for b in [b0, lp, out] {
            func.layout.append_block(b);
        }
        let v0 = func.dfg.append_block_param(b0, types::I64);
        let iv = func.dfg.append_block_param(lp, types::I64);
        let ov = func.dfg.append_block_param(out, types::I64);

        let k = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
            let k = cur.ins().iconst(types::I64, 5000);
            cur.ins().jump(lp, &[BlockArg::Value(v0)]);
            k
        };
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(lp);
            let n2 = cur.ins().iadd(iv, k);
            let c = cur.ins().icmp(IntCC::UnsignedLessThan, n2, v0);
            cur.ins()
                .brif(c, lp, &[BlockArg::Value(n2)], out, &[BlockArg::Value(n2)]);
        }
        {
            // `k` also used in the exit block — outside the loop.
            let mut cur = FuncCursor::new(&mut func).at_bottom(out);
            let s = cur.ins().iadd(ov, k);
            cur.ins().return_(&[s]);
        }
        verify(&func);
        assert_eq!(sink(&mut func), 1);
        verify(&func);
        // Two `iconst`s: the clone in the loop block, plus the original
        // which the out-block use keeps alive.
        let mut counts = (0, 0);
        for i in func.layout.block_insts(lp) {
            if let InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                ..
            } = func.dfg.insts[i]
            {
                counts.0 += 1;
            }
        }
        for i in func.layout.block_insts(out) {
            if let InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                ..
            } = func.dfg.insts[i]
            {
                counts.1 += 1;
            }
        }
        assert_eq!(counts, (1, 0));
    }

    /// A wide `iconst` needing a movz+movk chain is NOT sunk on aarch64 —
    /// re-running the chain per iteration costs more than the hoisted
    /// def's worst-case spill — while a logical immediate still sinks
    /// (one `orr xN, xzr`), and x64's single-`movabs` sinks everything.
    #[test]
    fn movk_chain_stays_hoisted_on_aarch64() {
        let build = || {
            let mut sig = Signature::new(CallConv::Fast);
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
            let b0 = func.dfg.make_block();
            let lp = func.dfg.make_block();
            let out = func.dfg.make_block();
            for b in [b0, lp, out] {
                func.layout.append_block(b);
            }
            let v0 = func.dfg.append_block_param(b0, types::I64);
            let iv = func.dfg.append_block_param(lp, types::I64);
            let ov = func.dfg.append_block_param(out, types::I64);
            let (big, lgc) = {
                let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
                // 0x5851_f42d_4c95_7f2d — no single-inst aarch64 encoding.
                let big = cur.ins().iconst(types::I64, 0x5851_f42d_4c95_7f2d);
                // 48 contiguous low ones — `orr xN, xzr` logical imm.
                let lgc = cur.ins().iconst(types::I64, 0xffff_ffff_ffff);
                cur.ins().jump(lp, &[BlockArg::Value(v0)]);
                (big, lgc)
            };
            {
                let mut cur = FuncCursor::new(&mut func).at_bottom(lp);
                let a = cur.ins().iadd(iv, big);
                let b = cur.ins().bxor(a, lgc);
                let c = cur.ins().icmp(IntCC::UnsignedLessThan, b, v0);
                cur.ins()
                    .brif(c, lp, &[BlockArg::Value(b)], out, &[BlockArg::Value(b)]);
            }
            {
                let mut cur = FuncCursor::new(&mut func).at_bottom(out);
                cur.ins().return_(&[ov]);
            }
            (func, lp)
        };

        let (mut func, lp) = build();
        verify(&func);
        // aarch64: only the logical-immediate const sinks (1 inst).
        assert_eq!(sink(&mut func), 1);
        verify(&func);
        let mut in_loop = 0;
        for i in func.layout.block_insts(lp) {
            if let InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } = func.dfg.insts[i]
            {
                in_loop += 1;
                assert_eq!(i64::from(imm), 0xffff_ffff_ffff);
            }
        }
        assert_eq!(in_loop, 1);

        // x64: both sink (movabs is always one inst).
        let (mut func, _) = build();
        assert_eq!(sink_at(&mut func, true), 2);
        verify(&func);
    }
}
