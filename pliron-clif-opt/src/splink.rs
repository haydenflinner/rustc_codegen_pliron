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
//! Only `splat` is handled: regalloc already remats scalar `iconst`s, and
//! anything else is not clearly cheaper than the reload it replaces.

use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Opcode, Value};
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

pub fn run(func: &mut Function) -> usize {
    if func.layout.blocks().count() > MAX_BLOCKS {
        return 0;
    }
    // splat inst -> set of blocks (other than its def block) using it.
    let mut uses: FxHashMap<Inst, FxHashSet<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                let Some(d) = func.dfg.value_def(v).inst() else {
                    continue;
                };
                if func.layout.inst_block(d) == Some(b) || !const_splat(func, d) {
                    continue;
                }
                uses.entry(d).or_default().insert(b);
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
        assert_eq!(run(&mut func), 1);
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
        assert_eq!(run(&mut func), 0);
        verify(&func);
    }
}
