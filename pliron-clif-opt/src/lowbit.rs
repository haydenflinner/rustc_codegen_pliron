//! Hacker's Delight `lowbit` idiom (`PLIRON_LOWBIT`, opt-in).
//!
//! ```text
//! lb = 1
//! while n % (lb * 2) == 0 { lb *= 2 }
//! ```
//! returns the least significant 1-bit of `n` — `n & -n` — computed by
//! the loop one `urem` per trailing zero. The rewrite swaps the loop
//! for `band(n, ineg n)`, the case study from "Efficient Extraction
//! for Effectful E-graphs" (OOPSLA'26) that neither LLVM nor GCC finds.
//!
//! Two inputs keep the original semantics the two-inst form loses, so
//! the loop stays as the slow path under a guard `n != 0 &&
//! n != 1<<(w-1)` — mirroring `loopidiom`'s runtime-check style:
//!   * `n == 0`: the divisibility test never fails (0 % k == 0); the
//!     loop runs until `lb*2` wraps to 0 and the divisor check traps.
//!   * `n == 1<<(w-1)` (i64::MIN / 2^63 etc.): the test that would exit
//!     divides by the wrapped `lb*2 == 0` — same trap.
//! For every other `n` the loop provably exits with `lb = 2^ctz(n)`
//! before the divisor wraps, and `band` returns the same value.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstructionData, Opcode, Type, Value, ValueDef,
};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::FxHashSet;

use crate::loopidiom::{self, Edge, Info, deepest_first, edge_args, edge_dest, gather, iconst};

/// Loops converted per function.
const MAX_CONV: usize = 4;

struct Plan {
    /// The single entry edge into the header (its terminator + slot).
    entry: Edge,
    /// Resolved args carried on that edge — the slow path jumps with
    /// them verbatim.
    entry_args: Vec<Value>,
    h: Block,
    /// The exiting terminator + slot and its dest.
    exit: Edge,
    exit_dest: Block,
    /// Args to pass on the fast path — `p` positions already swapped
    /// for the `lowbit` value slot marker `None`-> substituted at emit.
    exit_args: Vec<Value>,
    /// The `lb` header param, the invariant dividend `n` as a value
    /// available on the entry edge, and the int type.
    p: Value,
    n: Value,
    ty: Type,
}

/// `m` as the double of `p`: `imul p, 2`, `ishl p, 1`, or `iadd p, p`.
fn is_double(func: &Function, p: Value, m: Value) -> bool {
    let ValueDef::Result(i, _) = func.dfg.value_def(func.dfg.resolve_aliases(m)) else {
        return false;
    };
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return false;
    };
    let (a, b) = (
        func.dfg.resolve_aliases(args[0]),
        func.dfg.resolve_aliases(args[1]),
    );
    match opcode {
        Opcode::Imul => (a == p && iconst(func, b) == Some(2)) || (b == p && iconst(func, a) == Some(2)),
        Opcode::Ishl => a == p && iconst(func, b) == Some(1),
        Opcode::Iadd => a == p && b == p,
        _ => false,
    }
}

/// `v` loop-invariant in `info`'s view: defined outside the body, or a
/// header param the single latch feeds back unchanged.
fn invariant(func: &Function, info: &Info, v: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        ValueDef::Param(pb, idx) if pb == info.h => {
            info.latches.iter().all(|&l| {
                edge_args(func, l).get(idx).copied() == Some(v)
            })
        }
        ValueDef::Param(_, _) => true,
        ValueDef::Result(i, _) => func
            .layout
            .inst_block(i)
            .is_some_and(|b| !info.body.contains(&b)),
    }
}

