//! PLIRON (`PLIRON_RA2_POSTOPT=<bits>`, default off): spill-traffic cleanup
//! over the finished allocation.
//!
//! The allocator's own redundant-move eliminator (`redundant_moves.rs`)
//! forgets everything at a block boundary, and a spilled bundle's uses each
//! reload on their own. This pass looks at the final `Output` (the edits
//! and the operand allocations) with two whole-function dataflows and
//! removes the moves that the spill slot staying the value's home makes
//! unnecessary. It never changes an operand's allocation, a slot or a
//! register assignment, so the frame and every instruction stay as they
//! were; only edits are deleted or turned from a load into a register copy.
//! (After relax's post-allocation "windows", `redundant_saves` and
//! `dead_spill_stores`: relax/libs/relax/backend/src/regalloc.rs.)
//!
//! Forward, "register `r` holds what slot `s` holds" (`mirror[r] = s`),
//! met by intersection over all predecessors, optimistic around loops:
//!
//! * bit 1: a reload `s -> r` where `r` already mirrors `s` is deleted
//!   (a loop body's reload when the preheader loaded the same register and
//!   nothing in the loop clobbers it; a reload after a diamond both arms
//!   left intact).
//! * bit 2: a reload `s -> r2` where another register `r1` mirrors `s`
//!   becomes the copy `r1 -> r2`: one load serves the run of uses.
//! * bit 4: a spill store `r -> s` where `r` already mirrors `s` is
//!   deleted: the slot is clean.
//!
//! Backward, slot liveness:
//!
//! * bit 8: a spill store to a slot no path reads before it is written
//!   again is deleted (bits 1 and 2 leave such stores behind; the allocator
//!   itself stores at every split boundary).
//!
//! Soundness rests on the edits and the instructions' def operands and
//! clobbers being the only writers of allocatable registers and spill
//! slots, which is the contract the regalloc2 checker verifies; the checker
//! (`PLIRON_RA_CHECK=1`) runs on the output this pass leaves.
//! `debug_locations` stay valid for bits 1, 2 and 4 (every location still
//! holds the value it was said to hold); bit 8 can leave a labeled value's
//! slot stale where nothing reads it.

use super::data_structures::Ctx;
use crate::{Allocation, Block, Edit, Function, InstPosition, OperandKind, PReg, ProgPoint, RegClass};
use alloc::vec;

/// A spill slot extent: first word and word count.
type Slot = (u32, u32);
const NONE: Slot = (u32::MAX, 0);
/// Not computed yet (the optimistic start value): meets as the identity.
const TOP: Slot = (u32::MAX, 1);

fn overlaps(a: Slot, b: Slot) -> bool {
    a.0 < b.0 + b.1 && b.0 < a.0 + a.1
}

struct Stats {
    reload: u32,
    fwd: u32,
    store: u32,
    dead: u32,
}

fn size_of<F: Function>(func: &F, class: RegClass) -> u32 {
    func.spillslot_size(class) as u32
}

