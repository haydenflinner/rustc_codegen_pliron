//! SLP-lite: pack lane-wise scalar inst trees whose results are stored to
//! adjacent addresses into vector ops — the `Vec4 {x, y, z, w}` shape that
//! glam-style code produces and LLVM SLP-vectorizes (`ldr q; fadd.4s; str q`).
//!
//! Seeds from N stores to `base + i*esz` (N = 8 or 16 bytes of lanes) whose
//! stored values are isomorphic insts; operands pack recursively if they are
//! adjacent loads, shared scalars (`splat`), equal constants, or recursively
//! isomorphic. Emits one vector store + vector tree at the position of the
//! first member store, with the scalar members removed.
//!
//! Ordering safety: no load/store/call/terminator may sit between the group's
//! first and last store, and no side-effecting inst may sit between the
//! earliest packed load and the emit point. Each stored lane value may only
//! be used by the group stores (otherwise the pack would need extractlanes).
//!
//! `PLIRON_SLP=0` disables it; `PLIRON_XPACK_DEBUG` logs conversions.
//!
//! xpack: pairs of isomorphic scalar insts — most often adjacent iterations
//! of an unrolled loop body — merge into one 2-lane vector op (`sqrtsd` ×2 →
//! `sqrtpd`). Operand columns pack like the store-seeded path (splat,
//! adjacent loads, isomorphic ops) plus a `splat`+`insertlane` gather leaf.
//! Lane results with uses the pack can't serve get an `extractlane`; uses
//! that sit *before* the merged op's emit point are sunk after it (pure
//! insts move freely, stores only across provably disjoint memory, and dead
//! overwritten stores drop out). `PLIRON_XPACK=0` disables this half.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::{EntityRef, SecondaryMap};
use cranelift_codegen::ir::{
    Block, Function, Inst, InstBuilder, InstructionData, MemFlagsData, Opcode, Type, Value,
    ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// `v.index() -> insts that use v` (alias-resolved at build), CSR
/// layout: `vec![SmallVec; num_values]` paid ~40B/value of init plus a
/// spill alloc for every >4-use value; two passes (count, fill) build
/// offsets + one flat array instead. Applied packs tombstone their
/// removed insts into `dead` rather than shifting `flat`, so `off`
/// stays valid.
struct Uses {
    off: Vec<u32>,
    flat: Vec<Inst>,
    dead: FxHashSet<Inst>,
}

impl Uses {
    /// Insts using `v`, in layout order, minus tombstoned members.
    fn iter(&self, v: usize) -> impl Iterator<Item = Inst> + '_ {
        let flat: &[Inst] = if v + 1 < self.off.len() {
            &self.flat[self.off[v] as usize..self.off[v + 1] as usize]
        } else {
            &[]
        };
        flat.iter().copied().filter(|u| !self.dead.contains(u))
    }

    /// Drop `i` from every use list that holds it.
    fn remove(&mut self, i: Inst) {
        self.dead.insert(i);
    }
}

/// Max recursion depth when packing operand trees.
const MAX_DEPTH: usize = 6;

/// Lane-wise opcodes that map to a vector form of the same opcode.
pub(crate) fn vecable(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Fadd
            | Opcode::Fsub
            | Opcode::Fmul
            | Opcode::Fdiv
            | Opcode::Fmin
            | Opcode::Fmax
            | Opcode::Iadd
            | Opcode::Isub
            | Opcode::Imul
            | Opcode::Band
            | Opcode::Bor
            | Opcode::Bxor
            | Opcode::Umin
            | Opcode::Umax
            | Opcode::Smin
            | Opcode::Smax
            | Opcode::Fneg
            | Opcode::Fabs
            | Opcode::Ineg
            | Opcode::Bnot
            | Opcode::Sqrt
    )
}

/// Instructions that can observe or mutate memory / have other side effects
/// and therefore cannot be crossed by a merged access.
fn side_effecting(func: &Function, i: Inst) -> bool {
    let op = func.dfg.insts[i].opcode();
    op.is_terminator()
        || op.is_call()
        || op.is_branch()
        || op.can_load()
        || op.can_store()
        || op.other_side_effects()
        || op.can_trap()
}

/// Instructions that can write memory or have unobservable-ordering effects:
/// a merged vector load cannot be moved across them.
fn writer(func: &Function, i: Inst) -> bool {
    let op = func.dfg.insts[i].opcode();
    op.is_terminator() || op.is_call() || op.can_store() || op.other_side_effects() || op.can_trap()
}

/// A packable operand column: how to produce the vector for N lane values.
enum Pack {
    /// All lanes share one scalar value.
    Splat(Value),
    /// Lanes are adjacent `load`s from `base + off + i*esz`.
    Load {
        base: Value,
        off: i64,
        flags: MemFlagsData,
    },
    /// Lanes are isomorphic insts of `op`. `row` indexes the xpack member
    /// table so extracts can find each pair's emitted vector.
    Op {
        op: Opcode,
        args: Vec<Pack>,
        row: usize,
    },
    /// Unrelated scalars joined by `splat` + `insertlane` (xpack only).
    Gather(Vec<Value>),
    /// Reuse the vector emitted for an identical earlier column (xpack
    /// only); the index is the `row` of that column's `Op` node.
    Dup(usize),
    /// Column values are `extractlane(v, 0)`/`extractlane(v, 1)` of one
    /// earlier vector — reuse `v` directly so sequential packs fuse instead
    /// of round-tripping through scalars (xpack only).
    Vec(Value),
    /// A pair of adjacent `store`s merged into one vector store; `data`
    /// packs the two stored scalars. Pack-root only (xpack only).
    Store2 {
        data: Box<Pack>,
        base: Value,
        off: i64,
        flags: MemFlagsData,
    },
}

/// Sentinel for insts outside the scanned block: `SecondaryMap` can't
/// distinguish an unset entry from one set to the default, and real
/// positions are always < the block's inst count.
const NO_POS: usize = usize::MAX;
/// Dense inst → index-in-block map; queried inside `col`/`sinkable` hot
/// loops where a hash map costs ~10x an array index.
type PosMap = SecondaryMap<Inst, usize>;
/// Dense inst membership set for the current block's consumed insts.
type TakenMap = SecondaryMap<Inst, bool>;

fn block_pos(insts: &[Inst]) -> PosMap {
    let mut pos = PosMap::with_default(NO_POS);
    for (k, &i) in insts.iter().enumerate() {
        pos[i] = k;
    }
    pos
}

fn pos_get(pos: &PosMap, i: Inst) -> Option<usize> {
    (pos[i] != NO_POS).then(|| pos[i])
}

#[track_caller]
fn pos_at(pos: &PosMap, i: Inst) -> usize {
    let p = pos[i];
    assert_ne!(p, NO_POS, "inst {i:?} is not in the scanned block");
    p
}

struct Ctx<'a> {
    func: &'a Function,
    domtree: &'a DominatorTree,
    /// Layout-order index of insts in the current block.
    pos: &'a PosMap,
    /// Insts of the current block in layout order.
    insts: &'a [Inst],
    block: Block,
    /// Emit point: position of the group's first member store.
    emit: usize,
    /// Earliest position among all packed loads (writer-span check).
    min_load: usize,
    /// Whole-function uses: `v -> set of using insts`.
    uses: &'a Uses,
    /// Members of the current store group (allowed inside the store span).
    members: FxHashSet<Inst>,
}

