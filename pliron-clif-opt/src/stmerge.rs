//! `stmerge` (`PLIRON_STMERGE=0` off): fold a bytewise 2-byte aggregate
//! copy — `load.i8 p; load.i8 p+1` feeding `store.i8 q; store.i8 q+1` —
//! into one `load.i16`/`store.i16`, LLVM's consecutive-store merging for
//! the swaps in `drift::sort`/`quicksort` over `{u8,u8}` elements
//! (`ldrb`+`ldrb`+`strb`+`strb` where LLVM emits `ldrh`+`strh`).
//!
//! The source and destination fields must be byte-adjacent on the same
//! resolved base, all four accesses `notrap` in one block with no
//! memory write between each pair (the wide access reads/writes the
//! whole range where the first narrow one ran). The loaded bytes must
//! be dead past the stores — no `bor` tree is built, so uses elsewhere
//! keep their `ldrb` and the merge stays a pure win. Endianness is
//! pinned little on both new accesses, matching the LE assembly the
//! byte pair implied.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    Function, Inst, InstBuilder, InstructionData, MemFlagsData, Opcode, Value, ValueDef, types,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ldmerge::{merged_flags, no_writes_between};

/// Deepest `iadd` chain `peel_addr` chases; longer spines are left
/// unnormalized (no merge, never wrong).
const MAX_ADDR_PEEL: usize = 8;

/// `(base, off)` for `v + offset-field k`: peel `iadd x, const` into
/// the running offset.
fn peel_addr(func: &Function, v: Value, off: i64) -> (Value, i64) {
    let mut v = func.dfg.resolve_aliases(v);
    let mut off = off;
    for _ in 0..MAX_ADDR_PEEL {
        let ValueDef::Result(i, 0) = func.dfg.value_def(v) else {
            break;
        };
        let InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } = func.dfg.insts[i]
        else {
            break;
        };
        let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
        let k = const_i64(func, a).map(|k| (b, k)).or_else(|| const_i64(func, b).map(|k| (a, k)));
        let Some((nv, k)) = k else { break };
        off = off.wrapping_add(k);
        v = nv;
    }
    (v, off)
}

