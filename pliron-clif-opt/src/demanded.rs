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

/// Per-target extension/mask counts: what rebuilding a narrow web wide
/// adds and removes.
struct Cost {
    /// `Target::narrow_masked`.
    masked: bool,
    /// The machine word in bits (`Target::word_bytes`).
    word: u32,
}

/// `v` is a constant, a compare, a narrow load, an extension or an
/// `ireduce` (which aarch64 materializes as `uxtb`/`uxth`): its register
/// holds the zero-extended value on aarch64, and wasm's `norm` sees it as
/// canonical.
fn clean_leaf(func: &Function, v: Value) -> bool {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)).inst().map(|d| &func.dfg.insts[d]) {
        Some(InstructionData::Load { opcode, .. }) => !matches!(opcode, Opcode::Sload8 | Opcode::Sload16),
        Some(InstructionData::IntCompare { .. } | InstructionData::FloatCompare { .. }) => true,
        Some(InstructionData::UnaryImm { opcode: Opcode::Iconst, .. }) => true,
        Some(InstructionData::Unary { opcode: Opcode::Uextend | Opcode::Ireduce, .. }) => true,
        _ => false,
    }
}

impl Cost {
    /// An `ireduce.N` leaf the web alone reads: aarch64 materializes a
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

    /// The conversion of a leaf the web used to take narrow and now takes
    /// at `w`.
    fn leaf(&self, func: &Function, l: Value, w: Type) -> u32 {
        let l = func.dfg.resolve_aliases(l);
        match func.dfg.value_def(l).inst().map(|d| &func.dfg.insts[d]) {
            Some(InstructionData::Unary { opcode: Opcode::Ireduce, arg }) => {
                let st = func.dfg.value_type(*arg);
                if st.bits() < w.bits() {
                    // `ireduce.i8` of an i16 widens with `uextend`.
                    u32::from(!clean_leaf(func, *arg))
                } else {
                    u32::from(self.masked && st.bits() > w.bits())
                }
            }
            Some(InstructionData::UnaryImm { opcode: Opcode::Iconst, .. }) => 0,
            // Re-extended from the narrower source: the same cost as before.
            Some(InstructionData::Unary { opcode: Opcode::Uextend | Opcode::Sextend, .. }) => 0,
            // wasm keeps every narrow arithmetic result masked.
            Some(InstructionData::Binary { opcode, .. }) if self.masked && *opcode != Opcode::Sshr => 0,
            Some(InstructionData::Unary { opcode: Opcode::Bnot | Opcode::Ineg, .. }) if self.masked => 0,
            _ => u32::from(!clean_leaf(func, l)),
        }
    }

    /// The mask wasm puts after a narrow node of this kind.
    fn node(&self, op: Opcode) -> u32 {
        u32::from(self.masked && matches!(op, Opcode::Iadd | Opcode::Isub | Opcode::Imul | Opcode::Ishl | Opcode::Ineg | Opcode::Bnot))
    }
}

/// A narrow op whose low bits depend only on its operands' low bits.
fn widenable(func: &Function, i: Inst) -> bool {
    match func.dfg.insts[i] {
        InstructionData::Binary { opcode, args } => match opcode {
            Opcode::Iadd | Opcode::Isub | Opcode::Imul | Opcode::Band | Opcode::Bor | Opcode::Bxor => true,
            Opcode::Ishl => iconst(func, args[1]).is_some(),
            _ => false,
        },
        InstructionData::Unary { opcode: Opcode::Bnot | Opcode::Ineg, .. } => true,
        InstructionData::Ternary { opcode: Opcode::Select, .. } => true,
        _ => false,
    }
}

/// The operands of a widenable op that carry its value (not a `select`'s
/// condition, not a shift amount).
fn value_operands(func: &Function, i: Inst) -> smallvec::SmallVec<[Value; 2]> {
    match func.dfg.insts[i] {
        InstructionData::Ternary { args: [_, x, y], .. } => smallvec::smallvec![x, y],
        InstructionData::Binary { opcode: Opcode::Ishl, args: [x, _] } => smallvec::smallvec![x],
        _ => func.dfg.inst_args(i).iter().copied().collect(),
    }
}

