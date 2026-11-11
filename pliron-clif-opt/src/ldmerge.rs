//! `ldmerge` (`PLIRON_LDMERGE=0` off): fold byte-assembly `bor` trees of
//! consecutive narrow loads into one wide load — LLVM's
//! `CombineConsecutiveLoads`.
//!
//! Slice/array equality and `memcmp` expansion reach the end of the
//! pipeline as
//!
//! ```text
//!   b0 = uload8.W notrap p+0
//!   b1 = uload8.W notrap p+1
//!   b2 = uload8.W notrap p+2
//!   b3 = uload8.W notrap p+3
//!   v = bor b0, (ishl b1, 8), (ishl b2, 16), (ishl b3, 24)
//! ```
//!
//! one `ldrb` + `orr`/`lsl` per byte where LLVM emits a single `ldr w`.
//! For a `bor` tree whose terms are `uload8/16/32` (or `uextend` of a
//! narrow `load`) at consecutive offsets on one base, at shifts matching
//! little-endian assembly (`shift - 8*off` constant across the merged
//! range), this pass emits `load.i{16,32,64} (base+lo)` placed at the
//! same field position.
//!
//! Only `bor` trees are rewritten — `bxor` is not idempotent, so two
//! terms covering the same field would cancel where the merge would
//! keep one. All merged loads must share flags and be `notrap` (the
//! wide access must not introduce a trap), and no memory-writing inst
//! may sit between the first and last merged load in the block (the
//! wide load reads the whole range at the first load's position).
//! Explicit little-endian on the new load pins the byte order the shift
//! assembly implied.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    Endianness, Function, Inst, InstBuilder, InstructionData, MemFlagsData, Opcode, Type, Value,
    ValueDef, types,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// Largest position gap between first and last merged load scanned for
/// intervening stores. Byte assemblies are tight; a wider span is not
/// this pattern and the bound keeps the check linear.
const MAX_SPAN: usize = 96;

/// One leaf of a `bor` tree: a narrow unsigned load of `width` bytes at
/// `base+off`, shifted left `shift` bits inside the assembled value.
#[derive(Clone, Copy)]
struct Term {
    /// The load instruction itself (position anchor).
    ld: Inst,
    base: Value,
    /// Byte offset of the access, from the inst's offset field.
    off: i64,
    /// Bytes loaded (1, 2 or 4).
    width: i64,
    flags: MemFlagsData,
    /// Left shift applied to the loaded value (0 for none).
    shift: i64,
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
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

/// `v` as a narrow unsigned load: `uloadN`, or `uextend (load.iN)`.
/// Returns (load inst, base, offset, width in bytes, flags).
fn narrow_load(func: &Function, v: Value) -> Option<(Inst, Value, i64, i64, MemFlagsData)> {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(mut i, 0) = func.dfg.value_def(v) else {
        return None;
    };
    if let InstructionData::Unary {
        opcode: Opcode::Uextend,
        arg,
    } = func.dfg.insts[i]
        && let ValueDef::Result(li, 0) = func.dfg.value_def(func.dfg.resolve_aliases(arg))
    {
        i = li;
    }
    match func.dfg.insts[i] {
        InstructionData::Load {
            opcode,
            arg,
            offset,
            flags: lf,
        } => {
            let w = match opcode {
                Opcode::Uload8 => 1,
                Opcode::Uload16 => 2,
                Opcode::Uload32 => 4,
                Opcode::Load => match func.dfg.value_type(func.dfg.first_result(i)) {
                    t if t == types::I8 => 1,
                    t if t == types::I16 => 2,
                    t if t == types::I32 => 4,
                    _ => return None,
                },
                _ => return None,
            };
            Some((
                i,
                func.dfg.resolve_aliases(arg),
                i64::from(i32::from(offset)),
                w,
                func.dfg.mem_flags[lf],
            ))
        }
        _ => None,
    }
}

/// `v` as `leaf << shift`: peels one `ishl` by a positive constant
/// multiple of 8.
fn shifted(func: &Function, v: Value) -> (Value, i64) {
    let v = func.dfg.resolve_aliases(v);
    if let ValueDef::Result(i, 0) = func.dfg.value_def(v)
        && let InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [a, b],
        } = func.dfg.insts[i]
        && let Some(s) = iconst(func, b)
        && s > 0
        && s % 8 == 0
        && s < 128
    {
        return (a, s);
    }
    (v, 0)
}

/// Classify a `bor`-tree leaf as a mergeable narrow-load term.
fn term(func: &Function, leaf: Value) -> Option<Term> {
    let (inner, shift) = shifted(func, leaf);
    let (ld, base, off, width, flags) = narrow_load(func, inner)?;
    if !flags.notrap() {
        return None;
    }
    Some(Term {
        ld,
        base,
        off,
        width,
        flags,
        shift,
    })
}

