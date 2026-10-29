//! `copypair` (`PLIRON_COPYPAIR=1`, opt-in, aarch64/SIMD): regroup straight-line
//! memory-copy runs so the emitter's `ldp`/`stp` fusion can fire.
//!
//! By-value aggregate moves reach the end of the pipeline as runs of
//! `v = load [a+o]; store v, [b+p]` pairs (SROA's per-slice expansion, then
//! the egraph elaborator re-places every load next to its single use).
//! Register allocation then gives each pair the same temporary:
//! `ldr x2,[a,#o]; str x2,[b,#p]; ldr x2,[a,#o+8]; ...` — four instructions
//! per 16 bytes, none of them pairable. LLVM emits `ldp q0,q1; stp q0,q1`
//! (two per 32 bytes).
//!
//! For a run of >= 2 adjacent pairs this pass
//!   1. merges neighbouring 8-byte pairs at consecutive offsets (both sides)
//!      into one `i64x2` pair,
//!   2. emits each group of up to `GROUP` items as loads first, stores
//!      second (distinct temporaries, adjacent accesses: the pair fusion in
//!      `VCode::emit` takes it from there).
//!
//! The regrouping moves a load above earlier stores of its group, so the run
//! is skipped unless every (earlier store, later load) pair is provably
//! disjoint: different stack slots, the same base value with disjoint byte
//! ranges, or a `readonly` load. The new loads drop `can_move`, otherwise the
//! compile-time egraph would sink them back next to their stores.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    Function, Inst, InstBuilder, InstructionData, MemFlagsData, Opcode, StackSlot, Type, Value,
    ValueDef, types,
};
use rustc_hash::FxHashMap;

/// Items (16-byte vectors or 8-byte scalars) per loads-then-stores group.
const GROUP: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Root {
    S(StackSlot),
    V(Value),
}

#[derive(Clone, Copy)]
struct Acc {
    root: Root,
    /// Base value as written (what the new insts reuse).
    base: Value,
    /// Offset from `root` (peeled constant adds included).
    off: i64,
    /// Immediate offset to put on the new inst, relative to `base`.
    imm: i32,
    flags: MemFlagsData,
}

