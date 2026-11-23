//! 128-bit integer operations on word halves (`PLIRON_WIDE128`, default on
//! since 2026-11-18; `=0` off).
//!
//! `u64 x u64 -> u128` multiplies (foldhash's and ahash's
//! `folded_multiply`, `wyhash`), `(x >> 64) as u64` and `a | (b << 64)`
//! arrive as `i128` operations on `uextend`ed words, and the egraph sees
//! through none of them: lol_html's `Stack::build_state` (foldhash
//! inlined) kept a full 128-bit multiply (`mul`, `umulh`, two `madd`),
//! `isplit` / `iconcat x, 0` / `ireduce` round trips and `orr`s with a
//! zero register, where LLVM has `mul`, `umulh`, `eor`.
//!
//! Forward passes in layout order (repeated while they change something)
//! rewrite each `i128`
//! value whose halves are known into `iconcat lo, hi` of `i64` values,
//! and folds its uses:
//!
//! * `uextend.i128 x` (x: i64) -> `iconcat x, 0`
//! * `ishl c, 0|64`, `ushr c, 0|64` on `c = iconcat a, b` -> `c`,
//!   `iconcat 0, a`, `iconcat b, 0` (shift amounts from constants, also
//!   through `uextend`/`iconcat`)
//! * `bor|bxor|band c1, c2` -> `iconcat (op a1 a2), (op b1 b2)`
//! * `imul (iconcat a, 0), (iconcat b, 0)` -> `iconcat (imul a b),
//!   (umulhi a b)` where `umulhi.i64` is one instruction
//!   (`Target::mulhi64`)
//! * `ireduce.i64 c` -> `a`; `ireduce.iN c` -> `ireduce.iN a`;
//!   `isplit c` -> `a, b`
//!
//! A producer is rewritten only when every use of its result is one of
//! those folds, transitively (`halves_consumed`). An `iadd`/`icmp`/
//! branch keeps the value `i128`: splitting a multiply there inserts
//! `umulhi` whose high half stays live across the still-wide add
//! (pulldown `parse_block`'s widening overflow check, +19k loads with
//! the split). `ireduce` and `isplit` of an `iconcat` still fold; they
//! are the sinks the check stops at.
//!
//! An `imul` has an extra profitability gate (`hi_uses_light`): the
//! high half must be used at most twice after the split. foldhash's
//! `folded_multiply` takes `lo ^ hi` (one hi use); montgomery's
//! `mac_digit` loop isplits the product and reads both halves many
//! times, and the split there grew the function 680 → 789 instructions
//! with +9 weighted spills (blind-sig +2.9% with the ungated split).
//!
//! The replaced `i128` instructions become dead and the next
//! dead-code sweep drops them; block parameters stay `i128`.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, types};

fn def(func: &Function, v: Value) -> Option<(Inst, InstructionData)> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    Some((i, func.dfg.insts[i]))
}

/// The halves of `v` when it is `iconcat lo, hi`.
fn cat(func: &Function, v: Value) -> Option<(Value, Value)> {
    match def(func, v)?.1 {
        InstructionData::Binary { opcode: Opcode::Iconcat, args } => {
            Some((func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1])))
        }
        _ => None,
    }
}

/// An integer constant, also through `uextend` and the low half of an
/// `iconcat` whose high half is the constant zero.
fn konst(func: &Function, v: Value) -> Option<i64> {
    match def(func, v)?.1 {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        InstructionData::Unary { opcode: Opcode::Uextend, arg } => {
            let ty = func.dfg.value_type(arg);
            konst(func, arg).map(|k| if ty.bits() < 64 { k & ((1i64 << ty.bits()) - 1) } else { k })
        }
        InstructionData::Binary { opcode: Opcode::Iconcat, args } if konst(func, args[1]) == Some(0) => {
            konst(func, args[0])
        }
        // A shift amount masked to the width (`x << (n & 127)`) before the
        // egraph has folded the constant mask.
        InstructionData::Binary { opcode: Opcode::Band, args } => Some(konst(func, args[0])? & konst(func, args[1])?),
        _ => None,
    }
}

fn is_zero(func: &Function, v: Value) -> bool {
    konst(func, v) == Some(0)
}