/// The value `v` (an exit-edge arg) reads as on the entry edge — `None`
/// when it's a loop-computed value the fast path can't reproduce
/// (anything but a passthrough param or an outside def).
fn out_val(func: &Function, info: &Info, v: Value) -> Option<Value> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        ValueDef::Param(pb, idx) if pb == info.h => {
            if invariant(func, info, v) {
                info.entry_args.get(idx).copied()
            } else {
                None
            }
        }
        ValueDef::Param(_, _) => Some(v),
        ValueDef::Result(i, _) => {
            if func
                .layout
                .inst_block(i)
                .is_some_and(|b| !info.body.contains(&b))
            {
                Some(v)
            } else {
                None
            }
        }
    }
}

fn plan(func: &Function, info: &Info) -> Option<Plan> {
    if info.latches.len() != 1 {
        return None;
    }
    let latch_args = edge_args(func, info.latches[0]);
    // The exiting brif: `icmp cc (urem|srem n, m), 0` where the
    // continue-edge is taken iff `rem == 0` — `eq` when the stay slot
    // is 0, `ne` when it's 1.
    for &exit in &info.exits {
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[exit.inst] else {
            continue;
        };
        if blocks.len() != 2 || exit.slot > 1 {
            continue;
        }
        let stay_slot = 1 - exit.slot;
        if !info.body.contains(&blocks[stay_slot].block(&func.dfg.value_lists)) {
            continue;
        }
        let c = func.dfg.resolve_aliases(arg);
        let ValueDef::Result(ci, _) = func.dfg.value_def(c) else {
            continue;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: [a0, a1],
        } = func.dfg.insts[ci]
        else {
            continue;
        };
        // "stay iff rem == 0": slot0 staying wants Equal, slot1 NotEqual.
        let want = if stay_slot == 0 {
            IntCC::Equal
        } else {
            IntCC::NotEqual
        };
        if cond != want {
            continue;
        }
        let rem = if iconst(func, a1) == Some(0) {
            a0
        } else if iconst(func, a0) == Some(0) {
            a1
        } else {
            continue;
        };
        let ValueDef::Result(ri, _) = func.dfg.value_def(func.dfg.resolve_aliases(rem)) else {
            continue;
        };
        let InstructionData::Binary {
            opcode: Opcode::Urem | Opcode::Srem,
            args: [n0, m0],
        } = func.dfg.insts[ri]
        else {
            continue;
        };
        let n = func.dfg.resolve_aliases(n0);
        let m = func.dfg.resolve_aliases(m0);
        // The lb param: latch feeds `m` back to it and entry passes 1.
        let mut found = None;
        for (idx, &p) in func.dfg.block_params(info.h).iter().enumerate() {
            if latch_args.get(idx).copied().map(|v| func.dfg.resolve_aliases(v)) != Some(m) {
                continue;
            }
            if iconst(func, info.entry_args.get(idx).copied().unwrap_or(p)) != Some(1) {
                continue;
            }
            if !is_double(func, p, m) {
                continue;
            }
            found = Some((idx, p));
            break;
        }
        let Some((_idx, p)) = found else {
            continue;
        };
        let ty = func.dfg.value_type(p);
        if !ty.is_int() || ty.is_vector() || ty.bits() > 64 || func.dfg.value_type(n) != ty {
            continue;
        }
        if !invariant(func, info, n) {
            continue;
        }
        // `n` as a value on the entry edge: a param's entry arg, or the
        // outside def itself.
        let n_out = match func.dfg.value_def(n) {
            ValueDef::Param(pb, idx) if pb == info.h => *info.entry_args.get(idx)?,
            _ => n,
        };
        // Exit args: every position is `p` (becomes the lowbit value)
        // or loop-invariant (passes through out_val).
        let raw_exit_args = edge_args(func, exit);
        let mut exit_args = Vec::with_capacity(raw_exit_args.len());
        for &a in &raw_exit_args {
            let a = func.dfg.resolve_aliases(a);
            if a == p {
                exit_args.push(p); // marker: replaced by the computed value at emit
            } else {
                exit_args.push(out_val(func, info, a)?);
            }
        }
        // `p` may not be used outside the loop other than on this exit
        // edge — its def dominates only the body plus what the exit
        // dominates; outside uses we can't rewrite would dangle.
        let mut bad = false;
        for b in func.layout.blocks() {
            if info.body.contains(&b) {
                continue;
            }
            for i in func.layout.block_insts(b) {
                if func
                    .dfg
                    .inst_values(i)
                    .any(|w| func.dfg.resolve_aliases(w) == p)
                {
                    bad = true;
                    break;
                }
            }
            if bad {
                break;
            }
        }
        // `p` inside the exit inst's *other* slots / non-branch args
        // also dangles — count uses there too.
        if !bad {
            let ti = exit.inst;
            if func
                .dfg
                .inst_args(ti)
                .iter()
                .any(|&w| func.dfg.resolve_aliases(w) == p)
            {
                bad = true;
            }
            for (slot, bc) in func.dfg.insts[ti]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                if slot != exit.slot
                    && bc
                        .args(&func.dfg.value_lists)
                        .any(|x| x.as_value().is_some_and(|v| func.dfg.resolve_aliases(v) == p))
                {
                    bad = true;
                }
            }
        }
        if bad {
            continue;
        }
        return Some(Plan {
            entry: info.entry,
            entry_args: info.entry_args.clone(),
            h: info.h,
            exit,
            exit_dest: edge_dest(func, exit),
            exit_args,
            p,
            n: n_out,
            ty,
        });
    }
    None
}

