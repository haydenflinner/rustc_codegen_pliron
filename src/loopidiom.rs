//! Loop idiom recognition on CLIF (LLVM loop-idiom): a counted loop whose
//! only side effect is a store of an invariant value at an address advancing
//! by one element per iteration becomes a `memset`; a loop copying
//! `src[i] -> dst[i]` over provably disjoint roots becomes `memcpy`.
//!
//! `!=`-style exits (`p != end`, `i != n`) only terminate if the induction
//! reaches the bound exactly, so the rewrite is emitted under a runtime
//! guard (order and divisibility); the original loop stays as the fallback.
//! `PLIRON_IDIOM=0` disables it; `PLIRON_IDIOM_DEBUG` logs conversions.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, BlockCall, ExternalName, ExtFuncData, FuncRef, Function, Inst,
    InstBuilder, InstructionData, LibCall, MemFlagsData, Opcode, Signature, StackSlot, Type, Value,
    ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::loadfwd::{self, Root};

const MAX_LOOPS: usize = 16;
const MAX_CONV: usize = 8;

/// Debug macro: return None, logging `why` under PLIRON_IDIOM_DEBUG.
macro_rules! why {
    ($($t:tt)*) => {{
        if std::env::var_os("PLIRON_IDIOM_DEBUG").is_some() {
            eprintln!("idiom bail: {}", format_args!($($t)*));
        }
        return None;
    }};
}

pub(crate) fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits()),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

/// One destination of `inst`: its index in the destination list and target.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edge {
    pub(crate) inst: Inst,
    pub(crate) slot: usize,
}

pub(crate) fn edge_dest(func: &Function, e: Edge) -> Block {
    func.dfg.insts[e.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[e.slot]
        .block(&func.dfg.value_lists)
}

pub(crate) fn edge_args(func: &Function, e: Edge) -> Vec<Value> {
    func.dfg.insts[e.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[e.slot]
        .args(&func.dfg.value_lists)
        .map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
}

/// `v` as `base + lin` where `lin` is `iv*K`, `iv<<log2K`, or `iv` (K = 1):
/// (base, K). `None` for the bare-param form handled by the caller.
pub(crate) fn scaled_addr(func: &Function, iv: Value, addr: Value) -> Option<(Value, i64)> {
    let mut v = func.dfg.resolve_aliases(addr);
    // Tolerate a pointer-width uextend around the sum.
    for _ in 0..2 {
        let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
            break;
        };
        match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } => v = func.dfg.resolve_aliases(arg),
            _ => break,
        }
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    let InstructionData::Binary {
        opcode: Opcode::Iadd,
        args: [a, b],
    } = func.dfg.insts[i]
    else {
        return None;
    };
    for (x, y) in [(a, b), (b, a)] {
        if let Some(k) = scaled(func, iv, y) {
            return Some((func.dfg.resolve_aliases(x), k));
        }
    }
    None
}

/// `v` as `iv*K`: `imul(iv, K)`, `ishl(iv, log2K)`, or bare `iv` (K = 1).
pub(crate) fn scaled(func: &Function, iv: Value, v: Value) -> Option<i64> {
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..2 {
        let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
            break;
        };
        match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } => v = func.dfg.resolve_aliases(arg),
            _ => break,
        }
    }
    if v == iv {
        return Some(1);
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args: [a, b],
        } => {
            if func.dfg.resolve_aliases(a) == iv {
                iconst(func, b)
            } else if func.dfg.resolve_aliases(b) == iv {
                iconst(func, a)
            } else {
                None
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [a, b],
        } if func.dfg.resolve_aliases(a) == iv => {
            iconst(func, b).and_then(|s| (0..64).contains(&s).then(|| 1i64 << s))
        }
        _ => None,
    }
}

pub(crate) struct Info {
    pub(crate) h: Block,
    pub(crate) body: FxHashSet<Block>,
    /// The one edge into `h` from outside and its arguments.
    pub(crate) entry: Edge,
    pub(crate) entry_args: Vec<Value>,
    /// All edges out of the loop; `plan` picks the real exit and proves the
    /// rest are dead guards (e.g. rustc's bounds-check panic blocks).
    pub(crate) exits: Vec<Edge>,
    /// In-loop edges back to `h` (latches).
    pub(crate) latches: Vec<Edge>,
}

pub(crate) fn gather(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
) -> Option<Info> {
    let h = la.loop_header(lp);
    if !dt.is_reachable(h) {
        return None;
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    // Innermost only: nested loops confuse the count analysis.
    if body.iter().any(|&b| la.innermost_loop(b) != Some(lp)) {
        return None;
    }
    let mut entry: Option<Edge> = None;
    let mut latches = Vec::new();
    for p in cfg.pred_iter(h) {
        for (slot, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) != h {
                continue;
            }
            let e = Edge { inst: p.inst, slot };
            if la.is_in_loop(p.block, lp) {
                latches.push(e);
            } else if entry.is_none() {
                entry = Some(e);
            } else {
                return None;
            }
        }
    }
    let entry = entry?;
    if latches.is_empty() {
        return None;
    }
    let mut exits = Vec::new();
    for &b in &body {
        let t = func.layout.last_inst(b)?;
        for (slot, bc) in func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if !body.contains(&bc.block(&func.dfg.value_lists)) {
                exits.push(Edge { inst: t, slot });
            }
        }
    }
    if exits.is_empty() {
        return None;
    }
    let entry_args = edge_args(func, entry);
    if entry_args.len() != func.dfg.block_params(h).len() {
        return None;
    }
    Some(Info {
        h,
        body,
        entry,
        entry_args,
        exits,
        latches,
    })
}

/// `store`/`istore8`/`istore16`/`istore32` details: (value, addr, size).
pub(crate) fn store_parts(func: &Function, i: Inst) -> Option<(Value, Value, i64)> {
    match func.dfg.insts[i] {
        InstructionData::Store {
            opcode,
            args,
            offset,
            ..
        } => {
            if !loadfwd::notrap(func, i) || i32::from(offset) != 0 {
                return None;
            }
            let v = func.dfg.resolve_aliases(args[0]);
            let n = match opcode {
                Opcode::Store => i64::from(func.dfg.value_type(v).bytes()),
                Opcode::Istore8 => 1,
                Opcode::Istore16 => 2,
                Opcode::Istore32 => 4,
                _ => return None,
            };
            Some((v, args[1], n))
        }
        _ => None,
    }
}

/// `v` all bytes equal -> the byte, or an `i8` store value itself.
fn fill_byte(func: &Function, v: Value) -> Option<Option<Value>> {
    let v = func.dfg.resolve_aliases(v);
    let ty = func.dfg.value_type(v);
    if let Some(c) = iconst(func, v) {
        let b = c as u8;
        let mut pat = 0u64;
        for _ in 0..ty.bytes() {
            pat = pat << 8 | b as u64;
        }
        let mask = if ty.bits() >= 64 {
            u64::MAX
        } else {
            (1u64 << ty.bits()) - 1
        };
        return ((c as u64 & mask) == pat).then_some(None);
    }
    if ty.bytes() == 1 && ty.is_int() {
        return Some(Some(v));
    }
    None
}

/// Which libc call and the expressions for its arguments.
pub(crate) struct Plan {
    /// The entry edge to redirect through the check block.
    entry: Edge,
    h: Block,
    entry_args: Vec<Value>,
    exit_dest: Block,
    exit_args: Vec<Value>,
    /// Predicates that must all hold for the fast path (empty = always).
    preds: Vec<Pred>,
    dst: Ins,
    len: Ins,
    src: Option<Ins>,
    fill: Option<Fill>,
}

#[derive(Clone, Copy)]
pub(crate) enum Fill {
    C(i64),
    V(Value),
}

/// A guard predicate, emitted as an `i8` condition in the check block.
pub(crate) enum Pred {
    Cmp(IntCC, Ins, Ins),
    /// `x & (size-1) == 0` — divisibility of a byte distance.
    Aligned(Ins, i64),
    /// `[dst, dst+len)` and `[src, src+len)` don't overlap and don't wrap.
    Disjoint,
}

/// A tiny expression DSL emitted into the check/call blocks.
#[derive(Clone)]
pub(crate) enum Ins {
    Val(Value),
    K(i64),
    Add(Box<Ins>, Box<Ins>),
    Sub(Box<Ins>, Box<Ins>),
    SatSub(Box<Ins>, Box<Ins>),
    Mul(Box<Ins>, Box<Ins>),
    And(Box<Ins>, Box<Ins>),
    Div(Box<Ins>, Box<Ins>),
}

/// `e` evaluates to a known constant (constants and `iconst` values folded
/// through the tree). `None` on any non-constant input or `Div` by zero —
/// the ops here all wrap like the emitted code does.
fn ins_eval(func: &Function, e: &Ins) -> Option<i64> {
    Some(match e {
        Ins::K(k) => *k,
        Ins::Val(v) => iconst(func, *v)?,
        Ins::Add(a, b) => ins_eval(func, a)?.wrapping_add(ins_eval(func, b)?),
        Ins::Sub(a, b) => ins_eval(func, a)?.wrapping_sub(ins_eval(func, b)?),
        Ins::SatSub(a, b) => ins_eval(func, a)?.saturating_sub(ins_eval(func, b)?),
        Ins::Mul(a, b) => ins_eval(func, a)?.wrapping_mul(ins_eval(func, b)?),
        Ins::And(a, b) => ins_eval(func, a)? & ins_eval(func, b)?,
        Ins::Div(a, b) => (ins_eval(func, a)? as u64)
            .checked_div(ins_eval(func, b)? as u64)? as i64,
    })
}