/// Returns the number of instructions rewritten. Layout order is not
/// always dominance order (a cold block laid out last can define a value
/// a later-visited block uses), so the pass repeats while it changes
/// something, at most four times.
pub fn run(func: &mut Function, mulhi64: bool) -> usize {
    // montgomery is ~1.7k CLIF insts; foldhash's `build_state` is a few
    // hundred. Two uextend rewrites in montgomery cascade into +109
    // allocated instructions / +2.9% blind-sig. Skip big bodies.
    let fnmax: usize = crate::env_get("PLIRON_WIDE128_FNMAX")
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);
    let phi = crate::env_is("PLIRON_WIDE128_PHI", "1");
    // Over the cap, `PLIRON_WIDE128_PHI=1` still runs the param split and
    // the shift/bitwise folds, with no multiply split: lol_html's
    // `add_execution_branch` (~650 insts) builds a foldhash sponge from
    // shifts by 0 and 64, while montgomery's cascade comes from the
    // multiplies (+12.7% blind-sig when the cap alone was raised to 800).
    let mut mul = true;
    if fnmax > 0 {
        let insts = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b))
            .count();
        if insts > fnmax {
            if !phi {
                return 0;
            }
            mul = false;
        }
    }
    let mut n = 0;
    if phi {
        n += split_params(func, mul);
        // Over the cap, fold only where a param split opened a chain:
        // montgomery splits nothing (its params are 128-bit adds), and the
        // folds alone cascaded there (+13% blind-sig).
        if !mul && n == 0 {
            return 0;
        }
    }
    for _ in 0..4 {
        let k = pass(func, mulhi64, mul);
        n += k;
        if k == 0 {
            break;
        }
    }
    if n > 0 && std::env::var_os("PLIRON_WIDE128_STATS").is_some() {
        let name = func.name.to_string();
        eprintln!("wide128-stats {name}: rewrites={n}");
    }
    n
}

/// `v`'s uses are all ops this pass folds into `i64` halves, transitively.
/// `seen` is 0 unknown, 1 yes, 2 no, 3 on the stack (a cycle is not).
fn halves_consumed(
    func: &Function,
    uses: &crate::uses::Uses,
    v: Value,
    seen: &mut [u8],
    mul: bool,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    let i = v.index();
    if i >= seen.len() {
        return false;
    }
    match seen[i] {
        1 => return true,
        2 | 3 => return false,
        _ => {}
    }
    seen[i] = 3;
    let ok = uses.sites(v).iter().all(|&(_, inst)| {
        if func.layout.inst_block(inst).is_none() {
            return true;
        }
        let op = func.dfg.insts[inst].opcode();
        match op {
            Opcode::Ireduce | Opcode::Isplit => true,
            Opcode::Ishl | Opcode::Ushr => {
                let amt = func.dfg.inst_args(inst).get(1).copied();
                let Some(amt) = amt else { return false };
                // `PLIRON_WIDE128_VARSHIFT=1`: any amount, when `v` is the
                // shifted value (not the amount).
                let var = varshift_on()
                    && func.dfg.inst_args(inst).first().map(|&x| func.dfg.resolve_aliases(x)) == Some(v)
                    && func.dfg.resolve_aliases(amt) != v;
                match konst(func, amt) {
                    Some(k) if (k & 127) == 0 || (k & 127) == 64 => {}
                    Some(_) | None if var => {}
                    _ => return false,
                }
                let Some(r) = func.dfg.inst_results(inst).first().copied() else {
                    return false;
                };
                halves_consumed(func, uses, r, seen, mul)
            }
            // A `select` whose chosen values are `iconcat`s picks halves
            // (`PLIRON_WIDE128_PHI=1`); its condition is not a use of
            // `v`'s bits.
            Opcode::Select if phi_on() => {
                let a = func.dfg.inst_args(inst);
                if a.first().map(|&c| func.dfg.resolve_aliases(c)) == Some(v) {
                    return false;
                }
                halves_consumed(func, uses, func.dfg.first_result(inst), seen, mul)
            }
            Opcode::Imul if !mul => false,
            Opcode::Bor | Opcode::Bxor | Opcode::Band | Opcode::Imul => {
                let Some(r) = func.dfg.inst_results(inst).first().copied() else {
                    return false;
                };
                halves_consumed(func, uses, r, seen, mul)
            }
            _ => false,
        }
    });
    seen[i] = if ok { 1 } else { 2 };
    ok
}

