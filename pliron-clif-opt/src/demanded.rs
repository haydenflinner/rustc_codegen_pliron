//! Demanded bits (`PLIRON_DEMANDED=1`, opt-in; `=0` off): LLVM's
//! `DemandedBits` analysis and the `SimplifyDemandedBits` rewrites it
//! licenses, for the CLIF pipeline.
//!
//! **Analysis.** Every integer value of at most 64 bits gets the mask of
//! bits some observable use can read: the OR over its uses of what each
//! use demands of it, given what is demanded of the use's own result. An
//! add, subtract or multiply reads its operands' bits up to the highest
//! demanded result bit; a constant `band` reads only the mask's bits, a
//! constant `bor` only the bits the constant leaves clear; shifts by a
//! constant move the mask; `ireduce` reads its result's mask, `uextend`
//! the low part of it, `sextend` that plus the sign bit when a high bit is
//! demanded; `select` arms read the result's mask. A block parameter's
//! mask flows into the matching argument of every edge into its block.
//! Anything else (calls, compares, stores, returns, addresses, branch
//! conditions) reads every bit of its operands, except `istore8`/
//! `istore16` and narrow `store`s, which read the stored width. The
//! analysis is a least fixpoint over a worklist: masks only grow, each
//! growth re-queues the definition (or, for a block parameter, the edges
//! into the block); a budget linear in the function's size bounds it, and
//! running out leaves the function unchanged.
//!
//! **Rewrites.**
//! * `band x, K` whose cleared bits nobody demands is `x`; `bor`/`bxor x,
//!   K` whose set bits nobody demands is `x`.
//! * Narrow-chain widening. An `i8`/`i16` expression tree (`iadd`,
//!   `isub`, `imul`, `ineg`, `band`, `bor`, `bxor`, `bnot`, `ishl` by a
//!   constant, `select`; each inner node used once) computes its low bits
//!   from its leaves' low bits only. Where the tree's value is zero- or
//!   sign-extended, or stored narrow, the tree is rebuilt at 32 or 64 bits:
//!   an `ireduce` leaf becomes its wide source, a constant a wide constant,
//!   any other leaf a `uextend`. The extend at the root is dropped when no
//!   use demands the bits it defines, becomes a `band` with the narrow mask
//!   when one does; a narrow store becomes `istore8`/`istore16` of the wide
//!   value. This is what removes the `uxtb`/`uxth` the aarch64 lowering
//!   materializes for every `ireduce.i8`/`i16` (patch 36 in
//!   `vendor/cranelift-codegen/PLIRON_PATCH.md`) and the root extend, and
//!   on wasm the `& 255`/`& 65535` the emitter puts after every narrow
//!   add, subtract, multiply and shift. A rewrite is taken only when a
//!   per-target count of the extensions and masks it removes beats the
//!   count it adds (`Cost`).
//!
//! `PLIRON_WASM_MASKELIM` is the dual on wasm: it drops a mask whose
//! input already fits (known bits); this drops the work whose output bits
//! nobody reads (demanded bits).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::entity::SecondaryMap;
use cranelift_codegen::ir::{
    types, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value, ValueDef,
};
use pliron_cfg::target::Target;

fn tymask(ty: Type) -> u64 {
    if ty.bits() >= 64 { u64::MAX } else { (1u64 << ty.bits()) - 1 }
}

fn tracked(ty: Type) -> bool {
    ty.is_int() && ty.lane_count() == 1 && ty.bits() <= 64
}

/// Every bit up to and including the highest set bit of `m`.
fn lowfill(m: u64) -> u64 {
    if m == 0 { 0 } else { u64::MAX >> m.leading_zeros() }
}

/// Every bit from the lowest set bit of `m` up.
fn highfill(m: u64) -> u64 {
    if m == 0 { 0 } else { u64::MAX << m.trailing_zeros() }
}

fn iconst(func: &Function, v: Value) -> Option<u64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits() as u64),
        _ => None,
    }
}

/// The demanded-bits masks of one function.
pub struct Demanded {
    mask: SecondaryMap<Value, u64>,
}

impl Demanded {
    /// The bits of `v` some use reads (every bit for an untracked type).
    pub fn of(&self, func: &Function, v: Value) -> u64 {
        let v = func.dfg.resolve_aliases(v);
        let ty = func.dfg.value_type(v);
        if tracked(ty) { self.mask[v] } else { u64::MAX }
    }
}