/// `icmp cc x, y` on evaluated operands.
fn cmp_cc(cc: IntCC, x: i64, y: i64) -> bool {
    match cc {
        IntCC::Equal => x == y,
        IntCC::NotEqual => x != y,
        IntCC::SignedLessThan => x < y,
        IntCC::SignedLessThanOrEqual => x <= y,
        IntCC::SignedGreaterThan => x > y,
        IntCC::SignedGreaterThanOrEqual => x >= y,
        IntCC::UnsignedLessThan => (x as u64) < (y as u64),
        IntCC::UnsignedLessThanOrEqual => (x as u64) <= (y as u64),
        IntCC::UnsignedGreaterThan => (x as u64) > (y as u64),
        IntCC::UnsignedGreaterThanOrEqual => (x as u64) >= (y as u64),
    }
}

pub(crate) fn emit(pos: &mut FuncCursor, ty: Type, e: &Ins) -> Value {
    // Cheap folds for shapes stream/address exprs produce (e.g. `iv0=0`
    // turns `base + iv0*K` into `base + 0*K`).
    match e {
        Ins::Add(a, b) if ins_eval(pos.func, a) == Some(0) => return emit(pos, ty, b),
        Ins::Add(a, b) | Ins::Sub(a, b) if ins_eval(pos.func, b) == Some(0) => {
            return emit(pos, ty, a);
        }
        Ins::Mul(a, b) | Ins::And(a, b)
            if ins_eval(pos.func, a) == Some(0) || ins_eval(pos.func, b) == Some(0) =>
        {
            return emit(pos, ty, &Ins::K(0));
        }
        Ins::Mul(a, b) if ins_eval(pos.func, a) == Some(1) => return emit(pos, ty, b),
        Ins::Mul(a, b) | Ins::Div(a, b) if ins_eval(pos.func, b) == Some(1) => {
            return emit(pos, ty, a);
        }
        _ => {}
    }
    match e {
        Ins::Val(v) => {
            let v = *v;
            let vt = pos.func.dfg.value_type(v);
            if vt == ty {
                v
            } else if vt.bits() < ty.bits() {
                pos.ins().uextend(ty, v)
            } else {
                pos.ins().ireduce(ty, v)
            }
        }
        Ins::K(k) => {
            // `iconst` tops out at i64 (`NarrowInt`); wider types are built
            // by sign-extending an i64 constant.
            let v = pos.ins().iconst(types::I64, *k);
            if ty == types::I64 {
                v
            } else if ty.bits() > 64 {
                pos.ins().sextend(ty, v)
            } else {
                pos.ins().ireduce(ty, v)
            }
        }
        Ins::Add(a, b) => {
            let (a, b) = (emit(pos, ty, a), emit(pos, ty, b));
            pos.ins().iadd(a, b)
        }
        Ins::Sub(a, b) => {
            let (a, b) = (emit(pos, ty, a), emit(pos, ty, b));
            pos.ins().isub(a, b)
        }
        Ins::SatSub(a, b) => {
            // `usub_sat` is vector/narrow-int only in this version; emit
            // `a >= b ? a - b : 0` instead (same semantics, no trap).
            let (a, b) = (emit(pos, ty, a), emit(pos, ty, b));
            let d = pos.ins().isub(a, b);
            let c = pos.ins().icmp(IntCC::UnsignedLessThan, a, b);
            let z = pos.ins().iconst(ty, 0);
            pos.ins().select(c, z, d)
        }
        Ins::Mul(a, b) => {
            let (a, b) = (emit(pos, ty, a), emit(pos, ty, b));
            pos.ins().imul(a, b)
        }
        Ins::And(a, b) => {
            let (a, b) = (emit(pos, ty, a), emit(pos, ty, b));
            pos.ins().band(a, b)
        }
        Ins::Div(a, b) => {
            let (a, b) = (emit(pos, ty, a), emit(pos, ty, b));
            pos.ins().udiv(a, b)
        }
    }
}

/// The analysis result for one loop: how the store address advances (`iv`,
/// its positive `step`, the entry value, the per-element multiplier `k`),
/// the exit comparison, and which side of the `brif` stays in the loop.
pub(crate) struct Count {
    /// Header param index of the induction variable.
    pub(crate) iv: Value,
    pub(crate) iv0: Value,
    pub(crate) step: i64,
    /// Bound the exit test compares against (invariant value).
    pub(crate) bound: Value,
    /// Exit comparison normalized to "keep looping while `cc` holds".
    pub(crate) stay: IntCC,
    /// The store executes before the test (do-while): trip count can't be 0.
    pub(crate) post_tested: bool,
    /// The iv in the test is the latch value (`iv + step`), not the param.
    pub(crate) on_next: bool,
    /// Raw exit `icmp` (cc, resolved args) and whether the loop continues on
    /// `cond == true` — used to prove in-loop guards dead.
    pub(crate) cc: IntCC,
    pub(crate) x: Value,
    pub(crate) y: Value,
    pub(crate) stay_true: bool,
}

/// Per-param: constant step per back edge (`Some`) or effectively invariant
/// (`Inv`, re-passed unchanged on every latch).
#[derive(Clone, Copy)]
pub(crate) enum Param {
    Step(i64),
    Inv,
    Other,
}

pub(crate) fn param_kinds(func: &Function, info: &Info) -> Vec<Param> {
    let h = info.h;
    let params = func.dfg.block_params(h).to_vec();
    params
        .iter()
        .enumerate()
        .map(|(idx, &p)| {
            let mut same = true;
            let mut step: Option<i64> = Some(0);
            for e in &info.latches {
                let v = *edge_args(func, *e).get(idx).unwrap_or(&p);
                let v = func.dfg.resolve_aliases(v);
                if v == p {
                    continue;
                }
                same = false;
                let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
                    step = None;
                    continue;
                };
                let s = match func.dfg.insts[i] {
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args: [a, b],
                    } => {
                        if func.dfg.resolve_aliases(a) == p {
                            iconst(func, b)
                        } else if func.dfg.resolve_aliases(b) == p {
                            iconst(func, a)
                        } else {
                            None
                        }
                    }
                    InstructionData::Binary {
                        opcode: Opcode::Isub,
                        args: [a, b],
                    } if func.dfg.resolve_aliases(a) == p => {
                        iconst(func, b).and_then(|c| c.checked_neg())
                    }
                    _ => None,
                };
                step = match (step, s) {
                    (Some(0), Some(s)) => Some(s),
                    (Some(s0), Some(s)) if s0 == s => Some(s0),
                    _ => None,
                };
            }
            if same {
                Param::Inv
            } else {
                match step {
                    Some(s) if s != 0 => Param::Step(s),
                    _ => Param::Other,
                }
            }
        })
        .collect()
}

/// `v` resolves to header param `idx` or to `idx`'s back-edge value `param+step`.
pub(crate) fn iv_side(func: &Function, info: &Info, kinds: &[Param], v: Value) -> Option<(usize, bool)> {
    let v = func.dfg.resolve_aliases(v);
    let params = func.dfg.block_params(info.h).to_vec();
    for (i, &p) in params.iter().enumerate() {
        if !matches!(kinds[i], Param::Step(_)) {
            continue;
        }
        if v == p {
            return Some((i, false));
        }
        if let ValueDef::Result(inst, _) = func.dfg.value_def(v)
            && let InstructionData::Binary {
                opcode: Opcode::Iadd,
                args: [a, b],
            } = func.dfg.insts[inst]
        {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            if (a == p && iconst(func, b).is_some()) || (b == p && iconst(func, a).is_some()) {
                return Some((i, true));
            }
        }
    }
    None
}

/// Outside-the-loop view of `v`: invariant defs pass through; an effectively
/// invariant header param reads as its entry value.
pub(crate) fn outv(func: &Function, info: &Info, kinds: &[Param], v: Value) -> Option<Value> {
    let v = func.dfg.resolve_aliases(v);
    if let ValueDef::Param(b, i) = func.dfg.value_def(v)
        && b == info.h
    {
        return match kinds.get(i) {
            Some(Param::Inv) => info.entry_args.get(i).copied(),
            _ => None,
        };
    }
    if def_block(func, v).is_some_and(|b| info.body.contains(&b)) {
        return None;
    }
    Some(v)
}