fn apply(func: &mut Function, pl: &Plan) {
    let eb = func
        .layout
        .inst_block(pl.entry.inst)
        .expect("entry edge inst");
    let chk = func.dfg.make_block();
    func.layout.insert_block_after(chk, eb);
    let fast = func.dfg.make_block();
    func.layout.insert_block_after(fast, chk);
    if func.layout.is_cold(pl.h) {
        func.layout.set_cold(chk);
        func.layout.set_cold(fast);
    }
    // chk: `n != 0 && n != 1<<(w-1)` gates the fast path.
    let ok = {
        let mut pos = FuncCursor::new(func).at_bottom(chk);
        let z = pos.ins().iconst(pl.ty, 0);
        let top = pos.ins().iconst(pl.ty, 1i64 << (pl.ty.bits() - 1));
        let nz = pos.ins().icmp(IntCC::NotEqual, pl.n, z);
        let nt = pos.ins().icmp(IntCC::NotEqual, pl.n, top);
        pos.ins().band(nz, nt)
    };
    {
        let mut pos = FuncCursor::new(func).at_bottom(chk);
        let hargs: Vec<BlockArg> = pl.entry_args.iter().map(|&v| BlockArg::Value(v)).collect();
        pos.ins().brif(ok, fast, &[], pl.h, &hargs);
    }
    // fast: `band n, (ineg n)` then the exit edge's original args with
    // `p` positions swapped for it.
    {
        let mut pos = FuncCursor::new(func).at_bottom(fast);
        let neg = pos.ins().ineg(pl.n);
        let res = pos.ins().band(pl.n, neg);
        let args: Vec<BlockArg> = pl
            .exit_args
            .iter()
            .map(|&a| BlockArg::Value(if a == pl.p { res } else { a }))
            .collect();
        pos.ins().jump(pl.exit_dest, &args);
    }
    // The entry edge now enters `chk`.
    let dfg = &mut func.dfg;
    dfg.insts[pl.entry.inst].branch_destination_mut(
        &mut dfg.jump_tables,
        &mut dfg.exception_tables,
    )[pl.entry.slot] = BlockCall::new(chk, [], &mut dfg.value_lists);
}