/// One block's forward transfer over `state` (the mirror of every preg).
/// `on_edit(index, mirror state before the edit)` sees each edit in order.
fn transfer<F: Function>(
    func: &F,
    edits: &[(ProgPoint, Edit)],
    allocs: &[Allocation],
    offsets: &[u32],
    block: Block,
    first_edit: usize,
    state: &mut [Slot],
    mut on_edit: impl FnMut(usize, &[Slot]),
) -> usize {
    let kill_slot = |state: &mut [Slot], s: Slot| {
        for m in state.iter_mut() {
            if *m != NONE && overlaps(*m, s) {
                *m = NONE;
            }
        }
    };
    let mut e = first_edit;
    let mut apply = |state: &mut [Slot], e: usize| {
        on_edit(e, state);
        let Edit::Move { from, to } = edits[e].1;
        match (from.as_reg(), from.as_stack(), to.as_reg(), to.as_stack()) {
            (_, Some(s), Some(r), _) => {
                state[r.index()] = (s.index() as u32, size_of(func, r.class()));
            }
            (Some(r), _, _, Some(s)) => {
                let slot = (s.index() as u32, size_of(func, r.class()));
                kill_slot(state, slot);
                state[r.index()] = slot;
            }
            (Some(r1), _, Some(r2), _) => state[r2.index()] = state[r1.index()],
            (_, _, Some(r), _) => state[r.index()] = NONE,
            (_, _, _, Some(s)) => {
                // Slot to slot: the extent is unknown here, clear widely.
                kill_slot(state, (s.index() as u32, 4));
            }
            _ => {}
        }
    };
    for inst in func.block_insns(block).iter() {
        while e < edits.len() && edits[e].0 <= ProgPoint::new(inst, InstPosition::Before) {
            apply(state, e);
            e += 1;
        }
        let ops = func.inst_operands(inst);
        let base = offsets[inst.index()] as usize;
        for (i, op) in ops.iter().enumerate() {
            if op.kind() != OperandKind::Def {
                continue;
            }
            let a = allocs[base + i];
            if let Some(r) = a.as_reg() {
                state[r.index()] = NONE;
            } else if let Some(s) = a.as_stack() {
                kill_slot(state, (s.index() as u32, size_of(func, op.class())));
            }
        }
        for r in func.inst_clobbers(inst) {
            state[r.index()] = NONE;
        }
        while e < edits.len() && edits[e].0 <= ProgPoint::new(inst, InstPosition::After) {
            apply(state, e);
            e += 1;
        }
    }
    e
}