/// Match the exit test: a `brif` on `icmp cc x y` where one side tracks an
/// iv param and the other is loop-invariant. Returns the count model.
pub(crate) fn count(
    func: &Function,
    dt: &DominatorTree,
    info: &Info,
    kinds: &[Param],
    exit: Edge,
    store: Inst,
) -> Option<Count> {
    let InstructionData::Brif { arg, .. } = func.dfg.insts[exit.inst] else {
        return None;
    };
    let c = func.dfg.resolve_aliases(arg);
    let ValueDef::Result(ci, _) = func.dfg.value_def(c) else {
        return None;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args: [x, y],
    } = func.dfg.insts[ci]
    else {
        return None;
    };
    // Which destination stays in the loop?
    let dests: Vec<Block> = func.dfg.insts[exit.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .iter()
        .map(|bc| bc.block(&func.dfg.value_lists))
        .collect();
    if dests.len() != 2 {
        return None;
    }
    let stay_in = |i: usize| info.body.contains(&dests[i]);
    let stay = match (stay_in(0), stay_in(1), exit.slot) {
        (true, false, 1) => cond,
        (false, true, 0) => cond.complement(),
        _ => return None,
    };
    let (x, y) = (
        func.dfg.resolve_aliases(x),
        func.dfg.resolve_aliases(y),
    );
    let ((idx, on_next), bound) = if let Some(t) = iv_side(func, info, kinds, x) {
        (t, outv(func, info, kinds, y)?)
    } else {
        (iv_side(func, info, kinds, y)?, outv(func, info, kinds, x)?)
    };
    let Param::Step(step) = kinds[idx] else {
        return None;
    };
    if step <= 0 {
        return None;
    }
    Some(Count {
        iv: func.dfg.block_params(info.h)[idx],
        iv0: info.entry_args[idx],
        step,
        bound,
        stay,
        post_tested: dt.dominates(store, exit.inst, &func.layout),
        on_next,
        cc: cond,
        x,
        y,
        stay_true: exit.slot == 1,
    })
}

/// `b` is a diverging dead end: it ends in `trap` and has no successors.
pub(crate) fn deadend(func: &Function, b: Block) -> bool {
    func.layout
        .last_inst(b)
        .is_some_and(|t| func.dfg.insts[t].opcode() == Opcode::Trap)
}

/// Exit edge `e` is never taken because its `brif` arg is a constant
/// selecting the in-loop destination (a folded-away interior test).
fn never_taken(func: &Function, e: Edge) -> bool {
    let InstructionData::Brif { arg, .. } = func.dfg.insts[e.inst] else {
        return false;
    };
    match iconst(func, func.dfg.resolve_aliases(arg)) {
        Some(k) => e.slot == usize::from(k != 0),
        None => false,
    }
}

/// `x` as `iv + k` for the counted iv: the iv itself (`k = 0`), or an
/// `iadd`/`isub` by a constant. `isub(iv, c)` reports `-c`.
fn iv_add_k(func: &Function, iv: Value, x: Value) -> Option<i64> {
    let x = func.dfg.resolve_aliases(x);
    if x == iv {
        return Some(0);
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(x) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } => {
            if func.dfg.resolve_aliases(a) == iv {
                iconst(func, b)
            } else if func.dfg.resolve_aliases(b) == iv {
                iconst(func, a)
            } else {
                None
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args: [a, b],
        } if func.dfg.resolve_aliases(a) == iv => {
            iconst(func, b).and_then(|k| k.checked_neg())
        }
        _ => None,
    }
}

/// Exit edge `e` is a provably-dead bounds-check-style guard: its target is a
/// deadend block reached through a `brif` on the same `icmp` as the loop's
/// stay test, going the failing way. Only sound pre-test (`!post_tested`):
/// in-loop execution then implies the stay condition already held.
pub(crate) fn guard_dead(func: &Function, cnt: &Count, e: Edge) -> bool {
    if !deadend(func, edge_dest(func, e)) {
        return false;
    }
    let InstructionData::Brif { arg, .. } = func.dfg.insts[e.inst] else {
        return false;
    };
    let c = func.dfg.resolve_aliases(arg);
    // A constant-folded guard (`brif 1, stay, cold`) never takes the edge a
    // zero would select, independent of the stay test or test order.
    if let Some(k) = iconst(func, c) {
        return e.slot == usize::from(k != 0);
    }
    // A descending-index bounds check `ult(bound-1-iv, bound)` never fires:
    // a unit-step stay condition keeps `iv <= bound-1`, so the index stays
    // in `[0, bound-1]` (no wrap) on every iteration. The index must read
    // the *current* iv — `iv+step` wraps on the last iteration.
    if let ValueDef::Result(ci, _) = func.dfg.value_def(c)
        && let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond: IntCC::UnsignedLessThan,
            args: [x, y],
        } = func.dfg.insts[ci]
        && e.slot == 1
        && func.dfg.resolve_aliases(y) == cnt.bound
        && cnt.step == 1
        && matches!(cnt.stay, IntCC::UnsignedLessThan | IntCC::NotEqual)
    {
        let xv = func.dfg.resolve_aliases(x);
        if let ValueDef::Result(xi, _) = func.dfg.value_def(xv)
            && let InstructionData::Binary {
                opcode: Opcode::Isub,
                args: [a, b],
            } = func.dfg.insts[xi]
            && func.dfg.resolve_aliases(b) == cnt.iv
            && let ValueDef::Result(ai, _) =
                func.dfg.value_def(func.dfg.resolve_aliases(a))
        {
            let km1 = match func.dfg.insts[ai] {
                InstructionData::Binary {
                    opcode: Opcode::Isub,
                    args: [t, k],
                } => {
                    func.dfg.resolve_aliases(t) == cnt.bound && iconst(func, k) == Some(1)
                }
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args: [t, k],
                } => {
                    func.dfg.resolve_aliases(t) == cnt.bound && iconst(func, k) == Some(-1)
                }
                _ => false,
            };
            if km1 {
                return true;
            }
        }
    }
    // `ult(iv - k, bound)` — an offset-index bounds check (e.g. `a[i-4]`).
    // The pre-tested stay condition keeps every in-body iv below `bound`;
    // when `iv0 >= k` is provable the subtraction can't wrap, so
    // `iv - k` stays in `[0, bound)` and the check never fires. (The
    // `iv + k` mirror is NOT dead — it can exceed `bound`.)
    if !cnt.post_tested
        && cnt.step > 0
        && matches!(
            cnt.stay,
            IntCC::UnsignedLessThan
                | IntCC::UnsignedLessThanOrEqual
                | IntCC::NotEqual
        )
        && let ValueDef::Result(ci, _) = func.dfg.value_def(c)
        && let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond: IntCC::UnsignedLessThan,
            args: [x, y],
        } = func.dfg.insts[ci]
        && e.slot == 1
        && func.dfg.resolve_aliases(y) == cnt.bound
        && let Some(k) = iv_add_k(func, cnt.iv, x)
        && k < 0
        && iconst(func, cnt.iv0).is_some_and(|v0| {
            v0.checked_add(k).is_some_and(|s| s >= 0)
        })
    {
        return true;
    }
    // An `icmp` guard is implied by the stay test only when the test runs
    // before the body each iteration.
    if cnt.post_tested {
        return false;
    }
    let ValueDef::Result(ci, _) = func.dfg.value_def(c) else {
        return false;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args: [x, y],
    } = func.dfg.insts[ci]
    else {
        return false;
    };
    if cond != cnt.cc
        || func.dfg.resolve_aliases(x) != cnt.x
        || func.dfg.resolve_aliases(y) != cnt.y
    {
        return false;
    }
    // The cold edge fires iff the in-body cond differs from the stay value.
    e.slot == usize::from(cnt.stay_true)
}

/// When a diverging guard can't be proven dead outright, `ult(iv, lim)`-style
/// guards (cold edge on the false side, iv compared at the same test point)
/// still admit the fast path under the runtime check `bound <=u lim`: the iv
/// rises monotonically and every stored index stays below `bound`, hence below
/// `lim`. Works for any bound relationship (e.g. rustc's `min` trip counts).
///
/// Affine index expressions generalize this: `ult(iv + k, lim)` needs
/// `bound + k <=u lim`, `ult(iv - k, lim)` additionally needs `iv0 >=u k`
/// so the offset can't wrap, and a descending `ult(c - iv, lim)` needs the
/// entry-side value `c - iv0 <u lim` plus `bound - 1 <=u c` so it can't
/// wrap mid-loop. Returns `false` when the guard isn't an affine `ult`
/// against an invariant limit; otherwise pushes the runtime predicates.
pub(crate) fn guard_pred(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    cnt: &Count,
    e: Edge,
    out: &mut Vec<Pred>,
) -> bool {
    if !deadend(func, edge_dest(func, e)) {
        return false;
    }
    let InstructionData::Brif { arg, .. } = func.dfg.insts[e.inst] else {
        return false;
    };
    let c = func.dfg.resolve_aliases(arg);
    let ValueDef::Result(ci, _) = func.dfg.value_def(c) else {
        return false;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args: [x, y],
    } = func.dfg.insts[ci]
    else {
        return false;
    };
    if cond != IntCC::UnsignedLessThan || e.slot != 1 {
        return false;
    }
    let Some(lim) = outv(func, info, kinds, y) else {
        return false;
    };
    let xr = func.dfg.resolve_aliases(x);
    if xr == cnt.x {
        out.push(Pred::Cmp(
            IntCC::UnsignedLessThanOrEqual,
            Ins::Val(cnt.bound),
            Ins::Val(lim),
        ));
        return true;
    }
    if cnt.step <= 0 {
        return false;
    }
    // `iv + k` / `iv - k` against an invariant limit.
    if let Some(k) = iv_add_k(func, cnt.iv, x) {
        if k == 0 {
            // The same index as the stay test, compared at a different
            // bound: `bound <=u lim`.
            out.push(Pred::Cmp(
                IntCC::UnsignedLessThanOrEqual,
                Ins::Val(cnt.bound),
                Ins::Val(lim),
            ));
        } else if k > 0 {
            // Largest checked index is `bound-1+k`; `lim - k` saturates to
            // keep the compare honest when `lim < k`.
            out.push(Pred::Cmp(
                IntCC::UnsignedLessThanOrEqual,
                Ins::Val(cnt.bound),
                Ins::SatSub(Box::new(Ins::Val(lim)), Box::new(Ins::K(k))),
            ));
            // A post-tested loop runs its body once with `iv0` before any
            // stay test — cover that check too.
            if cnt.post_tested {
                out.push(Pred::Cmp(
                    IntCC::UnsignedLessThan,
                    Ins::Add(Box::new(Ins::Val(cnt.iv0)), Box::new(Ins::K(k))),
                    Ins::Val(lim),
                ));
            }
        } else {
            // `iv - |k|`: largest is `bound-1-|k|`, so `satsub(bound,|k|)
            // <= lim` covers the range; the sub itself must not wrap, so
            // `iv >= |k|` on every iteration — i.e. `iv0 >= |k|` (step > 0).
            let Some(nk) = k.checked_neg() else {
                return false;
            };
            if iconst(func, cnt.iv0)
                .is_none_or(|v0| v0.checked_add(k).is_none_or(|s| s < 0))
            {
                out.push(Pred::Cmp(
                    IntCC::UnsignedGreaterThanOrEqual,
                    Ins::Val(cnt.iv0),
                    Ins::K(nk),
                ));
            }
            out.push(Pred::Cmp(
                IntCC::UnsignedLessThanOrEqual,
                Ins::SatSub(Box::new(Ins::Val(cnt.bound)), Box::new(Ins::K(nk))),
                Ins::Val(lim),
            ));
            // Same post-tested first-iteration cover as above; the iv0 >=
            // |k| pred already emitted (or statically true) rules out wrap.
            if cnt.post_tested {
                out.push(Pred::Cmp(
                    IntCC::UnsignedLessThan,
                    Ins::Sub(Box::new(Ins::Val(cnt.iv0)), Box::new(Ins::K(nk))),
                    Ins::Val(lim),
                ));
            }
        }
        return true;
    }
    // `c - iv` against an invariant limit (descending index, e.g.
    // `a[m - i]`): the largest index is `c - iv0`; no wrap needs every
    // in-body `iv <= c`, implied by `bound - 1 <=u c` (saturated).
    let ValueDef::Result(xi, _) = func.dfg.value_def(x) else {
        return false;
    };
    if let InstructionData::Binary {
        opcode: Opcode::Isub,
        args: [a, b],
    } = func.dfg.insts[xi]
        && func.dfg.resolve_aliases(b) == cnt.iv
        && let Some(cv) = outv(func, info, kinds, a)
    {
        out.push(Pred::Cmp(
            IntCC::UnsignedLessThanOrEqual,
            Ins::SatSub(Box::new(Ins::Val(cnt.bound)), Box::new(Ins::K(1))),
            Ins::Val(cv),
        ));
        out.push(Pred::Cmp(
            IntCC::UnsignedLessThan,
            Ins::Sub(Box::new(Ins::Val(cv)), Box::new(Ins::Val(cnt.iv0))),
            Ins::Val(lim),
        ));
        return true;
    }
    false
}