/// How many times the high half of `v` would be read if `v` were an
/// `iconcat`. `ireduce` discards it; `isplit` / `ushr 64` / `ishl 64`
/// charge the uses of the half they expose; bitwise ops charge the hi
/// of their result. Anything else is "heavy" (a sentinel).
fn hi_use_count(func: &Function, uses: &crate::uses::Uses, v: Value, fuel: &mut usize) -> usize {
    if *fuel == 0 {
        return 999;
    }
    *fuel -= 1;
    let v = func.dfg.resolve_aliases(v);
    let mut n = 0usize;
    for &(_, inst) in uses.sites(v) {
        if func.layout.inst_block(inst).is_none() {
            continue;
        }
        let op = func.dfg.insts[inst].opcode();
        match op {
            Opcode::Ireduce => {}
            Opcode::Isplit => {
                let Some(hi) = func.dfg.inst_results(inst).get(1).copied() else {
                    return 999;
                };
                n = n.saturating_add(uses.sites(hi).len());
            }
            Opcode::Ushr => {
                let args = func.dfg.inst_args(inst);
                let Some(amt) = args.get(1).copied() else { return 999 };
                let Some(k) = konst(func, amt) else { return 999 };
                if k & 127 != 64 {
                    return 999;
                }
                let Some(r) = func.dfg.inst_results(inst).first().copied() else {
                    return 999;
                };
                // `ushr 64` of `iconcat a,b` is `iconcat b, 0`; counting
                // uses of `r` over-counts an `ireduce` of it as a hi use,
                // which is what we want (one).
                n = n.saturating_add(uses.sites(r).len());
            }
            Opcode::Ishl => {
                let args = func.dfg.inst_args(inst);
                let Some(amt) = args.get(1).copied() else { return 999 };
                let Some(k) = konst(func, amt) else { return 999 };
                if k & 127 != 64 {
                    return 999;
                }
                // `ishl 64` of `iconcat a,b` is `iconcat 0, a`: the new
                // hi is the old lo — not a hi use of `v`.
            }
            Opcode::Bor | Opcode::Bxor | Opcode::Band => {
                let Some(r) = func.dfg.inst_results(inst).first().copied() else {
                    return 999;
                };
                n = n.saturating_add(hi_use_count(func, uses, r, fuel));
            }
            _ => return 999,
        }
    }
    n
}

/// foldhash's `lo ^ hi` is one hi use; montgomery's mac step uses the
/// high half a few times per product and many products — `1` keeps the
/// fold and rejects the rest.
const IMUL_HI_USE_MAX: usize = 1;

fn hi_uses_light(func: &Function, uses: &crate::uses::Uses, v: Value) -> bool {
    let mut fuel = 64usize;
    hi_use_count(func, uses, v, &mut fuel) <= IMUL_HI_USE_MAX
}

/// A `uextend.i128` that only feeds a heavy-hi `imul` must stay: rewriting
/// it to `iconcat x, 0` lets Cranelift's own i128 legalizer emit `umulhi`
/// even when our imul rewrite refuses (montgomery).
fn uextend_ok(func: &Function, uses: &crate::uses::Uses, v: Value) -> bool {
    uses.sites(v).iter().all(|&(_, inst)| {
        if func.layout.inst_block(inst).is_none() {
            return true;
        }
        if func.dfg.insts[inst].opcode() != Opcode::Imul {
            return true;
        }
        let Some(r) = func.dfg.inst_results(inst).first().copied() else {
            return false;
        };
        hi_uses_light(func, uses, r)
    })
}