/// Collect the leaves of the `bor` tree at `v` into `out` (non-`bor`
/// defs included as leaves). `in_tree` gets every `bor` inst walked.
fn collect(func: &Function, v: Value, out: &mut Vec<Value>, in_tree: &mut FxHashSet<Inst>) {
    let v = func.dfg.resolve_aliases(v);
    if let ValueDef::Result(i, 0) = func.dfg.value_def(v)
        && let InstructionData::Binary {
            opcode: Opcode::Bor,
            args: [a, b],
        } = func.dfg.insts[i]
    {
        in_tree.insert(i);
        collect(func, a, out, in_tree);
        collect(func, b, out, in_tree);
        return;
    }
    out.push(v);
}

/// Flags the merged load keeps: minus `aligned` (a wider access is not
/// necessarily as aligned) and `can_move` (it is anchored at the first
/// load's position). Endianness is pinned little — the byte order the
/// shift assembly computed.
pub(crate) fn merged_flags(f: MemFlagsData) -> MemFlagsData {
    let mut nf = MemFlagsData::new()
        .with_alias_region(f.alias_region())
        .with_trap_code(f.trap_code())
        .with_endianness(Endianness::Little);
    if f.notrap() {
        nf = nf.with_notrap();
    }
    if f.readonly() {
        nf = nf.with_readonly();
    }
    nf
}

pub(crate) fn no_writes_between(func: &Function, insts: &[Inst], lo: usize, hi: usize) -> bool {
    insts[lo..=hi].iter().all(|&i| {
        let op = func.dfg.insts[i].opcode();
        !(op.can_store() || op.is_call() || op.other_side_effects())
    })
}

fn load_ty(w: i64) -> Type {
    match w {
        2 => types::I16,
        4 => types::I32,
        _ => types::I64,
    }
}

/// One planned piece: `load.ty (base+lo)` whose byte-0 value lands at
/// bit `place` of the assembled result, anchored at `anchor`'s
/// position. `cover` indexes the merged terms.
struct Piece {
    anchor: Inst,
    base: Value,
    lo: i64,
    ty: Type,
    flags: MemFlagsData,
    place: i64,
    cover: Vec<usize>,
}