/// How a web member is used.
#[derive(Clone, Copy)]
enum Use {
    /// `uextend`/`sextend` to 32 bits or more, or a narrow `store`.
    Root(Inst),
    /// A value operand of this widenable op (its result).
    Web(Value),
    /// An argument of an edge into this block parameter.
    Edge(Value),
    /// Anything else: the value stays narrow.
    Hard,
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

/// Union-find root.
fn find(up: &mut SecondaryMap<Value, Value>, v: Value) -> Value {
    let mut r = v;
    while up[r] != r {
        r = up[r];
    }
    let mut x = v;
    while up[x] != r {
        let n = up[x];
        up[x] = r;
        x = n;
    }
    r
}

/// Run the analysis and its rewrites on `func`.
pub fn run(func: &mut Function, target: &Target) -> usize {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_DEMANDED", "1"));
    if !*ON {
        return 0;
    }
    let Some(dm) = analyze(func) else { return 0 };
    let mut k = drop_masks(func, &dm);
    k += widen(func, &dm, target);
    k
}

/// Constant `band`/`bor`/`bxor` on bits nobody demands.
fn drop_masks(func: &mut Function, dm: &Demanded) -> usize {
    let mut drops: Vec<(Inst, Value, Value)> = Vec::new();
    // `uextend`/`sextend` of `ireduce y` whose extended bits nobody reads,
    // `y` wider than the result: `ireduce y` to the result type.
    let mut rereduce: Vec<(Inst, Type, Value)> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Unary { opcode: Opcode::Uextend | Opcode::Sextend, arg } = func.dfg.insts[i]
                && let Some(InstructionData::Unary { opcode: Opcode::Ireduce, arg: y }) =
                    func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst().map(|d| func.dfg.insts[d])
            {
                let r = func.dfg.first_result(i);
                let (m, n) = (func.dfg.value_type(r), func.dfg.value_type(arg));
                let y = func.dfg.resolve_aliases(y);
                let yt = func.dfg.value_type(y);
                if tracked(m) && dm.of(func, r) & !tymask(n) == 0 {
                    if yt == m {
                        drops.push((i, r, y));
                    } else if yt.bits() > m.bits() {
                        rereduce.push((i, m, y));
                    }
                }
                continue;
            }
            let InstructionData::Binary { opcode: opcode @ (Opcode::Band | Opcode::Bor | Opcode::Bxor), args: [a, b] } =
                func.dfg.insts[i]
            else {
                continue;
            };
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
    for (i, m, y) in rereduce {
        if !pliron_cfg::bisect("demanded") {
            continue;
        }
        func.replace(i).ireduce(m, y);
        k += 1;
    }
    k
}

/// Narrow webs rebuilt wide. A web is a set of `i8`/`i16` values — results
/// of widenable ops and block parameters — whose every use is another
/// member (a value operand of a member op, an edge argument of a member
/// parameter) or a root: a `uextend`/`sextend` to 32 bits or more, or a
/// narrow `store`. A greatest fixpoint drops any candidate with another
/// use. Each connected web is rebuilt at one type `w` when the counts say
/// it pays: member parameters change type, member ops get wide twins,
/// leaves (operands from outside) convert, and the roots read the wide
/// value (a zero extension whose bits are demanded keeps a mask).
fn widen(func: &mut Function, dm: &Demanded, target: &Target) -> usize {
    let masked = target.narrow_masked();
    let word = Type::int((target.word_bytes() * 8) as u16).expect("word is an integer type");
    let cost = Cost { masked, word: word.bits() };
    let narrow = |t: Type| matches!(t, types::I8 | types::I16);
    let entry = func.layout.entry_block();

    // Candidates and their uses, in one scan.
    let mut cand: SecondaryMap<Value, bool> = SecondaryMap::with_default(false);
    let mut uses: SecondaryMap<Value, smallvec::SmallVec<[Use; 2]>> = SecondaryMap::default();
    let mut nuses: SecondaryMap<Value, u32> = SecondaryMap::with_default(0);
    let mut preds: Preds = SecondaryMap::default();
    let mut any = false;
    for b in func.layout.blocks() {
        if Some(b) != entry {
            for &p in func.dfg.block_params(b) {
                if narrow(func.dfg.value_type(p)) {
                    cand[p] = true;
                    any = true;
                }
            }
        }
        for i in func.layout.block_insts(b) {
            if let [r] = func.dfg.inst_results(i)
                && narrow(func.dfg.value_type(*r))
                && widenable(func, i)
            {
                cand[*r] = true;
                any = true;
            }
        }
    }
    if !any {
        return 0;
    }
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let data = &func.dfg.insts[i];
            let dests = data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            for v in func.dfg.inst_values(i) {
                nuses[func.dfg.resolve_aliases(v)] += 1;
            }
            if !dests.is_empty() {
                for &a in func.dfg.inst_args(i) {
                    uses[func.dfg.resolve_aliases(a)].push(Use::Hard);
                }
                for bc in dests {
                    let t = bc.block(&func.dfg.value_lists);
                    if preds[t].last() != Some(&i) {
                        preds[t].push(i);
                    }
                    for (k, arg) in bc.args(&func.dfg.value_lists).enumerate() {
                        let p = func.dfg.block_params(t).get(k).copied();
                        match (arg, p) {
                            (BlockArg::Value(v), Some(p)) => uses[func.dfg.resolve_aliases(v)].push(Use::Edge(p)),
                            // An argument the branch makes itself (a
                            // `try_call` result): its parameter stays.
                            (_, Some(p)) => uses[p].push(Use::Hard),
                            _ => {}
                        }
                    }
                }
                continue;
            }
            let root = match *data {
                InstructionData::Unary { opcode: opcode @ (Opcode::Uextend | Opcode::Sextend), arg } => {
                    let r = func.dfg.first_result(i);
                    let m = func.dfg.value_type(r);
                    let n = func.dfg.value_type(arg);
                    let sign_ok = opcode == Opcode::Uextend || dm.of(func, r) & !tymask(n) == 0;
                    tracked(m) && m.bits() >= 32 && sign_ok
                }
                InstructionData::Store { opcode: Opcode::Store, .. } => true,
                _ => false,
            };
            if root {
                // A store's address is never narrow; its value is operand 0.
                let a = func.dfg.resolve_aliases(func.dfg.inst_args(i)[0]);
                uses[a].push(Use::Root(i));
                for &x in &func.dfg.inst_args(i)[1..] {
                    uses[func.dfg.resolve_aliases(x)].push(Use::Hard);
                }
                continue;
            }
            let member = matches!(func.dfg.inst_results(i), [r] if cand[*r]);
            if member {
                let r = func.dfg.first_result(i);
                for (k, &a) in func.dfg.inst_args(i).iter().enumerate() {
                    let a = func.dfg.resolve_aliases(a);
                    // The slots `value_operands` names carry the value.
                    let is_val = match func.dfg.insts[i] {
                        InstructionData::Ternary { .. } => k > 0,
                        InstructionData::Binary { opcode: Opcode::Ishl, .. } => k == 0,
                        _ => true,
                    };
                    uses[a].push(if is_val { Use::Web(r) } else { Use::Hard });
                }
            } else {
                for &a in func.dfg.inst_args(i) {
                    uses[func.dfg.resolve_aliases(a)].push(Use::Hard);
                }
            }
        }
    }

    // Greatest fixpoint: drop candidates with a hard use, then anything
    // feeding a dropped value.
    let mut feeds: SecondaryMap<Value, smallvec::SmallVec<[Value; 2]>> = SecondaryMap::default();
    let mut members: Vec<Value> = Vec::new();
    let mut work: Vec<Value> = Vec::new();
    for b in func.layout.blocks() {
        let params = func.dfg.block_params(b).to_vec();
        let results = func.layout.block_insts(b).flat_map(|i| func.dfg.inst_results(i).to_vec());
        for v in params.into_iter().chain(results) {
            if !cand[v] {
                continue;
            }
            members.push(v);
            let mut hard = uses[v].is_empty();
            for &u in &uses[v] {
                match u {
                    Use::Root(_) => {}
                    Use::Web(x) | Use::Edge(x) => {
                        if cand[x] {
                            feeds[x].push(v);
                        } else {
                            hard = true;
                        }
                    }
                    Use::Hard => hard = true,
                }
            }
            if hard {
                work.push(v);
            }
        }
    }
    while let Some(v) = work.pop() {
        if !cand[v] {
            continue;
        }
        cand[v] = false;
        for k in 0..feeds[v].len() {
            let d = feeds[v][k];
            if cand[d] {
                work.push(d);
            }
        }
    }
    members.retain(|&v| cand[v]);
    if members.is_empty() {
        return 0;
    }

    // Webs: connected through web and edge uses.
    let mut up: SecondaryMap<Value, Value> = SecondaryMap::with_default(Value::from_u32(0));
    for &v in &members {
        up[v] = v;
    }
    for &v in &members {
        for k in 0..uses[v].len() {
            if let Use::Web(x) | Use::Edge(x) = uses[v][k] {
                let (a, b) = (find(&mut up, v), find(&mut up, x));
                if a != b {
                    up[a] = b;
                }
            }
        }
    }
    struct Web {
        old: u32,
        new: u32,
        /// Widest extension root (0: stores only).
        widest: u32,
        members: Vec<Value>,
    }
    let mut webs: rustc_hash::FxHashMap<Value, Web> = Default::default();
    // Occurrences of each `ireduce` leaf per web, to tell whether the web
    // is its only reader.
    let mut red: rustc_hash::FxHashMap<(Value, Value), u32> = Default::default();
    for &v in &members {
        let root = find(&mut up, v);
        let n = func.dfg.value_type(v);
        let w = webs.entry(root).or_insert(Web { old: 0, new: 0, widest: 0, members: Vec::new() });
        w.members.push(v);
        // The member's own cost, and its leaves.
        let leaves: smallvec::SmallVec<[Value; 4]> = match func.dfg.value_def(v) {
            ValueDef::Result(i, _) => {
                w.old += cost.node(func.dfg.insts[i].opcode());
                value_operands(func, i).into_iter().map(|a| func.dfg.resolve_aliases(a)).filter(|&a| !cand[a]).collect()
            }
            ValueDef::Param(b, k) => {
                let mut ls = smallvec::SmallVec::new();
                for &t in &preds[b] {
                    for bc in func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                        if bc.block(&func.dfg.value_lists) != b {
                            continue;
                        }
                        if let Some(BlockArg::Value(a)) = bc.args(&func.dfg.value_lists).nth(k) {
                            let a = func.dfg.resolve_aliases(a);
                            if !cand[a] {
                                ls.push(a);
                            }
                        }
                    }
                }
                ls
            }
            _ => smallvec::SmallVec::new(),
        };
        for l in leaves {
            if let Some(InstructionData::Unary { opcode: Opcode::Ireduce, arg }) =
                func.dfg.value_def(l).inst().map(|d| &func.dfg.insts[d])
            {
                let c = red.entry((root, l)).or_insert(0);
                *c += 1;
                if *c == nuses[l] {
                    w.old += cost.ireduce_leaf(func, *arg, n);
                }
            }
            // The leaf's new conversion; its type is not known until every
            // root is seen, so count it at the word (a narrower `w` only
            // makes `ireduce` leaves cheaper).
            w.new += cost.leaf(func, l, if masked { word } else { types::I32 });
        }
        // Roots.
        for &u in &uses[v] {
            let Use::Root(i) = u else { continue };
            match func.dfg.insts[i] {
                InstructionData::Unary { opcode, .. } => {
                    let r = func.dfg.first_result(i);
                    let m = func.dfg.value_type(r);
                    let undemanded = dm.of(func, r) & !tymask(n) == 0;
                    w.widest = w.widest.max(m.bits());
                    let clean = member_clean(func, v);
                    let old = if masked {
                        u32::from(!clean) + u32::from(m.bits() > word.bits()) + u32::from(opcode == Opcode::Sextend)
                    } else {
                        let fused = opcode == Opcode::Uextend && nuses[r] == 1 && single_use_fuses(func, r);
                        u32::from(!fused && !(opcode == Opcode::Uextend && clean))
                    };
                    let new = u32::from(!undemanded) + u32::from(masked && m.bits() > word.bits());
                    w.old += old;
                    w.new += new;
                }
                _ => {}
            }
        }
    }

    let mut k = 0;
    let mut dead = false;
    // Deterministic order (the map is keyed by value).
    let mut webs: Vec<(Value, Web)> = webs.into_iter().collect();
    webs.sort_by_key(|(r, _)| *r);
    for (_, web) in webs {
        if web.new >= web.old {
            continue;
        }
        if !pliron_cfg::bisect("demanded") {
            continue;
        }
        // The widest root's type (32 bits for stores alone); where narrow
        // arithmetic is masked, the word, extended once at wider roots.
        let w = if masked { word } else { Type::int(web.widest.max(32) as u16).expect("int type") };
        rewrite(func, &web.members, &uses, &preds, w, dm);
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

/// A member whose register aarch64 already holds zero-extended (a `band`
/// with a clean operand, `bor`/`bxor` of two clean ones, a parameter: all
/// as `narrow_uext_clean` sees them, the parameter optimistically) — its
/// root extension costs nothing today. On wasm: `norm` sees it as
/// canonical (a narrow add, subtract, multiply or shift masked itself).
fn member_clean(func: &Function, v: Value) -> bool {
    match func.dfg.value_def(v) {
        ValueDef::Param(..) => true,
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::Binary { opcode: Opcode::Band, args: [a, b] } => clean_leaf(func, a) || clean_leaf(func, b),
            InstructionData::Binary { opcode: Opcode::Bor | Opcode::Bxor, args: [a, b] } => {
                clean_leaf(func, a) && clean_leaf(func, b)
            }
            _ => false,
        },
        _ => false,
    }
}