impl Ctx<'_> {
    /// `v` is defined by an inst in this block before the emit point.
    fn inst_before_emit(&self, v: Value) -> Option<Inst> {
        let ValueDef::Result(inst, 0) = self.func.dfg.resolved_value_def(v) else {
            return None;
        };
        (pos_get(self.pos, inst).is_some_and(|p| p < self.emit)).then_some(inst)
    }

    /// `v` is available at the emit point.
    fn avail(&self, v: Value) -> bool {
        match self.func.dfg.value_def(v) {
            ValueDef::Param(b, _) => self.domtree.block_dominates(b, self.block),
            ValueDef::Result(inst, _) => {
                if self.func.layout.inst_block(inst) == Some(self.block) {
                    pos_get(self.pos, inst).is_some_and(|p| p < self.emit)
                } else {
                    self.domtree
                        .dominates(inst, self.insts[self.emit], &self.func.layout)
                }
            }
            ValueDef::Union(..) => false,
        }
    }

    /// Try to pack the N lane values of one operand position (or the
    /// top-level stored values).
    fn pack(&mut self, vals: &[Value], depth: usize) -> Option<Pack> {
        if depth > MAX_DEPTH {
            return None;
        }
        let vals: Vec<Value> = vals
            .iter()
            .map(|&v| self.func.dfg.resolve_aliases(v))
            .collect();
        // All lanes identical → splat (any dominating scalar def).
        if vals.iter().all(|&v| v == vals[0]) {
            return self.avail(vals[0]).then_some(Pack::Splat(vals[0]));
        }
        self.pack_loads(&vals)
            .or_else(|| self.pack_ops(&vals, depth))
    }

    /// All lanes are `load` insts of the same element type/flags at
    /// consecutive addresses `base + i*esz`.
    fn pack_loads(&mut self, vals: &[Value]) -> Option<Pack> {
        let n = vals.len();
        let mut first = None;
        let mut esz = 0i64;
        let mut flags = None;
        for (i, &v) in vals.iter().enumerate() {
            let inst = self.inst_before_emit(v)?;
            let InstructionData::Load {
                opcode: Opcode::Load,
                arg,
                offset,
                flags: f,
            } = self.func.dfg.insts[inst]
            else {
                return None;
            };
            let fl = self.func.dfg.mem_flags[f];
            if !fl.notrap() {
                return None;
            }
            let ty = self.func.dfg.value_type(v);
            let e = i64::from(ty.bytes());
            match (esz, flags) {
                (0, None) => {
                    esz = e;
                    flags = Some(fl);
                }
                _ if esz == e && flags == Some(fl) => {}
                _ => return None,
            }
            let (base, off) = base_off(self.func, arg);
            let off = off + i64::from(offset);
            match first {
                None => first = Some((base, off)),
                Some((b0, o0)) if base == b0 && off == o0 + i as i64 * esz => {}
                _ => return None,
            }
            self.min_load = self.min_load.min(pos_at(self.pos, inst));
        }
        // The merged vector access must be 8 or 16 bytes.
        if esz * n as i64 != 8 && esz * n as i64 != 16 {
            return None;
        }
        let (base, off) = first?;
        i32::try_from(off).ok()?;
        Some(Pack::Load {
            base,
            off,
            flags: flags?,
        })
    }

    /// All lanes are inst results of one whitelisted opcode with packable
    /// operand columns.
    fn pack_ops(&mut self, vals: &[Value], depth: usize) -> Option<Pack> {
        let mut insts = Vec::with_capacity(vals.len());
        let (mut op, mut ty) = (None, None);
        for &v in vals {
            let inst = self.inst_before_emit(v)?;
            let o = self.func.dfg.insts[inst].opcode();
            let t = self.func.dfg.value_type(v);
            match (op, ty) {
                (None, None) if vecable(o) => {
                    op = Some(o);
                    ty = Some(t);
                }
                (Some(o0), Some(t0)) if o0 == o && t0 == t => {}
                _ => return None,
            }
            insts.push(inst);
        }
        let nargs = self.func.dfg.inst_args(insts[0]).len();
        let mut args = Vec::with_capacity(nargs);
        for j in 0..nargs {
            let col: Vec<Value> = insts
                .iter()
                .map(|&i| self.func.dfg.inst_args(i)[j])
                .collect();
            args.push(self.pack(&col, depth + 1)?);
        }
        Some(Pack::Op {
            op: op?,
            args,
            row: usize::MAX,
        })
    }
}

/// Split `ptr` into `(base, const_off)` — walks short `iadd(ptr, iconst)`
/// chains. The inst's immediate offset is added by the caller.
pub(crate) fn base_off(func: &Function, ptr: Value) -> (Value, i64) {
    let mut base = func.dfg.resolve_aliases(ptr);
    let mut off = 0i64;
    for _ in 0..4 {
        let ValueDef::Result(inst, 0) = func.dfg.value_def(base) else {
            break;
        };
        let InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } = func.dfg.insts[inst]
        else {
            break;
        };
        if let Some(k) = iconst_val(func, b) {
            off += k;
            base = func.dfg.resolve_aliases(a);
        } else if let Some(k) = iconst_val(func, a) {
            off += k;
            base = func.dfg.resolve_aliases(b);
        } else {
            break;
        }
    }
    (base, off)
}

fn iconst_val(func: &Function, v: Value) -> Option<i64> {
    let ValueDef::Result(inst, 0) = func.dfg.resolved_value_def(v) else {
        return None;
    };
    if let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = func.dfg.insts[inst]
    {
        Some(imm.bits())
    } else {
        None
    }
}

/// Emit a packed operand tree at the cursor; returns the vector value.
/// `rows`, when given, records the emitted vector for each `Pack::Op` whose
/// `row` index is valid (xpack extracts lane values from these).
fn emit_pack(pos: &mut FuncCursor, vt: Type, p: &Pack, mut rows: Option<&mut Vec<Value>>) -> Value {
    match p {
        Pack::Splat(v) => pos.ins().splat(vt, *v),
        Pack::Gather(vals) => {
            let mut v = pos.ins().splat(vt, vals[0]);
            for (k, &x) in vals.iter().enumerate().skip(1) {
                v = pos.ins().insertlane(v, x, k as u8);
            }
            v
        }
        &Pack::Load {
            base, off, flags, ..
        } => pos.ins().load(
            vt,
            wide_flags(flags),
            base,
            i32::try_from(off).expect("checked at pack time"),
        ),
        Pack::Op { op, args, row } => {
            let vs: Vec<Value> = args
                .iter()
                .map(|a| emit_pack(pos, vt, a, rows.as_deref_mut()))
                .collect();
            let v = emit_op(pos, *op, &vs);
            if let Some(rows) = rows
                && *row != usize::MAX
            {
                rows[*row] = v;
            }
            v
        }
        // The original column sits earlier in emission order, so its row
        // vector is already written.
        Pack::Dup(r) => rows.expect("xpack rows")[*r],
        Pack::Vec(v) => *v,
        Pack::Store2 {
            data,
            base,
            off,
            flags,
        } => {
            let v = emit_pack(pos, vt, data, rows);
            pos.ins().store(
                wide_flags(*flags),
                v,
                *base,
                i32::try_from(*off).expect("checked at pack time"),
            );
            v
        }
    }
}

fn emit_op(pos: &mut FuncCursor, op: Opcode, v: &[Value]) -> Value {
    let i = pos.ins();
    match (op, v) {
        (Opcode::Fadd, [a, b]) => i.fadd(*a, *b),
        (Opcode::Fsub, [a, b]) => i.fsub(*a, *b),
        (Opcode::Fmul, [a, b]) => i.fmul(*a, *b),
        (Opcode::Fdiv, [a, b]) => i.fdiv(*a, *b),
        (Opcode::Fmin, [a, b]) => i.fmin(*a, *b),
        (Opcode::Fmax, [a, b]) => i.fmax(*a, *b),
        (Opcode::Iadd, [a, b]) => i.iadd(*a, *b),
        (Opcode::Isub, [a, b]) => i.isub(*a, *b),
        (Opcode::Imul, [a, b]) => i.imul(*a, *b),
        (Opcode::Band, [a, b]) => i.band(*a, *b),
        (Opcode::Bor, [a, b]) => i.bor(*a, *b),
        (Opcode::Bxor, [a, b]) => i.bxor(*a, *b),
        (Opcode::Umin, [a, b]) => i.umin(*a, *b),
        (Opcode::Umax, [a, b]) => i.umax(*a, *b),
        (Opcode::Smin, [a, b]) => i.smin(*a, *b),
        (Opcode::Smax, [a, b]) => i.smax(*a, *b),
        (Opcode::Fneg, [a]) => i.fneg(*a),
        (Opcode::Fabs, [a]) => i.fabs(*a),
        (Opcode::Sqrt, [a]) => i.sqrt(*a),
        (Opcode::Ineg, [a]) => i.ineg(*a),
        (Opcode::Bnot, [a]) => i.bnot(*a),
        _ => unreachable!("vecable"),
    }
}

/// The merged access covers the full vector width, so a lane-sized `aligned`
/// flag would over-claim; everything else is preserved.
fn wide_flags(f: MemFlagsData) -> MemFlagsData {
    let mut nf = MemFlagsData::new()
        .with_alias_region(f.alias_region())
        .with_trap_code(f.trap_code());
    if let Some(e) = f.explicit_endianness() {
        nf = nf.with_endianness(e);
    }
    if f.readonly() {
        nf = nf.with_readonly();
    }
    nf
}

struct StoreInfo {
    inst: Inst,
    pos: usize,
    val: Value,
    base: Value,
    off: i64,
    flags: MemFlagsData,
    ty: Type,
}

/// Find groups of stores to `base + i*esz` in one block and pack them.
/// `uses` is the whole-function use map built once in `run()`; applied
/// packs remove their member insts' entries so later blocks see a fresh
/// map. Newly emitted pack insts are *not* added — an unrecorded use can
/// only keep a scalar lane def alive longer than needed, never corrupt
/// the plan (lane defs stay unless `remove_dead_insts` proves them dead).
fn build_uses(func: &Function) -> Uses {
    let n = func.dfg.num_values();
    let mut off = vec![0u32; n + 1];
    for b2 in func.layout.blocks() {
        for i in func.layout.block_insts(b2) {
            for v in func.dfg.inst_values(i) {
                off[func.dfg.resolve_aliases(v).index() + 1] += 1;
            }
        }
    }
    for i in 1..=n {
        off[i] += off[i - 1];
    }
    let mut flat = vec![Inst::new(0); off[n] as usize];
    let mut cur = off.clone();
    for b2 in func.layout.blocks() {
        for i in func.layout.block_insts(b2) {
            for v in func.dfg.inst_values(i) {
                let slot = &mut cur[func.dfg.resolve_aliases(v).index()];
                flat[*slot as usize] = i;
                *slot += 1;
            }
        }
    }
    Uses {
        off,
        flat,
        dead: FxHashSet::default(),
    }
}