struct Solver<'a> {
    func: &'a Function,
    mask: SecondaryMap<Value, u64>,
    queued: SecondaryMap<Inst, bool>,
    work: Vec<Inst>,
    /// Branches into each block (for a parameter's growth).
    into: SecondaryMap<cranelift_codegen::ir::Block, Vec<Inst>>,
}

impl Solver<'_> {
    fn push(&mut self, i: Inst) {
        if !self.queued[i] {
            self.queued[i] = true;
            self.work.push(i);
        }
    }

    fn demand(&mut self, v: Value, m: u64) {
        let v = self.func.dfg.resolve_aliases(v);
        let ty = self.func.dfg.value_type(v);
        if !tracked(ty) {
            return;
        }
        let m = m & tymask(ty);
        let old = self.mask[v];
        if m & !old == 0 {
            return;
        }
        self.mask[v] = old | m;
        match self.func.dfg.value_def(v) {
            ValueDef::Result(i, _) => self.push(i),
            ValueDef::Param(b, _) => {
                for k in 0..self.into[b].len() {
                    let i = self.into[b][k];
                    self.push(i);
                }
            }
            _ => {}
        }
    }

    fn full(&mut self, v: Value) {
        self.demand(v, u64::MAX);
    }

    fn transfer(&mut self, i: Inst) {
        let func = self.func;
        let data = &func.dfg.insts[i];
        let dests = data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        if !dests.is_empty() {
            for &a in func.dfg.inst_args(i) {
                self.full(a);
            }
            for bc in dests {
                let target = bc.block(&func.dfg.value_lists);
                for (k, arg) in bc.args(&func.dfg.value_lists).enumerate() {
                    if let BlockArg::Value(v) = arg {
                        let m = func.dfg.block_params(target).get(k).map_or(u64::MAX, |&p| self.mask[p]);
                        self.demand(v, m);
                    }
                }
            }
            return;
        }
        let results = func.dfg.inst_results(i);
        let (r, bits) = match results {
            [] => (u64::MAX, 64),
            [r] if tracked(func.dfg.value_type(*r)) => (self.mask[*r], func.dfg.value_type(*r).bits()),
            _ => {
                // Several results, or not an integer scalar of at most
                // 64 bits: every operand bit counts.
                for &a in func.dfg.inst_args(i) {
                    self.full(a);
                }
                return;
            }
        };
        let tm = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
        match *data {
            InstructionData::Binary { opcode, args: [a, b] } => match opcode {
                Opcode::Iadd | Opcode::Isub | Opcode::Imul => {
                    let m = lowfill(r);
                    self.demand(a, m);
                    self.demand(b, m);
                }
                Opcode::Band => match (iconst(func, a), iconst(func, b)) {
                    (_, Some(c)) => self.demand(a, r & c),
                    (Some(c), None) => self.demand(b, r & c),
                    _ => {
                        self.demand(a, r);
                        self.demand(b, r);
                    }
                },
                Opcode::Bor => match (iconst(func, a), iconst(func, b)) {
                    (_, Some(c)) => self.demand(a, r & !c),
                    (Some(c), None) => self.demand(b, r & !c),
                    _ => {
                        self.demand(a, r);
                        self.demand(b, r);
                    }
                },
                Opcode::Bxor => {
                    self.demand(a, r);
                    self.demand(b, r);
                }
                Opcode::Ishl | Opcode::Ushr | Opcode::Sshr | Opcode::Rotl | Opcode::Rotr => {
                    // The amount is taken modulo the width.
                    if r != 0 {
                        self.demand(b, u64::from(bits - 1));
                    }
                    let m = match iconst(func, b) {
                        Some(k) => {
                            let k = (k & u64::from(bits - 1)) as u32;
                            match opcode {
                                Opcode::Ishl => r >> k,
                                Opcode::Ushr => (r << k) & tm,
                                Opcode::Sshr => {
                                    let mut m = (r << k) & tm;
                                    // The top `k` result bits copy the sign.
                                    if k > 0 && (r >> (bits - k)) != 0 {
                                        m |= 1u64 << (bits - 1);
                                    }
                                    m
                                }
                                Opcode::Rotl => ((r >> k) | (r << (bits - k) % bits)) & tm,
                                _ => ((r << k) | (r >> (bits - k) % bits)) & tm,
                            }
                        }
                        None => match opcode {
                            Opcode::Ishl => lowfill(r),
                            Opcode::Ushr | Opcode::Sshr => highfill(r) & tm,
                            _ => {
                                if r != 0 {
                                    tm
                                } else {
                                    0
                                }
                            }
                        },
                    };
                    self.demand(a, m);
                }
                _ => {
                    self.full(a);
                    self.full(b);
                }
            },
            InstructionData::Unary { opcode, arg } => match opcode {
                Opcode::Bnot => self.demand(arg, r),
                Opcode::Ineg => self.demand(arg, lowfill(r)),
                Opcode::Ireduce => self.demand(arg, r),
                Opcode::Uextend => {
                    let n = tymask(func.dfg.value_type(arg));
                    self.demand(arg, r & n);
                }
                Opcode::Sextend => {
                    let at = func.dfg.value_type(arg);
                    let n = tymask(at);
                    let mut m = r & n;
                    if r & !n != 0 {
                        m |= 1u64 << (at.bits() - 1);
                    }
                    self.demand(arg, m);
                }
                _ => self.full(arg),
            },
            InstructionData::Ternary { opcode: Opcode::Select | Opcode::SelectSpectreGuard, args: [c, x, y] } => {
                if r != 0 {
                    self.full(c);
                }
                self.demand(x, r);
                self.demand(y, r);
            }
            InstructionData::Ternary { opcode: Opcode::Bitselect, args: [c, x, y] } => {
                self.demand(c, r);
                self.demand(x, r);
                self.demand(y, r);
            }
            InstructionData::IntCompare { args: [a, b], .. } => {
                if r != 0 {
                    self.full(a);
                    self.full(b);
                }
            }
            InstructionData::Store { opcode, args: [x, p], .. } => {
                let m = match opcode {
                    Opcode::Istore8 => 0xff,
                    Opcode::Istore16 => 0xffff,
                    Opcode::Istore32 => 0xffff_ffff,
                    _ => u64::MAX,
                };
                self.demand(x, m);
                self.full(p);
            }
            _ => {
                for &a in func.dfg.inst_args(i) {
                    self.full(a);
                }
            }
        }
    }
}

