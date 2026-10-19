//! Hacker's Delight `lowbit` idiom (`PLIRON_LOWBIT=0` disables).
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
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value,
    ValueDef,
};
use rustc_hash::FxHashSet;

use crate::loopidiom::{Edge, Info, deepest_first, edge_args, edge_dest, gather, iconst};

/// Loops converted per function.
const MAX_CONV: usize = 4;

struct Plan {
    /// The single entry edge into the header (its terminator + slot).
    entry: Edge,
    /// Resolved args carried on that edge — the slow path jumps with
    /// them verbatim.
    entry_args: Vec<Value>,
    h: Block,
    /// The exiting edge — needed at emit to append `p` when `exit_dest`
    /// gets a new param.
    exit: Edge,
    /// The exit edge's destination.
    exit_dest: Block,
    /// Args to pass on the fast path — positions reading `p` are
    /// swapped for the computed `band` at emit.
    exit_args: Vec<Value>,
    /// Out-of-loop insts whose args read `p` (or an alias of it), all
    /// dominated by `exit_dest`. When non-empty `exit_dest` is
    /// single-pred and gets a new param `r`; both incoming edges pass
    /// the lb value and these args rewrite to `r`. rustc's `while`
    /// lowering ends the loop in `return <alias of lb>` — a direct SSA
    /// use rather than an edge arg — so without this the idiom never
    /// matches real code.
    patch: Vec<(Inst, Vec<Value>)>,
    /// The `lb` header param, the invariant dividend `n` as a value
    /// available on the entry edge, and the int type.
    p: Value,
    n: Value,
    ty: Type,
}

/// Small constant evaluator for loop-entry seeds: `iconst`, or one
/// `imul`/`ishl`/`iadd` of constants — LICM's param-splitting leaves
/// the seeds as unfolded `imul 1, 2` / `imul m0, 2` trees.
fn const_val(func: &Function, v: Value, depth: u8) -> Option<i64> {
    if depth == 0 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    if let Some(k) = iconst(func, v) {
        return Some(k);
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return None;
    };
    let a = const_val(func, args[0], depth - 1)?;
    let b = const_val(func, args[1], depth - 1)?;
    match opcode {
        Opcode::Imul => Some(a.wrapping_mul(b)),
        Opcode::Ishl => (0..64)
            .contains(&b)
            .then(|| a.wrapping_shl(b as u32)),
        Opcode::Iadd => Some(a.wrapping_add(b)),
        _ => None,
    }
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
        Opcode::Imul => {
            (a == p && const_val(func, b, 2) == Some(2))
                || (b == p && const_val(func, a, 2) == Some(2))
        }
        Opcode::Ishl => a == p && const_val(func, b, 2) == Some(1),
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
        _ => false,
    }
}

/// `x` is a header param seeded `seed` that doubles every iteration.
/// The latch hands back `x*2` directly, or another param carrying it —
/// LICM splits the carry across params (`p' = m`, `m' = m2`,
/// `m2' = m2*2`), so the chain recurses with a doubled expected seed.
fn doubling_param(
    func: &Function,
    info: &Info,
    latch_args: &[Value],
    x: Value,
    seed: i64,
    seen: &mut FxHashSet<Value>,
) -> bool {
    let ValueDef::Param(b, idx) = func.dfg.value_def(x) else {
        return false;
    };
    if b != info.h || !seen.insert(x) {
        return false;
    }
    if const_val(func, info.entry_args.get(idx).copied().unwrap_or(x), 4) != Some(seed) {
        return false;
    }
    let Some(&la) = latch_args.get(idx) else {
        return false;
    };
    let la = func.dfg.resolve_aliases(la);
    is_double(func, x, la) || doubling_param(func, info, latch_args, la, seed * 2, seen)
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
        _ => None,
    }
}