fn try_block(
    func: &mut Function,
    domtree: &DominatorTree,
    uses: &mut Option<Uses>,
    _na: &FxHashSet<Value>,
    block: Block,
    x64: bool,
) -> usize {
    let insts: Vec<Inst> = func.layout.block_insts(block).collect();
    // Collect scalar stores.
    let mut stores: Vec<StoreInfo> = Vec::new();
    for &i in &insts {
        let InstructionData::Store {
            opcode: Opcode::Store,
            args,
            offset,
            flags,
        } = func.dfg.insts[i]
        else {
            continue;
        };
        let fl = func.dfg.mem_flags[flags];
        if !fl.notrap() {
            continue;
        }
        let val = func.dfg.resolve_aliases(args[0]);
        let ty = func.dfg.value_type(val);
        if ty.is_vector() || !matches!(ty.bytes(), 1 | 2 | 4 | 8) {
            continue;
        }
        let (base, off) = base_off(func, args[1]);
        stores.push(StoreInfo {
            inst: i,
            pos: 0, // filled below
            val,
            base,
            off: off + i64::from(offset),
            flags: fl,
            ty,
        });
    }
    if stores.len() < 2 {
        return 0;
    }
    let pos = block_pos(&insts);
    for s in &mut stores {
        s.pos = pos_at(&pos, s.inst);
    }
    // Group by (base, elem type, flags); within each, find consecutive-offset
    // runs of exactly `16/esz` or `8/esz` lanes.
    let mut by_key: FxHashMap<(Value, Type, MemFlagsData), Vec<&StoreInfo>> = FxHashMap::default();
    for s in &stores {
        by_key.entry((s.base, s.ty, s.flags)).or_default().push(s);
    }
    let mut changed = 0;
    let mut taken = TakenMap::new();
    for group in by_key.values() {
        let mut g: Vec<&StoreInfo> = group.clone();
        g.sort_by_key(|s| s.off);
        let esz = i64::from(g[0].ty.bytes());
        for &lanes in &[16 / esz, 8 / esz] {
            if lanes < 2 || lanes > 16 {
                continue;
            }
            // x64 ISLE only lowers 128-bit vectors — 8-byte packs
            // (i32x2, f32x2, i8x8, ...) reach emit as `unimplemented`.
            if x64 && lanes * esz != 16 {
                continue;
            }
            let mut run: Vec<&StoreInfo> = Vec::new();
            for &s in &g {
                if taken[s.inst] {
                    continue;
                }
                match run.last() {
                    None => run.push(s),
                    Some(&last) if s.off == last.off + esz => run.push(s),
                    Some(&last) if s.off == last.off => continue, // overwritten
                    _ => {
                        run.clear();
                        run.push(s);
                    }
                }
                if run.len() == lanes as usize && i32::try_from(run[0].off).is_ok() {
                    let emit = run.iter().map(|s| s.pos).min().unwrap();
                    let us = uses.get_or_insert_with(|| build_uses(func));
                    let plan = Ctx {
                        func: &*func,
                        domtree,
                        pos: &pos,
                        insts: &insts,
                        block,
                        emit,
                        min_load: usize::MAX,
                        uses: &*us,
                        members: run.iter().map(|s| s.inst).collect(),
                    }
                    .analyze(&run);
                    if let Some((p, vt)) = plan {
                        let emit_inst = insts[emit];
                        let mut cur = FuncCursor::new(func).at_inst(emit_inst);
                        let vec = emit_pack(&mut cur, vt, &p, None);
                        cur.ins().store(
                            wide_flags(run[0].flags),
                            vec,
                            run[0].base,
                            run[0].off as i32, // checked above
                        );
                        for s in &run {
                            // Drop the removed store's use entries so the
                            // map stays fresh for later blocks.
                            uses.as_mut().unwrap().remove(s.inst);
                            cur.func.layout.remove_inst(s.inst);
                            cur.func.dfg.clear_results(s.inst);
                        }
                        changed += run.len();
                        for s in &run {
                            taken[s.inst] = true;
                        }
                    }
                    run.clear();
                }
            }
        }
    }
    changed
}

impl Ctx<'_> {
    /// Validate one candidate store group; returns the packed tree and its
    /// vector type for the caller to emit.
    fn analyze(mut self, run: &[&StoreInfo]) -> Option<(Pack, Type)> {
        // Store span: between the first and last member store, only pure
        // insts or group members may appear.
        let last_pos = run.iter().map(|s| s.pos).max().unwrap();
        for p in self.emit + 1..last_pos {
            let i = self.insts[p];
            if !self.members.contains(&i) && side_effecting(self.func, i) {
                return None;
            }
        }
        // Stored values may only be used by the group stores.
        let vals: Vec<Value> = run.iter().map(|s| s.val).collect();
        for &v in &vals {
            if !self.uses.iter(v.index()).all(|u| self.members.contains(&u)) {
                return None;
            }
        }
        // The store base must dominate the emit point.
        if !self.avail(run[0].base) {
            return None;
        }
        // Pack the stored-value tree.
        let p = self.pack(&vals, 0)?;
        // Writer check between the earliest packed load and the emit point.
        if self.min_load != usize::MAX {
            for pp in self.min_load + 1..self.emit {
                if writer(self.func, self.insts[pp]) {
                    return None;
                }
            }
        }
        let lanes = vals.len() as u32;
        Some((p, run[0].ty.by(lanes)?))
    }
}

/// Pack lane-wise scalar inst trees feeding adjacent stores into vector ops.
pub fn run(
    func: &mut Function,
    an: &mut crate::pipeline::Analysis,
    na: &FxHashSet<Value>,
    simd: bool,
    x64: bool,
) -> usize {
    if !simd {
        return 0;
    }
    // One use map for the whole function, shared by every block's store
    // grouping — building it per block made this pass O(blocks × insts).
    // Built lazily on the first candidate run: most functions have no
    // adjacent-store group at all, so map construction — the pass's
    // dominant cost — is skipped entirely there.
    let mut uses: Option<Uses> = None;
    let (_, domtree) = an.cfg_dt(func);
    let mut changed = 0;
    for block in func.layout.blocks().collect::<Vec<_>>() {
        changed += try_block(func, domtree, &mut uses, na, block, x64);
    }
    // try_block's emits aren't recorded in the map (its own checks never
    // consult them — lane values are scalars it could re-pack, bases are
    // checked by domination, not use lists). xpack's plans *do* consult
    // uses of arbitrary values, so it must not see try_block's stale map.
    if changed > 0 {
        uses = None;
    }
    if crate::pass_enabled("PLIRON_XPACK") {
        // `param_src` walks the whole function (CFG + param alias
        // facts); xpack's applies never touch block params, so one
        // map serves every block.
        let ps = crate::loadfwd::param_src(func);
        for block in func.layout.blocks().collect::<Vec<_>>() {
            changed += xpack_block(func, &domtree, na, block, &mut uses, &ps, x64);
        }
    }
    if changed > 0 {
        crate::jumpthread::remove_dead_insts(func, false);
    }
    changed
}

// ============================ xpack ============================
//
// Seed on a pair of same-opcode scalar insts — adjacent components of one
// unrolled iteration or the same op in two adjacent iterations — and merge
// them into one 2-lane vector op. Operand columns pack like the store-seeded
// path, plus `Gather` (splat+insertlane) for unrelated scalars.
//
// Scheduling: the vector op emits just before the later lane inst. Uses of
// lane-0 results that sit earlier are sunk after the emit point: pure insts
// move freely, stores only across provably disjoint memory ops, and stores
// whose bytes are overwritten before any read drop dead.

/// A `(root, offset, size)` memory footprint for an access inst.
fn mem_range(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    i: Inst,
) -> Option<(crate::loadfwd::Root, i64, u32)> {
    match func.dfg.insts[i] {
        InstructionData::Load {
            opcode: Opcode::Load,
            arg,
            offset,
            ..
        } => {
            // Normalized root: a `noalias` scope name over a stack_addr
            // peels to the slot so `may_overlap` can't read a same-slot
            // `V`/`S` pair as disjoint.
            let (r, o) = crate::loadfwd::norm_root(func, ps, na, arg);
            let sz = func.dfg.value_type(func.dfg.inst_results(i)[0]).bytes();
            Some((r, o + i64::from(i32::from(offset)), sz))
        }
        InstructionData::Store {
            opcode: Opcode::Store,
            args,
            offset,
            ..
        } => {
            let (r, o) = crate::loadfwd::norm_root(func, ps, na, args[1]);
            let sz = func.dfg.value_type(args[0]).bytes();
            Some((r, o + i64::from(i32::from(offset)), sz))
        }
        _ => None,
    }
}

/// Whether two `(root, offset, size)` footprints may alias.
fn may_overlap(a: (crate::loadfwd::Root, i64, u32), b: (crate::loadfwd::Root, i64, u32)) -> bool {
    use crate::loadfwd::Root;
    match (a.0, b.0) {
        (Root::S(sa), Root::S(sb)) if sa != sb => return false,
        (Root::S(_), Root::V(_)) | (Root::V(_), Root::S(_)) => return false,
        (Root::V(va), Root::V(vb)) if va != vb => return true,
        _ => {}
    }
    a.1 < b.1 + i64::from(b.2) && b.1 < a.1 + i64::from(a.2)
}

/// `v` is the single result of an inst at `pos` in the block's inst list.
fn res_of(func: &Function, i: Inst) -> Option<Value> {
    let r = func.dfg.inst_results(i);
    (r.len() == 1).then(|| func.dfg.resolve_aliases(r[0]))
}