/// Solve the masks of `func`. `None` when the worklist outgrew its budget.
pub fn analyze(func: &Function) -> Option<Demanded> {
    let mut n = 0usize;
    let mut s = Solver {
        func,
        mask: SecondaryMap::with_default(0),
        queued: SecondaryMap::with_default(false),
        work: Vec::new(),
        into: SecondaryMap::default(),
    };
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            n += 1;
            for bc in func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                let t = bc.block(&func.dfg.value_lists);
                if s.into[t].last() != Some(&i) {
                    s.into[t].push(i);
                }
            }
            // Layout order on a stack: uses are popped before their defs
            // on the first round in straight-line code.
            s.queued[i] = true;
            s.work.push(i);
        }
    }
    let mut budget = 32 * n + 4096;
    while let Some(i) = s.work.pop() {
        s.queued[i] = false;
        if budget == 0 {
            return None;
        }
        budget -= 1;
        s.transfer(i);
    }
    Some(Demanded { mask: s.mask })
}

/// Per-target extension/mask counts (`Cost`): what a narrow-tree rewrite
/// adds and removes.
struct Cost {
    /// `Target::narrow_masked`.
    masked: bool,
    /// The machine word in bits (`Target::word_bytes`).
    word: u32,
}

impl Cost {
    /// An `ireduce.N` leaf used only by the tree: aarch64 materializes a
    /// `uxtb`/`uxth` unless the source is already clean; wasm masks (and
    /// wraps an i64 first).
    fn ireduce_leaf(&self, func: &Function, src: Value, n: Type) -> u32 {
        let st = func.dfg.value_type(src);
        let clean = match func.dfg.value_def(func.dfg.resolve_aliases(src)).inst().map(|d| &func.dfg.insts[d]) {
            // aarch64 narrows a once-used `ireduce (load)` to `ldrb`/`ldrh`;
            // `ldnarrow` does the same for wasm.
            Some(InstructionData::Load { .. }) => true,
            Some(InstructionData::Unary { opcode: Opcode::Uextend, arg }) => {
                func.dfg.value_type(*arg).bits() <= n.bits()
            }
            Some(InstructionData::IntCompare { .. }) => true,
            Some(InstructionData::UnaryImm { .. }) => true,
            Some(InstructionData::Binary { opcode: Opcode::Band, args }) => {
                iconst(func, args[1]).or_else(|| iconst(func, args[0])).is_some_and(|c| c & !tymask(n) == 0)
            }
            Some(InstructionData::Binary { opcode: Opcode::Ushr, args }) => {
                iconst(func, args[1]).is_some_and(|k| k < u64::from(st.bits()) && u64::from(st.bits()) - k <= u64::from(n.bits()))
            }
            _ => false,
        };
        let wrap = u32::from(self.masked && st.bits() > self.word);
        u32::from(!clean) + wrap
    }

