//! Constant sinking — move rematerializable constant defs down to the
//! deepest block that still dominates all their uses
//! (`PLIRON_CONSTSINK=1`, opt-in).
//!
//! `constdedup` and the block-cloning passes leave one `iconst` per
//! distinct constant at the dominating def, often the entry block. A
//! constant whose uses all sit in a deep bail-out block then pins a
//! register for the whole function: encoding_rs's `utf8_valid_up_to`
//! held constants 0..=8 in `x2..x11` from its entry on, which pushed the
//! ASCII stride loop's working set into five callee-saved pairs —
//! ~20 instructions of banked `mov`s plus `stp`/`ldp` per call where
//! LLVM materializes `mov w11, #k` inside each cold exit and spills
//! nothing.
//!
//! Rule: for every `iconst`/`f32const`/`f64const` def, find the nearest
//! common dominator of its use blocks and, when that is below the def's
//! block, move the def there (top of the block, or just ahead of the
//! first use when the uses are all in the def's own block). When the
//! uses are scattered over sibling subtrees (their NCD is the def's
//! own block), clone a copy into each use block instead — the same
//! per-site rematerialization LLVM performs: uses that fold into an
//! immediate operand cost nothing at isel, and the rest get their
//! `mov` in the block that actually runs it. Either way the live range
//! only shrinks.
//!
//! Loop rule: use blocks sharing one innermost loop get a single
//! shared copy at their common dominator inside the loop, not one
//! copy per block — several uses on the same dynamic path would each
//! pay a materialization per trip otherwise.
//!
//! Guard: no materialization lands in a deeper loop nest than the
//! def's block — `licm` hoists loop invariants on purpose, and
//! re-sinking one pays its materialization per trip of the inner loop
//! (regex's `cached_state` sank a `mov #1<<32` from an outer DFA block
//! into an inner slice loop, doubling its executed count: +215K movs).
//! One exception: a block whose uses are *all* cheap still gets its
//! own copy — cheap meaning a terminator edge arg that rides a rare
//! transition (LLVM's per-edge remat; utf8's bail preds — the block
//! must dominate no latch of its innermost loop, and the edge must
//! leave the loop or feed a latch) or, when the backend's immediate
//! encodings are known (`fold_imm`, aarch64), an operand that folds
//! into the instruction (`add #16`, `cmp #0`, `and #0x8080..`) and so
//! costs nothing wherever the copy lands. A use on the loop's main
//! path is not cheap: the copy lands at the block's top and pays its
//! materialization per iteration. Uses deeper than the def that are
//! neither keep the banked def.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::uses::Uses;

const MAX_BLOCKS: usize = 16384;

fn is_const(func: &Function, i: Inst) -> bool {
    matches!(
        func.dfg.insts[i],
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            ..
        } | InstructionData::UnaryIeee32 { .. }
            | InstructionData::UnaryIeee64 { .. }
    )
}

/// Dominator-tree depth of `b` (entry = 0), memoized over the whole run.
fn depth(
    b: Block,
    dt: &DominatorTree,
    memo: &mut FxHashMap<Block, u32>,
) -> Option<u32> {
    if let Some(&d) = memo.get(&b) {
        return Some(d);
    }
    let d = match dt.idom(b) {
        Some(p) if p != b => depth(p, dt, memo)? + 1,
        _ => 0,
    };
    memo.insert(b, d);
    Some(d)
}

/// Nearest common dominator of a set of blocks (themselves included).
fn ncd(
    mut a: Block,
    mut b: Block,
    dt: &DominatorTree,
    memo: &mut FxHashMap<Block, u32>,
) -> Option<Block> {
    let mut da = depth(a, dt, memo)?;
    let mut db = depth(b, dt, memo)?;
    while da > db {
        a = dt.idom(a)?;
        da -= 1;
    }
    while db > da {
        b = dt.idom(b)?;
        db -= 1;
    }
    while a != b {
        a = dt.idom(a)?;
        b = dt.idom(b)?;
    }
    Some(a)
}

// aarch64 immediate encodability for the foldable-use test below.
// cranelift's own `Imm12`/`ImmLogic` helpers live behind its `arm64`
// feature, which this crate does not build — these are the same
// checks re-expressed directly.
//
// `enc_add` covers add/sub/cmp (`Imm12`: a 12-bit value optionally
// shifted left 12 — or the negated value, `cmp x, #-16` lowers to
// `cmn`).
fn enc_add(v: u64) -> bool {
    let imm12 = |x: u64| x & !0xfff == 0 || x & !(0xfff << 12) == 0;
    imm12(v) || imm12(v.wrapping_neg())
}

