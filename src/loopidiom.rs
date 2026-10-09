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
    InstBuilder, InstructionData, LibCall, Opcode, Signature, Type, Value, ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_data_structures::fx::FxHashSet;

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
fn ins_eval(pos: &FuncCursor, e: &Ins) -> Option<i64> {
    Some(match e {
        Ins::K(k) => *k,
        Ins::Val(v) => iconst(pos.func, *v)?,
        Ins::Add(a, b) => ins_eval(pos, a)?.wrapping_add(ins_eval(pos, b)?),
        Ins::Sub(a, b) => ins_eval(pos, a)?.wrapping_sub(ins_eval(pos, b)?),
        Ins::SatSub(a, b) => ins_eval(pos, a)?.saturating_sub(ins_eval(pos, b)?),
        Ins::Mul(a, b) => ins_eval(pos, a)?.wrapping_mul(ins_eval(pos, b)?),
        Ins::And(a, b) => ins_eval(pos, a)? & ins_eval(pos, b)?,
        Ins::Div(a, b) => (ins_eval(pos, a)? as u64)
            .checked_div(ins_eval(pos, b)? as u64)? as i64,
    })
}

pub(crate) fn emit(pos: &mut FuncCursor, ty: Type, e: &Ins) -> Value {
    // Cheap folds for shapes stream/address exprs produce (e.g. `iv0=0`
    // turns `base + iv0*K` into `base + 0*K`).
    match e {
        Ins::Add(a, b) if ins_eval(pos, a) == Some(0) => return emit(pos, ty, b),
        Ins::Add(a, b) | Ins::Sub(a, b) if ins_eval(pos, b) == Some(0) => {
            return emit(pos, ty, a);
        }
        Ins::Mul(a, b) | Ins::And(a, b)
            if ins_eval(pos, a) == Some(0) || ins_eval(pos, b) == Some(0) =>
        {
            return emit(pos, ty, &Ins::K(0));
        }
        Ins::Mul(a, b) if ins_eval(pos, a) == Some(1) => return emit(pos, ty, b),
        Ins::Mul(a, b) | Ins::Div(a, b) if ins_eval(pos, b) == Some(1) => {
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
pub(crate) fn guard_pred(func: &Function, info: &Info, kinds: &[Param], cnt: &Count, e: Edge) -> Option<Pred> {
    if !deadend(func, edge_dest(func, e)) {
        return None;
    }
    let InstructionData::Brif { arg, .. } = func.dfg.insts[e.inst] else {
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
    if cond != IntCC::UnsignedLessThan
        || func.dfg.resolve_aliases(x) != cnt.x
        || e.slot != 1
    {
        return None;
    }
    let lim = outv(func, info, kinds, y)?;
    Some(Pred::Cmp(
        IntCC::UnsignedLessThanOrEqual,
        Ins::Val(cnt.bound),
        Ins::Val(lim),
    ))
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
                if !guard_dead(func, &c, e2) {
                    ps.push(guard_pred(func, &info, &kinds, &c, e2)?);
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
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let loops: Vec<Loop> = la.loops().collect();
    let debug = std::env::var_os("PLIRON_IDIOM_DEBUG").is_some();
    let mut n = 0;
    for lp in loops.into_iter().take(MAX_LOOPS) {
        let Some(p) = plan(func, &cfg, &dt, &la, lp, noalias) else {
            continue;
        };
        if debug {
            eprintln!("idiom {fname}: loop {:?} -> {}", p.h, if p.src.is_some() { "memcpy" } else { "memset" });
        }
        apply(func, &p, tcfg);
        n += 1;
        if n >= MAX_CONV {
            break;
        }
    }
    n
}