/// `base + iv0*K` as a plan expression.
pub(crate) fn start_expr(iv0: Value, base: Value, k: i64) -> Ins {
    Ins::Add(
        Box::new(Ins::Val(base)),
        Box::new(Ins::Mul(
            Box::new(Ins::Val(iv0)),
            Box::new(Ins::K(k)),
        )),
    )
}

/// Element count for an index induction (`iv` counts, address = base+iv*K).
/// `ne` needs `step == 1`; `ult` divides the saturated difference.
pub(crate) fn trips_idx(cnt: &Count) -> Option<(Ins, Vec<Pred>)> {
    let iv0 = || Ins::Val(cnt.iv0);
    let n = || Ins::Val(cnt.bound);
    match (cnt.stay, cnt.post_tested, cnt.on_next) {
        // `while iv < n`: count = ceil(sat(n - iv0) / step).
        (IntCC::UnsignedLessThan, false, false) => {
            let diff = Ins::SatSub(Box::new(n()), Box::new(iv0()));
            let count = if cnt.step == 1 {
                diff
            } else {
                Ins::Div(
                    Box::new(Ins::Add(
                        Box::new(diff),
                        Box::new(Ins::K(cnt.step - 1)),
                    )),
                    Box::new(Ins::K(cnt.step)),
                )
            };
            Some((count, Vec::new()))
        }
        // do-while `iv+step < n`: count = ceil((n - iv0)/step), guarded
        // `iv0 < n` (the body runs at least once even when it isn't).
        (IntCC::UnsignedLessThan, true, true) => {
            let diff = Ins::Sub(Box::new(n()), Box::new(iv0()));
            let count = if cnt.step == 1 {
                diff
            } else {
                Ins::Div(
                    Box::new(Ins::Add(
                        Box::new(diff),
                        Box::new(Ins::K(cnt.step - 1)),
                    )),
                    Box::new(Ins::K(cnt.step)),
                )
            };
            Some((
                count,
                vec![Pred::Cmp(
                    IntCC::UnsignedLessThan,
                    Ins::Val(cnt.iv0),
                    Ins::Val(cnt.bound),
                )],
            ))
        }
        // `while iv != n`: count = n - iv0, guarded `iv0 <= n` (pre).
        (IntCC::NotEqual, false, false) if cnt.step == 1 => Some((
            Ins::Sub(Box::new(n()), Box::new(iv0())),
            vec![Pred::Cmp(
                IntCC::UnsignedLessThanOrEqual,
                Ins::Val(cnt.iv0),
                Ins::Val(cnt.bound),
            )],
        )),
        // do-while `iv+1 != n`: count = n - iv0, guarded `iv0 < n`.
        (IntCC::NotEqual, true, true) if cnt.step == 1 => Some((
            Ins::Sub(Box::new(n()), Box::new(iv0())),
            vec![Pred::Cmp(
                IntCC::UnsignedLessThan,
                Ins::Val(cnt.iv0),
                Ins::Val(cnt.bound),
            )],
        )),
        // do-while `iv != n` (test on current): count = n - iv0 + 1.
        (IntCC::NotEqual, true, false) if cnt.step == 1 => Some((
            Ins::Add(
                Box::new(Ins::Sub(Box::new(n()), Box::new(iv0()))),
                Box::new(Ins::K(1)),
            ),
            vec![Pred::Cmp(
                IntCC::UnsignedLessThanOrEqual,
                Ins::Val(cnt.iv0),
                Ins::Val(cnt.bound),
            )],
        )),
        _ => None,
    }
}

/// Pointer-form count for `p` stepping `size` bytes; len is already bytes.
pub(crate) fn trips_ptr(cnt: &Count, size: i64) -> Option<(Ins, Vec<Pred>)> {
    if size <= 0 || (size & (size - 1)) != 0 {
        return None;
    }
    let p0 = || Ins::Val(cnt.iv0);
    let end = || Ins::Val(cnt.bound);
    let diff = || Ins::Sub(Box::new(end()), Box::new(p0()));
    match (cnt.stay, cnt.post_tested, cnt.on_next) {
        // `while p < end`: len = round_up(sat(end - p0), size).
        (IntCC::UnsignedLessThan, false, false) => Some((
            Ins::And(
                Box::new(Ins::Add(
                    Box::new(Ins::SatSub(Box::new(end()), Box::new(p0()))),
                    Box::new(Ins::K(size - 1)),
                )),
                Box::new(Ins::K(!(size - 1))),
            ),
            Vec::new(),
        )),
        // `while p != end`: len = end - p0, ordered and divisible.
        (IntCC::NotEqual, false, false) => Some((
            diff(),
            vec![
                Pred::Cmp(
                    IntCC::UnsignedGreaterThanOrEqual,
                    Ins::Val(cnt.bound),
                    Ins::Val(cnt.iv0),
                ),
                Pred::Aligned(diff(), size),
            ],
        )),
        // do-while `p+size != end`: same, but p0 == end spins forever.
        (IntCC::NotEqual, true, true) => Some((
            diff(),
            vec![
                Pred::Cmp(
                    IntCC::UnsignedGreaterThan,
                    Ins::Val(cnt.bound),
                    Ins::Val(cnt.iv0),
                ),
                Pred::Aligned(diff(), size),
            ],
        )),
        // do-while `p != end` (current ptr): one extra store after reaching it.
        (IntCC::NotEqual, true, false) => Some((
            Ins::Add(Box::new(diff()), Box::new(Ins::K(size))),
            vec![
                Pred::Cmp(
                    IntCC::UnsignedGreaterThanOrEqual,
                    Ins::Val(cnt.bound),
                    Ins::Val(cnt.iv0),
                ),
                Pred::Aligned(diff(), size),
            ],
        )),
        _ => None,
    }
}