pub fn run<F: Function>(func: &F, ctx: &mut Ctx) {
    let bits = crate::postopt_bits();
    if bits == 0 || ctx.output.edits.is_empty() || func.multi_spillslot_named_by_last_slot() {
        return;
    }
    let out = &mut ctx.output;
    let nb = func.num_blocks();
    let np = PReg::NUM_INDEX;
    let mut stats = Stats { reload: 0, fwd: 0, store: 0, dead: 0 };

    // First edit of each block (edits are sorted by program point, and
    // blocks' instruction ranges ascend with the block index in every
    // client we have; bail if not).
    let mut first = vec![0usize; nb + 1];
    {
        let mut e = 0;
        let mut prev_end = 0;
        for b in 0..nb {
            let r = func.block_insns(Block::new(b));
            if r.first().index() < prev_end {
                return;
            }
            prev_end = r.last().index() + 1;
            first[b] = e;
            while e < out.edits.len() && out.edits[e].0.inst().index() < prev_end {
                e += 1;
            }
        }
        first[nb] = e;
        if e != out.edits.len() {
            return;
        }
    }

    if bits & 7 != 0 {
        // Forward dataflow. `ins[b]` is `TOP` until a predecessor reached it.
        let mut ins = vec![TOP; nb * np];
        let mut outs = vec![TOP; nb * np];
        let entry = func.entry_block().index();
        let mut state = vec![NONE; np];
        let meet_into = |ins: &mut [Slot], outs: &[Slot], b: usize, pessimistic: bool| {
            let dst = &mut ins[b * np..(b + 1) * np];
            if b == entry {
                dst.fill(NONE);
                return;
            }
            dst.fill(TOP);
            for &p in func.block_preds(Block::new(b)) {
                let src = &outs[p.index() * np..(p.index() + 1) * np];
                for (d, &s) in dst.iter_mut().zip(src) {
                    let s = if pessimistic && s == TOP { NONE } else { s };
                    if *d == TOP {
                        *d = s;
                    } else if s != TOP && *d != s {
                        *d = NONE;
                    }
                }
            }
            // No predecessor known yet (or unreachable): nothing is known.
            for d in dst.iter_mut() {
                if *d == TOP {
                    *d = NONE;
                }
            }
        };
        let mut converged = false;
        for _sweep in 0..8 {
            let mut changed = false;
            for b in 0..nb {
                meet_into(&mut ins, &outs, b, false);
                state.copy_from_slice(&ins[b * np..(b + 1) * np]);
                transfer(func, &out.edits, &out.allocs, &out.inst_alloc_offsets, Block::new(b), first[b], &mut state, |_, _| {});
                let o = &mut outs[b * np..(b + 1) * np];
                if o != &state[..] {
                    o.copy_from_slice(&state);
                    changed = true;
                }
            }
            if !changed {
                converged = true;
                break;
            }
        }
        if !converged {
            // Give up on facts around back edges: one sweep in which a
            // predecessor not yet computed in this sweep knows nothing.
            outs.fill(TOP);
            for b in 0..nb {
                meet_into(&mut ins, &outs, b, true);
                state.copy_from_slice(&ins[b * np..(b + 1) * np]);
                transfer(func, &out.edits, &out.allocs, &out.inst_alloc_offsets, Block::new(b), first[b], &mut state, |_, _| {});
                outs[b * np..(b + 1) * np].copy_from_slice(&state);
            }
            // `ins` of a block with a later predecessor used `TOP -> NONE`
            // for it, which is what the rewrite below must see too: keep
            // `ins` as computed in this sweep.
        } else {
            for b in 0..nb {
                meet_into(&mut ins, &outs, b, false);
            }
        }

        // Decide per edit. A rewrite changes no location's value, so the
        // states computed above stay exact for the rewritten code.
        #[derive(Clone, Copy, PartialEq)]
        enum Act {
            Keep,
            Drop,
            From(PReg),
        }
        let mut acts = vec![Act::Keep; out.edits.len()];
        for b in 0..nb {
            state.copy_from_slice(&ins[b * np..(b + 1) * np]);
            let edits = &out.edits;
            transfer(func, edits, &out.allocs, &out.inst_alloc_offsets, Block::new(b), first[b], &mut state, |e, st| {
                let Edit::Move { from, to } = edits[e].1;
                if let (Some(s), Some(r)) = (from.as_stack(), to.as_reg()) {
                    let slot = (s.index() as u32, size_of(func, r.class()));
                    if st[r.index()] == slot {
                        if bits & 1 != 0 {
                            acts[e] = Act::Drop;
                            stats.reload += 1;
                        }
                    } else if bits & 2 != 0 {
                        // Any register of the same class that mirrors the slot.
                        for (i, &m) in st.iter().enumerate() {
                            if m == slot {
                                let r1 = PReg::from_index(i);
                                if r1.class() == r.class() {
                                    acts[e] = Act::From(r1);
                                    stats.fwd += 1;
                                    break;
                                }
                            }
                        }
                    }
                } else if let (Some(r), Some(s)) = (from.as_reg(), to.as_stack()) {
                    if bits & 4 != 0 && st[r.index()] == (s.index() as u32, size_of(func, r.class())) {
                        acts[e] = Act::Drop;
                        stats.store += 1;
                    }
                }
            });
        }
        if stats.reload + stats.fwd + stats.store > 0 {
            let has_vregs = out.edit_vregs.len() == out.edits.len();
            let mut k = 0;
            for e in 0..out.edits.len() {
                match acts[e] {
                    Act::Drop => continue,
                    Act::From(r1) => {
                        let Edit::Move { to, .. } = out.edits[e].1;
                        out.edits[k] = (out.edits[e].0, Edit::Move { from: Allocation::reg(r1), to });
                    }
                    Act::Keep => out.edits.swap(k, e),
                }
                if has_vregs {
                    out.edit_vregs[k] = out.edit_vregs[e];
                }
                k += 1;
            }
            out.edits.truncate(k);
            if has_vregs {
                out.edit_vregs.truncate(k);
            }
            // Edit indices moved: recompute the per-block starts.
            let mut e = 0;
            for b in 0..nb {
                first[b] = e;
                let end = func.block_insns(Block::new(b)).last().index() + 1;
                while e < out.edits.len() && out.edits[e].0.inst().index() < end {
                    e += 1;
                }
            }
            first[nb] = e;
        }
    }

    if bits & 8 != 0 && out.num_spillslots > 0 {
        dead_stores(func, out, &first, &mut stats);
    }

    if crate::postopt_stats() && stats.reload + stats.fwd + stats.store + stats.dead > 0 {
        #[cfg(feature = "std")]
        std::eprintln!(
            "ra2post: reload {} fwd {} store {} dead {} left {}",
            stats.reload,
            stats.fwd,
            stats.store,
            stats.dead,
            out.edits.len()
        );
    }
}

