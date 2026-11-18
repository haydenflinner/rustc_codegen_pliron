//! PLIRON (`PLIRON_RA2_SPILLHOIST`, default on since 2026-11-18, `=0` off):
//! one spill store at the definition instead of one at every place the
//! value moves to its slot.
//!
//! regalloc2 writes a value to its spill slot wherever a piece of it in a
//! register is followed by a piece on the stack: at every split boundary,
//! on every edge from a register region into a stack region. A region
//! split (`regionsplit.rs`) that keeps a value in a register around a
//! call-heavy loop stores it on every entry into that loop, inside an outer
//! loop on every trip; a value split around several calls stores before
//! each of them. The values are SSA, so the slot only ever holds one value
//! per vreg: when the definition writes a register, one store right after
//! it makes every later store of that vreg to that slot redundant (LLVM's
//! `InlineSpiller` hoists spills to the definition for the same reason).
//!
//! The pass runs on the finished `Output`: per vreg, the stores (register
//! to stack moves whose `edit_vregs` entry names it) to one slot are
//! summed by block frequency (`CFGInfo::hot_bonus`) and, when 1.5 x the
//! definition's block frequency is below that, replaced by one store at `After(def)`,
//! ahead of the other edits there (right after the instruction the
//! register holds the definition). Soundness: the definition dominates
//! every point the vreg is live at; the slot is written between the
//! definition and a removed store only by this spillset (spillslots are
//! shared only between spillsets with disjoint extents, and an extent
//! covers every path the vreg is live along), and within the spillset only
//! by vregs live there, which this one is (bundle ranges never overlap).
//! Block parameters (no defining instruction) and definitions on a block's
//! last instruction (a call terminator: nothing runs after it in the
//! block) are left alone. Block-parameter edge transfers are also left
//! alone: with `PLIRON_RA2_SRCVREG` they are labelled with the branch
//! argument, whose spill home is not the destination, and deleting them
//! left the parameter's stack slot cold (smoke `neonpair` failed the
//! checker). The checker (`PLIRON_RA_CHECK=1`) runs on the result.

use super::data_structures::Ctx;
use crate::{Allocation, Edit, Function, FxHashMap, InstPosition, OperandKind, ProgPoint};
use alloc::vec::Vec;

pub(crate) fn run<F: Function>(func: &F, ctx: &mut Ctx, edit_blockparam: &[bool]) {
    if !crate::spillhoist_on() {
        return;
    }
    // Real registers only: a "stack preg" (MachineEnv placeholder the
    // fuzzer uses for memory) is not a legal `from` for a spill store —
    // writing it into a SpillSlot is a stack-to-stack move.
    let stack_preg: Vec<bool> = ctx.pregs.iter().map(|p| p.is_stack).collect();
    let is_reg = |a: Allocation| {
        a.as_reg().is_some_and(|r| !stack_preg.get(r.index()).copied().unwrap_or(false))
    };
    let out = &mut ctx.output;
    let n = out.edits.len();
    if n == 0 || out.edit_vregs.len() != n {
        return;
    }
    // Stores per vreg: (slot, edit indices); a vreg stored to two slots is
    // skipped. Block-parameter edge transfers are not spill stores.
    let mut stores: FxHashMap<u32, (Allocation, Vec<u32>, bool)> = FxHashMap::default();
    for (i, (_, e)) in out.edits.iter().enumerate() {
        let Edit::Move { from, to } = *e;
        if !(is_reg(from) && to.is_stack()) {
            continue;
        }
        if edit_blockparam.get(i).copied().unwrap_or(false) {
            continue;
        }
        let Some(v) = out.edit_vregs[i] else { continue };
        let ent = stores.entry(v.vreg() as u32).or_insert((to, Vec::new(), true));
        if ent.0 != to {
            ent.2 = false;
        }
        ent.1.push(i as u32);
    }
    if stores.is_empty() {
        return;
    }
    let cfg = &ctx.cfginfo;
    let hot = |p: ProgPoint| cfg.hot_bonus[cfg.insn_block[p.inst().index()].index()];
    // New stores (position, from, to, vreg) and edits to delete.
    let mut add: Vec<(ProgPoint, Allocation, Allocation, crate::VReg)> = Vec::new();
    let mut del = alloc::vec![false; n];
    let mut hoisted = 0u32;
    let mut removed = 0u32;
    for b in 0..func.num_blocks() {
        let block = crate::Block::new(b);
        let insns = func.block_insns(block);
        let last = insns.last();
        for inst in insns.iter() {
            if inst == last {
                continue;
            }
            let base = out.inst_alloc_offsets[inst.index()] as usize;
            for (k, op) in func.inst_operands(inst).iter().enumerate() {
                if op.kind() != OperandKind::Def {
                    continue;
                }
                let Some(ent) = stores.get(&(op.vreg().vreg() as u32)) else {
                    continue;
                };
                let (slot, ref list, one_slot) = *ent;
                if !one_slot {
                    continue;
                }
                let a = out.allocs[base + k];
                if !is_reg(a) {
                    continue;
                }
                let at = ProgPoint::new(inst, InstPosition::After);
                let cost_def = cfg.hot_bonus[b];
                let cost: f32 = list.iter().map(|&i| hot(out.edits[i as usize].0)).sum();
                // A margin: stores at the split points often pair
                // (`stp`), a lone store at the definition does not.
                if !(cost_def * 1.5 < cost) {
                    continue;
                }
                for &i in list {
                    del[i as usize] = true;
                }
                removed += list.len() as u32;
                hoisted += 1;
                add.push((at, a, slot, op.vreg()));
            }
        }
    }
    if add.is_empty() {
        return;
    }
    add.sort_by_key(|x| x.0);
    let old_edits = core::mem::take(&mut out.edits);
    let old_vregs = core::mem::take(&mut out.edit_vregs);
    let mut edits = Vec::with_capacity(n + add.len());
    let mut vregs = Vec::with_capacity(n + add.len());
    let mut j = 0;
    for (i, (pos, e)) in old_edits.into_iter().enumerate() {
        // New stores go ahead of every edit at their point.
        while j < add.len() && add[j].0 <= pos {
            edits.push((add[j].0, Edit::Move { from: add[j].1, to: add[j].2 }));
            vregs.push(Some(add[j].3));
            j += 1;
        }
        if !del[i] {
            edits.push((pos, e));
            vregs.push(old_vregs[i]);
        }
    }
    while j < add.len() {
        edits.push((add[j].0, Edit::Move { from: add[j].1, to: add[j].2 }));
        vregs.push(Some(add[j].3));
        j += 1;
    }
    out.edits = edits;
    out.edit_vregs = vregs;
    explain!("spillhoist: {hoisted} definition store(s) replace {removed} store(s)");
}