pub fn run(func: &mut Function, an: &mut crate::pipeline::Analysis) -> usize {
    let mut n = 0;
    let mut done: FxHashSet<Block> = FxHashSet::default();
    'outer: for _ in 0..MAX_CONV {
        let crate::pipeline::CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
        for lp in deepest_first(la) {
            let h = la.loop_header(lp);
            if done.contains(&h) {
                continue;
            }
            let Some(info) = gather(func, cfg, dt, la, lp) else {
                continue;
            };
            let Some(pl) = plan(func, &info) else {
                continue;
            };
            tracing::debug!(target: "pliron::lowbit", "lowbit {:?} -> band/ineg", pl.h);
            apply(func, &pl);
            done.insert(h);
            n += 1;
            an.invalidate(func);
            continue 'outer;
        }
        break;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::types::I64;
    use cranelift_codegen::ir::{
        AbiParam, BlockArg, Function, InstBuilder, Opcode, Signature, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        cranelift_codegen::verify_function(func, &flags).unwrap();
    }

    /// The textbook shape: `lb=1; while n % (lb*2)==0 { lb*=2 }`.
    #[test]
    fn converts_lowbit_loop() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let h = func.dfg.make_block();
        let out = func.dfg.make_block();
        for bl in [entry, h, out] {
            func.layout.append_block(bl);
        }
        let n = func.dfg.append_block_param(entry, I64);
        let lb = func.dfg.append_block_param(h, I64);
        let hn = func.dfg.append_block_param(h, I64);
        let ro = func.dfg.append_block_param(out, I64);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
            let one = cur.ins().iconst(I64, 1);
            cur.ins()
                .jump(h, &[BlockArg::Value(one), BlockArg::Value(n)]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(h);
            let two = cur.ins().iconst(I64, 2);
            let lb2 = cur.ins().imul(lb, two);
            let r = cur.ins().urem(hn, lb2);
            let z = cur.ins().iconst(I64, 0);
            let c = cur.ins().icmp(IntCC::Equal, r, z);
            cur.ins()
                .brif(c, h, &[BlockArg::Value(lb2), BlockArg::Value(hn)], out, &[BlockArg::Value(lb)]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(out);
            cur.ins().return_(&[ro]);
        }
        verify(&func);
        let mut an = crate::pipeline::Analysis::default();
        assert_eq!(run(&mut func, &mut an), 1, "{}", func.display());
        verify(&func);
        // The fast path computes `band n, (ineg n)` off `n`.
        let found = func.layout.blocks().any(|b| {
            func.layout.block_insts(b).any(|i| {
                matches!(
                    func.dfg.insts[i],
                    InstructionData::Binary {
                        opcode: Opcode::Band,
                        ..
                    }
                ) && func
                    .dfg
                    .inst_args(i)
                    .iter()
                    .any(|&a| func.dfg.resolve_aliases(a) == n)
            })
        });
        assert!(found, "{}", func.display());
    }

    /// `lb0 = 3` doesn't match (result isn't `n & -n`).
    #[test]
    fn rejects_non_unit_seed() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let h = func.dfg.make_block();
        let out = func.dfg.make_block();
        for bl in [entry, h, out] {
            func.layout.append_block(bl);
        }
        let n = func.dfg.append_block_param(entry, I64);
        let lb = func.dfg.append_block_param(h, I64);
        let hn = func.dfg.append_block_param(h, I64);
        let ro = func.dfg.append_block_param(out, I64);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
            let three = cur.ins().iconst(I64, 3);
            cur.ins()
                .jump(h, &[BlockArg::Value(three), BlockArg::Value(n)]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(h);
            let two = cur.ins().iconst(I64, 2);
            let lb2 = cur.ins().imul(lb, two);
            let r = cur.ins().urem(hn, lb2);
            let z = cur.ins().iconst(I64, 0);
            let c = cur.ins().icmp(IntCC::Equal, r, z);
            cur.ins()
                .brif(c, h, &[BlockArg::Value(lb2), BlockArg::Value(hn)], out, &[BlockArg::Value(lb)]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(out);
            cur.ins().return_(&[ro]);
        }
        verify(&func);
        let mut an = crate::pipeline::Analysis::default();
        assert_eq!(run(&mut func, &mut an), 0, "{}", func.display());
        verify(&func);
    }
}