/// Bit 8: delete stores to slots that no path reads before the next write.
fn dead_stores<F: Function>(func: &F, out: &mut crate::Output, first: &[usize], stats: &mut Stats) {
    let nb = func.num_blocks();
    // Slot words; `num_spillslots` counts words. One spare word block for
    // extents that run past it.
    let nw = out.num_spillslots + 8;
    let words = (nw + 63) / 64;
    let set = |v: &mut [u64], s: Slot, on: bool| {
        for w in s.0..(s.0 + s.1).min(nw as u32) {
            let (i, m) = ((w / 64) as usize, 1u64 << (w % 64));
            if on {
                v[i] |= m;
            } else {
                v[i] &= !m;
            }
        }
    };
    let any = |v: &[u64], s: Slot| (s.0..(s.0 + s.1).min(nw as u32)).any(|w| v[(w / 64) as usize] & (1u64 << (w % 64)) != 0);

    // Backward transfer of one block; `on_store(edit, live after it)`.
    let walk = |live: &mut [u64], b: usize, out: &crate::Output, on_store: &mut dyn FnMut(usize, bool)| {
        let edits = &out.edits;
        let mut e = first[b + 1];
        let mut apply = |live: &mut [u64], e: usize| {
            let Edit::Move { from, to } = edits[e].1;
            if let Some(s) = to.as_stack() {
                let n = from.as_reg().map_or(4, |r| size_of(func, r.class()));
                let slot = (s.index() as u32, n);
                on_store(e, any(live, slot));
                if from.is_reg() {
                    set(live, slot, false);
                }
            }
            if let Some(s) = from.as_stack() {
                let n = to.as_reg().map_or(4, |r| size_of(func, r.class()));
                set(live, (s.index() as u32, n), true);
            }
        };
        for inst in func.block_insns(Block::new(b)).iter().rev() {
            while e > first[b] && edits[e - 1].0 >= ProgPoint::new(inst, InstPosition::After) {
                e -= 1;
                apply(live, e);
            }
            let ops = func.inst_operands(inst);
            let base = out.inst_alloc_offsets[inst.index()] as usize;
            // Defs first (they come after the uses in time), then uses.
            for (i, op) in ops.iter().enumerate() {
                if op.kind() == OperandKind::Def {
                    if let Some(s) = out.allocs[base + i].as_stack() {
                        set(live, (s.index() as u32, size_of(func, op.class())), false);
                    }
                }
            }
            for (i, op) in ops.iter().enumerate() {
                if op.kind() != OperandKind::Def {
                    if let Some(s) = out.allocs[base + i].as_stack() {
                        set(live, (s.index() as u32, size_of(func, op.class())), true);
                    }
                }
            }
            while e > first[b] && edits[e - 1].0 >= ProgPoint::new(inst, InstPosition::Before) {
                e -= 1;
                apply(live, e);
            }
        }
    };

    let mut live_in = vec![0u64; nb * words];
    let mut live = vec![0u64; words];
    let live_out = |live: &mut [u64], live_in: &[u64], b: usize| {
        live.fill(0);
        for &s in func.block_succs(Block::new(b)) {
            for (d, &x) in live.iter_mut().zip(&live_in[s.index() * words..(s.index() + 1) * words]) {
                *d |= x;
            }
        }
    };
    loop {
        let mut changed = false;
        for b in (0..nb).rev() {
            live_out(&mut live, &live_in, b);
            walk(&mut live, b, out, &mut |_, _| {});
            let dst = &mut live_in[b * words..(b + 1) * words];
            if dst != &live[..] {
                dst.copy_from_slice(&live);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut dead = vec![false; out.edits.len()];
    let mut n = 0;
    for b in 0..nb {
        live_out(&mut live, &live_in, b);
        let edits = &out.edits;
        walk(&mut live, b, out, &mut |e, live_after| {
            let Edit::Move { from, .. } = edits[e].1;
            if !live_after && from.is_reg() {
                dead[e] = true;
                n += 1;
            }
        });
    }
    if n == 0 {
        return;
    }
    stats.dead = n;
    let has_vregs = out.edit_vregs.len() == out.edits.len();
    let mut k = 0;
    for e in 0..out.edits.len() {
        if dead[e] {
            continue;
        }
        out.edits.swap(k, e);
        if has_vregs {
            out.edit_vregs[k] = out.edit_vregs[e];
        }
        k += 1;
    }
    out.edits.truncate(k);
    if has_vregs {
        out.edit_vregs.truncate(k);
    }
}