    /// The new conversion of a leaf the tree used to take narrow.
    fn other_leaf(&self, func: &Function, l: Value) -> u32 {
        let l = func.dfg.resolve_aliases(l);
        match func.dfg.value_def(l) {
            ValueDef::Param(..) => 1,
            ValueDef::Result(d, _) => match &func.dfg.insts[d] {
                InstructionData::Load { opcode, .. } => {
                    u32::from(matches!(opcode, Opcode::Sload8 | Opcode::Sload16))
                }
                InstructionData::IntCompare { .. } | InstructionData::FloatCompare { .. } => 0,
                // wasm keeps every narrow arithmetic result masked.
                InstructionData::Binary { opcode, .. }
                    if self.masked && !matches!(opcode, Opcode::Sshr) =>
                {
                    0
                }
                InstructionData::Unary { opcode: Opcode::Bnot | Opcode::Ineg, .. } if self.masked => 0,
                _ => 1,
            },
            _ => 1,
        }
    }

    /// The mask wasm puts after a narrow node of this kind.
    fn node(&self, op: Opcode) -> u32 {
        u32::from(self.masked && matches!(op, Opcode::Iadd | Opcode::Isub | Opcode::Imul | Opcode::Ishl | Opcode::Ineg | Opcode::Bnot))
    }
}

/// One narrow tree: its nodes in post-order (children first), with the
/// counts of what rebuilding it removes and adds.
#[derive(Default)]
struct Tree {
    /// Inner nodes, children before parents.
    nodes: Vec<Inst>,
    old: u32,
    new: u32,
}

const MAX_NODES: usize = 24;

struct Ctx<'a> {
    func: &'a Function,
    uses: &'a SecondaryMap<Value, u32>,
    cost: Cost,
}

impl Ctx<'_> {
    fn widenable(&self, i: Inst) -> bool {
        match self.func.dfg.insts[i] {
            InstructionData::Binary { opcode, args } => match opcode {
                Opcode::Iadd | Opcode::Isub | Opcode::Imul | Opcode::Band | Opcode::Bor | Opcode::Bxor => true,
                Opcode::Ishl => iconst(self.func, args[1]).is_some(),
                _ => false,
            },
            InstructionData::Unary { opcode: Opcode::Bnot | Opcode::Ineg, .. } => true,
            InstructionData::Ternary { opcode: Opcode::Select, .. } => true,
            _ => false,
        }
    }

    /// Collect the tree under `v` (type `n`): inner nodes are widenable
    /// and used once (by their parent, or by the root); the rest are
    /// leaves. `false` when the tree is too big.
    fn collect(&self, v: Value, n: Type, w: Type, t: &mut Tree) -> bool {
        let v = self.func.dfg.resolve_aliases(v);
        let def = self.func.dfg.value_def(v);
        if let ValueDef::Result(i, _) = def
            && self.uses[v] == 1
            && self.widenable(i)
        {
            if t.nodes.len() >= MAX_NODES {
                return false;
            }
            t.old += self.cost.node(self.func.dfg.insts[i].opcode());
            let args: Vec<Value> = match self.func.dfg.insts[i] {
                InstructionData::Ternary { args: [_, x, y], .. } => vec![x, y],
                InstructionData::Binary { opcode: Opcode::Ishl, args: [x, _] } => vec![x],
                _ => self.func.dfg.inst_args(i).to_vec(),
            };
            for a in args {
                if !self.collect(a, n, w, t) {
                    return false;
                }
            }
            t.nodes.push(i);
            return true;
        }
        // A leaf.
        match def.inst().map(|d| &self.func.dfg.insts[d]) {
            Some(InstructionData::Unary { opcode: Opcode::Ireduce, arg }) => {
                if self.uses[v] == 1 {
                    t.old += self.cost.ireduce_leaf(self.func, *arg, n);
                }
                let st = self.func.dfg.value_type(*arg);
                // A narrower source (`ireduce.i8` of an i16) widens with
                // `uextend`.
                if st.bits() < w.bits() {
                    t.new += self.cost.other_leaf(self.func, *arg);
                } else if self.cost.masked && st.bits() > w.bits() {
                    t.new += 1;
                }
            }
            Some(InstructionData::UnaryImm { opcode: Opcode::Iconst, .. }) => {}
            Some(InstructionData::Unary { opcode: Opcode::Uextend | Opcode::Sextend, .. }) => {}
            _ => t.new += self.cost.other_leaf(self.func, v),
        }
        true
    }
}