/// `mul`: `imul` splits (and `uextend`s feeding an `imul`) are allowed;
/// off for bodies over `PLIRON_WIDE128_FNMAX`, where only the shift and
/// bitwise folds run (`PLIRON_WIDE128_PHI=1`).
fn pass(func: &mut Function, mulhi64: bool, mul: bool) -> usize {
    let mulhi64 = mulhi64 && mul;
    let uses = crate::uses::Uses::new(func);
    let mut seen = vec![0u8; func.dfg.num_values()];
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            let data = pos.func.dfg.insts[inst];
            // Copy results out so the match guards can borrow `pos.func`
            // (`halves_consumed`) without overlapping this slice.
            let (res, res1) = {
                let results = pos.func.dfg.inst_results(inst);
                if results.is_empty() {
                    continue;
                }
                (results[0], results.get(1).copied())
            };
            let rty = pos.func.dfg.value_type(res);
            match data {
                InstructionData::Unary { opcode: Opcode::Uextend, arg }
                    if rty == types::I128
                        && pos.func.dfg.value_type(arg) == types::I64
                        && halves_consumed(pos.func, &uses, res, &mut seen, mul)
                        && uextend_ok(pos.func, &uses, res) =>
                {
                    let z = pos.ins().iconst(types::I64, 0);
                    pos.func.replace(inst).iconcat(arg, z);
                    n += 1;
                }
                InstructionData::Ternary { opcode: Opcode::Select, args }
                    if rty == types::I128 && phi_on() && halves_consumed(pos.func, &uses, res, &mut seen, mul) =>
                {
                    let (Some((a1, b1)), Some((a2, b2))) = (cat(pos.func, args[1]), cat(pos.func, args[2])) else { continue };
                    let lo = pos.ins().select(args[0], a1, a2);
                    let hi = pos.ins().select(args[0], b1, b2);
                    pos.func.replace(inst).iconcat(lo, hi);
                    n += 1;
                }
                InstructionData::Binary { opcode: op @ (Opcode::Ishl | Opcode::Ushr), args }
                    if rty == types::I128
                        && konst(pos.func, args[1]).is_some()
                        && halves_consumed(pos.func, &uses, res, &mut seen, mul) =>
                {
                    let Some((a, b)) = cat(pos.func, args[0]) else { continue };
                    let Some(k) = konst(pos.func, args[1]) else { continue };
                    // CLIF masks i128 shift amounts to 7 bits.
                    match (op, k & 127) {
                        (_, 0) => {
                            let src = pos.func.dfg.resolve_aliases(args[0]);
                            pos.func.replace(inst).iconcat(a, b);
                            let _ = src;
                        }
                        (Opcode::Ishl, 64) => {
                            let z = pos.ins().iconst(types::I64, 0);
                            pos.func.replace(inst).iconcat(z, a);
                        }
                        (Opcode::Ushr, 64) => {
                            let z = pos.ins().iconst(types::I64, 0);
                            pos.func.replace(inst).iconcat(b, z);
                        }
                        _ => continue,
                    }
                    n += 1;
                }
                // `PLIRON_WIDE128_VARSHIFT=1`: a shift by an unknown amount
                // `s` (CLIF masks it to 7 bits), as aarch64 lowers it: the
                // bits crossing the halves are `(a >> 1) >> !s` (zero for
                // `s = 0` without a select), and `s & 64` picks the
                // swapped form.
                InstructionData::Binary { opcode: op @ (Opcode::Ishl | Opcode::Ushr), args }
                    if rty == types::I128
                        && varshift_on()
                        && konst(pos.func, args[1]).is_none()
                        && halves_consumed(pos.func, &uses, res, &mut seen, mul) =>
                {
                    let Some((a, b)) = cat(pos.func, args[0]) else { continue };
                    let Some(s) = amount64(&mut pos, args[1]) else { continue };
                    let ns = pos.ins().bnot(s);
                    let big = pos.ins().band_imm_u(s, 64);
                    let z = pos.ins().iconst(types::I64, 0);
                    let (lo, hi) = if op == Opcode::Ishl {
                        let lo_s = pos.ins().ishl(a, s);
                        let b_s = pos.ins().ishl(b, s);
                        let a1 = pos.ins().ushr_imm(a, 1);
                        let cross = pos.ins().ushr(a1, ns);
                        let hi_s = pos.ins().bor(b_s, cross);
                        let lo = pos.ins().select(big, z, lo_s);
                        let hi = pos.ins().select(big, lo_s, hi_s);
                        (lo, hi)
                    } else {
                        let hi_s = pos.ins().ushr(b, s);
                        let a_s = pos.ins().ushr(a, s);
                        let b1 = pos.ins().ishl_imm(b, 1);
                        let cross = pos.ins().ishl(b1, ns);
                        let lo_s = pos.ins().bor(a_s, cross);
                        let lo = pos.ins().select(big, hi_s, lo_s);
                        let hi = pos.ins().select(big, z, hi_s);
                        (lo, hi)
                    };
                    pos.func.replace(inst).iconcat(lo, hi);
                    n += 1;
                }
                InstructionData::Binary { opcode: op @ (Opcode::Bor | Opcode::Bxor | Opcode::Band), args }
                    if rty == types::I128 && halves_consumed(pos.func, &uses, res, &mut seen, mul) =>
                {
                    let (Some((a1, b1)), Some((a2, b2))) = (cat(pos.func, args[0]), cat(pos.func, args[1])) else {
                        continue;
                    };
                    let (lo, hi) = match op {
                        Opcode::Bor => (pos.ins().bor(a1, a2), pos.ins().bor(b1, b2)),
                        Opcode::Bxor => (pos.ins().bxor(a1, a2), pos.ins().bxor(b1, b2)),
                        _ => (pos.ins().band(a1, a2), pos.ins().band(b1, b2)),
                    };
                    pos.func.replace(inst).iconcat(lo, hi);
                    n += 1;
                }
                // Only where `umulhi.i64` is one instruction
                // (`Target::mulhi64`): on wasm32 it is a long sequence and
                // the multiply stays a `__multi3` call (dragon/grisu grew).
                InstructionData::Binary { opcode: Opcode::Imul, args }
                    if rty == types::I128
                        && mulhi64
                        && halves_consumed(pos.func, &uses, res, &mut seen, mul)
                        && hi_uses_light(pos.func, &uses, res) =>
                {
                    let (Some((a, ah)), Some((b, bh))) = (cat(pos.func, args[0]), cat(pos.func, args[1])) else {
                        continue;
                    };
                    if !is_zero(pos.func, ah) || !is_zero(pos.func, bh) {
                        continue;
                    }
                    let lo = pos.ins().imul(a, b);
                    let hi = pos.ins().umulhi(a, b);
                    pos.func.replace(inst).iconcat(lo, hi);
                    n += 1;
                }
                InstructionData::Unary { opcode: Opcode::Ireduce, arg }
                    if pos.func.dfg.value_type(arg) == types::I128 =>
                {
                    let Some((a, _)) = cat(pos.func, arg) else { continue };
                    if rty == types::I64 {
                        pos.func.dfg.clear_results(inst);
                        pos.remove_inst_and_step_back();
                        pos.func.dfg.change_to_alias(res, a);
                    } else {
                        pos.func.replace(inst).ireduce(rty, a);
                    }
                    n += 1;
                }
                InstructionData::Unary { opcode: Opcode::Isplit, arg } => {
                    let Some((a, b)) = cat(pos.func, arg) else { continue };
                    let (r0, r1) = (res, res1.expect("isplit has two results"));
                    pos.func.dfg.clear_results(inst);
                    pos.remove_inst_and_step_back();
                    pos.func.dfg.change_to_alias(r0, a);
                    pos.func.dfg.change_to_alias(r1, b);
                    n += 1;
                }
                _ => {}
            }
        }
    }
    n
}