#[derive(Clone, Copy)]
struct Pair {
    l: Inst,
    s: Inst,
    ty: Type,
    ld: Acc,
    st: Acc,
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

/// `v` as `root + off`, peeling `stack_addr` and constant adds.
fn root_of(func: &Function, v: Value) -> (Root, i64) {
    let mut v = func.dfg.resolve_aliases(v);
    let mut off = 0i64;
    for _ in 0..8 {
        let ValueDef::Result(i, 0) = func.dfg.value_def(v) else {
            break;
        };
        match func.dfg.insts[i] {
            InstructionData::StackAddr {
                opcode: Opcode::StackAddr,
                stack_slot,
                offset,
            } => return (Root::S(stack_slot), off + i64::from(i32::from(offset))),
            InstructionData::Binary {
                opcode: Opcode::Iadd,
                args: [a, b],
            } => {
                if let Some(c) = iconst(func, b) {
                    off = off.wrapping_add(c);
                    v = func.dfg.resolve_aliases(a);
                } else if let Some(c) = iconst(func, a) {
                    off = off.wrapping_add(c);
                    v = func.dfg.resolve_aliases(b);
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    (Root::V(v), off)
}

fn mk_acc(func: &Function, base: Value, imm: i32, f: MemFlagsData) -> Acc {
    let base = func.dfg.resolve_aliases(base);
    let (root, off) = root_of(func, base);
    Acc {
        root,
        base,
        off: off.wrapping_add(i64::from(imm)),
        imm,
        flags: f,
    }
}

/// Flags for a regrouped access: everything but `can_move` (see module doc)
/// and `aligned` (a wider access may not be as aligned as its halves).
fn plain(f: MemFlagsData) -> MemFlagsData {
    let mut nf = MemFlagsData::new()
        .with_alias_region(f.alias_region())
        .with_trap_code(f.trap_code());
    if let Some(e) = f.explicit_endianness() {
        nf = nf.with_endianness(e);
    }
    if f.notrap() {
        nf = nf.with_notrap();
    }
    if f.readonly() {
        nf = nf.with_readonly();
    }
    nf
}

fn disjoint(l: &Acc, lsz: i64, s: &Acc, ssz: i64) -> bool {
    if l.flags.readonly() {
        return true;
    }
    let same = match (l.root, s.root) {
        (Root::S(a), Root::S(b)) => {
            if a != b {
                return true;
            }
            true
        }
        (Root::V(a), Root::V(b)) => a == b,
        _ => false,
    };
    same && (l.off + lsz <= s.off || s.off + ssz <= l.off)
}

fn use_counts(func: &Function) -> FxHashMap<Value, u32> {
    let mut m: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *m.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    m
}

fn as_pair(func: &Function, uses: &FxHashMap<Value, u32>, l: Inst, s: Inst) -> Option<Pair> {
    let InstructionData::Load {
        opcode: Opcode::Load,
        arg,
        offset: loff,
        flags: lf,
    } = func.dfg.insts[l]
    else {
        return None;
    };
    let InstructionData::Store {
        opcode: Opcode::Store,
        args: [sv, sbase],
        offset: soff,
        flags: sf,
    } = func.dfg.insts[s]
    else {
        return None;
    };
    let v = func.dfg.first_result(l);
    if func.dfg.resolve_aliases(sv) != v || uses.get(&v) != Some(&1) {
        return None;
    }
    let ty = func.dfg.value_type(v);
    if ty != types::I64 && ty != types::I64X2 {
        return None;
    }
    let lflags = func.dfg.mem_flags[lf];
    let sflags = func.dfg.mem_flags[sf];
    // Never reorder an access that may trap (the loads move above stores).
    if !lflags.notrap() || !sflags.notrap() {
        return None;
    }
    Some(Pair {
        l,
        s,
        ty,
        ld: mk_acc(func, arg, i32::from(loff), lflags),
        st: mk_acc(func, sbase, i32::from(soff), sflags),
    })
}

/// Returns the number of runs rewritten.
pub fn run(func: &mut Function) -> usize {
    let uses = use_counts(func);
    let mut n = 0;
    let blocks: Vec<_> = func.layout.blocks().collect();
    for b in blocks {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        let mut i = 0;
        while i + 1 < insts.len() {
            let Some(first) = as_pair(func, &uses, insts[i], insts[i + 1]) else {
                i += 1;
                continue;
            };
            let mut run = vec![first];
            let mut j = i + 2;
            while j + 1 < insts.len() {
                match as_pair(func, &uses, insts[j], insts[j + 1]) {
                    Some(p) => run.push(p),
                    None => break,
                }
                j += 2;
            }
            if run.len() >= 2 && legal(&run) {
                let next = insts[j];
                if rewrite(func, &run, next) {
                    n += 1;
                }
            }
            i = j.max(i + 1);
        }
    }
    n
}

/// Every load of the run may move above every earlier store of it.
fn legal(run: &[Pair]) -> bool {
    for (k, p) in run.iter().enumerate() {
        let lsz = i64::from(p.ty.bytes());
        for q in &run[..k] {
            if !disjoint(&p.ld, lsz, &q.st, i64::from(q.ty.bytes())) {
                return false;
            }
        }
    }
    true
}

#[derive(Clone, Copy)]
struct Item {
    ty: Type,
    ld: Acc,
    st: Acc,
    /// Original pair when unmerged (its value type/flags are reused).
    merged: bool,
}

fn ok_vec_off(off: i64) -> bool {
    // `ldur`/`stur` reach [-256, 255] unscaled; beyond that the scaled
    // form needs a multiple of 16.
    (-256..256).contains(&off) || off % 16 == 0
}

fn rewrite(func: &mut Function, run: &[Pair], next: Inst) -> bool {
    // 1. merge adjacent i64 pairs into i64x2 pairs.
    let mut items: Vec<Item> = Vec::new();
    let mut k = 0;
    while k < run.len() {
        let p = run[k];
        let mergeable = |a: &Pair, b: &Pair| {
            a.ty == types::I64
                && b.ty == types::I64
                && a.ld.base == b.ld.base
                && a.st.base == b.st.base
                && b.ld.imm == a.ld.imm.wrapping_add(8)
                && b.st.imm == a.st.imm.wrapping_add(8)
                && plain(a.ld.flags) == plain(b.ld.flags)
                && plain(a.st.flags) == plain(b.st.flags)
                && ok_vec_off(i64::from(a.ld.imm))
                && ok_vec_off(i64::from(a.st.imm))
        };
        if k + 1 < run.len() && mergeable(&p, &run[k + 1]) {
            items.push(Item {
                ty: types::I64X2,
                ld: p.ld,
                st: p.st,
                merged: true,
            });
            k += 2;
        } else {
            items.push(Item {
                ty: p.ty,
                ld: p.ld,
                st: p.st,
                merged: false,
            });
            k += 1;
        }
    }
    // Worth it only if something merged or two neighbours can pair up.
    let adjacent = items.windows(2).any(|w| {
        let sz = i32::try_from(w[0].ty.bytes()).unwrap_or(0);
        w[0].ld.base == w[1].ld.base
            && w[0].st.base == w[1].st.base
            && w[0].ty == w[1].ty
            && w[1].ld.imm.wrapping_sub(w[0].ld.imm).abs() == sz
            && w[1].st.imm.wrapping_sub(w[0].st.imm).abs() == sz
    });
    if !adjacent && !items.iter().any(|it| it.merged) {
        return false;
    }
    // 2. emit groups, loads then stores, before `next`; drop the old insts.
    let mut pos = FuncCursor::new(func).at_inst(next);
    for g in items.chunks(GROUP) {
        let mut vals: Vec<Value> = Vec::with_capacity(g.len());
        for it in g {
            let f = plain(it.ld.flags);
            let v = pos.ins().load(it.ty, f, it.ld.base, it.ld.imm);
            vals.push(v);
        }
        for (it, v) in g.iter().zip(vals) {
            let f = plain(it.st.flags);
            pos.ins().store(f, v, it.st.base, it.st.imm);
        }
    }
    for p in run {
        func.layout.remove_inst(p.s);
        func.layout.remove_inst(p.l);
    }
    true
}