// `enc_log` covers `band`/`bor`/`bxor` bitmask immediates: the
// encoding is an `esize`-periodic pattern (esize in 2..=64) that is
// a rotated contiguous run of ones, length 1..=esize-1. Narrow
// logical ops lower through the 32-bit form, so try both extensions.
fn enc_log(v: u64, ty: Type) -> bool {
    if ty.bits() <= 32 {
        let v32 = v as u32 as u64;
        if enc_log_bits(v32 | (v32 << 32)) {
            return true;
        }
        if ty.bits() < 32 {
            // A narrow logical op lowers through the 32-bit form and
            // its constant may arrive sign-extended — try that too.
            let s = 64 - ty.bits();
            let sext = (((v << s) as i64) >> s) as u32 as u64;
            return enc_log_bits(sext | (sext << 32));
        }
        return false;
    }
    enc_log_bits(v)
}

fn enc_log_bits(v: u64) -> bool {
    let mut esize = 64u32;
    while esize >= 2 {
        let m = if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 };
        let e = v & m;
        // `u64::MAX / m` is the geometric-series multiplier that
        // repeats an `esize`-bit value to 64 bits.
        if v == e.wrapping_mul(u64::MAX / m) {
            for r in 0..esize {
                // Rotate `e` right by `r` within its element.
                let x = (e >> r) | e.wrapping_shl(esize - r) & m;
                if x != 0 && x != m && (x & (x + 1)) == 0 {
                    return true;
                }
            }
        }
        esize >>= 1;
    }
    false
}

/// Does `u`'s use of `res` (an `iconst` of `bits` in `ty`) fold into
/// an aarch64 immediate operand at lowering? Only the shapes checked
/// here; every other use keeps the banked def. For noncommutative ops
/// the constant must sit in the RHS arg — `isub c, x` and `cmp c, x`
/// have no immediate encoding (which is what keeps regex's hot-loop
/// `mov #1<<32` banked).
fn folds_imm(func: &Function, res: Value, bits: u64, ty: Type, u: Inst) -> bool {
    let (args, enc, rhs_only) = match func.dfg.insts[u] {
        InstructionData::Binary { opcode, args } => match opcode {
            Opcode::Iadd => (args, enc_add(bits), false),
            Opcode::Isub => (args, enc_add(bits), true),
            Opcode::Band | Opcode::Bor | Opcode::Bxor => (args, enc_log(bits, ty), false),
            Opcode::Ishl | Opcode::Ushr | Opcode::Sshr | Opcode::Rotl | Opcode::Rotr => {
                (args, bits < 64, true)
            }
            _ => return false,
        },
        InstructionData::IntCompare { args, .. } => (args, enc_add(bits), true),
        _ => return false,
    };
    let mut any = false;
    for (k, &v) in args.iter().enumerate() {
        if func.dfg.resolve_aliases(v) == res {
            if rhs_only && k != 1 {
                return false;
            }
            any = true;
        }
    }
    any && enc
}

/// A terminator use of `res` in block `ub` is cheap only when the
/// const rides a rare transition, not the loop's main flow: `ub`
/// must dominate no latch (back-edge source) of its innermost loop —
/// every trip through a dominated back edge ran `ub` first (regex's
/// `next_state`, +305K `movz`s) — and every edge carrying `res` must
/// leave that loop or feed a latch (utf8's bail `jump block276`; an
/// edge into a regular in-loop block — regex's `c_utf8_seq`, +129K —
/// pays the copy per trip). The clone lands at the use block's top,
/// so this is a frequency proxy for what LLVM's per-edge remat does
/// with a profile. A selector arg (`brif` cond, `br_table` index)
/// always needs the register and never counts cheap.
fn term_cheap(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    res: Value,
    u: Inst,
    ub: Block,
) -> bool {
    let d = &func.dfg.insts[u];
    match d {
        InstructionData::Brif { arg, .. } | InstructionData::BranchTable { arg, .. } => {
            if func.dfg.resolve_aliases(*arg) == res {
                return false;
            }
        }
        // `res` as a call or return operand of a terminator (`try_call`
        // counts as one: it has exception edges): the value goes in a
        // register per execution, like any other non-edge use — never
        // the free edge-arg remat this routine prices. brotli's
        // `SetCost(model, 704, .., 0)` inside `BrotliCreateHqZopfli-
        // BackwardReferences`'s loop cloned a per-trip `mov` into its
        // call arms (+358K program-wide before this arm).
        InstructionData::TryCall { args, .. }
        | InstructionData::TryCallIndirect { args, .. }
        | InstructionData::MultiAry { args, .. } => {
            if args
                .as_slice(&func.dfg.value_lists)
                .iter()
                .any(|&a| func.dfg.resolve_aliases(a) == res)
            {
                return false;
            }
        }
        _ => {}
    }
    // The latches of `ub`'s innermost loop: back-edge sources, the
    // preds of the header dominated by it.
    let mut latches: FxHashSet<Block> = FxHashSet::default();
    if let Some(l) = la.innermost_loop(ub) {
        let h = la.loop_header(l);
        for p in cfg.pred_iter(h) {
            let pb = p.block;
            if dt.dominates(h, pb, &func.layout) {
                latches.insert(pb);
            }
        }
        // `ub` dominates a latch: every trip through that back edge
        // ran `ub` first — the copy pays the loop's main path
        // (regex's `next_state`, `c_utf8_seq`).
        if latches
            .iter()
            .any(|&p| dt.dominates(ub, p, &func.layout))
        {
            return false;
        }
        // Nor may a carrying edge feed a latch: that routes the copy
        // into the back-edge flow, so it materializes every trip the
        // same way (`c_utf8_seq`'s `iconst` args into latch blocks —
        // +103K). An edge into a plain in-loop block is fine — those
        // are the rare transition paths that pay for this
        // (encoding_rs's utf8 bail edge `jump block276`).
        if !d
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .all(|bc| {
                let carries = bc.args(&func.dfg.value_lists).any(|a| match a {
                    cranelift_codegen::ir::BlockArg::Value(v) => {
                        func.dfg.resolve_aliases(v) == res
                    }
                    _ => false,
                });
                !carries || !latches.contains(&bc.block(&func.dfg.value_lists))
            })
        {
            return false;
        }
    }
    true
}