struct X<'a> {
    func: &'a Function,
    ps: &'a FxHashMap<Value, Value>,
    na: &'a FxHashSet<Value>,
    domtree: &'a DominatorTree,
    pos: &'a PosMap,
    insts: &'a [Inst],
    block: Block,
    /// Emit point: layout position of the lane-1 (later) root inst.
    emit: usize,
    /// Member lane-inst pairs, in pack order.
    rows: Vec<(Inst, Inst)>,
    members: FxHashSet<Inst>,
    /// Merged vector-load footprints `(root, off, bytes)` for writer checks.
    loads: Vec<(crate::loadfwd::Root, i64, u32)>,
    min_load: usize,
    gathers: usize,
    splats: usize,
    /// Rows the profitability prune dropped back to gathers.
    pruned: FxHashSet<usize>,
    /// Member insts retained as scalar computation because some other column
    /// gathers/splats their result; they keep their args scalar too.
    keep: FxHashSet<Inst>,
    /// Identical columns `(a, b)` already packed, mapped to their `Op` row.
    colmap: FxHashMap<(Value, Value), usize>,
    /// `PLIRON_XPACK_DEBUG` — hoisted out of the `col` recursion where
    /// the env lookup was re-taken per node.
    dbg: bool,
}

impl X<'_> {
    fn def(&self, v: Value) -> Option<Inst> {
        let ValueDef::Result(inst, 0) = self.func.dfg.resolved_value_def(v) else {
            return None;
        };
        Some(inst)
    }

    /// `v` is defined in this block strictly before the emit point.
    fn before_emit(&self, v: Value) -> Option<Inst> {
        let i = self.def(v)?;
        (pos_get(self.pos, i).is_some_and(|p| p < self.emit)).then_some(i)
    }

    /// `a` and `b` are the same constant materialized twice (unroll copies
    /// duplicate `iconst`/`f64const` rather than sharing one value).
    fn same_const(&self, a: Value, b: Value) -> bool {
        let (ia, ib) = match (self.def(a), self.def(b)) {
            (Some(x), Some(y)) => (x, y),
            _ => return false,
        };
        match (&self.func.dfg.insts[ia], &self.func.dfg.insts[ib]) {
            (
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm: x,
                },
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm: y,
                },
            ) => x.bits() == y.bits(),
            (
                InstructionData::UnaryIeee64 {
                    opcode: Opcode::F64const,
                    imm: x,
                },
                InstructionData::UnaryIeee64 {
                    opcode: Opcode::F64const,
                    imm: y,
                },
            ) => x.bits() == y.bits(),
            (
                InstructionData::UnaryIeee32 {
                    opcode: Opcode::F32const,
                    imm: x,
                },
                InstructionData::UnaryIeee32 {
                    opcode: Opcode::F32const,
                    imm: y,
                },
            ) => x.bits() == y.bits(),
            _ => false,
        }
    }

    /// `v` can be read at the emit point.
    fn avail(&self, v: Value) -> bool {
        match self.func.dfg.value_def(v) {
            ValueDef::Param(b, _) => self.domtree.block_dominates(b, self.block),
            ValueDef::Result(inst, _) => {
                if self.func.layout.inst_block(inst) == Some(self.block) {
                    pos_get(self.pos, inst).is_some_and(|p| p < self.emit)
                } else {
                    self.domtree
                        .dominates(inst, self.insts[self.emit], &self.func.layout)
                }
            }
            ValueDef::Union(..) => false,
        }
    }

    /// Pack one operand column `[a, b]`; `depth` bounds recursion.
    fn col(&mut self, a: Value, b: Value, depth: usize) -> Option<Pack> {
        if depth > MAX_DEPTH {
            return None;
        }
        let (a, b) = (
            self.func.dfg.resolve_aliases(a),
            self.func.dfg.resolve_aliases(b),
        );
        if a == b || self.same_const(a, b) {
            self.splats += 1;
            return self.avail(a).then_some(Pack::Splat(a));
        }
        // An identical earlier column (e.g. the two operands of `dx*dx`)
        // reuses the vector its row already emits.
        if let Some(&r) = self.colmap.get(&(a, b)) {
            return Some(Pack::Dup(r));
        }
        // `extractlane(v,0)`/`extractlane(v,1)` of one earlier vector: the
        // column is `v` itself. This is what fuses sequential packs — a
        // consumer-side pack reuses the lanes a producer pack emitted
        // instead of gathering the extracted scalars back.
        let lane_of = |v: Value| -> Option<(Value, u8)> {
            let i = self.def(v)?;
            let InstructionData::BinaryImm8 {
                opcode: Opcode::Extractlane,
                imm,
                arg,
            } = self.func.dfg.insts[i]
            else {
                return None;
            };
            Some((arg, imm.into()))
        };
        let (la, lb) = (lane_of(a), lane_of(b));
        if let (Some((va, 0)), Some((vb, 1))) = (la, lb) {
            if va == vb
                && self
                    .func
                    .dfg
                    .value_type(a)
                    .by(2)
                    .is_some_and(|vt| vt == self.func.dfg.value_type(va))
                && self.avail(va)
            {
                return Some(Pack::Vec(va));
            }
            if self.dbg {
                tracing::debug!(target: "pliron::slp",
                    "    xpack novec d{depth}: {a:?}/{b:?} va={va:?} vb={vb:?} samev={} avail={}",
                    va == vb,
                    self.avail(va)
                );
            }
        } else if self.dbg && (la.is_some() || lb.is_some()) {
            tracing::debug!(target: "pliron::slp", "    xpack novec-lanes d{depth}: {a:?}={la:?} {b:?}={lb:?}");
        }
        // A column may legitimately need a member's scalar result (e.g. the
        // `dx` shared by `dx*dx` and another row). Mark the producer to stay
        // alive as a scalar inst rather than rejecting the pack.
        for &v in &[a, b] {
            if let Some(d) = self.def(v)
                && self.members.contains(&d)
            {
                self.keep.insert(d);
            }
        }
        // Both loads of the same type: identical addresses splat, adjacent
        // merge, otherwise the lanes are an unrelated gather.
        if let (Some(ia), Some(ib)) = (self.before_emit(a), self.before_emit(b))
            && let (Some(ra), Some(rb)) = (
                mem_range(self.func, self.ps, self.na, ia),
                mem_range(self.func, self.ps, self.na, ib),
            )
            && matches!(
                self.func.dfg.insts[ia],
                InstructionData::Load {
                    opcode: Opcode::Load,
                    ..
                }
            )
            && matches!(
                self.func.dfg.insts[ib],
                InstructionData::Load {
                    opcode: Opcode::Load,
                    ..
                }
            )
        {
            let fa = self.func.dfg.mem_flags[self.func.dfg.insts[ia].memflags().unwrap()];
            let fb = self.func.dfg.mem_flags[self.func.dfg.insts[ib].memflags().unwrap()];
            if fa == fb && fa.notrap() && ra.0 == rb.0 && ra.2 == rb.2 {
                if ra.1 == rb.1 {
                    self.splats += 1;
                    return Some(Pack::Splat(a));
                }
                if rb.1 == ra.1 + i64::from(ra.2) && (ra.2 == 4 || ra.2 == 8) {
                    self.min_load = self
                        .min_load
                        .min(pos_at(self.pos, ia))
                        .min(pos_at(self.pos, ib));
                    self.loads.push(ra);
                    self.loads.push(rb);
                    let InstructionData::Load { arg, offset, .. } = self.func.dfg.insts[ia] else {
                        unreachable!()
                    };
                    let (base, off) = base_off(self.func, arg);
                    let off = off + i64::from(offset);
                    i32::try_from(off).ok()?;
                    return Some(Pack::Load {
                        base,
                        off,
                        flags: fa,
                    });
                }
            }
            self.gathers += 1;
            return (self.avail(a) && self.avail(b)).then_some(Pack::Gather(vec![a, b]));
        }
        // Isomorphic ops: pack recursively. A failed attempt rolls back the
        // rows/members it pushed, then the column falls back to a gather.
        let dbg = self.dbg;
        if dbg
            && depth <= 6
            && let (da, db) = (self.def(a), self.def(b))
            && (da.is_none()
                || db.is_none()
                || self.func.dfg.insts[da.unwrap()].opcode()
                    == self.func.dfg.insts[db.unwrap()].opcode())
        {
            tracing::debug!(target: "pliron::slp",
                "    xpack col-enter d{depth}: {a:?}(def={da:?} pos={:?}) {b:?}(def={db:?} pos={:?}) emit={}",
                da.and_then(|i| pos_get(self.pos, i)),
                db.and_then(|i| pos_get(self.pos, i)),
                self.emit
            );
        }
        if let (Some(ia), Some(ib)) = (self.before_emit(a), self.before_emit(b)) {
            let oa = self.func.dfg.insts[ia].opcode();
            let ta = self.func.dfg.value_type(a);
            let tb = self.func.dfg.value_type(b);
            if oa == self.func.dfg.insts[ib].opcode()
                && ta == tb
                && vecable(oa)
                && !self.members.contains(&ia)
                && !self.members.contains(&ib)
            {
                let nargs = self.func.dfg.inst_args(ia).len();
                if nargs == self.func.dfg.inst_args(ib).len()
                    && self.func.dfg.inst_results(ia).len() == 1
                    && self.func.dfg.inst_results(ib).len() == 1
                {
                    let row = self.rows.len();
                    let n_loads = self.loads.len();
                    let min_load = self.min_load;
                    self.members.insert(ia);
                    self.members.insert(ib);
                    self.rows.push((ia, ib));
                    let mut args = Vec::with_capacity(nargs);
                    let mut ok = true;
                    for j in 0..nargs {
                        let (ca, cb) = (
                            self.func.dfg.inst_args(ia)[j],
                            self.func.dfg.inst_args(ib)[j],
                        );
                        match self.col(ca, cb, depth + 1) {
                            Some(p) => args.push(p),
                            None => {
                                if dbg {
                                    tracing::debug!(target: "pliron::slp",
                                        "    xpack col-rec d{depth}: {oa:?} arg{j} {ca:?}/{cb:?}"
                                    );
                                }
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        self.colmap.insert((a, b), row);
                        return Some(Pack::Op { op: oa, args, row });
                    }
                    // Rolled-back rows own their `members` entries: the
                    // `!members.contains` guard above only ever inserts
                    // fresh insts, so remove just the truncated pairs
                    // instead of rebuilding the whole set.
                    for &(x, y) in &self.rows[row..] {
                        self.members.remove(&x);
                        self.members.remove(&y);
                    }
                    self.rows.truncate(row);
                    self.loads.truncate(n_loads);
                    self.min_load = min_load;
                    self.colmap.retain(|_, &mut r| r < row);
                }
                if dbg {
                    tracing::debug!(target: "pliron::slp",
                        "    xpack nopair d{depth}: {a:?}/{b:?} -> {:?}/{:?}",
                        self.func.dfg.insts[ia].opcode(),
                        self.func.dfg.insts[ib].opcode()
                    );
                }
            } else if dbg {
                let why = if oa != self.func.dfg.insts[ib].opcode() {
                    "op"
                } else if ta != tb {
                    "ty"
                } else if !vecable(oa) {
                    "vecable"
                } else {
                    "member"
                };
                tracing::debug!(target: "pliron::slp", "    xpack nopair-{why} d{depth}: {a:?}/{b:?} ({oa:?})");
            }
        }
        self.gathers += 1;
        if dbg && !(self.avail(a) && self.avail(b)) {
            tracing::debug!(target: "pliron::slp",
                "    xpack gather-unavail: {a:?} (avail={}) {b:?} (avail={})",
                self.avail(a),
                self.avail(b)
            );
        }
        (self.avail(a) && self.avail(b)).then_some(Pack::Gather(vec![a, b]))
    }

    /// Mark `p`'s row and every descendant row pruned: their lane insts stay
    /// scalar and must not join the member set.
    fn prune_rows(&mut self, p: &Pack) {
        if let Pack::Op { args, row, .. } = p {
            self.pruned.insert(*row);
            for a in args {
                self.prune_rows(a);
            }
        }
    }

    /// `(emitted insts, kept rows)` cost of a column/subtree, pruning `Op`
    /// subtrees whose vector form costs more than gathering the scalar
    /// results: an `Op` subtree emits `e` insts and deletes `2·n` scalars,
    /// while a gather emits 2 and keeps the scalars, so the subtree survives
    /// iff `e − 2n ≤ 2 + bonus`. `Sqrt`/`Fdiv` rows get a bonus — halving
    /// port pressure is worth insts.
    fn col_cost(&mut self, p: &mut Pack) -> (usize, usize) {
        match p {
            Pack::Splat(_) | Pack::Load { .. } => (1, 0),
            Pack::Gather(_) => (2, 0),
            Pack::Vec(_) => (0, 0),
            Pack::Store2 { data, .. } => {
                let (e, n) = self.col_cost(data);
                (1 + e, 1 + n)
            }
            Pack::Dup(r) => {
                // The original column was already emitted; if its row was
                // pruned away, this lane must gather the scalar results.
                if self.pruned.contains(r)
                    && let (Some(va), Some(vb)) = (
                        res_of(self.func, self.rows[*r].0),
                        res_of(self.func, self.rows[*r].1),
                    )
                {
                    *p = Pack::Gather(vec![va, vb]);
                    return (2, 0);
                }
                (0, 0)
            }
            Pack::Op { op, args, row } => {
                let (mut e, mut n) = (1, 1);
                for a in args.iter_mut() {
                    let (e2, n2) = self.col_cost(a);
                    e += e2;
                    n += n2;
                }
                let bonus = 2 * usize::from(matches!(op, Opcode::Sqrt | Opcode::Fdiv));
                if *row != 0
                    && e > 2 * n + 2 + bonus
                    && let (Some(va), Some(vb)) = (
                        res_of(self.func, self.rows[*row].0),
                        res_of(self.func, self.rows[*row].1),
                    )
                {
                    self.prune_rows(p);
                    *p = Pack::Gather(vec![va, vb]);
                    (2, 0)
                } else {
                    (e, n)
                }
            }
        }
    }
}

/// A lane value's external use `u` must be servable by an extract emitted at
/// `emit`: uses in this block at positions after `emit`, or insts in other
/// blocks `emit` dominates.
fn ext_ok(x: &X<'_>, u: Inst) -> bool {
    if x.func.layout.inst_block(u) == Some(x.block) {
        pos_get(x.pos, u).is_some_and(|p| p > x.emit)
    } else {
        x.domtree.dominates(x.insts[x.emit], u, &x.func.layout)
    }
}

/// `u` can sink past the emit point: it is pure, or a store that crosses no
/// overlapping access in `(pos(u), emit]`. `skip` holds insts the plan is
/// already removing or moving, which stay ordered the same way relative to
/// `u` and so can't block it.
fn sinkable(x: &X<'_>, u: Inst, dead: &FxHashSet<Inst>, sink: &FxHashSet<Inst>) -> Option<bool> {
    let op = x.func.dfg.insts[u].opcode();
    if !(op.is_terminator()
        || op.is_branch()
        || op.is_call()
        || op.can_load()
        || op.can_store()
        || op.other_side_effects()
        || op.can_trap())
    {
        return Some(true); // pure inst: sinks freely
    }
    if op != Opcode::Store {
        return None;
    }
    let ur = mem_range(x.func, x.ps, x.na, u)?;
    if store_dead(x, u, ur, dead) {
        return Some(false); // dead store: drop, don't sink
    }
    // The merged vector loads execute at the emit point; `u` must not write
    // their bytes after they've been read.
    if x.loads.iter().any(|&lr| may_overlap(ur, lr)) {
        return None;
    }
    let pu = pos_at(x.pos, u);
    for p in pu + 1..=x.emit {
        let i2 = x.insts[p];
        if dead.contains(&i2) || sink.contains(&i2) {
            continue;
        }
        let o2 = x.func.dfg.insts[i2].opcode();
        if o2.is_call() || o2.is_terminator() || o2.is_branch() {
            return None;
        }
        if let Some(r2) = mem_range(x.func, x.ps, x.na, i2)
            && may_overlap(ur, r2)
        {
            return None;
        }
        if (o2.can_load() || o2.can_store() || o2.other_side_effects())
            && mem_range(x.func, x.ps, x.na, i2).is_none()
        {
            return None;
        }
    }
    Some(true)
}

/// `u` is a store whose bytes are fully overwritten before any read of them.
fn store_dead(
    x: &X<'_>,
    u: Inst,
    ur: (crate::loadfwd::Root, i64, u32),
    dead: &FxHashSet<Inst>,
) -> bool {
    let mut p = pos_at(x.pos, u) + 1;
    while p < x.insts.len() {
        let i2 = x.insts[p];
        p += 1;
        if dead.contains(&i2) {
            continue;
        }
        let o2 = x.func.dfg.insts[i2].opcode();
        if o2.is_terminator() || o2.is_branch() || o2.is_call() {
            return false;
        }
        let Some(r2) = mem_range(x.func, x.ps, x.na, i2) else {
            continue;
        };
        if !may_overlap(ur, r2) {
            continue;
        }
        if o2 == Opcode::Store && r2.1 <= ur.1 && ur.1 + i64::from(ur.2) <= r2.1 + i64::from(r2.2) {
            return true; // fully covered by a later store
        }
        return false; // a read or partial write of the bytes
    }
    false
}

/// Try to merge lane insts `ia`/`ib` into a vector op; on success the plan's
/// edits are applied by [`apply_pair`].
fn plan_pair(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    domtree: &DominatorTree,
    uses: &Uses,
    insts: &[Inst],
    pos: &PosMap,
    block: Block,
    taken: &TakenMap,
    ia: Inst,
    ib: Inst,
    x64: bool,
) -> Option<(
    Pack,
    Type,
    Vec<(usize, (Inst, Inst))>,
    Vec<Inst>,
    Vec<Inst>,
    Vec<(Value, usize, u8)>,
    FxHashSet<Inst>,
)> {
    let dbg = tracing::enabled!(target: "pliron::slp", tracing::Level::DEBUG);
    macro_rules! bail {
        ($why:expr) => {{
            if dbg {
                tracing::debug!(target: "pliron::slp",
                    "  xpack {} {ia:?}/{ib:?}: {}",
                    $why,
                    func.dfg.insts[ia].opcode()
                );
            }
            return None;
        }};
    }
    if taken[ia] || taken[ib] {
        return None;
    }
    let op = func.dfg.insts[ia].opcode();
    if op != func.dfg.insts[ib].opcode() || !vecable(op) {
        return None;
    }
    let (ra, rb) = (res_of(func, ia)?, res_of(func, ib)?);
    let ty = func.dfg.value_type(ra);
    if ty.is_vector() || ty != func.dfg.value_type(rb) || !matches!(ty.bytes(), 4 | 8) {
        return None;
    }
    if func.dfg.inst_args(ia).len() != func.dfg.inst_args(ib).len() {
        return None;
    }
    // x64 lowers only 128-bit vectors: i32/f32 pairs (i32x2/f32x2) are
    // legal CLIF but reach ISLE as `unimplemented`.
    let vt = ty.by(2).filter(|v| !x64 || v.bits() == 128)?;
    let mut x = X {
        func,
        ps: &ps,
        na,
        domtree,
        pos,
        insts,
        block,
        emit: pos_at(pos, ia).max(pos_at(pos, ib)),
        rows: Vec::new(),
        members: FxHashSet::default(),
        loads: Vec::new(),
        min_load: usize::MAX,
        gathers: 0,
        splats: 0,
        pruned: FxHashSet::default(),
        keep: FxHashSet::default(),
        colmap: FxHashMap::default(),
        dbg: tracing::enabled!(target: "pliron::slp", tracing::Level::DEBUG),
    };
    x.members.insert(ia);
    x.members.insert(ib);
    x.rows.push((ia, ib));
    let nargs = func.dfg.inst_args(ia).len();
    let mut args = Vec::with_capacity(nargs);
    for j in 0..nargs {
        let (ca, cb) = (func.dfg.inst_args(ia)[j], func.dfg.inst_args(ib)[j]);
        let Some(p) = x.col(ca, cb, 1) else {
            bail!("col")
        };
        args.push(p);
    }
    let root = Pack::Op { op, args, row: 0 };
    finish_plan(&mut x, root, vt, uses, (ia, ib))
}

/// Shared legality/profitability tail for pack plans: prune unprofitable
/// subtrees, fix the keep set, verify load/sink ordering, assign extracts,
/// and score. `tag` is the debug seed pair.
fn finish_plan(
    x: &mut X<'_>,
    mut root: Pack,
    vt: Type,
    uses: &Uses,
    tag: (Inst, Inst),
) -> Option<(
    Pack,
    Type,
    Vec<(usize, (Inst, Inst))>,
    Vec<Inst>,
    Vec<Inst>,
    Vec<(Value, usize, u8)>,
    FxHashSet<Inst>,
)> {
    let (func, pos, insts) = (x.func, x.pos, x.insts);
    let (ia, ib) = tag;
    let dbg = tracing::enabled!(target: "pliron::slp", tracing::Level::DEBUG);
    macro_rules! bail {
        ($why:expr) => {{
            if dbg {
                tracing::debug!(target: "pliron::slp",
                    "  xpack {} {ia:?}/{ib:?}: {}",
                    $why,
                    func.dfg.insts[ia].opcode()
                );
            }
            return None;
        }};
    }
    if x.rows.len() > 24 || x.gathers > 12 {
        bail!("size");
    }
    // Prune subtrees whose vector form costs more than gathering the scalar
    // results — the scalars then just stay alive.
    let (emit_cost, _) = x.col_cost(&mut root);
    let kept: Vec<(usize, (Inst, Inst))> = x
        .rows
        .iter()
        .enumerate()
        .filter(|(r, _)| !x.pruned.contains(r))
        .map(|(r, &p)| (r, p))
        .collect();
    x.members = kept.iter().flat_map(|&(_, (a, b))| [a, b]).collect();
    if x.members.len() < 2 {
        bail!("empty");
    }
    // Upper bound on the final score: extracts and keep-pins only lower
    // `net`, so when `2·kept + 5·heavy − emit` already falls short the
    // leaf/consumer/scheduler analysis below cannot change the verdict.
    // Most planned pairs end here (scalar kernels seed thousands of
    // same-opcode pairs whose columns all gather).
    let heavy = kept
        .iter()
        .filter(|&&(_, (i, _))| matches!(func.dfg.insts[i].opcode(), Opcode::Sqrt | Opcode::Fdiv))
        .count();
    let min_net = match &root {
        Pack::Store2 { .. } => 1 - kept.len() as i32,
        _ => 1,
    };
    if (2 * kept.len() as i32 + 5 * heavy as i32 - emit_cost as i32) < min_net {
        bail!("cost-bound");
    }
    // Leaf operands (gather/splat scalars, load/store bases) may read a value
    // whose def became a member *after* the leaf was built — too late for the
    // in-`col` keep marking to catch. Collect every leaf-operand def: member
    // defs get kept, and non-member defs must stay before the emit point
    // (they may not sink).
    fn leaf_walk(x: &X<'_>, p: &Pack, leaf_defs: &mut FxHashSet<Inst>) {
        match p {
            Pack::Splat(v) | Pack::Vec(v) | Pack::Load { base: v, .. } => {
                if let Some(d) = x.def(*v) {
                    leaf_defs.insert(d);
                }
            }
            Pack::Gather(vs) => {
                for &v in vs {
                    if let Some(d) = x.def(v) {
                        leaf_defs.insert(d);
                    }
                }
            }
            Pack::Op { args, .. } => {
                for a in args {
                    leaf_walk(x, a, leaf_defs);
                }
            }
            Pack::Store2 { data, base, .. } => {
                leaf_walk(x, data, leaf_defs);
                if let Some(d) = x.def(*base) {
                    leaf_defs.insert(d);
                }
            }
            Pack::Dup(_) => {}
        }
    }
    let mut leaf_defs: FxHashSet<Inst> = FxHashSet::default();
    leaf_walk(x, &root, &mut leaf_defs);
    // `stay(i)` marks an inst that must not move: members are kept alive as
    // scalars, other same-block defs are pinned against sinking. Expansion
    // walks arg defs transitively — an inst that stays still reads its args
    // at its original position, so their producers must stay too. Fails if a
    // producer already committed to sinking.
    fn stay(
        x: &mut X<'_>,
        pin: &mut FxHashSet<Inst>,
        sink_set: &FxHashSet<Inst>,
        seed: Inst,
    ) -> bool {
        let mut seen: FxHashSet<Inst> = FxHashSet::default();
        let mut stack = vec![seed];
        while let Some(p) = stack.pop() {
            if !seen.insert(p) {
                continue;
            }
            if sink_set.contains(&p) {
                return false;
            }
            if x.members.contains(&p) {
                x.keep.insert(p);
            } else if pos_get(x.pos, p).is_some() {
                pin.insert(p);
            } else if p != seed {
                // Other-block defs can't move; no need to chase their args.
                continue;
            }
            for &a in x.func.dfg.inst_args(p) {
                if let Some(d) = x.def(a) {
                    stack.push(d);
                }
            }
        }
        true
    }
    let mut pin: FxHashSet<Inst> = FxHashSet::default();
    let sink_set0 = FxHashSet::default();
    for &d in &leaf_defs {
        if !stay(x, &mut pin, &sink_set0, d) {
            bail!("use-order");
        }
    }
    // Members kept during `col` (a leaf reads their scalar result) stay put
    // too — expand their arg defs the same way.
    for m in x.keep.iter().copied().collect::<Vec<_>>() {
        if !stay(x, &mut pin, &sink_set0, m) {
            bail!("use-order");
        }
    }
    // Merged loads read both lanes at the emit point: every writer between
    // the earliest lane load and the emit point must be disjoint.
    if x.min_load != usize::MAX {
        for p in x.min_load + 1..x.emit {
            let i2 = insts[p];
            if x.members.contains(&i2) {
                continue;
            }
            let o2 = func.dfg.insts[i2].opcode();
            if o2.is_call() || o2.is_terminator() || o2.is_branch() {
                bail!("ld-order");
            }
            if o2.can_store() || o2.other_side_effects() || o2.can_trap() {
                match mem_range(x.func, x.ps, x.na, i2) {
                    Some(r2) if x.loads.iter().all(|&lr| !may_overlap(lr, r2)) => {}
                    _ => bail!("ld-order"),
                }
            }
        }
    }
    // Consumer analysis: external uses of lane results need extracts; lane-0
    // uses at or before the emit point must sink, or drop if dead stores.
    let mut sink: Vec<Inst> = Vec::new();
    let mut sink_set: FxHashSet<Inst> = FxHashSet::default();
    let mut dead: FxHashSet<Inst> = FxHashSet::default();
    let mut extracts: Vec<(Value, usize, u8)> = Vec::new();
    let mut work: Vec<Inst> = Vec::new();
    for &(r, (la, lb)) in &kept {
        // Store rows have no results — nothing to extract.
        if func.dfg.inst_results(la).is_empty() {
            continue;
        }
        // Both lanes need the same treatment: either lane's inst may sit
        // before the emit point, so uses there must sink rather than take an
        // extract (the extract would land after the use).
        for (l, lane) in [(la, 0u8), (lb, 1)] {
            if x.keep.contains(&l) {
                continue;
            }
            let v = func.dfg.inst_results(l)[0];
            let mut need = false;
            for u in uses.iter(ra_fix(func, v).index()) {
                if x.members.contains(&u) {
                    continue;
                }
                need = true;
                if !ext_ok(&x, u) {
                    work.push(u);
                }
            }
            if need {
                extracts.push((v, r, lane));
            }
        }
    }
    while let Some(u) = work.pop() {
        if x.members.contains(&u) || sink_set.contains(&u) || dead.contains(&u) {
            continue;
        }
        // A pinned inst must stay before the emit point, so it can't sink —
        // and it can't sink from another block into this one either. Those
        // and anything that can't legally sink get the same fallback: keep
        // every member result it reads alive, and pin its other local defs.
        let same_blk = func.layout.inst_block(u) == Some(x.block);
        let sk = if pin.contains(&u) || !same_blk {
            None
        } else {
            sinkable(&x, u, &dead, &sink_set)
        };
        match sk {
            Some(true) => {
                sink_set.insert(u);
                sink.push(u);
                for &ru in func.dfg.inst_results(u) {
                    let ru = ra_fix(func, ru);
                    for u2 in uses.iter(ru.index()) {
                        if !x.members.contains(&u2) && !ext_ok(&x, u2) {
                            work.push(u2);
                        }
                    }
                }
            }
            Some(false) => {
                dead.insert(u);
            }
            None => {
                if !stay(x, &mut pin, &sink_set, u) {
                    bail!("use-order");
                }
            }
        }
    }
    // The fallback path may add keeps; drop extracts for kept lanes (their
    // results stay real scalars).
    extracts.retain(|&(_, r, lane)| {
        let (la, lb) = x.rows[r];
        !x.keep.contains(if lane == 0 { &la } else { &lb })
    });
    sink.sort_by_key(|&i| pos_at(pos, i));
    // Profitability: every kept row swaps two scalars for one vector op;
    // gathers/splats/extracts cost insts. High-port-pressure rows (sqrt/div)
    // count extra — halving a ~13–20-cycle op is worth many simple insts,
    // and deep trees around them are what unlocks the packed sqrt/div at
    // all (nbody's `dt/(d2·sqrt d2)` pairs).
    let n_keep = x.members.iter().filter(|m| x.keep.contains(m)).count() as i32;
    let net = 2 * kept.len() as i32 + 5 * heavy as i32
        - emit_cost as i32
        - extracts.len() as i32
        - n_keep;
    if dbg && net <= 10 {
        tracing::debug!(target: "pliron::slp",
            "    xpack cost {:?}: rows={} heavy={heavy} emit={emit_cost} ext={} sink={} net={net}",
            func.dfg.insts[ia].opcode(),
            kept.len(),
            extracts.len(),
            sink.len()
        );
    }
    if net < min_net {
        bail!("cost");
    }
    Some((
        root,
        vt,
        kept,
        sink,
        dead.into_iter().collect(),
        extracts,
        std::mem::take(&mut x.keep),
    ))
}

/// `resolve_aliases` through a helper so `uses` lookups stay uniform.
fn ra_fix(func: &Function, v: Value) -> Value {
    func.dfg.resolve_aliases(v)
}

/// Try to merge two address-adjacent `store`s into one vector store; the
/// stored scalars' producer chain packs as ordinary columns. Lane order is
/// address order (lane 0 covers the lower `sz` bytes) — fixed by the store
/// layout, unlike op pairs which can swap.
fn plan_store(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    domtree: &DominatorTree,
    uses: &Uses,
    insts: &[Inst],
    pos: &PosMap,
    block: Block,
    taken: &TakenMap,
    i1: Inst,
    i2: Inst,
    x64: bool,
) -> Option<(
    Pack,
    Type,
    Vec<(usize, (Inst, Inst))>,
    Vec<Inst>,
    Vec<Inst>,
    Vec<(Value, usize, u8)>,
    FxHashSet<Inst>,
)> {
    if taken[i1] || taken[i2] {
        return None;
    }
    let (
        InstructionData::Store {
            opcode: Opcode::Store,
            args: args1,
            ..
        },
        InstructionData::Store {
            opcode: Opcode::Store,
            args: args2,
            ..
        },
    ) = (func.dfg.insts[i1], func.dfg.insts[i2])
    else {
        return None;
    };
    let (r1, r2) = (mem_range(func, ps, na, i1)?, mem_range(func, ps, na, i2)?);
    let (sa, sb) = if r1.0 == r2.0
        && r1.2 == r2.2
        && r1.1 + i64::from(r1.2) == r2.1
        && matches!(r1.2, 4 | 8)
    {
        (i1, i2)
    } else if r1.0 == r2.0
        && r1.2 == r2.2
        && r2.1 + i64::from(r2.2) == r1.1
        && matches!(r1.2, 4 | 8)
    {
        (i2, i1)
    } else {
        return None;
    };
    let fa = func.dfg.mem_flags[func.dfg.insts[sa].memflags()?];
    if !fa.notrap() || fa != func.dfg.mem_flags[func.dfg.insts[sb].memflags()?] {
        return None;
    }
    let args = if sa == i1 { args1 } else { args2 };
    let (da, db) = if sa == i1 {
        (args1[0], args2[0])
    } else {
        (args2[0], args1[0])
    };
    let ty = func.dfg.value_type(da);
    if ty.is_vector() || ty != func.dfg.value_type(db) || ty.bytes() != r1.2 {
        return None;
    }
    let vt = ty.by(2).filter(|v| !x64 || v.bits() == 128)?;
    // The store earlier in layout re-emits its write at the emit point, so
    // nothing in between may touch its bytes (or be an opaque side effect).
    let (first, last) = if pos_at(pos, sa) < pos_at(pos, sb) {
        (sa, sb)
    } else {
        (sb, sa)
    };
    let r_first = mem_range(func, ps, na, first)?;
    for p in pos_at(pos, first) + 1..pos_at(pos, last) {
        let i2 = insts[p];
        let o2 = func.dfg.insts[i2].opcode();
        if o2.is_call() || o2.is_terminator() || o2.is_branch() {
            return None;
        }
        match mem_range(func, ps, na, i2) {
            Some(rx) if may_overlap(r_first, rx) => return None,
            Some(_) => {}
            None if o2.can_store() || o2.other_side_effects() || o2.can_trap() => return None,
            None => {}
        }
    }
    let mut x = X {
        func,
        ps: &ps,
        na,
        domtree,
        pos,
        insts,
        block,
        emit: pos_at(pos, last),
        rows: Vec::new(),
        members: FxHashSet::default(),
        loads: Vec::new(),
        min_load: usize::MAX,
        gathers: 0,
        splats: 0,
        pruned: FxHashSet::default(),
        keep: FxHashSet::default(),
        colmap: FxHashMap::default(),
        dbg: tracing::enabled!(target: "pliron::slp", tracing::Level::DEBUG),
    };
    x.members.insert(sa);
    x.members.insert(sb);
    x.rows.push((sa, sb));
    let data = x.col(da, db, 1)?;
    let (base, off) = base_off(func, args[1]);
    // `base_off` leaves the store's immediate offset to the caller.
    let InstructionData::Store { offset: imm, .. } = func.dfg.insts[sa] else {
        unreachable!()
    };
    let off = off + i64::from(imm);
    i32::try_from(off).ok()?;
    if !x.avail(base) {
        return None;
    }
    let root = Pack::Store2 {
        data: Box::new(data),
        base,
        off,
        flags: fa,
    };
    finish_plan(&mut x, root, vt, uses, (sa, sb))
}

/// Emit the pair tree, extracts, and apply the sink/dead plan for one pack.
/// `kept` carries `(row_index, (lane0, lane1))` for the surviving rows;
/// `n_rows` sizes the row vector (pruned rows keep their index slots).
fn apply_pair(
    func: &mut Function,
    pos_insts: &[Inst],
    emit_pos: usize,
    root: &Pack,
    vt: Type,
    kept: &[(usize, (Inst, Inst))],
    n_rows: usize,
    sink: &[Inst],
    dead: &[Inst],
    extracts: &[(Value, usize, u8)],
    keep: &FxHashSet<Inst>,
) -> usize {
    let ib = pos_insts[emit_pos];
    let mut rowvec: Vec<Value> = vec![Value::from_u32(0); n_rows];
    let mut cur = FuncCursor::new(func).at_inst(ib);
    let _ = emit_pack(&mut cur, vt, root, Some(&mut rowvec));
    // Detach deleted members' results so they can be aliased to extracts.
    for &(_, (la, lb)) in kept {
        for l in [la, lb] {
            if !keep.contains(&l) {
                cur.func.dfg.clear_results(l);
            }
        }
    }
    for &(v, r, lane) in extracts {
        let ex = cur.ins().extractlane(rowvec[r], lane);
        cur.func.dfg.change_to_alias(v, ex);
    }
    // Sink consumers that used lane results before the emit point, then
    // drop dead overwritten stores. `insert_inst` inserts before the anchor,
    // so inserting each `u` before `ib` lands them in order.
    for &u in sink {
        cur.func.layout.remove_inst(u);
        cur.func.layout.insert_inst(u, ib);
    }
    for &u in dead {
        cur.func.layout.remove_inst(u);
        cur.func.dfg.clear_results(u);
    }
    // The lane insts' results were replaced by extracts (or only used inside
    // the pack); delete the scalar members, except insts kept alive because a
    // gather still reads their scalar result.
    for &(_, (la, lb)) in kept {
        for l in [la, lb] {
            if !keep.contains(&l) {
                cur.func.layout.remove_inst(l);
                cur.func.dfg.clear_results(l);
            }
        }
    }
    kept.len()
}

/// Greedily pair same-op insts in one block; rebuilds position state after
/// each applied pack.
fn xpack_block(
    func: &mut Function,
    domtree: &DominatorTree,
    na: &FxHashSet<Value>,
    block: Block,
    uses: &mut Option<Uses>,
    ps: &FxHashMap<Value, Value>,
    x64: bool,
) -> usize {
    let mut total = 0;
    let mut tried = 0usize;
    let mut taken = TakenMap::new();
    // Pairs whose plan failed, → span at failure (current-frame
    // positions, refreshed each iteration so apply-time edits compare
    // in the same frame). A rescan after each apply only ever finds new
    // packs where the apply moved insts, so a failed pair is retried
    // only when the apply edited inside its span, moved an endpoint, or
    // changed an operand/result's uses — checked in `failed.retain`
    // below. Cached failures don't consume a candidate slot.
    let mut failed: FxHashMap<(Inst, Inst), (usize, usize)> = FxHashMap::default();
    let dbg = tracing::enabled!(target: "pliron::slp", tracing::Level::DEBUG);
    let nocache = crate::env_set("PLIRON_XPACK_NOCACHE");
    loop {
        let insts: Vec<Inst> = func.layout.block_insts(block).collect();
        // Seed check: xpack pairs need ≥2 stores or ≥2 same-opcode
        // vecable insts among the remaining insts. Skip the
        // whole-function use-map build when no pair can form.
        // `byop` is also the candidate index: the seed scan below would
        // otherwise re-scan `insts[p+1..]` per `ia` — O(n²) per round on
        // blocks with few same-opcode insts.
        let mut byop: FxHashMap<Opcode, Vec<usize>> = FxHashMap::default();
        for (k, &i) in insts.iter().enumerate() {
            if taken[i] {
                continue;
            }
            let op = func.dfg.insts[i].opcode();
            if op == Opcode::Store || (vecable(op) && res_of(func, i).is_some()) {
                byop.entry(op).or_default().push(k);
            }
        }
        let nstores = byop.get(&Opcode::Store).map_or(0, |v| v.len());
        let dup = byop
            .iter()
            .any(|(&o, v)| o != Opcode::Store && v.len() >= 2);
        if nstores < 2 && !dup {
            return total;
        }
        let pos = block_pos(&insts);
        // The map is function-wide and shared across blocks; an apply
        // mutates the IR, so it's rebuilt on the next loop iteration.
        let us = &*uses.get_or_insert_with(|| build_uses(func));
        if !failed.is_empty() {
            failed.retain(|&(x, y), span| match (pos_get(&pos, x), pos_get(&pos, y)) {
                (Some(a), Some(b)) => {
                    *span = (a.min(b), a.max(b));
                    true
                }
                _ => false,
            });
        }
        if tracing::enabled!(target: "pliron::slp", tracing::Level::DEBUG) && total > 0 {
            for (p, &i) in insts.iter().enumerate() {
                let d = &func.dfg.insts[i];
                let args: Vec<String> = func
                    .dfg
                    .inst_args(i)
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect();
                let ress: Vec<String> = func
                    .dfg
                    .inst_results(i)
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect();
                tracing::debug!(target: "pliron::slp",
                    "    map p{p} {i:?} {:?} {} -> {}",
                    d.opcode(),
                    args.join(","),
                    ress.join(",")
                );
            }
        }
        let mut applied = None;
        'scan: for (p, &ia) in insts.iter().enumerate() {
            if taken[ia] {
                continue;
            }
            let op = func.dfg.insts[ia].opcode();
            let is_store = op == Opcode::Store;
            if !is_store && (!vecable(op) || res_of(func, ia).is_none()) {
                continue;
            }
            let mut cands = 0;
            // `byop` entries are already non-taken and opcode-matched;
            // positions are in order, so this is the first ≤4 later hits.
            for &q in byop.get(&op).map_or(&[][..], |v| &v[..]) {
                if q <= p {
                    continue;
                }
                if cands >= 4 {
                    break;
                }
                cands += 1;
                let ib = insts[q];
                let mut try_plan = |a: Inst, b: Inst| {
                    // Store-pair plans resolve memory footprints through
                    // upstream address chains that eviction can't cheaply
                    // track, and they're rare — always re-plan them.
                    if !is_store && !nocache && failed.contains_key(&(a, b)) {
                        return None;
                    }
                    tried += 1;
                    let plan = if is_store {
                        plan_store(
                            func, ps, na, domtree, us, &insts, &pos, block, &taken, a, b, x64,
                        )
                    } else {
                        plan_pair(
                            func, ps, na, domtree, us, &insts, &pos, block, &taken, a, b, x64,
                        )
                    };
                    if plan.is_none() && !is_store {
                        if let (Some(a1), Some(b1)) = (pos_get(&pos, a), pos_get(&pos, b)) {
                            failed.insert((a, b), (a1.min(b1), a1.max(b1)));
                        }
                    }
                    plan
                };
                // Both lane orders: swapping lanes changes column order,
                // which decides whether adjacent loads merge.
                let plan = try_plan(ia, ib).or_else(|| try_plan(ib, ia));
                if let Some((root, vt, kept, sink, dead, extracts, keep)) = plan {
                    let emit_at = pos_at(&pos, ib).max(pos_at(&pos, ia));
                    applied = Some((emit_at, root, vt, kept, sink, dead, extracts, keep));
                    break 'scan;
                }
            }
        }
        let Some((emit_at, root, vt, kept, sink, dead, extracts, keep)) = applied else {
            if dbg && (tried > 0 || total > 0) {
                tracing::debug!(target: "pliron::slp", "xpack block{block:?}: {total} rows packed, {tried} plans tried");
            }
            return total;
        };
        // Apply, tracking every edit in `pos` coordinates so cached
        // failures that could now succeed get evicted below.
        let mut changed_pos: Vec<usize> = vec![emit_at];
        let mut changed_insts: FxHashSet<Inst> = FxHashSet::default();
        let mut changed_vals: FxHashSet<Value> = FxHashSet::default();
        for &(_, (la, lb)) in &kept {
            for l in [la, lb] {
                changed_insts.insert(l);
                if !keep.contains(&l) {
                    for &v in func.dfg.inst_args(l) {
                        changed_vals.insert(func.dfg.resolve_aliases(v));
                    }
                }
            }
            taken[la] = true;
            taken[lb] = true;
        }
        for &u in sink.iter().chain(dead.iter()) {
            changed_insts.insert(u);
            if let Some(d) = pos_get(&pos, u) {
                changed_pos.push(d);
            }
            for &v in func.dfg.inst_args(u) {
                changed_vals.insert(func.dfg.resolve_aliases(v));
            }
        }
        for &(v, _, _) in &extracts {
            changed_vals.insert(v);
        }
        pack_vals(&root, func, &mut changed_vals);
        let n_rows = kept.iter().map(|&(r, _)| r).max().unwrap() + 1;
        let n = apply_pair(
            func, &insts, emit_at, &root, vt, &kept, n_rows, &sink, &dead, &extracts, &keep,
        );
        // Operands that resolved through an extracted value now resolve
        // to the fresh extractlane result — record the post-apply
        // targets so alias-chain users get evicted.
        for &(v, _, _) in &extracts {
            changed_vals.insert(func.dfg.resolve_aliases(v));
        }
        total += n;
        if dbg {
            tracing::debug!(target: "pliron::slp",
                "xpack APPLY @{emit_at}: {n} rows ({} kept) sink={sink:?} keep={keep:?}",
                kept.len()
            );
        }
        // Evict failed pairs whose span the apply edited (inst moved or
        // deleted inside, or emit landed inside), whose endpoint moved,
        // or whose operands/results had uses or aliases change. `affected`
        // = values transitively feeding a changed inst: plans consult
        // use-sets along the whole consumer chain, so a lane result is
        // stale whenever any downstream user moved, died, or re-aliased.
        if !failed.is_empty() {
            changed_pos.sort_unstable();
            changed_pos.dedup();
            let mut affected: FxHashSet<Value> = FxHashSet::default();
            let mut stack: Vec<Value> = changed_vals.iter().copied().collect();
            while let Some(v) = stack.pop() {
                if !affected.insert(v) {
                    continue;
                }
                if let ValueDef::Result(i, _) = func.dfg.value_def(v) {
                    for &a in func.dfg.inst_args(i) {
                        stack.push(func.dfg.resolve_aliases(a));
                    }
                }
            }
            let hit = |i: Inst| {
                func.dfg
                    .inst_args(i)
                    .iter()
                    .any(|&v| affected.contains(&func.dfg.resolve_aliases(v)))
                    || func
                        .dfg
                        .inst_results(i)
                        .iter()
                        .any(|v| affected.contains(v))
            };
            failed.retain(|&(x, y), &mut (a, b)| {
                if changed_insts.contains(&x) || changed_insts.contains(&y) {
                    return false;
                }
                let i = changed_pos.partition_point(|&d| d <= a);
                if i < changed_pos.len() && changed_pos[i] < b {
                    return false;
                }
                !(hit(x) || hit(y))
            });
        }
        // apply_pair moved/added insts — the shared map is stale.
        *uses = None;
    }
}

/// All scalar values a pack's emitted insts will read.
fn pack_vals(p: &Pack, func: &Function, out: &mut FxHashSet<Value>) {
    match p {
        Pack::Splat(v) | Pack::Vec(v) => {
            out.insert(func.dfg.resolve_aliases(*v));
        }
        Pack::Load { base, .. } => {
            out.insert(func.dfg.resolve_aliases(*base));
        }
        Pack::Op { args, .. } => {
            for a in args {
                pack_vals(a, func, out);
            }
        }
        Pack::Gather(vs) => {
            for &v in vs {
                out.insert(func.dfg.resolve_aliases(v));
            }
        }
        Pack::Dup(_) => {}
        Pack::Store2 { data, base, .. } => {
            pack_vals(data, func, out);
            out.insert(func.dfg.resolve_aliases(*base));
        }
    }
}