/// The affine destination `dst0` + byte `len` + guards for one loop, or None.
pub(crate) fn plan(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    noalias: &FxHashSet<Value>,
) -> Option<Plan> {
    let info = gather(func, cfg, dt, la, lp)?;
    let kinds = param_kinds(func, &info);
    // Collect the store, any load, and reject everything else that is not a
    // speculatable side-effect-free instruction or a plain branch. Trap-
    // terminating dead ends (bounds-check panics) are skipped here and must
    // still be proven unreachable via `guard_dead` below.
    let mut store: Option<Inst> = None;
    let mut load: Option<Inst> = None;
    for &b in &info.body {
        if deadend(func, b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            let op = func.dfg.insts[i].opcode();
            if op.is_terminator() {
                if !matches!(op, Opcode::Jump | Opcode::Brif) {
                    return None;
                }
                continue;
            }
            if store_parts(func, i).is_some() {
                if store.is_some() {
                    return None;
                }
                store = Some(i);
                continue;
            }
            if matches!(func.dfg.insts[i], InstructionData::Load { opcode: Opcode::Load, .. })
                && loadfwd::notrap(func, i)
            {
                if load.is_some() {
                    return None;
                }
                load = Some(i);
                continue;
            }
            if func.dfg.inst_results(i).is_empty()
                || op.can_trap()
                || op.can_load()
                || op.can_store()
                || op.is_call()
                || op.other_side_effects()
            {
                return None;
            }
        }
    }
    let Some(s) = store else {
        why!("no unique store: {:?}", la.loop_header(lp));
    };
    let Some((val, addr, size)) = store_parts(func, s) else {
        why!("store shape: {:?}", la.loop_header(lp));
    };
    // The store must run on every iteration: it must dominate every latch,
    // otherwise a conditionally-executed store becomes a full-range
    // memset/memcpy. A load feeding the store transitively dominates the
    // latches too; a load that does not is unobservable once the loop is
    // replaced.
    for &l in &info.latches {
        if !dt.dominates(s, l.inst, &func.layout) {
            why!("conditional store: {:?}", la.loop_header(lp));
        }
    }
    // Find the exit edge that gives a usable count; every other exit must be
    // a provably-dead guard or yield a runtime bound check for the fast path.
    let (exit, cnt, extra) = info
        .exits
        .iter()
        .find_map(|&e| {
            let c = count(func, dt, &info, &kinds, e, s)?;
            let mut ps = Vec::new();
            for &e2 in &info.exits {
                if e2 == e {
                    continue;
                }
                if !guard_dead(func, &c, e2)
                    && !guard_pred(func, &info, &kinds, &c, e2, &mut ps)
                {
                    return None;
                }
            }
            Some((e, c, ps))
        })
        .or_else(|| {
            if std::env::var_os("PLIRON_IDIOM_DEBUG").is_some() {
                eprintln!("idiom bail: count/guards {:?}", la.loop_header(lp));
            }
            None
        })?;
    // An address affine to the loop: a header param stepping `size` bytes, or
    // `base + iv*K` on the counted iv. Returns the expression plus the
    // outside-the-loop base value (for alias roots).
    let affine = |func: &Function, a: Value| -> Option<(Ins, Value)> {
        let a = func.dfg.resolve_aliases(a);
        let params = func.dfg.block_params(info.h).to_vec();
        for (j, &p) in params.iter().enumerate() {
            if a == p {
                return match kinds[j] {
                    Param::Step(s) if s == size => {
                        Some((Ins::Val(info.entry_args[j]), info.entry_args[j]))
                    }
                    _ => None,
                };
            }
        }
        let (base, k) = scaled_addr(func, cnt.iv, a)?;
        if k * cnt.step != size {
            return None;
        }
        let base = outv(func, &info, &kinds, base)?;
        Some((start_expr(cnt.iv0, base, k), base))
    };
    let Some((dst0, dbase)) = affine(func, addr) else {
        why!("dst affine: {:?}", la.loop_header(lp));
    };
    // Byte length comes from the exit test: when the counted iv is itself a
    // byte-stepping pointer the difference is already bytes.
    let iv_is_addr = func.dfg.resolve_aliases(addr) == cnt.iv
        || load.is_some_and(|l| {
            matches!(func.dfg.insts[l], InstructionData::Load { arg, .. } if func.dfg.resolve_aliases(arg) == cnt.iv)
        });
    let (len, mut preds) = if iv_is_addr {
        let Some(t) = trips_ptr(&cnt, size) else {
            why!("trips_ptr: {:?}", la.loop_header(lp));
        };
        t
    } else {
        let Some((count, preds)) = trips_idx(&cnt) else {
            why!("trips_idx: {:?}", la.loop_header(lp));
        };
        (Ins::Mul(Box::new(count), Box::new(Ins::K(size))), preds)
    };
    preds.extend(extra);
    // Exit block arguments must all be loop-invariant.
    let exit_args: Option<Vec<Value>> = edge_args(func, exit)
        .iter()
        .map(|&v| outv(func, &info, &kinds, v))
        .collect();
    let Some(exit_args) = exit_args else {
        why!("exit args: {:?}", la.loop_header(lp));
    };
    // No value defined in the loop may be used outside it, except in the
    // proven-dead guard blocks (their only incoming edges can't execute).
    let dead: FxHashSet<Block> = info
        .exits
        .iter()
        .filter(|&&e| e != exit)
        .map(|&e| edge_dest(func, e))
        .collect();
    let defs: FxHashSet<Value> = info
        .body
        .iter()
        .flat_map(|&b| {
            func.dfg
                .block_params(b)
                .iter()
                .copied()
                .chain(
                    func.layout
                        .block_insts(b)
                        .flat_map(|i| func.dfg.inst_results(i).iter().copied()),
                )
                .collect::<Vec<Value>>()
        })
        .map(|v| func.dfg.resolve_aliases(v))
        .collect();
    for b in func.layout.blocks() {
        if info.body.contains(&b) || dead.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            if func
                .dfg
                .inst_values(i)
                .any(|v| defs.contains(&func.dfg.resolve_aliases(v)))
            {
                why!("loop def escapes: {:?}", la.loop_header(lp));
            }
            let t = func.layout.last_inst(b).unwrap_or(i);
            for bc in func.dfg.insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                if bc
                    .args(&func.dfg.value_lists)
                    .any(|a| matches!(a, BlockArg::Value(v) if defs.contains(&func.dfg.resolve_aliases(v))))
                {
                    why!("loop def in edge arg: {:?}", la.loop_header(lp));
                }
            }
        }
    }
    let fill;
    let src;
    if let Some(b) = fill_byte(func, val) {
        src = None;
        fill = Some(match b {
            Some(v) => {
                // The fill byte must be materializable before the loop.
                Fill::V(outv(func, &info, &kinds, v)?)
            }
            None => Fill::C(iconst(func, val)? & 0xff),
        });
    } else {
        // Copy form: one `notrap` load of the same element size at the same
        // stride, stored value is the loaded value, roots provably disjoint.
        fill = None;
        let l = load?;
        let InstructionData::Load {
            arg: laddr, offset, ..
        } = func.dfg.insts[l]
        else {
            return None;
        };
        if i32::from(offset) != 0
            || !loadfwd::notrap(func, l)
            || func.dfg.resolve_aliases(func.dfg.first_result(l)) != val
            || func.dfg.value_type(func.dfg.first_result(l)).bytes() as i64 != size
        {
            return None;
        }
        let Some((s0, sbase)) = affine(func, laddr) else {
            why!("src affine: {:?}", la.loop_header(lp));
        };
        // Disjointness on the entry-side base values: stack slots differ, or
        // either base resolves to a rustc-`noalias` param (its pointee
        // aliases nothing else accessed in the function). Otherwise version
        // the loop under a runtime overlap check (LLVM-style alias
        // predicates) — `isolated` itself is stricter than needed here since
        // it also guards load-forwarding, which drops roots whose pointers
        // are offset by dynamic amounts.
        let (sr, _) = loadfwd::root(func, sbase);
        let (dr, _) = loadfwd::root(func, dbase);
        let na = |r: Root| matches!(r, Root::V(v) if noalias.contains(&v));
        let proven = match (sr, dr) {
            (Root::S(a), Root::S(b)) => a != b,
            // A lone `noalias` is no proof: the other side could be a pointer
            // derived from it. Two distinct `noalias` params can't be based
            // on each other, and nothing derived can address a stack slot.
            _ => {
                sr != dr
                    && ((na(sr) && (na(dr) || matches!(dr, Root::S(_))))
                        || (na(dr) && matches!(sr, Root::S(_))))
            }
        };
        if !proven {
            preds.push(Pred::Disjoint);
        }
        src = Some(s0);
    }
    Some(Plan {
        entry: info.entry,
        h: info.h,
        entry_args: info.entry_args,
        exit_dest: edge_dest(func, exit),
        exit_args,
        preds,
        dst: dst0,
        len,
        src,
        fill,
    })
}

/// `memset`/`memcpy` as a `LibCall` import of `func`.
pub(crate) fn libcall(
    func: &mut Function,
    tcfg: cranelift_codegen::isa::TargetFrontendConfig,
    lc: LibCall,
) -> FuncRef {
    let pt = tcfg.pointer_type();
    let mut sig = Signature::new(tcfg.default_call_conv);
    let argt = match lc {
        LibCall::Memset => vec![pt, types::I32, pt],
        _ => vec![pt, pt, pt],
    };
    sig.params = argt.into_iter().map(AbiParam::new).collect();
    sig.returns.push(AbiParam::new(pt));
    let sr = func.import_signature(sig);
    func.import_function(ExtFuncData {
        name: ExternalName::LibCall(lc),
        signature: sr,
        colocated: false,
        patchable: false,
    })
}

/// Rewrite `p`: route the entry edge through a check block running `preds`,
/// emit the libcall on success and keep the loop as the fallback path.
pub(crate) fn apply(func: &mut Function, p: &Plan, tcfg: cranelift_codegen::isa::TargetFrontendConfig) {
    let pblock = func.layout.inst_block(p.entry.inst).unwrap();
    let nb = func.dfg.make_block();
    func.layout.insert_block_after(nb, pblock);
    // Redirect the entry edge to `nb`.
    {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[p.entry.inst].branch_destination_mut(
            &mut dfg.jump_tables,
            &mut dfg.exception_tables,
        )[p.entry.slot];
        *bc = BlockCall::new(nb, core::iter::empty(), &mut dfg.value_lists);
    }
    let pty = tcfg.pointer_type();
    let mut pos = FuncCursor::new(func).at_bottom(nb);
    let emit_call = |pos: &mut FuncCursor| {
        let dst = emit(pos, pty, &p.dst);
        let len = emit(pos, pty, &p.len);
        let exit_args: Vec<BlockArg> =
            p.exit_args.iter().map(|&v| BlockArg::Value(v)).collect();
        match (p.fill, &p.src) {
            (Some(fill), None) => {
                let f = libcall(pos.func, tcfg, LibCall::Memset);
                let byte = match fill {
                    Fill::C(c) => pos.ins().iconst(types::I32, c),
                    Fill::V(v) => {
                        if pos.func.dfg.value_type(v) == types::I32 {
                            v
                        } else {
                            pos.ins().uextend(types::I32, v)
                        }
                    }
                };
                pos.ins().call(f, &[dst, byte, len]);
            }
            (None, Some(src)) => {
                let f = libcall(pos.func, tcfg, LibCall::Memcpy);
                let src = emit(pos, pty, src);
                pos.ins().call(f, &[dst, src, len]);
            }
            _ => unreachable!(),
        }
        pos.ins().jump(p.exit_dest, &exit_args);
    };
    if p.preds.is_empty() {
        emit_call(&mut pos);
        return;
    }
    let mb = pos.func.dfg.make_block();
    pos.func.layout.insert_block_after(mb, nb);
    // Emit guards: all must hold to take the fast path.
    let mut ok: Option<Value> = None;
    for pr in &p.preds {
        let c = match pr {
            Pred::Cmp(cc, a, b) => {
                let (a, b) = (emit(&mut pos, pty, a), emit(&mut pos, pty, b));
                pos.ins().icmp(*cc, a, b)
            }
            Pred::Aligned(a, size) => {
                let m = emit(&mut pos, pty, a);
                let k = pos.ins().iconst(pty, size - 1);
                let r = pos.ins().band(m, k);
                let z = pos.ins().iconst(pty, 0);
                pos.ins().icmp(IntCC::Equal, r, z)
            }
            Pred::Disjoint => {
                let dlo = emit(&mut pos, pty, &p.dst);
                let l1 = emit(&mut pos, pty, &p.len);
                let dhi = pos.ins().iadd(dlo, l1);
                let slo = emit(&mut pos, pty, p.src.as_ref().unwrap());
                let l2 = emit(&mut pos, pty, &p.len);
                let shi = pos.ins().iadd(slo, l2);
                // Neither interval may wrap, then one must end before the
                // other begins.
                let c1 = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, dlo, dhi);
                let c2 = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, slo, shi);
                let a = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, dhi, slo);
                let b = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, shi, dlo);
                let ab = pos.ins().bor(a, b);
                let nw = pos.ins().band(c1, c2);
                pos.ins().band(nw, ab)
            }
        };
        ok = Some(match ok {
            None => c,
            Some(o) => pos.ins().band(o, c),
        });
    }
    let ok = ok.unwrap();
    let h_args: Vec<BlockArg> = p
        .entry_args
        .iter()
        .map(|&v| BlockArg::Value(v))
        .collect();
    pos.ins().brif(ok, mb, &[], p.h, &h_args);
    let mut pos = FuncCursor::new(func).at_bottom(mb);
    emit_call(&mut pos);
}