/// `v` (of a narrow type) as a value of type `w` whose low bits are `v`'s,
/// inserted before `at`.
fn widen_leaf(func: &mut Function, v: Value, w: Type, at: Inst) -> Value {
    let v = func.dfg.resolve_aliases(v);
    let n = func.dfg.value_type(v);
    let def = func.dfg.value_def(v).inst().map(|d| func.dfg.insts[d]);
    let mut cur = FuncCursor::new(func).at_inst(at);
    match def {
        Some(InstructionData::Unary { opcode: Opcode::Ireduce, arg }) => {
            let arg = cur.func.dfg.resolve_aliases(arg);
            let st = cur.func.dfg.value_type(arg);
            if st == w {
                arg
            } else if st.bits() > w.bits() {
                cur.ins().ireduce(w, arg)
            } else {
                cur.ins().uextend(w, arg)
            }
        }
        Some(InstructionData::UnaryImm { opcode: Opcode::Iconst, imm }) => {
            cur.ins().iconst(w, (imm.bits() as u64 & tymask(n)) as i64)
        }
        Some(InstructionData::Unary { opcode: Opcode::Uextend, arg }) => {
            let arg = cur.func.dfg.resolve_aliases(arg);
            cur.ins().uextend(w, arg)
        }
        Some(InstructionData::Unary { opcode: Opcode::Sextend, arg }) => {
            let arg = cur.func.dfg.resolve_aliases(arg);
            cur.ins().sextend(w, arg)
        }
        _ => cur.ins().uextend(w, v),
    }
}

/// Rebuild the tree rooted at `top` at type `w`; returns the wide value.
/// Each wide node goes right before its narrow original.
fn rebuild(func: &mut Function, top: Value, w: Type, nodes: &[Inst]) -> Value {
    let mut wide: rustc_hash::FxHashMap<Value, Value> = Default::default();
    for &i in nodes {
        let data = func.dfg.insts[i];
        let n = func.dfg.value_type(func.dfg.first_result(i));
        let get = |func: &mut Function, v: Value| -> Value {
            let v = func.dfg.resolve_aliases(v);
            match wide.get(&v) {
                Some(&x) => x,
                None => widen_leaf(func, v, w, i),
            }
        };
        let r = match data {
            InstructionData::Binary { opcode: Opcode::Ishl, args: [x, k] } => {
                let x = get(func, x);
                let kk = iconst(func, k).unwrap() & u64::from(n.bits() - 1);
                let mut cur = FuncCursor::new(func).at_inst(i);
                let kc = cur.ins().iconst(w, kk as i64);
                cur.ins().ishl(x, kc)
            }
            InstructionData::Binary { opcode, args: [a, b] } => {
                let a = get(func, a);
                let b = get(func, b);
                let mut cur = FuncCursor::new(func).at_inst(i);
                let (ni, dfg) = cur.ins().Binary(opcode, w, a, b);
                dfg.first_result(ni)
            }
            InstructionData::Unary { opcode, arg } => {
                let a = get(func, arg);
                let mut cur = FuncCursor::new(func).at_inst(i);
                let (ni, dfg) = cur.ins().Unary(opcode, w, a);
                dfg.first_result(ni)
            }
            InstructionData::Ternary { opcode, args: [c, x, y] } => {
                let x = get(func, x);
                let y = get(func, y);
                let mut cur = FuncCursor::new(func).at_inst(i);
                let (ni, dfg) = cur.ins().Ternary(opcode, w, c, x, y);
                dfg.first_result(ni)
            }
            _ => unreachable!("not a widenable node"),
        };
        let res = func.dfg.first_result(i);
        wide.insert(res, r);
    }
    // The last node is the top (post-order); a leaf top never gets here.
    wide[&func.dfg.resolve_aliases(top)]
}

