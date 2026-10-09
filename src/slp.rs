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
//! `PLIRON_SLP=0` disables it; `PLIRON_SLP_DEBUG` logs conversions.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, Function, Inst, InstBuilder, InstructionData, MemFlagsData, Opcode, Type, Value,
    ValueDef,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

/// Max recursion depth when packing operand trees.
const MAX_DEPTH: usize = 6;

/// Lane-wise opcodes that map to a vector form of the same opcode.
fn vecable(op: Opcode) -> bool {
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
    op.is_terminator()
        || op.is_call()
        || op.can_store()
        || op.other_side_effects()
        || op.can_trap()
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
    /// Lanes are isomorphic insts of `op`.
    Op { op: Opcode, args: Vec<Pack> },
}

struct Ctx<'a> {
    func: &'a Function,
    domtree: &'a DominatorTree,
    /// Layout-order index of insts in the current block.
    pos: FxHashMap<Inst, usize>,
    /// Insts of the current block in layout order.
    insts: Vec<Inst>,
    block: Block,
    /// Emit point: position of the group's first member store.
    emit: usize,
    /// Earliest position among all packed loads (writer-span check).
    min_load: usize,
    /// Whole-function uses: `v -> set of using insts`.
    uses: &'a FxHashMap<Value, FxHashSet<Inst>>,
    /// Members of the current store group (allowed inside the store span).
    members: FxHashSet<Inst>,
}

impl Ctx<'_> {
    /// `v` is defined by an inst in this block before the emit point.
    fn inst_before_emit(&self, v: Value) -> Option<Inst> {
        let ValueDef::Result(inst, 0) = self.func.dfg.value_def(self.func.dfg.resolve_aliases(v))
        else {
            return None;
        };
        (self.pos.get(&inst).is_some_and(|&p| p < self.emit)).then_some(inst)
    }

    /// `v` is available at the emit point.
    fn avail(&self, v: Value) -> bool {
        match self.func.dfg.value_def(v) {
            ValueDef::Param(b, _) => self.domtree.block_dominates(b, self.block),
            ValueDef::Result(inst, _) => {
                if self.func.layout.inst_block(inst) == Some(self.block) {
                    self.pos.get(&inst).is_some_and(|&p| p < self.emit)
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
            self.min_load = self.min_load.min(self.pos[&inst]);
        }
        // The merged vector access must be 8 or 16 bytes.
        if esz * n as i64 != 8 && esz * n as i64 != 16 {
            return None;
        }
        let (base, off) = first?;
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
        })
    }
}

/// Split `ptr` into `(base, const_off)` — walks short `iadd(ptr, iconst)`
/// chains. The inst's immediate offset is added by the caller.
fn base_off(func: &Function, ptr: Value) -> (Value, i64) {
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
    let ValueDef::Result(inst, 0) = func.dfg.value_def(func.dfg.resolve_aliases(v)) else {
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
fn emit_pack(pos: &mut FuncCursor, vt: Type, p: &Pack) -> Value {
    match p {
        Pack::Splat(v) => pos.ins().splat(vt, *v),
        &Pack::Load {
            base,
            off,
            flags,
            ..
        } => pos
            .ins()
            .load(vt, wide_flags(flags), base, i32::try_from(off).unwrap_or(0)),
        Pack::Op { op, args } => {
            let vs: Vec<Value> = args.iter().map(|a| emit_pack(pos, vt, a)).collect();
            emit_op(pos, *op, &vs)
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
fn try_block(func: &mut Function, domtree: &DominatorTree, block: Block) -> usize {
    let insts: Vec<Inst> = func.layout.block_insts(block).collect();
    let pos: FxHashMap<Inst, usize> = insts
        .iter()
        .enumerate()
        .map(|(k, &i)| (i, k))
        .collect();
    // Use map over the whole function.
    let mut uses: FxHashMap<Value, FxHashSet<Inst>> = FxHashMap::default();
    for b2 in func.layout.blocks() {
        for i in func.layout.block_insts(b2) {
            for v in func.dfg.inst_values(i) {
                uses.entry(func.dfg.resolve_aliases(v)).or_default().insert(i);
            }
        }
    }
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
            pos: pos[&i],
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
    // Group by (base, elem type, flags); within each, find consecutive-offset
    // runs of exactly `16/esz` or `8/esz` lanes.
    let mut by_key: FxHashMap<(Value, Type, MemFlagsData), Vec<&StoreInfo>> = FxHashMap::default();
    for s in &stores {
        by_key.entry((s.base, s.ty, s.flags)).or_default().push(s);
    }
    let mut changed = 0;
    let mut taken: FxHashSet<Inst> = FxHashSet::default();
    for group in by_key.values() {
        let mut g: Vec<&StoreInfo> = group.clone();
        g.sort_by_key(|s| s.off);
        let esz = i64::from(g[0].ty.bytes());
        for &lanes in &[16 / esz, 8 / esz] {
            if lanes < 2 || lanes > 16 {
                continue;
            }
            let mut run: Vec<&StoreInfo> = Vec::new();
            for &s in &g {
                if taken.contains(&s.inst) {
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
                if run.len() == lanes as usize {
                    let emit = run.iter().map(|s| s.pos).min().unwrap();
                    let plan = Ctx {
                        func: &*func,
                        domtree,
                        pos: pos.clone(),
                        insts: insts.clone(),
                        block,
                        emit,
                        min_load: usize::MAX,
                        uses: &uses,
                        members: run.iter().map(|s| s.inst).collect(),
                    }
                    .analyze(&run);
                    if let Some((p, vt)) = plan {
                        let emit_inst = insts[emit];
                        let mut cur = FuncCursor::new(func).at_inst(emit_inst);
                        let vec = emit_pack(&mut cur, vt, &p);
                        cur.ins().store(
                            wide_flags(run[0].flags),
                            vec,
                            run[0].base,
                            run[0].off as i32,
                        );
                        for s in &run {
                            cur.func.layout.remove_inst(s.inst);
                            cur.func.dfg.clear_results(s.inst);
                        }
                        changed += run.len();
                        for s in &run {
                            taken.insert(s.inst);
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
            if !self
                .uses
                .get(&v)
                .is_some_and(|us| us.iter().all(|u| self.members.contains(u)))
            {
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
pub fn run(func: &mut Function, simd: bool) -> usize {
    if !simd {
        return 0;
    }
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let mut changed = 0;
    for block in func.layout.blocks().collect::<Vec<_>>() {
        changed += try_block(func, &domtree, block);
    }
    if changed > 0 {
        crate::jumpthread::remove_dead_insts(func, false);
    }
    changed
}