/// A counted loop whose body is pure computation: every inst is removable
/// (no memory, calls, traps, or other side effects) and every exit is
/// either the provable count exit or an already-dead guard edge. Skipping
/// the loop is then just computing the exit values from the trip count —
/// route the entry through a check block that verifies the trips' preds
/// and jumps to the exit with them, else falls back to the loop.
struct DeadPlan {
    entry: Edge,
    h: Block,
    entry_args: Vec<Value>,
    exit_dest: Block,
    exit_args: Vec<Ins>,
    /// Loop-defined values read by `exit_dest` directly (SSA uses rather
    /// than edge args): each becomes an appended `exit_dest` param that
    /// loop-side edges bind to the original value and the fast edge binds
    /// to the closed form. This is how a licm-promoted loop's exit store
    /// (`store acc` just past the loop) survives deletion.
    escapes: Vec<(Value, Ins)>,
    /// Same-address RMW chains (`*p += K` per iteration) the loop performs:
    /// the fast path emits one load, the closed form, one store per loc.
    /// Emitted only when `iters` provably ≥ 1 (post-tested counts).
    iters: Ins,
    rmw: Vec<RmwLoc>,
    preds: Vec<Pred>,
}

/// How a stored value relates to the location's current contents.
pub(crate) enum RmwDelta {
    /// `st(ld)` — contents unchanged.
    Keep,
    /// `st(ld + d)` or `st(d + ld)`: final = `init + d * iters`.
    Add(Ins),
    /// `st(ld - d)`: final = `init - d * iters`.
    Sub(Ins),
    /// `st(v)`, `v` invariant: final = `v`.
    Set(Ins),
}

/// One invariant-address location the loop RMWs every iteration.
struct RmwLoc {
    ty: Type,
    /// Address root: an emitted expression, or a stack slot.
    addr: RmwAddr,
    offset: i32,
    load_flags: MemFlagsData,
    store_flags: MemFlagsData,
    delta: RmwDelta,
}

enum RmwAddr {
    V(Ins),
    S(StackSlot),
}

/// Exit-edge arg `v` as an `Ins` over the entry args and `itb` — the trip
/// count less one for post-tested loops, since a header param `i` reads
/// `entry_i + itb*step_i` in the exiting iteration (pre-tested exits
/// evaluate params after the last update, so `itb` is `iters` there).
/// Anything else must be a loop-invariant value or foldable arithmetic.
fn arg_ins(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    itb: &Ins,
    fwd: &FxHashMap<Value, Ins>,
    v: Value,
    depth: u32,
) -> Option<Ins> {
    if depth > 8 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    if let Some(ins) = fwd.get(&v) {
        return Some(ins.clone());
    }
    if let ValueDef::Param(b, i) = func.dfg.value_def(v) {
        return if b == info.h {
            match kinds[i] {
                Param::Inv => Some(Ins::Val(info.entry_args[i])),
                Param::Step(s) => Some(Ins::Add(
                    Box::new(Ins::Val(info.entry_args[i])),
                    Box::new(Ins::Mul(Box::new(itb.clone()), Box::new(Ins::K(s)))),
                )),
                Param::Other => None,
            }
        } else {
            Some(Ins::Val(v))
        };
    }
    if let Some(k) = iconst(func, v) {
        return Some(Ins::K(k));
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    if !func
        .layout
        .inst_block(i)
        .is_some_and(|b| info.body.contains(&b))
    {
        return Some(Ins::Val(v));
    }
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return None;
    };
    let (a, b) = (
        arg_ins(func, info, kinds, itb, fwd, args[0], depth + 1)?,
        arg_ins(func, info, kinds, itb, fwd, args[1], depth + 1)?,
    );
    Some(match opcode {
        Opcode::Iadd => Ins::Add(Box::new(a), Box::new(b)),
        Opcode::Isub => Ins::Sub(Box::new(a), Box::new(b)),
        Opcode::Imul => Ins::Mul(Box::new(a), Box::new(b)),
        Opcode::Band => Ins::And(Box::new(a), Box::new(b)),
        _ => return None,
    })
}

/// Exit edge `e` is never taken when its `brif` condition is loop-invariant
/// and the fast path requires the value that keeps control in the loop.
/// This is a one-branch slice of loop unswitching: an invariant guard such
/// as rustc's `0 < len` bounds check is the same test on every iteration,
/// so requiring it once up front makes the edge provably dead.
fn inv_guard_pred(func: &Function, info: &Info, kinds: &[Param], e: Edge) -> Option<Pred> {
    let InstructionData::Brif { arg, .. } = func.dfg.insts[e.inst] else {
        return None;
    };
    let c = outv(func, info, kinds, func.dfg.resolve_aliases(arg))?;
    // `brif c, then, else`: the exit edge fires on `slot==0` iff `c != 0`,
    // on `slot==1` iff `c == 0`. Staying in the loop needs the complement.
    let cc = if e.slot == 1 {
        IntCC::NotEqual
    } else {
        IntCC::Equal
    };
    Some(Pred::Cmp(cc, Ins::Val(c), Ins::K(0)))
}

/// Classify `loads`/`stores` as per-location same-address RMW chains:
/// every access touches an invariant address, each location has at most
/// one store, byte ranges on a shared root don't overlap, distinct roots
/// are only stack slots (a param root may alias another param's pointee),
/// and each store's value is `ld`, `ld ± inv`, or an invariant. Returns
/// None when the shape doesn't fit — the loop stays undeleted.
fn rmw_locs(
    func: &Function,
    dt: &DominatorTree,
    info: &Info,
    kinds: &[Param],
    loads: &[Inst],
    stores: &[Inst],
) -> Option<Vec<RmwLoc>> {
    // (root, byte offset) -> (ty, byte width, load insts, store inst)
    let mut groups: FxHashMap<(Root, i64), (Type, i64, Vec<Inst>, Option<Inst>)> =
        FxHashMap::default();
    for &i in loads {
        let InstructionData::Load { arg, offset, .. } = func.dfg.insts[i] else {
            return None;
        };
        let ty = func.dfg.value_type(func.dfg.first_result(i));
        let (r, o) = loadfwd::root(func, arg);
        let o = o.wrapping_add(i64::from(i32::from(offset)));
        let e = groups
            .entry((r, o))
            .or_insert((ty, i64::from(ty.bytes()), Vec::new(), None));
        if e.0 != ty {
            return None;
        }
        e.2.push(i);
    }
    for &i in stores {
        let InstructionData::Store { args, offset, .. } = func.dfg.insts[i] else {
            return None;
        };
        let ty = func.dfg.value_type(func.dfg.resolve_aliases(args[0]));
        let (r, o) = loadfwd::root(func, args[1]);
        let o = o.wrapping_add(i64::from(i32::from(offset)));
        let e = groups
            .entry((r, o))
            .or_insert((ty, i64::from(ty.bytes()), Vec::new(), None));
        if e.0 != ty || e.3.is_some() {
            return None;
        }
        e.1 = e.1.max(i64::from(ty.bytes()));
        e.3 = Some(i);
    }
    // At most one non-stack root, and its locations must not overlap.
    let mut vroot: Option<(Root, Vec<(i64, i64)>)> = None;
    for (&(r, o), &(_, w, _, _)) in &groups {
        if let Root::S(_) = r {
            continue;
        }
        match &mut vroot {
            None => vroot = Some((r, vec![(o, w)])),
            Some((vr, ranges)) if *vr == r => {
                for &(o2, w2) in ranges.iter() {
                    if o < o2 + w2 && o2 < o + w {
                        return None;
                    }
                }
                ranges.push((o, w));
            }
            Some(_) => return None,
        }
    }
    let mut out = Vec::new();
    for (&(r, o), &(ty, _, ref ls, st)) in &groups {
        // Invariant address: stack slots qualify; a value root must be
        // defined outside the loop.
        if let Root::V(v) = r
            && def_block(func, v).is_some_and(|b| info.body.contains(&b))
        {
            return None;
        }
        let Some(st) = st else {
            // Loads without a matching store have no effect to preserve:
            // they can be dropped with the loop as long as their results
            // don't escape (checked by the caller's escape analysis, which
            // can't express a `load` and will bail).
            continue;
        };
        // The store runs on every iteration.
        if !info
            .latches
            .iter()
            .all(|&l| dt.dominates(st, l.inst, &func.layout))
        {
            return None;
        }
        // Each load reads the pre-store value.
        if !ls.iter().all(|&l| dt.dominates(l, st, &func.layout)) {
            return None;
        }
        let sval = func.dfg.resolve_aliases(match func.dfg.insts[st] {
            InstructionData::Store { args, .. } => args[0],
            _ => return None,
        });
        let is_ld = |v: Value| {
            func.dfg
                .value_def(func.dfg.resolve_aliases(v))
                .inst()
                .is_some_and(|d| ls.contains(&d))
        };
        let delta = if is_ld(sval) {
            RmwDelta::Keep
        } else {
            match func.dfg.value_def(sval) {
                ValueDef::Result(i, _) => match func.dfg.insts[i] {
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args: [a, b],
                    } => {
                        let d = if is_ld(a) {
                            b
                        } else if is_ld(b) {
                            a
                        } else {
                            return None;
                        };
                        RmwDelta::Add(Ins::Val(outv(func, info, kinds, d)?))
                    }
                    InstructionData::Binary {
                        opcode: Opcode::Isub,
                        args: [a, b],
                    } if is_ld(a) => {
                        RmwDelta::Sub(Ins::Val(outv(func, info, kinds, b)?))
                    }
                    _ => return None,
                },
                _ => {
                    if let Some(k) = iconst(func, sval) {
                        RmwDelta::Set(Ins::K(k))
                    } else {
                        let v = outv(func, info, kinds, sval)?;
                        RmwDelta::Set(Ins::Val(v))
                    }
                }
            }
        };
        let (load_flags, store_flags) = (
            ls.first()
                .and_then(|&l| func.dfg.insts[l].memflags_data(&func.dfg))
                .unwrap_or_else(MemFlagsData::new),
            match func.dfg.insts[st].memflags_data(&func.dfg) {
                Some(f) => f,
                None => return None,
            },
        );
        // The peeled root carries the address; `o` already folds any `+k`
        // in the address expression plus the inst offset.
        let addr = match r {
            Root::V(v) => RmwAddr::V(Ins::Val(outv(func, info, kinds, v)?)),
            Root::S(ss) => RmwAddr::S(ss),
        };
        out.push(RmwLoc {
            ty,
            addr,
            offset: i32::try_from(o).ok()?,
            load_flags,
            store_flags,
            delta,
        });
    }
    Some(out)
}