fn const_i64(func: &Function, v: Value) -> Option<i64> {
    let ValueDef::Result(i, 0) = func.dfg.value_def(v) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

/// `i` as a narrow 1-byte load: `Load` with an i8 result or `uload8`.
/// Returns `(base, off, flags)` of the access.
fn u8_load(func: &Function, v: Value) -> Option<(Inst, Value, i64, MemFlagsData)> {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, 0) = func.dfg.value_def(v) else {
        return None;
    };
    let InstructionData::Load {
        opcode,
        arg,
        offset,
        flags: lf,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let one = match opcode {
        Opcode::Uload8 => true,
        Opcode::Load => func.dfg.value_type(func.dfg.first_result(i)) == types::I8,
        _ => false,
    };
    one.then(|| (i, func.dfg.resolve_aliases(arg), i64::from(i32::from(offset)), func.dfg.mem_flags[lf]))
}

/// `i` as a 1-byte store: `Store` of an i8-typed value or `istore8`.
/// Returns `(stored, base, off, flags)`.
fn u8_store(func: &Function, i: Inst) -> Option<(Value, Value, i64, MemFlagsData)> {
    let InstructionData::Store {
        opcode,
        args,
        offset,
        flags: lf,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let one = match opcode {
        Opcode::Istore8 => true,
        Opcode::Store => func.dfg.value_type(func.dfg.resolve_aliases(args[0])) == types::I8,
        _ => false,
    };
    one.then(|| {
        (
            func.dfg.resolve_aliases(args[0]),
            func.dfg.resolve_aliases(args[1]),
            i64::from(i32::from(offset)),
            func.dfg.mem_flags[lf],
        )
    })
}

pub fn run(func: &mut Function) -> usize {
    // Single-use check needs one Uses build for the whole scan.
    let uses = crate::uses::Uses::new(func);
    let mut rewrites = 0;
    let mut consumed: FxHashSet<Inst> = FxHashSet::default();
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        let pos: FxHashMap<Inst, usize> =
            insts.iter().enumerate().map(|(k, &i)| (i, k)).collect();
        // Store index -> (stored, base, off, flags), u8 stores only.
        let stores: Vec<(usize, Value, Value, i64, MemFlagsData)> = insts
            .iter()
            .enumerate()
            .filter_map(|(k, &i)| {
                u8_store(func, i).map(|(v, a, o, f)| (k, v, a, o, f))
            })
            .collect();
        for w in stores.windows(2) {
            let (k0, v0, a0, o0, f0) = w[0];
            let (k1, v1, a1, o1, f1) = w[1];
            let s0 = insts[k0];
            let s1 = insts[k1];
            if consumed.contains(&s0) || consumed.contains(&s1) {
                continue;
            }
            let (b0, bo0) = peel_addr(func, a0, o0);
            let (b1, bo1) = peel_addr(func, a1, o1);
            // The wide store replaces both narrow ones at s0's position;
            // no access may sit between them — even a load could read
            // the byte the merged store moves earlier (endpoints are
            // the stores themselves, so the span is exclusive).
            if b0 != b1
                || bo0.wrapping_add(1) != bo1
                || i64::from(bo0 as i32) != bo0
                || (k0 + 1 < k1
                    && !insts[k0 + 1..k1].iter().all(|&i| {
                        let op = func.dfg.insts[i].opcode();
                        !(op.can_load()
                            || op.can_store()
                            || op.is_call()
                            || op.other_side_effects())
                    }))
            {
                continue;
            }
            if f0 != f1
                || !f0.notrap()
                || uses.sites(v0) != [(b, s0)]
                || uses.sites(v1) != [(b, s1)]
            {
                continue;
            }
            let Some((l0, la0, lo0, lf0)) = u8_load(func, v0) else {
                continue;
            };
            let Some((l1, la1, lo1, lf1)) = u8_load(func, v1) else {
                continue;
            };
            let (Some(&p0), Some(&p1)) = (pos.get(&l0), pos.get(&l1)) else {
                continue;
            };
            let (lb0, lbo0) = peel_addr(func, la0, lo0);
            let (lb1, lbo1) = peel_addr(func, la1, lo1);
            if consumed.contains(&l0)
                || consumed.contains(&l1)
                || lb0 != lb1
                || lbo0.wrapping_add(1) != lbo1
                || i64::from(lbo0 as i32) != lbo0
                || lf0 != lf1
                || !lf0.notrap()
                || !no_writes_between(func, &insts, p0.min(p1), p0.max(p1))
            {
                continue;
            }
            if !pliron_cfg::bisect("stmerge") {
                continue;
            }
            let w = {
                let mut cur = FuncCursor::new(func).at_inst(l0.min(l1));
                cur.ins().load(types::I16, merged_flags(lf0), lb0, lbo0 as i32)
            };
            {
                let mut cur = FuncCursor::new(func).at_inst(s0);
                cur.ins().store(merged_flags(f0), w, b0, bo0 as i32);
            }
            func.layout.remove_inst(s0);
            func.layout.remove_inst(s1);
            func.dfg.clear_results(s0);
            func.dfg.clear_results(s1);
            consumed.extend([s0, s1, l0, l1]);
            rewrites += 1;
        }
    }
    if rewrites > 0 {
        // Dead now: the two byte loads and any peeled `iadd`s.
        crate::uses::worklist_dce(func, |f, i| {
            let op = f.dfg.insts[i].opcode();
            !op.is_terminator()
                && (crate::jumpthread::pure_op(f, i)
                    || matches!(
                        &f.dfg.insts[i],
                        InstructionData::Load { flags: lf, .. }
                            if matches!(
                                op,
                                Opcode::Uload8
                                    | Opcode::Uload16
                                    | Opcode::Uload32
                                    | Opcode::Load
                            ) && f.dfg.mem_flags[*lf].notrap()
                    ))
        });
    }
    rewrites
}