/// Is `ub` off its innermost loop's cyclic path — it dominates no
/// latch, so no trip is guaranteed to run it (utf8's bail preds). A
/// block on the main path dominates a back-edge source: every trip
/// through the loop ran it (Zopfli's loop bodies). The block-level
/// version of `term_cheap`'s test.
fn off_path(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    ub: Block,
) -> bool {
    if let Some(l) = la.innermost_loop(ub) {
        let h = la.loop_header(l);
        for p in cfg.pred_iter(h) {
            let pb = p.block;
            if dt.dominates(h, pb, &func.layout) && dt.dominates(ub, pb, &func.layout) {
                return false;
            }
        }
    }
    true
}

/// `fold_imm`: the backend's immediate encodings are known (aarch64),
/// so an operand that provably folds counts as a cheap deeper use.
pub fn run(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    uses: &Uses,
    fold_imm: bool,
) -> usize {
    if func.layout.num_blocks() > MAX_BLOCKS {
        return 0;
    }
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    if !dt.is_reachable(entry) {
        return 0;
    }
    let cands: Vec<(Inst, Block)> = func
        .layout
        .blocks()
        .flat_map(|b| {
            func.layout
                .block_insts(b)
                .filter(|&i| is_const(func, i))
                .map(|i| (i, b))
                .collect::<Vec<_>>()
        })
        .collect();
    if cands.is_empty() {
        return 0;
    }
    let mut memo: FxHashMap<Block, u32> = FxHashMap::default();
    let mut n = 0;
    for (i, home) in cands {
        let res = func.dfg.resolve_aliases(func.dfg.first_result(i));
        let sites = uses.sites(res);
        if sites.is_empty() {
            continue;
        }
        let home_level = la.loop_level(home).level();
        // `sites` is in layout order, so same-block sites are contiguous
        // and inst-ordered: group them, keeping each block's earliest
        // use first.
        let mut groups: Vec<(Block, Vec<Inst>)> = Vec::new();
        for &(b, u) in sites {
            match groups.last_mut() {
                Some((gb, us)) if *gb == b => us.push(u),
                _ => groups.push((b, vec![u])),
            }
        }
        // Whether any use actually consumes the constant as a register
        // operand decides both the move below and every clone further
        // down, so the fold machinery is defined ahead of both.
        let id = func.dfg.insts[i];
        let ty = func.dfg.value_type(res);
        // The constant's value masked to its type, for the foldable
        // half of the cheap-use test below (`f32const`/`f64const`
        // never fold into an immediate operand).
        let cbits = match id {
            InstructionData::UnaryImm { imm, .. } => {
                let m = if ty.bits() >= 64 {
                    u64::MAX
                } else {
                    (1u64 << ty.bits()) - 1
                };
                Some(imm.bits() as u64 & m)
            }
            _ => None,
        };
        let cheap_use = |func: &Function, ub: Block, u: Inst| -> bool {
            if func.dfg.insts[u].opcode().is_terminator() {
                return term_cheap(func, cfg, dt, la, res, u, ub);
            }
            fold_imm && cbits.is_some_and(|bits| folds_imm(func, res, bits, ty, u))
        };
        // A use whose constant folds into an immediate is cheap
        // wherever the def sits — the banked def serves it for free
        // too. Only a genuine edge-arg use (a cheap terminator under
        // `term_cheap`) gains anything from a closer copy: it needs
        // the constant in a register, and the clone moves that
        // materialization onto a rare path.
        let edge_use = |func: &Function, ub: Block, u: Inst| -> bool {
            func.dfg.insts[u].opcode().is_terminator() && cheap_use(func, ub, u)
        };
        // Does a use need the constant in a register at all? A
        // non-cheap operand does; a cheap terminator arg does (the
        // edge carries it); a fold-immediate use does not — the
        // banked def serves it for free already, so relocating or
        // cloning for it alone buys nothing and only perturbs the
        // register allocation (brotli's Zopfli loops shuffled every
        // in-loop live range over fold-imm clones of `2`/`1`/`128`,
        // +336K executed as reloads and per-trip remats).
        let materializes = |func: &Function, ub: Block, u: Inst| -> bool {
            !cheap_use(func, ub, u) || edge_use(func, ub, u)
        };
        let home_group = groups.iter().position(|&(b, _)| b == home);

        if home_group.is_none() {
            // Every use sits strictly below: the deepest position still
            // dominating them all is their NCD (a branch-arg use counts
            // at its own block — the def must dominate the edge's
            // tail). Moving there never adds an instruction, but an
            // all-fold graph has nothing reading the banked register —
            // leave the def put.
            let mut target = groups[0].0;
            let mut ok = groups
                .iter()
                .any(|&(b, ref us)| us.iter().any(|&ui| materializes(func, b, ui)));
            for &(b, _) in &groups[1..] {
                match ncd(target, b, dt, &mut memo) {
                    Some(t) => target = t,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            // Never push a materialization into a deeper loop nest.
            if ok && target != home && la.loop_level(target).level() <= home_level {
                if let Some(anchor) = func.layout.first_inst(target) {
                    func.layout.remove_inst(i);
                    func.layout.insert_inst(i, anchor);
                    n += 1;
                    continue;
                }
            }
        }

        // The uses either share the def's own block or are scattered
        // under sibling subtrees: rematerialize a copy per use site,
        // just ahead of the site's earliest use. Use args reach `res`
        // possibly through aliases; collect the raw spellings once so
        // the rewrite can match them directly.
        let mut raw: FxHashSet<Value> = FxHashSet::default();
        for &(_, u) in sites {
            raw.extend(
                func.dfg
                    .inst_values(u)
                    .filter(|&v| func.dfg.resolve_aliases(v) == res),
            );
        }
        let remat = |func: &mut Function, anchor: Inst| {
            let mut cur = FuncCursor::new(func).at_inst(anchor);
            match id {
                InstructionData::UnaryImm { imm, .. } => cur.ins().iconst(ty, imm.bits()),
                InstructionData::UnaryIeee32 { imm, .. } => cur.ins().f32const(imm),
                InstructionData::UnaryIeee64 { imm, .. } => cur.ins().f64const(imm),
                _ => unreachable!("is_const filtered the candidates"),
            }
        };
        // Partition the other use blocks by innermost loop. Top-level
        // uses clone per block; blocks sharing one loop share a single
        // copy at their common dominator so one trip can never pay more
        // materializations than the banked def would.
        let mut by_loop: FxHashMap<Option<Loop>, Vec<usize>> = FxHashMap::default();
        for (gi, &(u, _)) in groups.iter().enumerate() {
            if u != home {
                by_loop
                    .entry(la.innermost_loop(u))
                    .or_default()
                    .push(gi);
            }
        }
        // A group is served when a copy lands where it replaces a use
        // that reads the constant in a register — or where the copy's
        // materialization can never cost a trip of the loop's main
        // path. Same- or shallower-level blocks clone only when a use
        // materializes the register (a fold-only group's clone is
        // pure allocation noise); deeper blocks clone only when the
        // block is off its loop's cyclic path — it dominates no latch
        // (`off_path`) or is `cold` — and every use there is cheap,
        // so the copy rides a rare transition (utf8's bail preds).
        let mut cloned = false;
        let mut served = 0;
        for (lp, gis) in by_loop {
            // A multi-block loop group's copies merge at the group's
            // common dominator (always inside the loop: its header
            // dominates every member). Anything else — a lone block,
            // top-level uses, a failed or `home` NCD — keeps one copy
            // per use block.
            let shared = if lp.is_some() && gis.len() > 1 {
                let mut t = groups[gis[0]].0;
                let mut ok = t != home;
                for &gi in &gis[1..] {
                    match ncd(t, groups[gi].0, dt, &mut memo) {
                        Some(x) => t = x,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                (ok && t != home).then_some(t)
            } else {
                None
            };
            match shared {
                Some(t)
                    if la.loop_level(t).level() <= home_level
                        && gis.iter().any(|&gi| {
                            groups[gi]
                                .1
                                .iter()
                                .any(|&ui| materializes(func, groups[gi].0, ui))
                        }) =>
                {
                    // If the dominator is itself a use block, land
                    // ahead of its earliest use; else at its top.
                    let anchor = gis
                        .iter()
                        .find(|&&gi| groups[gi].0 == t)
                        .map(|&gi| groups[gi].1[0])
                        .or_else(|| func.layout.first_inst(t));
                    let Some(anchor) = anchor else { continue };
                    let nv = remat(func, anchor);
                    for &gi in &gis {
                        for &ui in &groups[gi].1 {
                            func.dfg.map_inst_values(ui, |v| {
                                if raw.contains(&v) { nv } else { v }
                            });
                        }
                    }
                    cloned = true;
                    served += gis.len();
                }
                _ => {
                    // A failed/deeper NCD falls back to one copy per
                    // use block (still guarded below).
                    for &gi in &gis {
                        let (u, ref us) = groups[gi];
                        // Never materialize into a deeper loop nest
                        // than the def sits in — unless the block is
                        // off its loop's cyclic path
                        // (`off_path`/`cold`) and every use there is
                        // cheap. A clone on the main path pays a
                        // `mov` per trip on top of the banked
                        // register that still reaches the rest; a
                        // real register use (a call arg, `isub`'s
                        // LHS) pays the copy's `mov` whenever the
                        // block does run (brotli's Zopfli inner loops
                        // bled +236K as reloads/remats; `SetCost`'s
                        // call arm +358K). The banked def keeps
                        // serving the rest.
                        if la.loop_level(u).level() > home_level {
                            if !us.iter().all(|&ui| cheap_use(func, u, ui))
                                || !(off_path(func, cfg, dt, la, u)
                                    || func.layout.is_cold(u))
                            {
                                continue;
                            }
                        } else if !us.iter().any(|&ui| materializes(func, u, ui)) {
                            // Same- or shallower-level block where
                            // every use folds the constant into an
                            // immediate: the banked def already serves
                            // those for free, so the clone only
                            // renames a dead register and perturbs
                            // allocation.
                            continue;
                        }
                        let nv = remat(func, us[0]);
                        for &ui in &groups[gi].1 {
                            func.dfg.map_inst_values(ui, |v| {
                                if raw.contains(&v) { nv } else { v }
                            });
                        }
                        served += 1;
                        cloned = true;
                    }
                }
            }
        }
        let all_served = served == groups.len() - usize::from(home_group.is_some());
        if let Some(hg) = home_group {
            // The def keeps serving its in-block uses: slide it down
            // to the first of them.
            let first_use = groups[hg].1[0];
            if func.layout.next_inst(i) != Some(first_use) {
                func.layout.remove_inst(i);
                func.layout.insert_inst(i, first_use);
                cloned = true;
            }
        } else if all_served {
            // Every use got a closer copy: the banked def is dead.
            func.layout.remove_inst(i);
            func.dfg.clear_results(i);
        }
        n += usize::from(cloned);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    use cranelift_codegen::ir::{AbiParam, BlockArg, InstBuilder, Signature, UserFuncName, types};
    use cranelift_codegen::isa::CallConv;

    fn sink(func: &mut Function) -> usize {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        let uses = Uses::new(func);
        run(func, &cfg, &dt, &la, &uses, true)
    }

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(func, &flags) {
            panic!("constsink produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// The block holding `v`'s def.
    fn home(func: &Function, v: cranelift_codegen::ir::Value) -> Option<Block> {
        func.dfg
            .value_def(func.dfg.resolve_aliases(v))
            .inst()
            .and_then(|i| func.layout.inst_block(i))
    }

    /// `iconst` defined in block0, used only in the block2 arm: sinks to
    /// block2's top.
    #[test]
    fn sinks_to_single_use_block() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let b1 = func.dfg.make_block();
        let b2 = func.dfg.make_block();
        for b in [b0, b1, b2] {
            func.layout.append_block(b);
        }
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let c = cur.ins().iconst(types::I64, 5);
        let hot = cur.ins().iconst(types::I64, 0);
        let t = cur.ins().icmp_imm(cranelift_codegen::ir::condcodes::IntCC::Equal, hot, 0);
        cur.ins().brif(t, b1, &[], b2, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(b1);
        cur.ins().return_(&[hot]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(b2);
        // `c` on the `isub`'s LHS has no immediate encoding — the use
        // needs the constant in a register.
        let s = cur.ins().isub(c, hot);
        cur.ins().return_(&[s]);

        sink(&mut func);
        assert_eq!(home(&func, c), Some(b2));
        verify(&func);
    }

    /// Uses in two blocks dominated by a shared region head: the def
    /// sinks to the region head, not the entry.
    #[test]
    fn sinks_to_ncd() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let a = func.dfg.make_block();
        let l = func.dfg.make_block();
        let r = func.dfg.make_block();
        for b in [b0, a, l, r] {
            func.layout.append_block(b);
        }
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let c = cur.ins().iconst(types::I64, 9);
        let k = cur.ins().iconst(types::I64, 0);
        let t = cur.ins().icmp_imm(cranelift_codegen::ir::condcodes::IntCC::Equal, k, 0);
        cur.ins().brif(t, a, &[], r, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(a);
        // `c` on the `isub` LHS needs a register: `isub c, x` cannot
        // fold into an immediate operand.
        let s = cur.ins().isub(c, k);
        cur.ins().jump(l, &[BlockArg::Value(s)]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(r);
        cur.ins().return_(&[k]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(l);
        let p = cur.func.dfg.append_block_param(l, types::I64);
        let s2 = cur.ins().isub(c, p);
        cur.ins().return_(&[s2]);

        // Uses are in `a` (isub) and `l` (isub); NCD = a.
        sink(&mut func);
        assert_eq!(home(&func, c), Some(a));
        verify(&func);
    }

    /// A constant used on both arms of a brif — uses' NCD is the def's
    /// own block — is rematerialized per arm: each gets a local copy at
    /// its first use and the banked def dies.
    #[test]
    fn remats_per_scattered_use() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let l = func.dfg.make_block();
        let r = func.dfg.make_block();
        for b in [b0, l, r] {
            func.layout.append_block(b);
        }
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let c = cur.ins().iconst(types::I64, 5);
        let k = cur.ins().iconst(types::I64, 0);
        let t = cur.ins().icmp_imm(cranelift_codegen::ir::condcodes::IntCC::Equal, k, 0);
        cur.ins().brif(t, l, &[], r, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(l);
        // `c` on the `isub` LHS needs a register (no `isub c, x`
        // immediate encoding); `k` on the RHS folds.
        let s = cur.ins().isub(c, k);
        cur.ins().return_(&[s]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(r);
        let s = cur.ins().isub(c, c);
        cur.ins().return_(&[s]);

        sink(&mut func);
        // `c`'s def is dead; each arm defines its own `iconst 5`.
        assert_eq!(home(&func, c), None);
        let iconsts = |b: Block| {
            func.layout
                .block_insts(b)
                .filter(|&i| {
                    matches!(
                        func.dfg.insts[i],
                        InstructionData::UnaryImm {
                            opcode: Opcode::Iconst,
                            ..
                        }
                    )
                })
                .count()
        };
        // `l` gets a copy of `c` only: its use of `k` folds into the
        // `isub`'s immediate operand, so `k`'s banked def serves it for
        // free and no clone is made.
        assert_eq!(iconsts(l), 1);
        assert_eq!(iconsts(r), 1);
        assert_eq!(iconsts(b0), 2); // `c` is gone; `k` and `icmp`'s own iconst stay
        verify(&func);
    }

    /// A loop-invariant constant whose use cannot fold into an
    /// immediate operand is never materialized inside the loop — not
    /// moved, not cloned: re-sinking it would pay one `mov` per trip
    /// where the hoisted def pays one per function. `1<<32` fits no
    /// `Imm12`/`ImmLogic` encoding (regex's `cached_state` compared
    /// against it in a hot loop). The def stays at the entry and
    /// keeps serving the loop uses.
    #[test]
    fn never_materializes_nonfolding_use_into_deeper_loop() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let head = func.dfg.make_block();
        let out = func.dfg.make_block();
        for b in [b0, head, out] {
            func.layout.append_block(b);
        }
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let c = cur.ins().iconst(types::I64, 1 << 32);
        let zero = cur.ins().iconst(types::I64, 0);
        cur.ins().jump(head, &[BlockArg::Value(zero)]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(head);
        let iv = cur.func.dfg.append_block_param(head, types::I64);
        let step = cur.ins().iadd(iv, c);
        let cond = cur.ins().icmp_imm(cranelift_codegen::ir::condcodes::IntCC::UnsignedLessThan, iv, 10);
        cur.ins().brif(cond, head, &[BlockArg::Value(step)], out, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(out);
        cur.ins().return_(&[iv]);

        // `c` is used only inside `head` (level 1) from a def in `b0`
        // (level 0): `add x, x, #1<<32` has no immediate encoding, so
        // the use is not cheap — the def neither moves nor clones into
        // the deeper nest, it stays banked at the entry.
        sink(&mut func);
        assert_eq!(home(&func, c), Some(b0));
        let copies = func
            .layout
            .block_insts(head)
            .filter(|&i| {
                matches!(
                    func.dfg.insts[i],
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                        ..
                    } if imm.bits() == 1 << 32
                )
            })
            .count();
        assert_eq!(copies, 0, "no materialization inside the loop");
        verify(&func);
    }

    /// The mirror image: a loop-invariant constant whose deeper use
    /// folds into an immediate operand keeps its banked def — the fold
    /// encodes the constant anyway, so a copy would emit nothing and
    /// only perturb the register allocation (brotli's Zopfli loops,
    /// +336K executed). The one deeper use worth a copy is an edge arg
    /// on a rare path (encoding_rs's utf8 bail edge): it needs the
    /// constant in a register, and killing the banked def shortens its
    /// live range out of the loop.
    #[test]
    fn deeper_edge_arg_use_gets_a_copy() {
        let clif = "
function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 5
    v9 = iconst.i64 10
    v8 = iconst.i64 1
    jump block1(v0)
block1(v10: i64):
    v2 = icmp ult v10, v9
    brif v2, block2, block3
block2:
    v3 = icmp ult v10, v8
    brif v3, block5(v1), block3
block3:
    v4 = iadd v10, v8
    jump block1(v4)
block5(v11: i64):
    return v11
}
";
        let mut func = cranelift_reader::parse_functions(clif).unwrap().remove(0);
        let v1 = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b))
            .find(|&i| {
                matches!(
                    func.dfg.insts[i],
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                        ..
                    } if imm.bits() == 5
                )
            })
            .map(|i| func.dfg.first_result(i))
            .unwrap();
        sink(&mut func);
        // `block2` carries `v1` onto the rare `block5` exit edge (it
        // dominates no latch and the edge feeds none): the clone kills
        // the banked def, while the other constants' loop uses all
        // fold and keep theirs.
        assert_eq!(home(&func, v1), None);
        let fives: Vec<Block> = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b).map(move |i| (b, i)))
            .filter(|&(_, i)| {
                matches!(
                    func.dfg.insts[i],
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                        ..
                    } if imm.bits() == 5
                )
            })
            .map(|(b, _)| b)
            .collect();
        let blocks: Vec<Block> = func.layout.blocks().collect();
        assert_eq!(fives, vec![blocks[2]], "one clone in block2");
        verify(&func);
    }

    /// A constant whose uses all fold into immediate operands is left
    /// exactly where the allocator left it — not moved to the uses'
    /// NCD, not cloned per block. The fold encodes the constant
    /// either way, so relocation buys nothing, while every dead copy
    /// and every moved def perturbs the register allocation
    /// (brotli's Zopfli loops paid +336K executed in reloads and
    /// per-trip remats over such clones of `2`/`1`/`128`).
    #[test]
    fn fold_imm_only_uses_neither_move_nor_clone() {
        let clif = "
function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 2
    v8 = iconst.i64 0xffff
    v9 = iconst.i64 128
    v2 = icmp ult v0, v9
    brif v2, block1, block2
block1:
    v3 = ishl v0, v1
    v5 = band v3, v8
    return v5
block2:
    v4 = band v0, v8
    return v4
}
";
        let mut func = cranelift_reader::parse_functions(clif).unwrap().remove(0);
        let find = |func: &Function, bits: u64| {
            func.layout
                .blocks()
                .flat_map(|b| func.layout.block_insts(b))
                .find(|&i| {
                    matches!(
                        func.dfg.insts[i],
                        InstructionData::UnaryImm {
                            opcode: Opcode::Iconst,
                            imm,
                            ..
                        } if imm.bits() as u64 == bits
                    )
                })
                .map(|i| func.dfg.first_result(i))
                .unwrap()
        };
        let (v1, v8) = (find(&func, 2), find(&func, 0xffff));
        sink(&mut func);
        let blocks: Vec<Block> = func.layout.blocks().collect();
        // `ishl`'s shift amount and `band`'s mask both encode in the
        // instruction: `v1`'s single fold use cannot pull the def down
        // to `block1`, and `v8`'s two fold uses get no clones — the
        // banked defs stay in `block0`.
        assert_eq!(home(&func, v1), Some(blocks[0]));
        assert_eq!(home(&func, v8), Some(blocks[0]));
        let copies = |func: &Function, bits: u64| -> Vec<Block> {
            func.layout
                .blocks()
                .flat_map(|b| func.layout.block_insts(b).map(move |i| (b, i)))
                .filter(|&(_, i)| {
                    matches!(
                        func.dfg.insts[i],
                        InstructionData::UnaryImm {
                            opcode: Opcode::Iconst,
                            imm,
                            ..
                        } if imm.bits() as u64 == bits
                    )
                })
                .map(|(b, _)| b)
                .collect()
        };
        assert_eq!(copies(&func, 2), vec![blocks[0]]);
        assert_eq!(copies(&func, 0xffff), vec![blocks[0]]);
        verify(&func);
    }

    /// Two use blocks inside the same loop share one rematerialized
    /// copy at their common dominator — one materialization per trip
    /// at most, never one per block. (regex's `cached_state` doubled a
    /// `mov #1<<32` inside its DFA loop before this rule.)
    #[test]
    fn same_loop_uses_share_one_copy() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let head = func.dfg.make_block();
        let mid = func.dfg.make_block();
        let then = func.dfg.make_block();
        let tail = func.dfg.make_block();
        let out = func.dfg.make_block();
        for b in [b0, head, mid, then, tail, out] {
            func.layout.append_block(b);
        }
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let zero = cur.ins().iconst(types::I64, 0);
        cur.ins().jump(head, &[BlockArg::Value(zero)]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(head);
        let iv = cur.func.dfg.append_block_param(head, types::I64);
        // `c` is defined inside the loop (level 1) and used only in
        // `then` and `tail` at the same nest level — no use at home,
        // so the copies kill the banked def outright.
        let c = cur.ins().iconst(types::I64, 5);
        let t = cur
            .ins()
            .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::UnsignedLessThan, iv, 9);
        cur.ins().brif(t, mid, &[], out, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(mid);
        let t2 = cur
            .ins()
            .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::UnsignedLessThan, iv, 4);
        cur.ins()
            .brif(t2, then, &[], tail, &[BlockArg::Value(iv)]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(then);
        // `c` on the `isub` LHS cannot fold into an immediate — the
        // uses below need the constant in a register, so they get
        // copies.
        let s1 = cur.ins().isub(c, iv);
        cur.ins().jump(tail, &[BlockArg::Value(s1)]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(tail);
        let tp = cur.func.dfg.append_block_param(tail, types::I64);
        let s2 = cur.ins().isub(c, tp);
        let cond = cur
            .ins()
            .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::UnsignedLessThan, s2, 10);
        cur.ins().brif(cond, head, &[BlockArg::Value(s2)], out, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(out);
        cur.ins().return_(&[iv]);

        // `then` and `tail` share the innermost loop and their NCD is
        // `mid`, dominated by `head` and at its own level: the def
        // moves there rather than copying per block — a trip pays one
        // materialization, never two.
        sink(&mut func);
        assert_eq!(home(&func, c), Some(mid));
        let fives: Vec<Block> = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b).map(move |i| (b, i)))
            .filter(|&(_, i)| {
                matches!(
                    func.dfg.insts[i],
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                        ..
                    } if imm.bits() == 5
                )
            })
            .map(|(b, _)| b)
            .collect();
        assert_eq!(fives, vec![mid], "one copy at the in-loop NCD");
        verify(&func);
    }

    /// A constant passed as a `try_call` argument inside a loop is a
    /// register use like any other — it rides no edge and must never
    /// count cheap: the clone would materialize a `mov` per trip
    /// (brotli's `SetCost(model, 704, .., 0)` inside
    /// `BrotliCreateHqZopfliBackwardReferences`).
    #[test]
    fn call_arg_in_loop_is_not_cheap() {
        let clif = "
function %f(i64) -> i64 tail {
    sig0 = (i64) tail
    fn0 = %f sig0
block0(v0: i64):
    v1 = iconst.i64 5
    v3 = iconst.i64 100
    v9 = iconst.i64 7
    jump block1(v0)
block1(v10: i64):
    v2 = icmp ult v10, v3
    brif v2, block2, block6
block2:
    v4 = icmp ult v10, v9
    brif v4, block4, block7(v10)
block4:
    try_call fn0(v1), sig0, block7(v10), [ default: block3(exn0) ]
block7(v11: i64):
    v8 = iconst.i64 1
    v7 = iadd v11, v8
    jump block1(v7)
block3(v12: i64) cold:
    return v12
block6:
    return v10
}
";
        let mut func = cranelift_reader::parse_functions(clif).unwrap().remove(0);
        let v1 = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b))
            .find(|&i| {
                matches!(
                    func.dfg.insts[i],
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                        ..
                    } if imm.bits() == 5
                )
            })
            .map(|i| func.dfg.first_result(i))
            .unwrap();
        sink(&mut func);
        // The banked def survives at block0 and nothing materializes
        // in the loop.
        let blocks: Vec<Block> = func.layout.blocks().collect();
        let b0 = blocks[0];
        assert_eq!(home(&func, v1), Some(b0));
        let fives: Vec<Block> = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b).map(move |i| (b, i)))
            .filter(|&(_, i)| {
                matches!(
                    func.dfg.insts[i],
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                        ..
                    } if imm.bits() == 5
                )
            })
            .map(|(b, _)| b)
            .collect();
        assert_eq!(fives, vec![b0], "no clone inside the loop");
        verify(&func);
    }
}