fn plan(func: &Function, info: &Info, cfg: &ControlFlowGraph, dt: &DominatorTree) -> Option<Plan> {
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
        // The lb param `p`: entry passes 1 and the latch hands back the
        // next double — either the divisor `m` itself or a recomputed
        // `p*2`. The divisor in turn is `p*2` either as an inst
        // (`imul p, 2`) or — after LICM carries it — as a doubling
        // header param seeded 2 (then `p' = m` gives `p = 2^k`).
        let m_is_double_of = |p: Value| -> bool {
            is_double(func, p, m)
                || doubling_param(func, info, &latch_args, m, 2, &mut FxHashSet::default())
        };
        let mut found = None;
        for (idx, &p) in func.dfg.block_params(info.h).iter().enumerate() {
            let Some(la) = latch_args.get(idx).copied() else {
                continue;
            };
            let la = func.dfg.resolve_aliases(la);
            if la != m && !is_double(func, p, la) {
                continue;
            }
            if const_val(func, info.entry_args.get(idx).copied().unwrap_or(p), 4) != Some(1) {
                continue;
            }
            if !m_is_double_of(p) {
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
        let exit_dest = edge_dest(func, exit);
        // After the rewrite the header no longer dominates `exit_dest`,
        // so any value defined in the loop — a header param or a body
        // inst result — read outside the body would dangle. `p` reads
        // in blocks `exit_dest` dominates get a new `exit_dest` param
        // (single-pred required — see `apply`); every other loop-def
        // escape, or a `p` read we can't reach, rejects.
        let mut bad = false;
        let mut patch: Vec<(Inst, Vec<Value>)> = Vec::new();
        let single_pred = cfg.pred_iter(exit_dest).count() == 1;
        'scan: for b in func.layout.blocks() {
            if info.body.contains(&b) {
                continue;
            }
            for i in func.layout.block_insts(b) {
                let mut ws: Vec<Value> = Vec::new();
                for w in func.dfg.inst_values(i) {
                    let wr = func.dfg.resolve_aliases(w);
                    if wr == p {
                        ws.push(w);
                        continue;
                    }
                    let loop_def = match func.dfg.value_def(wr) {
                        ValueDef::Param(pb, _) => pb == info.h,
                        ValueDef::Result(di, _) => func
                            .layout
                            .inst_block(di)
                            .is_some_and(|db| info.body.contains(&db)),
                        ValueDef::Union(_, _) => true,
                    };
                    if loop_def {
                        bad = true;
                        break 'scan;
                    }
                }
                if ws.is_empty() {
                    continue;
                }
                if single_pred && dt.dominates(exit_dest, b, &func.layout) {
                    patch.push((i, ws));
                } else {
                    bad = true;
                    break 'scan;
                }
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
            exit_dest,
            exit_args,
            patch,
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
    // `lb` read directly in `exit_dest`'s dominated region (rustc's
    // `return <alias>` shape): `exit_dest` is single-pred, so append a
    // param `r` — the loop edge passes `p`, the fast path `res` — and
    // point the dominated uses at `r`.
    let r = if pl.patch.is_empty() {
        None
    } else {
        let r = func.dfg.append_block_param(pl.exit_dest, pl.ty);
        {
            let dfg = &mut func.dfg;
            dfg.insts[pl.exit.inst]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                [pl.exit.slot]
                .append_argument(BlockArg::Value(pl.p), &mut dfg.value_lists);
        }
        for &(i, ref ws) in &pl.patch {
            let mut data = func.dfg.insts[i];
            {
                let dfg = &mut func.dfg;
                data.map_values(
                    &mut dfg.value_lists,
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                    |v| if ws.contains(&v) { r } else { v },
                );
            }
            func.dfg.insts[i] = data;
        }
        Some(r)
    };
    // fast: `band n, (ineg n)` then the exit edge's original args with
    // `p` positions swapped for it.
    {
        let mut pos = FuncCursor::new(func).at_bottom(fast);
        let neg = pos.ins().ineg(pl.n);
        let res = pos.ins().band(pl.n, neg);
        let mut args: Vec<BlockArg> = pl
            .exit_args
            .iter()
            .map(|&a| BlockArg::Value(if a == pl.p { res } else { a }))
            .collect();
        if r.is_some() {
            args.push(BlockArg::Value(res));
        }
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
            let Some(pl) = plan(func, &info, cfg, dt) else {
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

    /// The shape rustc+MIR actually emits: the divisor is carried as a
    /// second header param (`p' = m`, `m' = m*2`) and `lb` escapes as a
    /// direct use in the single-pred exit block (`return v3`), not an
    /// edge arg — the rewrite paramizes the exit block.
    #[test]
    fn converts_carried_divisor_loop() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let h = func.dfg.make_block();
        let rem = func.dfg.make_block();
        let latch = func.dfg.make_block();
        let out = func.dfg.make_block();
        for bl in [entry, h, rem, latch, out] {
            func.layout.append_block(bl);
        }
        let n = func.dfg.append_block_param(entry, I64);
        let lb = func.dfg.append_block_param(h, I64);
        let m = func.dfg.append_block_param(h, I64);
        let m2 = func.dfg.append_block_param(h, I64);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
            let one = cur.ins().iconst(I64, 1);
            let two = cur.ins().iconst(I64, 2);
            let four = cur.ins().iconst(I64, 4);
            cur.ins().jump(
                h,
                &[
                    BlockArg::Value(one),
                    BlockArg::Value(two),
                    BlockArg::Value(four),
                ],
            );
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(h);
            cur.ins().jump(rem, &[]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(rem);
            let r = cur.ins().urem(n, m);
            let z = cur.ins().iconst(I64, 0);
            let c = cur.ins().icmp(IntCC::Equal, r, z);
            cur.ins().brif(c, latch, &[], out, &[]);
        }
        {
            // latch: `lb' = m`, `m' = m2`, `m2' = m2*2`.
            let mut cur = FuncCursor::new(&mut func).at_bottom(latch);
            let two = cur.ins().iconst(I64, 2);
            let m3 = cur.ins().imul(m2, two);
            cur.ins().jump(
                h,
                &[
                    BlockArg::Value(m),
                    BlockArg::Value(m2),
                    BlockArg::Value(m3),
                ],
            );
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(out);
            cur.ins().return_(&[lb]);
        }
        verify(&func);
        let mut an = crate::pipeline::Analysis::default();
        assert_eq!(run(&mut func, &mut an), 1, "{}", func.display());
        verify(&func);
        // `out` gained a param and its `return` reads it.
        assert_eq!(func.dfg.num_block_params(out), 1, "{}", func.display());
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