/// Plan the dead-loop rewrite for `lp`, or None if the body has effects,
/// the count isn't provable, or an exit edge arg can't be recomputed.
fn plan_dead(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
) -> Option<DeadPlan> {
    let Some(info) = gather(func, cfg, dt, la, lp) else {
        why!("dead: gather {:?}", la.loop_header(lp));
    };
    let kinds = param_kinds(func, &info);
    if kinds.iter().any(|k| matches!(k, Param::Other)) {
        why!("dead: non-affine param {:?}", info.h);
    }
    let mut loads = Vec::new();
    let mut stores = Vec::new();
    for &b in &info.body {
        if deadend(func, b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            let op = func.dfg.insts[i].opcode();
            if op.is_terminator() {
                match op {
                    Opcode::Jump => {}
                    Opcode::Brif if info.exits.iter().any(|e| e.inst == i) => {}
                    _ => why!("dead: term {:?} {b:?}", op),
                }
                continue;
            }
            if op.can_load() || op.can_store() {
                match func.dfg.insts[i] {
                    InstructionData::Load {
                        opcode: Opcode::Load,
                        ..
                    } => loads.push(i),
                    InstructionData::Store {
                        opcode: Opcode::Store,
                        ..
                    } => stores.push(i),
                    _ => why!("dead: mem inst {:?} {b:?}", op),
                }
                continue;
            }
            if op.can_trap() || op.is_call() || op.other_side_effects() {
                why!("dead: inst {:?} {b:?}", op);
            }
        }
    }
    // Memory work confined to disjoint invariant-address `ld`/`st` pairs
    // folds into a same-address RMW chain closed-form on the fast path.
    let rmw = if loads.is_empty() && stores.is_empty() {
        Vec::new()
    } else {
        let Some(r) = rmw_locs(func, dt, &info, &kinds, &loads, &stores) else {
            why!("dead: rmw shape {:?}", info.h);
        };
        r
    };
    'exits: for &e in &info.exits {
        // The iv update inst stands in for `count`'s `store` argument: its
        // dominance over the exit is exactly "the body ran before the
        // test", which is what `post_tested` means for a pure counter.
        let upd = (|| {
            let InstructionData::Brif { arg, .. } = func.dfg.insts[e.inst] else {
                return None;
            };
            let c = func.dfg.resolve_aliases(arg);
            let ValueDef::Result(ci, _) = func.dfg.value_def(c) else {
                return None;
            };
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                args: [x, y],
                ..
            } = func.dfg.insts[ci]
            else {
                return None;
            };
            let (x, y) = (
                func.dfg.resolve_aliases(x),
                func.dfg.resolve_aliases(y),
            );
            let (idx, on_next) = iv_side(func, &info, &kinds, x)
                .or_else(|| iv_side(func, &info, &kinds, y))?;
            let upd = info.latches.iter().find_map(|&l| {
                let a = func.dfg.resolve_aliases(*edge_args(func, l).get(idx)?);
                match func.dfg.value_def(a) {
                    ValueDef::Result(i, _) => Some(i),
                    _ => None,
                }
            })?;
            // When the test reads the next value (`iv+step < n`), it must
            // be the very value this brif's latch edge passes back —
            // `iv_side` accepts any `iv+k`, and a different `k` would
            // miscount the closed-form trip count.
            if on_next {
                let side = if iv_side(func, &info, &kinds, x).is_some() {
                    x
                } else {
                    y
                };
                let l = info.latches.iter().find(|l| l.inst == e.inst)?;
                let a = func.dfg.resolve_aliases(*edge_args(func, *l).get(idx)?);
                (a == side).then_some(upd)
            } else {
                Some(upd)
            }
        })();
        let Some(cnt) = upd.and_then(|s| count(func, dt, &info, &kinds, e, s)) else {
            if std::env::var_os("PLIRON_IDIOM_DEBUG").is_some() {
                eprintln!("dead: no count {:?} exit {:?}", info.h, e.inst);
            }
            continue;
        };
        // A store folded into the RMW form only runs when the loop body
        // executes at least once — post-tested counts guarantee that.
        if !rmw.is_empty() && !cnt.post_tested {
            continue;
        }
        let mut inv_preds = Vec::new();
        if !info.exits.iter().all(|&e2| {
            e2 == e
                || never_taken(func, e2)
                || guard_dead(func, &cnt, e2)
                || inv_guard_pred(func, &info, &kinds, e2)
                    .is_some_and(|p| {
                        inv_preds.push(p);
                        true
                    })
        }) {
            continue;
        }
        // Trip count: `trips_idx` already divides by the step for `ult`;
        // the pointer form returns a byte distance, scaled back to
        // iterations by `step`.
        let (iters, preds) = if let Some(t) = trips_idx(&cnt) {
            t
        } else {
            let Some((len, ps)) = trips_ptr(&cnt, cnt.step) else {
                why!("dead: trips {:?}", info.h);
            };
            (Ins::Div(Box::new(len), Box::new(Ins::K(cnt.step))), ps)
        };
        let itb = if cnt.post_tested {
            Ins::Sub(Box::new(iters.clone()), Box::new(Ins::K(1)))
        } else {
            iters.clone()
        };
        // Fold statically-known preds (e.g. `iv0 < bound` with both
        // constant); a statically-false one means the fast path can never
        // run, so skip the transform entirely.
        let raw_preds: Vec<Pred> = preds.into_iter().chain(inv_preds).collect();
        let mut preds: Vec<Pred> = Vec::with_capacity(raw_preds.len());
        for pr in raw_preds {
            let known = match &pr {
                Pred::Cmp(cc, a, b) => ins_eval(func, a)
                    .zip(ins_eval(func, b))
                    .map(|(x, y)| cmp_cc(*cc, x, y)),
                Pred::Aligned(a, size) => {
                    ins_eval(func, a).map(|x| *size > 0 && x % size == 0)
                }
                Pred::Disjoint => None,
            };
            match known {
                Some(true) => {}
                Some(false) => continue 'exits,
                None => preds.push(pr),
            }
        }
        // Resolve the exit edge's args, then walk trivial forwarder blocks
        // (a lone `jump next(args)`): their bodies may read the deleted
        // loop's header params directly, which the new edge would leave
        // undefined, so the params get bound to the resolved args instead.
        let mut fwd: FxHashMap<Value, Ins> = FxHashMap::default();
        let mut dest = edge_dest(func, e);
        let mut args = edge_args(func, e);
        let mut exit_args = Vec::new();
        for _ in 0..8 {
            if func.dfg.block_params(dest).len() != args.len() {
                continue 'exits;
            }
            exit_args.clear();
            for &v in &args {
                let Some(a) = arg_ins(func, &info, &kinds, &itb, &fwd, v, 0) else {
                    continue 'exits;
                };
                exit_args.push(a);
            }
            let mut it = func.layout.block_insts(dest);
            let Some(t) = it.next() else { break };
            if it.next().is_some() {
                break;
            }
            if !matches!(func.dfg.insts[t], InstructionData::Jump { .. }) {
                break;
            }
            for (p, a) in func
                .dfg
                .block_params(dest)
                .iter()
                .copied()
                .zip(exit_args.iter().cloned())
            {
                fwd.insert(p, a);
            }
            let ne = Edge { inst: t, slot: 0 };
            dest = edge_dest(func, ne);
            args = edge_args(func, ne);
        }
        // No inst reachable from the new edge's dest — without re-entering
        // the loop — may read a loop-internal value: the deleted body no
        // longer dominates those uses. The exception is `dest` itself: a
        // loop value it reads directly (e.g. the promoted accumulator in a
        // `store acc` just past the loop) becomes an appended block param,
        // bound to the original value on the loop-side edges — it dominated
        // `dest`, hence every pred edge — and to the closed form on the
        // fast edge.
        let loop_val = |v: Value| match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
            ValueDef::Param(b, _) => b == info.h,
            ValueDef::Result(i, _) => func
                .layout
                .inst_block(i)
                .is_some_and(|b| info.body.contains(&b)),
            _ => false,
        };
        let mut escapes: Vec<(Value, Ins)> = Vec::new();
        let mut seen = info.body.clone();
        let mut wl = vec![dest];
        while let Some(b) = wl.pop() {
            if !seen.insert(b) {
                continue;
            }
            for i in func.layout.block_insts(b) {
                for &a in func.dfg.inst_args(i) {
                    if !loop_val(a) {
                        continue;
                    }
                    if b != dest {
                        why!("dead: {b:?} reads loop values");
                    }
                    if !escapes.iter().any(|&(v, _)| v == a) {
                        let Some(ins) = arg_ins(func, &info, &kinds, &itb, &fwd, a, 0)
                        else {
                            continue 'exits;
                        };
                        escapes.push((a, ins));
                    }
                }
                for bc in func
                    .dfg
                    .insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    for a in bc.args(&func.dfg.value_lists) {
                        let BlockArg::Value(v) = a else {
                            continue;
                        };
                        if !loop_val(v) {
                            continue;
                        }
                        if b != dest {
                            why!("dead: {b:?} reads loop values");
                        }
                        if !escapes.iter().any(|&(e, _)| e == v) {
                            let Some(ins) =
                                arg_ins(func, &info, &kinds, &itb, &fwd, v, 0)
                            else {
                                continue 'exits;
                            };
                            escapes.push((v, ins));
                        }
                    }
                    wl.push(bc.block(&func.dfg.value_lists));
                }
            }
        }
        return Some(DeadPlan {
            entry: info.entry,
            h: info.h,
            entry_args: info.entry_args,
            exit_dest: dest,
            exit_args,
            escapes,
            iters,
            rmw,
            preds,
        });
    }
    None
}