/// `PLIRON_WIDE128_PHI=1`: an `i128` block param whose incoming edges are
/// all `jump`/`brif` arguments becomes two `i64` params. Each edge passes
/// `isplit arg` and the block rebuilds `iconcat lo, hi` at its top, which
/// the folds above see through. foldhash's `u128` sponge (`sponge |= x <<
/// sponge_len`, flushed when full) reaches the join after each write as a
/// block param, and that one non-fold use kept the whole chain `i128`:
/// lol_html's `add_execution_branch` shifted by the constants 0 and 64
/// with the generic `lsl/lsr/orr/tst/csel` sequence. Returns the params
/// split.
fn split_params(func: &mut Function, under_cap: bool) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    // Which params: every eligible one (`PLIRON_WIDE128_PHI_ALL`, default
    // on: blind-sig -0.365% and html's sponge loop -0.13% more than the
    // foldable ones alone), but over the size cap only in a function with
    // at least one foldable param (montgomery has none and cascades).
    let all = !crate::env_is("PLIRON_WIDE128_PHI_ALL", "0");
    let keep = foldable_params(func, &cfg);
    if !under_cap && keep.is_empty() {
        return 0;
    }
    let mut n = 0;
    for b in blocks {
        if Some(b) == entry {
            continue;
        }
        let wide: Vec<(usize, Value)> = func
            .dfg
            .block_params(b)
            .iter()
            .enumerate()
            .filter(|(_, p)| func.dfg.value_type(**p) == types::I128 && (all || keep.contains(p)))
            .map(|(i, p)| (i, *p))
            .collect();
        if wide.is_empty() || func.layout.first_inst(b).is_none() {
            continue;
        }
        // Every edge into `b`: a `jump`/`brif` destination whose args are
        // plain values.
        let mut edges: Vec<(Inst, usize)> = Vec::new();
        let mut ok = true;
        for pr in cfg.pred_iter(b) {
            if !matches!(func.dfg.insts[pr.inst].opcode(), Opcode::Jump | Opcode::Brif) {
                ok = false;
                break;
            }
            let dests = func.dfg.insts[pr.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            for (d, bc) in dests.iter().enumerate() {
                if bc.block(&func.dfg.value_lists) == b {
                    if !bc.args(&func.dfg.value_lists).all(|a| matches!(a, BlockArg::Value(_))) {
                        ok = false;
                    }
                    edges.push((pr.inst, d));
                }
            }
        }
        if !ok || edges.is_empty() {
            continue;
        }
        edges.sort();
        edges.dedup();
        // Highest index first: removing an argument keeps the lower ones in
        // place; the halves go to the end, params and args in one order.
        for &(ix, p) in wide.iter().rev() {
            if !pliron_cfg::bisect("wide128-phi") {
                continue;
            }
            let lo = func.dfg.append_block_param(b, types::I64);
            let hi = func.dfg.append_block_param(b, types::I64);
            for &(t, d) in &edges {
                let arg = {
                    let bc = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
                    match bc.args(&func.dfg.value_lists).nth(ix) {
                        Some(BlockArg::Value(v)) => v,
                        _ => unreachable!("checked above"),
                    }
                };
                let (alo, ahi) = FuncCursor::new(func).at_inst(t).ins().isplit(arg);
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
                bc.remove(ix, &mut dfg.value_lists);
                bc.append_argument(alo, &mut dfg.value_lists);
                bc.append_argument(ahi, &mut dfg.value_lists);
            }
            let first = func.layout.first_inst(b).unwrap();
            let cv = FuncCursor::new(func).at_inst(first).ins().iconcat(lo, hi);
            func.dfg.remove_block_param(p);
            func.dfg.change_to_alias(p, cv);
            n += 1;
        }
    }
    n
}