/// Rewrite one `bor` tree rooted at inst `root`. `leaves` are the
/// tree's leaf values in order. Returns wide loads emitted.
fn rewrite(func: &mut Function, root: Inst, leaves: &[Value]) -> usize {
    let wty = func.dfg.value_type(func.dfg.first_result(root));
    let wbits = i64::from(wty.bits());
    let mut terms: Vec<Term> = Vec::new();
    let mut term_of_leaf: FxHashMap<Value, usize> = FxHashMap::default();
    for &leaf in leaves {
        if let Some(t) = term(func, leaf) {
            term_of_leaf.insert(leaf, terms.len());
            terms.push(t);
        }
    }
    if terms.len() < 2 {
        return 0;
    }
    // Group by (base, flags, c): within one merged piece every byte's
    // shift is `8*off + c` (little-endian field placement).
    let mut groups: FxHashMap<(Value, MemFlagsData, i64), Vec<usize>> = FxHashMap::default();
    for (k, t) in terms.iter().enumerate() {
        groups
            .entry((t.base, t.flags, t.shift - 8 * t.off))
            .or_default()
            .push(k);
    }
    let mut pieces: Vec<Piece> = Vec::new();
    for (&(base, flags, c), g) in &groups {
        let mut g = g.clone();
        g.sort_by_key(|&k| terms[k].off);
        let mut k = 0;
        while k < g.len() {
            // Largest consecutive-access prefix starting at g[k] whose
            // cumulative width is a power of two, at most 8.
            let (mut width, mut end, mut best) = (0i64, terms[g[k]].off, (0usize, 0i64));
            for (n, &x) in g[k..].iter().enumerate() {
                let t = &terms[x];
                if t.off != end || width + t.width > 8 {
                    break;
                }
                width += t.width;
                end += t.width;
                if matches!(width, 2 | 4 | 8) {
                    best = (n + 1, width);
                }
            }
            let (take, w) = best;
            if take >= 2 {
                let lo = terms[g[k]].off;
                let place = 8 * lo + c;
                if place >= 0 && place + 8 * w <= wbits {
                    pieces.push(Piece {
                        anchor: terms[g[k]].ld,
                        base,
                        lo,
                        ty: load_ty(w),
                        flags,
                        place,
                        cover: g[k..k + take].to_vec(),
                    });
                }
                k += take;
            } else {
                k += 1;
            }
        }
    }
    if pieces.is_empty() {
        return 0;
    }
    // Each piece's merged loads must sit in one block (the anchor's)
    // with no store/call between the earliest and the latest — the
    // wide load reads the whole range at the anchor's position. Loads
    // and the `bor` tree may live in different blocks; the anchor
    // block dominates the tree since the leaf values do.
    for p in &pieces {
        let Some(block) = func.layout.inst_block(p.anchor) else {
            return 0;
        };
        let insts: Vec<Inst> = func.layout.block_insts(block).collect();
        let pos: FxHashMap<Inst, usize> =
            insts.iter().enumerate().map(|(k, &i)| (i, k)).collect();
        let (mut lo_pos, mut hi_pos) = (usize::MAX, 0usize);
        for &t in &p.cover {
            let Some(&x) = pos.get(&terms[t].ld) else {
                return 0;
            };
            lo_pos = lo_pos.min(x);
            hi_pos = hi_pos.max(x);
        }
        if hi_pos.wrapping_sub(lo_pos) > MAX_SPAN
            || !no_writes_between(func, &insts, lo_pos, hi_pos)
        {
            return 0;
        }
    }
    // Emit each piece's load at its anchor, then any extend/shift and
    // the rebuilt `bor` chain just before `root`.
    let mut new_terms: Vec<Value> = Vec::with_capacity(pieces.len());
    for p in &pieces {
        let f = merged_flags(p.flags);
        let ld = {
            let mut cur = FuncCursor::new(func).at_inst(p.anchor);
            cur.ins().load(p.ty, f, p.base, p.lo as i32)
        };
        let v = {
            let mut cur = FuncCursor::new(func).at_inst(root);
            let v = if p.ty.bits() < wty.bits() {
                cur.ins().uextend(wty, ld)
            } else {
                ld
            };
            if p.place > 0 {
                let k = cur.ins().iconst(wty, p.place);
                cur.ins().ishl(v, k)
            } else {
                v
            }
        };
        new_terms.push(v);
    }
    let mut used: FxHashSet<usize> = FxHashSet::default();
    for p in &pieces {
        used.extend(p.cover.iter().copied());
    }
    let mut chain: Vec<Value> = Vec::new();
    for &leaf in leaves {
        match term_of_leaf.get(&leaf) {
            Some(&k) if used.contains(&k) => {}
            _ => chain.push(leaf),
        }
    }
    chain.extend(new_terms);
    if chain.len() == leaves.len() {
        return 0;
    }
    let mut cur = FuncCursor::new(func).at_inst(root);
    let mut it = chain.into_iter();
    let Some(mut acc) = it.next() else { return 0 };
    for v in it {
        acc = cur.ins().bor(acc, v);
    }
    let rv = func.dfg.first_result(root);
    func.layout.remove_inst(root);
    func.dfg.detach_inst_results(root);
    func.dfg.change_to_alias(rv, acc);
    pieces.len()
}

/// Returns the number of wide loads emitted.
pub fn run(func: &mut Function) -> usize {
    // Values that feed a `bor`: a `bor` result among them is mid-tree.
    let mut feeds_bor: FxHashSet<Value> = FxHashSet::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Binary {
                opcode: Opcode::Bor,
                args,
            } = func.dfg.insts[i]
            {
                feeds_bor.insert(func.dfg.resolve_aliases(args[0]));
                feeds_bor.insert(func.dfg.resolve_aliases(args[1]));
            }
        }
    }
    let mut n = 0;
    let mut done: FxHashSet<Inst> = FxHashSet::default();
    let blocks: Vec<_> = func.layout.blocks().collect();
    for b in blocks {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for i in insts {
            if done.contains(&i)
                || !matches!(
                    func.dfg.insts[i],
                    InstructionData::Binary {
                        opcode: Opcode::Bor,
                        ..
                    }
                )
            {
                continue;
            }
            let rv = func.dfg.first_result(i);
            if feeds_bor.contains(&func.dfg.resolve_aliases(rv)) {
                continue;
            }
            let mut leaves = Vec::new();
            let mut in_tree = FxHashSet::default();
            collect(func, rv, &mut leaves, &mut in_tree);
            done.extend(in_tree.iter().copied());
            if leaves.len() >= 2 && pliron_cfg::bisect("ldmerge") {
                n += rewrite(func, i, &leaves);
            }
        }
    }
    if n > 0 {
        // Covered loads, their `ishl`/`uextend` covers and the
        // mid-tree `bor`s are dead once the root is aliased — sweep
        // them here or they emit (the pipeline's `since_reap` window
        // is already spent by the late `ldmerge2` run).
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
    n
}