/// Rewrite `p`: the entry edge goes to a check block that verifies the
/// preds and jumps to the exit with the recomputed args, or enters the
/// loop unchanged.
fn apply_dead(
    func: &mut Function,
    p: &DeadPlan,
    tcfg: cranelift_codegen::isa::TargetFrontendConfig,
) {
    let pblock = func.layout.inst_block(p.entry.inst).unwrap();
    let nb = func.dfg.make_block();
    func.layout.insert_block_after(nb, pblock);
    {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[p.entry.inst].branch_destination_mut(
            &mut dfg.jump_tables,
            &mut dfg.exception_tables,
        )[p.entry.slot];
        *bc = BlockCall::new(nb, core::iter::empty(), &mut dfg.value_lists);
    }
    let pty = tcfg.pointer_type();
    let dst_params: Vec<Value> = func.dfg.block_params(p.exit_dest).to_vec();
    // Escaped loop values: `exit_dest` gains one param each. Every existing
    // edge into it passes the original value (it dominated `exit_dest`, so
    // it's in scope on all preds); the new fast edge passes the closed form.
    let new_params: Vec<Value> = p
        .escapes
        .iter()
        .map(|&(v, _)| {
            let ty = func.dfg.value_type(v);
            func.dfg.append_block_param(p.exit_dest, ty)
        })
        .collect();
    if !new_params.is_empty() {
        // Edges into `exit_dest`, before the fast edge exists.
        let mut edges: Vec<(Inst, usize)> = Vec::new();
        for b in func.layout.blocks() {
            let Some(t) = func.layout.last_inst(b) else {
                continue;
            };
            for (slot, bc) in func.dfg.insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                if bc.block(&func.dfg.value_lists) == p.exit_dest {
                    edges.push((t, slot));
                }
            }
        }
        for (inst, slot) in edges {
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[inst].branch_destination_mut(
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
            )[slot];
            for &(v, _) in &p.escapes {
                bc.append_argument(BlockArg::Value(v), &mut dfg.value_lists);
            }
        }
        // Rewrite the escaping uses in `exit_dest` to the new params. Key
        // both the escape's raw spelling and its resolved form — uses can
        // appear under either.
        let remap: FxHashMap<Value, Value> = p
            .escapes
            .iter()
            .zip(&new_params)
            .flat_map(|(&(v, _), &np)| {
                [(v, np), (func.dfg.resolve_aliases(v), np)]
            })
            .collect();
        let insts: Vec<Inst> = func.layout.block_insts(p.exit_dest).collect();
        for i in insts {
            for a in func.dfg.inst_args_mut(i) {
                if let Some(&np) = remap.get(&*a) {
                    *a = np;
                }
            }
            let dfg = &mut func.dfg;
            for bc in dfg.insts[i].branch_destination_mut(
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
            ) {
                bc.update_args(&mut dfg.value_lists, |a| match a {
                    BlockArg::Value(v) => remap
                        .get(&v)
                        .map(|&np| BlockArg::Value(np))
                        .unwrap_or(a),
                    a => a,
                });
            }
        }
    }
    let mut pos = FuncCursor::new(func).at_bottom(nb);
    let mut ok: Option<Value> = None;
    for pr in &p.preds {
        let c = match pr {
            Pred::Cmp(cc, a, b) => {
                let (a, b) = (emit(&mut pos, pty, a), emit(&mut pos, pty, b));
                pos.ins().icmp(*cc, a, b)
            }
            Pred::Aligned(a, size) => {
                let m = emit(&mut pos, pty, a);
                let k = pos.ins().iconst(pty, size - 1);
                let r = pos.ins().band(m, k);
                let z = pos.ins().iconst(pty, 0);
                pos.ins().icmp(IntCC::Equal, r, z)
            }
            Pred::Disjoint => unreachable!(),
        };
        ok = Some(match ok {
            None => c,
            Some(o) => pos.ins().band(o, c),
        });
    }
    let mut exit_args: Vec<BlockArg> =
        Vec::with_capacity(p.exit_args.len() + p.escapes.len());
    for (j, ins) in p.exit_args.iter().enumerate() {
        let ty = pos.func.dfg.value_type(dst_params[j]);
        let v = emit(&mut pos, ty, ins);
        exit_args.push(BlockArg::Value(v));
    }
    for (&np, (_, ins)) in new_params.iter().zip(&p.escapes) {
        let ty = pos.func.dfg.value_type(np);
        let v = emit(&mut pos, ty, ins);
        exit_args.push(BlockArg::Value(v));
    }
    let h_args: Vec<BlockArg> = p
        .entry_args
        .iter()
        .map(|&v| BlockArg::Value(v))
        .collect();
    if !p.rmw.is_empty() {
        // The folded RMW load/stores are side effects: they may only run
        // on the fast path, so they go in a block past the preds check.
        let mb = pos.func.dfg.make_block();
        pos.func.layout.insert_block_after(mb, nb);
        match ok {
            Some(ok) => {
                pos.ins().brif(ok, mb, &[], p.h, &h_args);
            }
            None => {
                pos.ins().jump(mb, &[]);
            }
        }
        let mut pos = FuncCursor::new(func).at_bottom(mb);
        for r in &p.rmw {
            let a = match &r.addr {
                RmwAddr::V(e) => emit(&mut pos, pty, e),
                RmwAddr::S(ss) => pos.ins().stack_addr(pty, *ss, 0),
            };
            let init = pos.ins().load(r.ty, r.load_flags, a, r.offset);
            let fin = match &r.delta {
                RmwDelta::Keep => init,
                RmwDelta::Add(d) => {
                    let d = emit(&mut pos, r.ty, d);
                    let n = emit(&mut pos, r.ty, &p.iters);
                    let m = pos.ins().imul(d, n);
                    pos.ins().iadd(init, m)
                }
                RmwDelta::Sub(d) => {
                    let d = emit(&mut pos, r.ty, d);
                    let n = emit(&mut pos, r.ty, &p.iters);
                    let m = pos.ins().imul(d, n);
                    pos.ins().isub(init, m)
                }
                RmwDelta::Set(v) => emit(&mut pos, r.ty, v),
            };
            pos.ins().store(r.store_flags, fin, a, r.offset);
        }
        pos.ins().jump(p.exit_dest, &exit_args);
        return;
    }
    match ok {
        Some(ok) => {
            pos.ins().brif(ok, p.exit_dest, &exit_args, p.h, &h_args);
        }
        None => {
            pos.ins().jump(p.exit_dest, &exit_args);
        }
    }
}

pub fn run(
    func: &mut Function,
    noalias: &FxHashSet<Value>,
    tcfg: cranelift_codegen::isa::TargetFrontendConfig,
    fname: &str,
) -> usize {
    // A libcall inside `memset`/`memcpy` itself would be an infinite call cycle.
    if ["memset", "memcpy", "memmove", "memcmp", "bcmp"]
        .iter()
        .any(|s| fname.contains(s))
    {
        return 0;
    }
    let debug = std::env::var_os("PLIRON_IDIOM_DEBUG").is_some();
    let mut n = 0;
    // `apply` rewires the CFG, so recompute the analyses before each
    // conversion rather than letting `plan` see stale dominance/latches.
    let mut done = FxHashSet::default();
    'outer: for _ in 0..MAX_LOOPS {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        for lp in la.loops() {
            if done.contains(&la.loop_header(lp)) {
                continue;
            }
            if let Some(p) = plan(func, &cfg, &dt, &la, lp, noalias) {
                if debug {
                    eprintln!("idiom {fname}: loop {:?} -> {}", p.h, if p.src.is_some() { "memcpy" } else { "memset" });
                }
                apply(func, &p, tcfg);
                done.insert(p.h);
            } else if let Some(d) = plan_dead(func, &cfg, &dt, &la, lp) {
                if debug {
                    eprintln!("idiom {fname}: dead loop {:?} eliminated", d.h);
                }
                apply_dead(func, &d, tcfg);
                done.insert(d.h);
            } else {
                continue;
            }
            n += 1;
            if n >= MAX_CONV {
                break 'outer;
            }
            continue 'outer;
        }
        break;
    }
    n
}