/// The `i128` block params worth splitting: every incoming argument is
/// built from halves (`uextend` of a word, `iconcat`, a shift by 0 or 64,
/// a bitwise op of such, or another kept param) and every use is a fold
/// (`ireduce`, `isplit`, such a shift or bitwise op) or an argument to a
/// kept param. A param fed by a real 128-bit add (montgomery's carry)
/// stays: splitting it only wraps the add in `isplit`/`iconcat` (+12.7%
/// blind-sig when every param was split). A greatest fixpoint: start from
/// all params, drop violators until nothing changes.
fn foldable_params(func: &Function, cfg: &ControlFlowGraph) -> rustc_hash::FxHashSet<Value> {
    use rustc_hash::FxHashSet;
    let entry = func.layout.entry_block();
    // Incoming args per param, and the params each value is passed to.
    let mut args_of: rustc_hash::FxHashMap<Value, Vec<Value>> = Default::default();
    let mut cand: FxHashSet<Value> = FxHashSet::default();
    for b in func.layout.blocks() {
        if Some(b) == entry {
            continue;
        }
        let params = func.dfg.block_params(b);
        if !params.iter().any(|p| func.dfg.value_type(*p) == types::I128) {
            continue;
        }
        let mut ok = true;
        let mut per: Vec<Vec<Value>> = vec![Vec::new(); params.len()];
        for pr in cfg.pred_iter(b) {
            if !matches!(func.dfg.insts[pr.inst].opcode(), Opcode::Jump | Opcode::Brif) {
                ok = false;
                break;
            }
            for bc in func.dfg.insts[pr.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                for (k, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    match a {
                        BlockArg::Value(v) if k < per.len() => per[k].push(func.dfg.resolve_aliases(v)),
                        _ => ok = false,
                    }
                }
            }
        }
        if !ok {
            continue;
        }
        for (k, &p) in params.iter().enumerate() {
            if func.dfg.value_type(p) == types::I128 {
                cand.insert(p);
                args_of.insert(p, std::mem::take(&mut per[k]));
            }
        }
    }
    if cand.is_empty() {
        return cand;
    }
    let uses = crate::uses::Uses::new(func);
    // `v` is built from halves, through at most `depth` producers.
    fn built(func: &Function, cand: &FxHashSet<Value>, v: Value, depth: u32) -> bool {
        let v = func.dfg.resolve_aliases(v);
        if cand.contains(&v) {
            return true;
        }
        if depth == 0 {
            return false;
        }
        let Some((_, d)) = def(func, v) else { return false };
        match d {
            InstructionData::Unary { opcode: Opcode::Uextend, arg } => func.dfg.value_type(arg) == types::I64,
            InstructionData::Binary { opcode: Opcode::Iconcat, .. } => true,
            InstructionData::Binary { opcode: Opcode::Ishl | Opcode::Ushr, args } => {
                konst(func, args[1]).is_some_and(|k| k & 63 == 0) && built(func, cand, args[0], depth - 1)
            }
            InstructionData::Binary { opcode: Opcode::Bor | Opcode::Bxor | Opcode::Band, args } => {
                built(func, cand, args[0], depth - 1) && built(func, cand, args[1], depth - 1)
            }
            InstructionData::Ternary { opcode: Opcode::Select, args } => {
                built(func, cand, args[1], depth - 1) && built(func, cand, args[2], depth - 1)
            }
            _ => false,
        }
    }
    // Every use of `v` folds or passes it to a kept param.
    fn folds(func: &Function, uses: &crate::uses::Uses, cand: &FxHashSet<Value>, v: Value, depth: u32) -> bool {
        if depth == 0 {
            return false;
        }
        uses.sites(func.dfg.resolve_aliases(v)).iter().all(|&(_, inst)| {
            if func.layout.inst_block(inst).is_none() {
                return true;
            }
            let d = &func.dfg.insts[inst];
            match d.opcode() {
                Opcode::Ireduce | Opcode::Isplit => true,
                Opcode::Ishl | Opcode::Ushr => {
                    let a = func.dfg.inst_args(inst);
                    a.len() == 2
                        && konst(func, a[1]).is_some_and(|k| k & 63 == 0)
                        && folds(func, uses, cand, func.dfg.first_result(inst), depth - 1)
                }
                Opcode::Bor | Opcode::Bxor | Opcode::Band => folds(func, uses, cand, func.dfg.first_result(inst), depth - 1),
                Opcode::Select => {
                    let a = func.dfg.inst_args(inst);
                    a.first().map(|&c| func.dfg.resolve_aliases(c)) != Some(func.dfg.resolve_aliases(v))
                        && folds(func, uses, cand, func.dfg.first_result(inst), depth - 1)
                }
                Opcode::Jump | Opcode::Brif => {
                    // Every param this value reaches must be kept.
                    let v = func.dfg.resolve_aliases(v);
                    d.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables).iter().all(|bc| {
                        let b = bc.block(&func.dfg.value_lists);
                        bc.args(&func.dfg.value_lists).enumerate().all(|(k, a)| match a {
                            BlockArg::Value(x) if func.dfg.resolve_aliases(x) == v => {
                                func.dfg.block_params(b).get(k).is_some_and(|p| cand.contains(p))
                            }
                            _ => true,
                        })
                    })
                }
                _ => false,
            }
        })
    }
    for _ in 0..8 {
        let drop: Vec<Value> = cand
            .iter()
            .copied()
            .filter(|&p| {
                !args_of.get(&p).is_some_and(|a| a.iter().all(|&v| built(func, &cand, v, 8)))
                    || !folds(func, &uses, &cand, p, 8)
            })
            .collect();
        if drop.is_empty() {
            break;
        }
        for p in drop {
            cand.remove(&p);
        }
    }
    cand
}

fn varshift_on() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_WIDE128_VARSHIFT", "1"));
    *ON
}

/// A shift amount as an `i64` (only its low 7 bits matter): through a
/// `uextend.i128` or the low half of an `iconcat`, else extended or reduced.
fn amount64(pos: &mut FuncCursor, v: Value) -> Option<Value> {
    let v = pos.func.dfg.resolve_aliases(v);
    let src = match def(pos.func, v).map(|d| d.1) {
        Some(InstructionData::Unary { opcode: Opcode::Uextend, arg }) if pos.func.dfg.value_type(v) == types::I128 => arg,
        Some(InstructionData::Binary { opcode: Opcode::Iconcat, args }) => args[0],
        _ => v,
    };
    let src = pos.func.dfg.resolve_aliases(src);
    let ty = pos.func.dfg.value_type(src);
    Some(match ty.bits() {
        64 => src,
        b if b < 64 && ty.is_int() => pos.ins().uextend(types::I64, src),
        128 => pos.ins().ireduce(types::I64, src),
        _ => return None,
    })
}

fn phi_on() -> bool {
    crate::env_is("PLIRON_WIDE128_PHI", "1")
}