enum Root {
    /// `uextend`/`sextend` result `r`; `mask`: the extended bits are
    /// demanded, so a zero extension keeps a `band`.
    Extend { r: Value, mask: bool },
    /// A narrow `store` of the tree.
    Store,
}

/// Run the analysis and its rewrites on `func`.
pub fn run(func: &mut Function, target: &Target) -> usize {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_DEMANDED", "1"));
    if !*ON {
        return 0;
    }
    let Some(dm) = analyze(func) else { return 0 };
    let mut uses: SecondaryMap<Value, u32> = SecondaryMap::with_default(0);
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                uses[func.dfg.resolve_aliases(v)] += 1;
            }
        }
    }
    let masked = target.narrow_masked();
    let word = Type::int((target.word_bytes() * 8) as u16).expect("word is an integer type");
    // (root inst, root kind, narrow operand, wide type, tree)
    let mut trees: Vec<(Inst, Root, Value, Type, Tree)> = Vec::new();
    // (inst, result, replacement) for useless masks.
    let mut drops: Vec<(Inst, Value, Value)> = Vec::new();
    {
        let cx = Ctx { func, uses: &uses, cost: Cost { masked, word: word.bits() } };
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                match func.dfg.insts[i] {
                    InstructionData::Binary { opcode: opcode @ (Opcode::Band | Opcode::Bor | Opcode::Bxor), args: [a, b] } => {
                        let r = func.dfg.first_result(i);
                        if !tracked(func.dfg.value_type(r)) {
                            continue;
                        }
                        let (x, c) = match (iconst(func, a), iconst(func, b)) {
                            (_, Some(c)) => (a, c),
                            (Some(c), None) => (b, c),
                            _ => continue,
                        };
                        let d = dm.of(func, r);
                        let tm = tymask(func.dfg.value_type(r));
                        let c = c & tm;
                        let useless = match opcode {
                            Opcode::Band => d & !c == 0 && c != tm,
                            _ => d & c == 0 && c != 0,
                        };
                        if useless {
                            drops.push((i, r, func.dfg.resolve_aliases(x)));
                        }
                    }
                    InstructionData::Unary { opcode: opcode @ (Opcode::Uextend | Opcode::Sextend), arg } => {
                        let r = func.dfg.first_result(i);
                        let m = func.dfg.value_type(r);
                        let n = func.dfg.value_type(arg);
                        if !matches!(n, types::I8 | types::I16) || !tracked(m) || m.bits() < 32 {
                            continue;
                        }
                        let undemanded = dm.of(func, r) & !tymask(n) == 0;
                        if opcode == Opcode::Sextend && !undemanded {
                            continue;
                        }
                        // Where narrow arithmetic is masked, the chain is
                        // rebuilt at the word and extended once.
                        let w = if masked && m.bits() > word.bits() { word } else { m };
                        let mut t = Tree::default();
                        let arg = func.dfg.resolve_aliases(arg);
                        if !cx.collect(arg, n, w, &mut t) {
                            continue;
                        }
                        // The root: its extend goes when nothing demands
                        // the bits it defines; a demanded zero extension
                        // becomes a mask. On aarch64 a zero extension used
                        // once by an add/sub/compare rides that use's
                        // extended-register operand for free.
                        let fused = !masked
                            && opcode == Opcode::Uextend
                            && uses[r] == 1
                            && single_use_fuses(func, r);
                        let root_old = if masked { u32::from(m.bits() > w.bits()) + u32::from(opcode == Opcode::Sextend) } else { u32::from(!fused) };
                        let root_new = u32::from(!undemanded) + u32::from(m.bits() > w.bits());
                        t.old += root_old;
                        t.new += root_new;
                        if t.nodes.is_empty() {
                            // A leaf operand: only `uextend (ireduce y)` /
                            // constants, and only when the extension is
                            // not demanded.
                            if !undemanded {
                                continue;
                            }
                            let ok = matches!(
                                func.dfg.value_def(arg).inst().map(|d| &func.dfg.insts[d]),
                                Some(InstructionData::Unary { opcode: Opcode::Ireduce, arg: y })
                                    if func.dfg.value_type(*y).bits() >= w.bits()
                            );
                            if !ok {
                                continue;
                            }
                        } else if t.new >= t.old {
                            continue;
                        }
                        trees.push((i, Root::Extend { r, mask: !undemanded }, arg, w, t));
                    }
                    InstructionData::Store { opcode: Opcode::Store, args: [x, _], .. } => {
                        let x = func.dfg.resolve_aliases(x);
                        let n = func.dfg.value_type(x);
                        if !matches!(n, types::I8 | types::I16) {
                            continue;
                        }
                        let mut t = Tree::default();
                        // i32: the narrowest whole register on every target.
                        if !cx.collect(x, n, types::I32, &mut t) || t.nodes.is_empty() || t.new >= t.old {
                            continue;
                        }
                        trees.push((i, Root::Store, x, types::I32, t));
                    }
                    _ => {}
                }
            }
        }
    }
    let mut k = 0;
    for (i, r, x) in drops {
        if !pliron_cfg::bisect("demanded") {
            continue;
        }
        func.layout.remove_inst(i);
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(r, x);
        k += 1;
    }
    let mut dead = false;
    for (i, root, x, w, t) in trees {
        if !pliron_cfg::bisect("demanded") {
            continue;
        }
        // A drop above may have aliased a node away; a stale tree is
        // skipped (the next run sees it again).
        if t.nodes.iter().any(|&n| func.layout.inst_block(n).is_none()) || func.layout.inst_block(i).is_none() {
            continue;
        }
        let wide = if t.nodes.is_empty() {
            widen_leaf(func, x, w, i)
        } else {
            rebuild(func, x, w, &t.nodes)
        };
        match root {
            Root::Extend { r, mask } => {
                let m = func.dfg.value_type(r);
                let n = func.dfg.value_type(x);
                let mut cur = FuncCursor::new(func).at_inst(i);
                let mut v = wide;
                if mask {
                    let c = cur.ins().iconst(w, tymask(n) as i64);
                    v = cur.ins().band(v, c);
                }
                if m.bits() > w.bits() {
                    v = cur.ins().uextend(m, v);
                }
                func.layout.remove_inst(i);
                func.dfg.clear_results(i);
                func.dfg.change_to_alias(r, v);
            }
            Root::Store => {
                let n = func.dfg.value_type(x);
                if let InstructionData::Store { opcode, args, .. } = &mut func.dfg.insts[i] {
                    *opcode = if n == types::I8 { Opcode::Istore8 } else { Opcode::Istore16 };
                    args[0] = wide;
                }
            }
        }
        dead = true;
        k += 1;
    }
    if dead {
        crate::uses::worklist_dce(func, |f, i| {
            matches!(
                f.dfg.insts[i].opcode(),
                Opcode::Iadd
                    | Opcode::Isub
                    | Opcode::Imul
                    | Opcode::Band
                    | Opcode::Bor
                    | Opcode::Bxor
                    | Opcode::Bnot
                    | Opcode::Ineg
                    | Opcode::Ishl
                    | Opcode::Select
                    | Opcode::Iconst
                    | Opcode::Ireduce
                    | Opcode::Uextend
                    | Opcode::Sextend
            )
        });
    }
    k
}

/// `r`'s one use reads it as an aarch64 extended-register operand: a
/// 32/64-bit `iadd`/`isub` or `icmp`.
fn single_use_fuses(func: &Function, r: Value) -> bool {
    // The use is found by a scan of `r`'s block's successors' worth of
    // code in the worst case; callers ask only for single-use roots, and
    // the answer only changes a cost, so a bounded look at the next few
    // instructions of the root's block is enough.
    let Some(def) = func.dfg.value_def(r).inst() else { return false };
    let mut cur = func.layout.next_inst(def);
    for _ in 0..8 {
        let Some(u) = cur else { return false };
        if func.dfg.inst_args(u).iter().any(|&a| func.dfg.resolve_aliases(a) == r) {
            return matches!(func.dfg.insts[u].opcode(), Opcode::Iadd | Opcode::Isub | Opcode::Icmp);
        }
        cur = func.layout.next_inst(u);
    }
    false
}