type Preds = SecondaryMap<cranelift_codegen::ir::Block, Vec<Inst>>;

/// Rebuild one web at `w`.
fn rewrite(
    func: &mut Function,
    members: &[Value],
    uses: &SecondaryMap<Value, smallvec::SmallVec<[Use; 2]>>,
    preds: &Preds,
    w: Type,
    dm: &Demanded,
) {
    let mut wide: rustc_hash::FxHashMap<Value, Value> = Default::default();
    let is_member: rustc_hash::FxHashSet<Value> = members.iter().copied().collect();
    let n = func.dfg.value_type(members[0]);
    // Parameters first: member ops may read them, and every cycle in the
    // web passes through one.
    let mut params: Vec<(cranelift_codegen::ir::Block, usize)> = Vec::new();
    for &v in members {
        if let ValueDef::Param(b, k) = func.dfg.value_def(v) {
            let p2 = func.dfg.replace_block_param(v, w);
            wide.insert(v, p2);
            params.push((b, k));
        }
    }
    // Member ops, operands first (an explicit DFS: chains can be long).
    // Each wide twin goes right before its original, where every operand
    // is already defined.
    let mut order: Vec<Inst> = Vec::new();
    {
        let mut done: rustc_hash::FxHashSet<Inst> = Default::default();
        for &v in members {
            let ValueDef::Result(i0, _) = func.dfg.value_def(v) else { continue };
            let mut stack: Vec<(Inst, bool)> = vec![(i0, false)];
            while let Some((i, expanded)) = stack.pop() {
                if done.contains(&i) {
                    continue;
                }
                if expanded {
                    done.insert(i);
                    order.push(i);
                    continue;
                }
                stack.push((i, true));
                for a in value_operands(func, i) {
                    let a = func.dfg.resolve_aliases(a);
                    if is_member.contains(&a)
                        && let ValueDef::Result(d, _) = func.dfg.value_def(a)
                        && !done.contains(&d)
                    {
                        stack.push((d, false));
                    }
                }
            }
        }
    }
    for i in order {
        let r = func.dfg.first_result(i);
        let data = func.dfg.insts[i];
        let get = |func: &mut Function, v: Value| -> Value {
            let v = func.dfg.resolve_aliases(v);
            match wide.get(&v) {
                Some(&x) => x,
                None => widen_leaf(func, v, w, i),
            }
        };
        let nv = match data {
            InstructionData::Binary { opcode: Opcode::Ishl, args: [x, kk] } => {
                let x = get(func, x);
                let amt = iconst(func, kk).unwrap() & u64::from(n.bits() - 1);
                let mut cur = FuncCursor::new(func).at_inst(i);
                let kc = cur.ins().iconst(w, amt as i64);
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
            _ => unreachable!("not a widenable op"),
        };
        wide.insert(r, nv);
    }
    // Edges into member parameters.
    for &(b, k) in &params {
        for &t in &preds[b] {
            // The new arguments first (conversions go before the branch).
            let mut news: Vec<Value> = Vec::new();
            for bc in func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                if let Some(BlockArg::Value(a)) = bc.args(&func.dfg.value_lists).nth(k) {
                    news.push(func.dfg.resolve_aliases(a));
                }
            }
            let news: Vec<Value> = news
                .into_iter()
                .map(|a| match wide.get(&a) {
                    Some(&x) => x,
                    None => widen_leaf(func, a, w, t),
                })
                .collect();
            let dfg = &mut func.dfg;
            let mut it = news.into_iter();
            for bc in dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
                if bc.block(&dfg.value_lists) != b {
                    continue;
                }
                let nv = it.next().expect("one new argument per edge");
                let mut ix = 0;
                bc.update_args(&mut dfg.value_lists, |arg| {
                    ix += 1;
                    if ix - 1 == k { nv.into() } else { arg }
                });
            }
        }
    }
    // Roots.
    for &v in members {
        let wv = wide[&v];
        for &u in &uses[v] {
            let Use::Root(i) = u else { continue };
            if func.layout.inst_block(i).is_none() {
                continue;
            }
            match func.dfg.insts[i] {
                InstructionData::Unary { .. } => {
                    let r = func.dfg.first_result(i);
                    let m = func.dfg.value_type(r);
                    let mut cur = FuncCursor::new(func).at_inst(i);
                    let mut x = wv;
                    if cur.func.dfg.insts[i].opcode() == Opcode::Uextend && dm.of(cur.func, r) & !tymask(n) != 0 {
                        let c = cur.ins().iconst(w, tymask(n) as i64);
                        x = cur.ins().band(x, c);
                    }
                    if m.bits() > w.bits() {
                        x = cur.ins().uextend(m, x);
                    } else if m.bits() < w.bits() {
                        x = cur.ins().ireduce(m, x);
                    }
                    func.layout.remove_inst(i);
                    func.dfg.clear_results(i);
                    func.dfg.change_to_alias(r, x);
                }
                InstructionData::Store { .. } => {
                    if let InstructionData::Store { opcode, args, .. } = &mut func.dfg.insts[i] {
                        *opcode = if n == types::I8 { Opcode::Istore8 } else { Opcode::Istore16 };
                        args[0] = wv;
                    }
                }
                _ => {}
            }
        }
    }
}

/// `r`'s one use reads it as an aarch64 extended-register operand: a
/// 32/64-bit `iadd`/`isub` or `icmp`.
fn single_use_fuses(func: &Function, r: Value) -> bool {
    // The answer only changes a cost: a bounded look at the next few
    // instructions of the root's block.
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
