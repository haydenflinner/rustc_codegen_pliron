//! Bounds-check loop versioning.
//!
//! A `for i in 0..n { ... a[idx] ... }` loop checks `idx < len` every
//! iteration. When `idx` is affine in the loop counter, its last-iteration
//! value `hi` can be computed in the preheader: `hi < len` implies every
//! iteration's check passes, so the loop is cloned and the checks in the
//! clone jump straight to their in-bounds successor. The original stays as
//! the slow path and still panics on the same iteration, so this is exact.
//!
//! `hi` is evaluated in double-width arithmetic (i128 for i64 indices): a
//! wrapped recurrence or empty/degenerate trip range makes `hi` huge and the
//! guard fails to the slow path — no separate overflow/domain tests needed.
//! When every affine contribution is non-negative the same wrap-catching
//! falls out of native add/sub/mul carry flags, so the guard is emitted in
//! the index type with `!carry` ANDed in — the i128 path only survives for
//! checks with negative terms.
//!
//! Monotonicity: idx = b + i*m for i in [0, T). As an unsigned machine value
//! m is >= 0 in true-arithmetic terms, so idx is non-decreasing in i and
//! idx(T-1) = hi is the max. If hi < len fits the index type, no
//! intermediate i*m or b + i*m wrapped either, so every iteration's
//! machine-computed idx equals its true value and is < len.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::instructions::InstructionMapper;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Constant, DynamicStackSlot, ExceptionTable, ExceptionTableItem,
    FuncRef, Function, GlobalValue, Immediate, Inst, InstBuilder, InstructionData, JumpTable,
    JumpTableData, Opcode, SigRef, StackSlot, Type, Value, ValueList, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::loopidiom::deepest_first;

const MAX_LOOPS: usize = 8;
const MAX_BODY: usize = 48;
const MAX_CHECKS: usize = 8;
const MAX_AFFINE_DEPTH: usize = 4;
const MAX_VERSIONS: usize = 4;
const MAX_REGION_EXTRA: usize = 32;

/// `v * k` scaled linear term, emitted widened to the guard type.
#[derive(Clone, Copy)]
struct Term {
    v: Lin,
    k: i64,
}

#[derive(Clone, Copy)]
enum Lin {
    K(i64),
    V(Value),
    /// Entry value of header param N (the guard block's param N).
    P(usize),
    /// `-(v)` widened: only produced when an `isub` negates an affine expr.
    N(Value),
}

/// `idx = m * i + sum(b)` over true integers (`i` = counter value).
struct Aff {
    m: Lin,
    b: SmallVec<[Term; 2]>,
}

#[derive(Clone, Copy)]
struct Edge {
    inst: Inst,
    slot: usize,
}

/// A `P + step` backedge update; step may be a runtime value or a constant.
#[derive(Clone, Copy)]
enum Step {
    V(Value),
    K(i64),
}

struct LoopInfo {
    /// Header param stepping by `istep` per iteration.
    iv: Value,
    /// Param index of `iv`; the entry value is the guard block's param.
    iv0: usize,
    /// The iv's entry value when constant and identical on all entry edges.
    iv0k: Option<i64>,
    /// Loop-invariant exit bound.
    bound: Value,
    /// Last executed counter value: `bound - adj`.
    adj: i64,
    /// Stepped header params: `P_i = init + i*step` (init = entry param idx).
    stepped: FxHashMap<Value, (usize, Step)>,
    /// Guard block; `block_params(g)[i]` is the entry value of param i.
    g: Option<Block>,
    /// Header block.
    h: Block,
    /// Backedge arg for each header param (None = unanalyzable).
    bargs: Vec<Option<Value>>,
    /// Resolved entry arg for each header param when all entry edges pass
    /// the same value (None = they disagree or are unanalyzable).
    eargs: Vec<Option<Value>>,
}

/// A `brif` whose in-bounds edge is provably always taken given `hi < len`.
struct Chk {
    inst: Inst,
    safe_slot: usize,
    aff: Aff,
    le: bool,
    len: Value,
    /// Bound-subsumption check on `iv + k` (no counting iv): the header's
    /// own `idx < B` test bounds every in-body iteration, so the guard is
    /// `B - adj + k < len` rather than a trip-count `hi`.
    bsub: Option<i64>,
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

fn iconst_masked(pos: &mut FuncCursor, t: Type, k: i64) -> Value {
    if t == types::I128 {
        let lo = pos.ins().iconst(types::I64, k);
        let hi = pos.ins().iconst(types::I64, k >> 63);
        return pos.ins().iconcat(lo, hi);
    }
    let mask = if t.bits() < 64 {
        (1i64 << t.bits()) - 1
    } else {
        -1
    };
    pos.ins().iconst(t, k & mask)
}

/// `v` provably equals `p` or `e` at runtime: it is one of them, or a body
/// block param whose every incoming edge arg recursively is. Used to show a
/// loop param fed through forwarded params stays invariant.
fn feeds(
    func: &Function,
    cfg: &ControlFlowGraph,
    body: &FxHashSet<Block>,
    p: Value,
    e: Value,
    v: Value,
    seen: &mut FxHashSet<Value>,
    depth: usize,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if v == p || v == e {
        return true;
    }
    if depth == 0 || !seen.insert(v) {
        return false;
    }
    let (Some(b), cranelift_codegen::ir::ValueDef::Param(_, n)) =
        (def_block(func, v), func.dfg.value_def(v))
    else {
        return false;
    };
    if !body.contains(&b) {
        return false;
    }
    edges_to(func, cfg, b)
        .iter()
        .all(|edge| match edge_arg(func, edge, n) {
            Some(a) => feeds(func, cfg, body, p, e, a, seen, depth - 1),
            None => false,
        })
}

fn def_block(func: &Function, v: Value) -> Option<Block> {
    use cranelift_codegen::ir::ValueDef;
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        ValueDef::Union(..) => None,
    }
}

fn edges_to(func: &Function, cfg: &ControlFlowGraph, h: Block) -> Vec<Edge> {
    let mut out = Vec::new();
    for p in cfg.pred_iter(h) {
        for (slot, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) == h {
                out.push(Edge { inst: p.inst, slot });
            }
        }
    }
    out
}

fn edge_arg(func: &Function, e: &Edge, idx: usize) -> Option<Value> {
    let bc = func.dfg.insts[e.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[e.slot];
    match bc.args(&func.dfg.value_lists).nth(idx)? {
        BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
        _ => None,
    }
}

/// `arg` is `P + step` for header param `P`: the per-iteration delta.
fn step_of(func: &Function, p: Value, arg: Value) -> Option<Step> {
    let arg = func.dfg.resolve_aliases(arg);
    let i = func.dfg.value_def(arg).inst()?;
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } => {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            if a == p {
                Some(iconst(func, b).map(Step::K).unwrap_or(Step::V(b)))
            } else if b == p {
                Some(iconst(func, a).map(Step::K).unwrap_or(Step::V(a)))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `v` is loop-invariant: defined outside the body, or a header param whose
/// backedge passes it through unchanged.
fn invariant(
    func: &Function,
    cfg: &ControlFlowGraph,
    info: &LoopInfo,
    body: &FxHashSet<Block>,
    v: Value,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    // Constants are trivially invariant no matter where the iconst sits.
    if iconst(func, v).is_some() {
        return true;
    }
    match def_block(func, v) {
        Some(b) if b == info.h => {
            if let cranelift_codegen::ir::ValueDef::Param(_, n) = func.dfg.value_def(v)
                && let Some(Some(ba)) = info.bargs.get(n)
            {
                let ba = func.dfg.resolve_aliases(*ba);
                if ba == v {
                    return true;
                }
                // The param's value each iteration is `ba`; when every
                // source of `ba` is the param itself or the
                // identical-on-all-entries entry value, the param never
                // changes across iterations.
                if let Some(Some(ea)) = info.eargs.get(n) {
                    let ea = func.dfg.resolve_aliases(*ea);
                    let mut seen = FxHashSet::default();
                    seen.insert(v);
                    return feeds(func, cfg, body, v, ea, ba, &mut seen, 8);
                }
            }
            false
        }
        Some(b) => !body.contains(&b),
        None => true,
    }
}

/// (x64 wt) Unsigned bound of `v` when `clifpeep::vmax` proves one tighter
/// than the value's own type — `uload8`/`uload16` loads, `uextend`s, masks.
/// Used to version data-dependent but range-limited checks (`cnt[a[i]]`
/// with a u8 `a`): the synthesized "affine" hi is the constant bound, so
/// the guard is `tymax < len`. Only small-table bounds are worth a clone:
/// a guard that can never pass (u32-range bound vs a realistic slice len)
/// is pure bloat.
fn ubound(func: &Function, v: Value) -> Option<u64> {
    let ty = func.dfg.value_type(v);
    if !ty.is_int() || ty.is_vector() {
        return None;
    }
    let ty_umax = if ty.bits() >= 64 {
        u64::MAX
    } else {
        (1u64 << ty.bits()) - 1
    };
    let k = crate::clifpeep::vmax(func, v);
    (k < ty_umax && k <= 0xFFFF).then_some(k)
}

/// `PLIRON_BCHECK_UBOUND=0` disables bounded-index versioning.
fn ubound_on() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| crate::pass_enabled("PLIRON_BCHECK_UBOUND"));
    *ON
}

/// Decompose `v` as `m*i + sum(b)` over the counting iv. Each `b` term is
/// `(lin, factor)` so `imul` by a constant scales through.
fn affine(
    func: &Function,
    cfg: &ControlFlowGraph,
    info: &LoopInfo,
    body: &FxHashSet<Block>,
    depth: usize,
    v: Value,
) -> Option<Aff> {
    if depth == 0 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    if v == info.iv {
        // The count test bounds the iv itself: `iv <= bound - adj` at every
        // iteration, whatever the step. Modeling it as `iv0 + i` evaluates at
        // t = bound - iv0 - adj to exactly `bound - adj` — the tightest
        // sound bound on every affine combination of the iv.
        return Some(Aff {
            m: Lin::K(1),
            b: [Term {
                v: Lin::P(info.iv0),
                k: 1,
            }]
            .into_iter()
            .collect(),
        });
    }
    if let Some(&(init, step)) = info.stepped.get(&v) {
        let m = match step {
            Step::K(k) => Lin::K(k),
            Step::V(s) => Lin::V(s),
        };
        return Some(Aff {
            m,
            b: [Term {
                v: Lin::P(init),
                k: 1,
            }]
            .into_iter()
            .collect(),
        });
    }
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } => {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            for (x, y) in [(a, b), (b, a)] {
                if let Some(mut af) = affine(func, cfg, info, body, depth - 1, x)
                    && invariant(func, cfg, info, body, y)
                {
                    // Constants sign-extend into the wide domain; a `V` term
                    // would zext `-k` into `2^64-k` and sink every guard.
                    let v = match iconst(func, y) {
                        Some(k) => Lin::K(k),
                        None => Lin::V(y),
                    };
                    af.b.push(Term { v, k: 1 });
                    return Some(af);
                }
            }
            None
        }
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args: [a, b],
        } => {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            // `affine - invariant`: subtract one b term.
            if let Some(mut af) = affine(func, cfg, info, body, depth - 1, a)
                && invariant(func, cfg, info, body, b)
            {
                let (v, k) = match iconst(func, b) {
                    Some(k) => (Lin::K(k), -1),
                    None => (Lin::V(b), -1),
                };
                af.b.push(Term { v, k });
                return Some(af);
            }
            // `invariant - affine` (e.g. `n - 1 - i`): negate m and every
            // b term, then add the invariant. The index sequence decreases,
            // so the guard bounds the entry value instead of the last one.
            if invariant(func, cfg, info, body, a)
                && let Some(mut af) = affine(func, cfg, info, body, depth - 1, b)
            {
                af.m = match af.m {
                    Lin::K(k) => Lin::K(k.checked_neg()?),
                    Lin::V(s) => Lin::N(s),
                    Lin::N(s) => Lin::V(s),
                    Lin::P(_) => return None,
                };
                for t in af.b.iter_mut() {
                    t.k = t.k.checked_neg()?;
                }
                let v = match iconst(func, a) {
                    Some(k) => Lin::K(k),
                    None => Lin::V(a),
                };
                af.b.push(Term { v, k: 1 });
                return Some(af);
            }
            None
        }
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args: [a, b],
        } => {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            for (x, y) in [(a, b), (b, a)] {
                // counter * invariant: idx = i*y + iv0*y; the iv0*y term is
                // a V*V product we can only express when iv0 is a constant.
                if x == info.iv && invariant(func, cfg, info, body, y) {
                    let k = info.iv0k?;
                    return Some(Aff {
                        m: Lin::V(y),
                        b: [Term { v: Lin::V(y), k }].into_iter().collect(),
                    });
                }
                // affine * const: scale m and every b term.
                if let Some(k) = iconst(func, y)
                    && let Some(mut af) = affine(func, cfg, info, body, depth - 1, x)
                {
                    // A runtime m stays linear only when it scales a const.
                    if let Lin::K(c) = af.m {
                        af.m = Lin::K(c.wrapping_mul(k));
                        for t in af.b.iter_mut() {
                            t.k = t.k.wrapping_mul(k);
                        }
                        return Some(af);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Same-function remapper for cloning the loop body. Blocks inside the loop
/// map to their clones; everything else is the identity.
struct CloneMap<'a> {
    func: &'a mut Function,
    vmap: &'a FxHashMap<Value, Value>,
    bmap: &'a FxHashMap<Block, Block>,
}

impl CloneMap<'_> {
    fn mv(&self, v: Value) -> Value {
        let v = self.func.dfg.resolve_aliases(v);
        self.vmap.get(&v).copied().unwrap_or(v)
    }
}

impl InstructionMapper for CloneMap<'_> {
    fn map_value(&mut self, value: Value) -> Value {
        self.mv(value)
    }
    fn map_value_list(&mut self, value_list: ValueList) -> ValueList {
        let mut out = ValueList::new();
        let vals: Vec<Value> = value_list.as_slice(&self.func.dfg.value_lists).to_vec();
        for v in vals {
            out.push(self.mv(v), &mut self.func.dfg.value_lists);
        }
        out
    }
    fn map_global_value(&mut self, gv: GlobalValue) -> GlobalValue {
        gv
    }
    fn map_jump_table(&mut self, jt: JumpTable) -> JumpTable {
        let def = self.map_block_call(self.func.dfg.jump_tables[jt].default_block());
        let calls: Vec<BlockCall> = self.func.dfg.jump_tables[jt].as_slice().to_vec();
        let table: SmallVec<[BlockCall; 8]> =
            calls.iter().map(|bc| self.map_block_call(*bc)).collect();
        self.func
            .dfg
            .jump_tables
            .push(JumpTableData::new(def, &table))
    }
    fn map_exception_table(&mut self, et: ExceptionTable) -> ExceptionTable {
        let (sig, nrv, orig): (_, _, SmallVec<[ExceptionTableItem; 4]>) = {
            let e = &self.func.dfg.exception_tables[et];
            (e.signature(), *e.normal_return(), e.items().collect())
        };
        let nr = self.map_block_call(nrv);
        let items: SmallVec<[ExceptionTableItem; 4]> = orig
            .iter()
            .map(|item| match *item {
                ExceptionTableItem::Tag(t, bc) => {
                    ExceptionTableItem::Tag(t, self.map_block_call(bc))
                }
                ExceptionTableItem::Default(bc) => {
                    ExceptionTableItem::Default(self.map_block_call(bc))
                }
                ExceptionTableItem::Context(v) => ExceptionTableItem::Context(self.mv(v)),
            })
            .collect();
        self.func
            .dfg
            .exception_tables
            .push(cranelift_codegen::ir::ExceptionTableData::new(
                sig, nr, items,
            ))
    }
    fn map_block_call(&mut self, bc: BlockCall) -> BlockCall {
        let blk = bc.block(&self.func.dfg.value_lists);
        let nb = self.bmap.get(&blk).copied().unwrap_or(blk);
        let args: Vec<BlockArg> = bc
            .args(&self.func.dfg.value_lists)
            .map(|a| match a {
                BlockArg::Value(v) => BlockArg::Value(self.mv(v)),
                a => a,
            })
            .collect();
        BlockCall::new(nb, args.iter().copied(), &mut self.func.dfg.value_lists)
    }
    fn map_block(&mut self, block: Block) -> Block {
        self.bmap.get(&block).copied().unwrap_or(block)
    }
    fn map_func_ref(&mut self, r: FuncRef) -> FuncRef {
        r
    }
    fn map_sig_ref(&mut self, r: SigRef) -> SigRef {
        r
    }
    fn map_stack_slot(&mut self, s: StackSlot) -> StackSlot {
        s
    }
    fn map_dynamic_stack_slot(&mut self, s: DynamicStackSlot) -> DynamicStackSlot {
        s
    }
    fn map_constant(&mut self, c: Constant) -> Constant {
        c
    }
    fn map_immediate(&mut self, i: Immediate) -> Immediate {
        i
    }
}

/// `bad` must die for the fold to be sound: the clone replaces the check's
/// unsafe edge with an unconditional jump to the safe successor, so every
/// path out of `bad` must stay in cold code and never reach a `return`.
/// A panic/trap chain qualifies; a cold *normal* exit does not — e.g. the
/// not-found edge of a `position`/`contains` scan under a cold caller is
/// cold yet live, and folding it drops the loop's exit, turning the scan
/// into an out-of-bounds read (stage2 rustc `parse_cfg`: `s.contains('=')`
/// spuriously true for every `s`).
/// `throw_ret`: under emulated EH (COFF panic=unwind) a "throw" unwinds by
/// early-return, so a panic cold-path ends `call panic_*; load tls_flag;
/// return` instead of trapping. Admit a `return` only when a call and a load
/// off the `tls_value` flag address precede it — a live cold exit like
/// `contains`' `call missing; return` never reads the EH slot, so it still
/// counts as a live edge.
fn dead_end(func: &Function, bad: Block, throw_ret: bool) -> bool {
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut stack = vec![bad];
    // Emulated-EH rethrow paths run drop glue (dozens of cold blocks) between
    // the flag check and the dummy `return` that propagates the exception to
    // the caller. Admit `return`s when a block itself does `call panic_*;
    // load flag` — every reachable return is then past the flag check (the
    // unset arm traps), i.e. a rethrow. The TLS-flag load only exists under
    // emulated EH, so the `call && flag` shape is self-identifying and needs
    // no `throw_ret` gate: leaf throwers (panic_bounds_check callers with no
    // landing pad of their own) end every OOB edge in exactly this shape.
    let mut rethrow = false;
    let dbg = crate::env_set("PLIRON_BCHECK_DEBUG");
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        if seen.len() > 64 {
            if dbg {
                eprintln!(
                    "dead_end blk{}: bail seen={} cold={}",
                    b.as_u32(),
                    seen.len(),
                    func.layout.is_cold(b)
                );
            }
            return false;
        }
        let (mut call, mut flag) = (false, false);
        let mut flag_brif = false;
        for i in func.layout.block_insts(b) {
            let d = &func.dfg.insts[i];
            match d.opcode() {
                Opcode::Call | Opcode::CallIndirect => call = true,
                Opcode::Load => {
                    if let InstructionData::Load { arg, .. } = *d
                        && let Some(ai) = func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst()
                        && func.dfg.insts[ai].opcode() == Opcode::TlsValue
                    {
                        flag = true;
                    }
                }
                Opcode::Brif => {
                    if let InstructionData::Brif { arg, .. } = *d {
                        if let Some(li) = func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst()
                            && let InstructionData::Load { arg: la, .. } = func.dfg.insts[li]
                            && let Some(ai) =
                                func.dfg.value_def(func.dfg.resolve_aliases(la)).inst()
                            && func.dfg.insts[ai].opcode() == Opcode::TlsValue
                        {
                            flag_brif = true;
                        }
                    }
                }
                Opcode::Return if !(call && flag || (throw_ret && rethrow)) => {
                    if dbg {
                        eprintln!(
                            "dead_end blk{}: live return throw_ret={throw_ret} call={call} flag={flag} rethrow={rethrow}",
                            b.as_u32()
                        );
                    }
                    return false;
                }
                _ => {}
            }
            if b == bad {
                // `call; load flag` identifies a throw only when nothing
                // branches on the flag: a known-throwing callee (panic_*)
                // leaves the load dead, while a normal call's flag check
                // keeps a live flag-clear continuation — exactly the
                // `call f; load flag; brif flag` every emulated-EH call
                // site emits. `rethrow` admitting downstream returns is
                // sound only in the first case.
                rethrow = call && flag && !flag_brif;
            }
            let dests = d.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            // `try_call` lists its exception-handler edges first and the
            // normal-return continuation last. An unwinding callee leaves
            // the frame — the exception path runs cleanup and resignals,
            // it can't resume this loop — so only the normal edge can
            // reach back (loopidiom::deadend makes the same assumption).
            let dests: &[cranelift_codegen::ir::BlockCall] = match d.opcode() {
                Opcode::TryCall | Opcode::TryCallIndirect => {
                    dests.last().map_or(&[], |bc| std::slice::from_ref(bc))
                }
                _ => dests,
            };
            for bc in dests {
                stack.push(bc.block(&func.dfg.value_lists));
            }
        }
    }
    true
}

/// The clone set for loop versioning: `body` plus every block fully
/// enclosed by it (all CFG preds inside). Cold trap blocks and private
/// exits can freely use body values, so sharing them between versions
/// would break dominance. Blocks with a pred outside the region stay
/// shared — but only when nothing past them uses a region value: a
/// region edge into a shared block is cloned too, and the clone path
/// bypasses the value's def, breaking dominance of uses reachable
/// through the shared block. Any boundary target whose non-region cone
/// uses a region value is absorbed as well. `None` when the region
/// outgrows `body` by more than MAX_REGION_EXTRA.
fn closed_region(
    func: &Function,
    cfg: &ControlFlowGraph,
    body: &FxHashSet<Block>,
) -> Option<FxHashSet<Block>> {
    let mut region: FxHashSet<Block> = body.clone();
    // "All preds in region" as a decremental count + worklist rather
    // than rescanning every block per growth round: `pending[b]` is the
    // number of b's preds still outside the region and each edge is
    // decremented once for the whole call — O(edges) total, not
    // O(rounds * blocks). Zero-pred blocks are never absorbed.
    let mut pending: FxHashMap<Block, usize> = FxHashMap::default();
    let mut ready: Vec<Block> = Vec::new();
    for b in func.layout.blocks() {
        if region.contains(&b) || cfg.pred_iter(b).next().is_none() {
            continue;
        }
        let n = cfg
            .pred_iter(b)
            .filter(|p| !region.contains(&p.block))
            .count();
        if n == 0 {
            ready.push(b);
        }
        pending.insert(b, n);
    }
    loop {
        while let Some(x) = ready.pop() {
            if !region.insert(x) {
                continue;
            }
            for s in cfg.succ_iter(x) {
                if region.contains(&s) {
                    continue;
                }
                if let Some(c) = pending.get_mut(&s) {
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        ready.push(s);
                    }
                }
            }
        }
        if region.len() - body.len() > MAX_REGION_EXTRA {
            return None;
        }
        let mut rvals: FxHashSet<Value> = FxHashSet::default();
        for &r in &region {
            rvals.extend(func.dfg.block_params(r).iter().copied());
            for i in func.layout.block_insts(r) {
                rvals.extend(func.dfg.inst_results(i).iter().copied());
            }
        }
        let uses_region = |func: &Function, x: Block| {
            for i in func.layout.block_insts(x) {
                if func
                    .dfg
                    .inst_args(i)
                    .iter()
                    .any(|&v| rvals.contains(&func.dfg.resolve_aliases(v)))
                {
                    return true;
                }
                for bc in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    if bc.args(&func.dfg.value_lists).any(|a| {
                        matches!(a, BlockArg::Value(v) if {
                            rvals.contains(&func.dfg.resolve_aliases(v))
                        })
                    }) {
                        return true;
                    }
                }
                if let InstructionData::TryCall { exception, .. }
                | InstructionData::TryCallIndirect { exception, .. } = func.dfg.insts[i]
                    && func.dfg.exception_tables[exception].items().any(|it| {
                        matches!(it, ExceptionTableItem::Context(v) if {
                            rvals.contains(&func.dfg.resolve_aliases(v))
                        })
                    })
                {
                    return true;
                }
            }
            false
        };
        // Leak check, batched: the original walked each boundary
        // target's forward cone separately and the cones overlap, so
        // the round cost O(targets * blocks). One backward walk from
        // every non-region block that uses a region value marks exactly
        // the blocks that can reach such a use — a target leaks iff it
        // is marked. This absorbs every leaking target in one pass
        // (the original absorbed one at a time and could prune a later
        // target's cone through an earlier absorb), so the result is a
        // superset of the old fixpoint — conservative, and may hit the
        // region cap one round sooner.
        let mut reached: FxHashSet<Block> = FxHashSet::default();
        let mut stack: Vec<Block> = Vec::new();
        for b in func.layout.blocks() {
            if !region.contains(&b) && uses_region(func, b) && reached.insert(b) {
                stack.push(b);
            }
        }
        let mut targets: FxHashSet<Block> = FxHashSet::default();
        for &r in &region {
            let Some(t) = func.layout.last_inst(r) else {
                continue;
            };
            for bc in func.dfg.insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                let d = bc.block(&func.dfg.value_lists);
                if !region.contains(&d) {
                    targets.insert(d);
                }
            }
        }
        while let Some(x) = stack.pop() {
            for p in cfg.pred_iter(x) {
                if !region.contains(&p.block) && reached.insert(p.block) {
                    stack.push(p.block);
                }
            }
        }
        let mut grew = false;
        for t in targets {
            if reached.contains(&t) {
                ready.push(t);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    Some(region)
}

/// Clone `region` (the loop body plus enclosed blocks) in RPO so in-loop
/// defs precede their uses. Insts in `fold` (mapped to a branch-dest
/// slot) become an unconditional `jump` to that destination in the
/// clone. Returns the block map; `bmap[h]` is the clone's header.
fn clone_region(
    func: &mut Function,
    h: Block,
    region: &FxHashSet<Block>,
    fold: &FxHashMap<Inst, usize>,
) -> FxHashMap<Block, Block> {
    let rpo = {
        let mut seen = FxHashSet::default();
        let mut post = Vec::new();
        let mut stack = vec![(h, false)];
        while let Some((b, done)) = stack.pop() {
            if done {
                post.push(b);
                continue;
            }
            if !seen.insert(b) {
                continue;
            }
            stack.push((b, true));
            if let Some(t) = func.layout.last_inst(b) {
                for bc in func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    let d = bc.block(&func.dfg.value_lists);
                    if region.contains(&d) {
                        stack.push((d, false));
                    }
                }
            }
        }
        post.reverse();
        post
    };
    let mut bmap: FxHashMap<Block, Block> = FxHashMap::default();
    let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
    for &b in &rpo {
        let nb = func.dfg.make_block();
        func.layout.append_block(nb);
        if func.layout.is_cold(b) {
            func.layout.set_cold(nb);
        }
        let bparams: Vec<Value> = func.dfg.block_params(b).to_vec();
        for &p in &bparams {
            let ty = func.dfg.value_type(p);
            let np = func.dfg.append_block_param(nb, ty);
            vmap.insert(p, np);
        }
        bmap.insert(b, nb);
    }
    for &b in &rpo {
        let nb = bmap[&b];
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for i in insts {
            if let Some(&slot) = fold.get(&i) {
                let bc = func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[slot];
                let nbc = {
                    let mut cm = CloneMap {
                        func,
                        vmap: &vmap,
                        bmap: &bmap,
                    };
                    cm.map_block_call(bc)
                };
                let args: Vec<BlockArg> = nbc.args(&func.dfg.value_lists).collect();
                let nblk = nbc.block(&func.dfg.value_lists);
                let mut pos = FuncCursor::new(func).at_bottom(nb);
                pos.ins().jump(nblk, &args);
                continue;
            }
            let src_data = func.dfg.insts[i];
            let data = {
                let mut cm = CloneMap {
                    func,
                    vmap: &vmap,
                    bmap: &bmap,
                };
                src_data.map(&mut cm)
            };
            let ni = func.dfg.make_inst(data);
            let ctv = func.dfg.ctrl_typevar(i);
            func.dfg.make_inst_results(ni, ctv);
            func.layout.append_inst(ni, nb);
            for (&o, &n) in func
                .dfg
                .inst_results(i)
                .iter()
                .zip(func.dfg.inst_results(ni).iter())
            {
                vmap.insert(func.dfg.resolve_aliases(o), n);
            }
        }
    }
    bmap
}

/// Redirect every entry edge in `entries` to `g`, keeping its args.
fn retarget_entries(func: &mut Function, entries: &[Edge], g: Block) {
    for e in entries {
        let args: Vec<BlockArg> = func.dfg.insts[e.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[e.slot]
            .args(&func.dfg.value_lists)
            .collect();
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[e.inst]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.slot];
        *bc = BlockCall::new(g, args.iter().copied(), &mut dfg.value_lists);
    }
}

fn run_loop(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    name: &str,
    throw_ret: bool,
    seen: &mut FxHashSet<Block>,
) -> usize {
    let debug = crate::env_set("PLIRON_BCHECK_DEBUG");
    let h = la.loop_header(lp);
    if !seen.insert(h) {
        return 0;
    }
    macro_rules! bail {
        ($why:expr) => {{
            if debug {
                eprintln!("bcheck {name} block{}: {}", h.as_u32(), $why);
            }
            return 0;
        }};
    }
    if !dt.is_reachable(h) {
        bail!("unreachable");
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    let n_insts: usize = body
        .iter()
        .map(|&b| func.layout.block_insts(b).count())
        .sum();
    if n_insts > MAX_BODY {
        bail!("too big");
    }
    let Some(region) = closed_region(func, cfg, &body) else {
        bail!("region too big");
    };
    let edges = edges_to(func, cfg, h);
    let mut entries: Vec<Edge> = Vec::new();
    let mut back = None;
    for e in edges {
        let pb = func.layout.inst_block(e.inst).unwrap();
        if body.contains(&pb) {
            if back.replace(e).is_some() {
                bail!("multiple backedges");
            }
        } else {
            entries.push(e);
        }
    }
    let (Some(back), false) = (back, entries.is_empty()) else {
        bail!("no entry/back edge");
    };
    let latch = func.layout.inst_block(back.inst).unwrap();
    let params = func.dfg.block_params(h).to_vec();
    let bargs: Vec<Option<Value>> = params
        .iter()
        .enumerate()
        .map(|(i, _)| edge_arg(func, &back, i))
        .collect();
    let eargs: Vec<Option<Value>> = params
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let mut it = entries
                .iter()
                .map(|e| edge_arg(func, e, i).map(|v| func.dfg.resolve_aliases(v)));
            let first = it.next().flatten();
            first.filter(|f| it.all(|v| v == Some(*f)))
        })
        .collect();

    // Bound test `brif (icmp cc a, bound)` at `site` whose continue edge is
    // `cs`: for the latch that's the edge back to h; for the header it's the
    // edge staying in the body. Returns (bound, adj, inst) where the last
    // counter value entering the body is `bound - adj`.
    let bound_test = |site: Block, p: Value, ba: Value| -> Option<(Value, i64, Inst)> {
        let i = func.layout.last_inst(site)?;
        if func.dfg.insts[i].opcode() != Opcode::Brif {
            return None;
        }
        let dests =
            func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        let in_latch = site == latch;
        let cs = if in_latch {
            dests
                .iter()
                .position(|bc| bc.block(&func.dfg.value_lists) == h)?
        } else {
            let mut cs = None;
            for (s, bc) in dests.iter().enumerate() {
                if body.contains(&bc.block(&func.dfg.value_lists)) {
                    if cs.replace(s).is_some() {
                        return None;
                    }
                }
            }
            cs?
        };
        if body.contains(&dests[1 - cs].block(&func.dfg.value_lists)) {
            return None;
        }
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[i] else {
            return None;
        };
        let ci = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst()?;
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: [x, y],
        } = func.dfg.insts[ci]
        else {
            return None;
        };
        // Normalize to the continue-condition.
        let cond = if cs == 0 { cond } else { cond.complement() };
        for (a, b, cc) in [(x, y, cond), (y, x, cond.swap_args())] {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            // At the latch, testing `next` (ba = iv+1) bounds the next trip:
            // `next < bound` -> last iv is bound-1. Testing `p` bounds the
            // *following* iteration, so the body can still see iv = bound.
            // At the header the test gates the body directly.
            let adj = match (cc, in_latch, a == ba, a == p) {
                (IntCC::UnsignedLessThan, true, true, _) => 1,
                (IntCC::UnsignedLessThanOrEqual, true, true, _) => 0,
                (IntCC::UnsignedLessThan, true, false, true) => 0,
                (IntCC::UnsignedLessThanOrEqual, true, false, true) => -1,
                (IntCC::UnsignedLessThan, false, _, true) => 1,
                (IntCC::UnsignedLessThanOrEqual, false, _, true) => 0,
                _ => continue,
            };
            return Some((b, adj, i));
        }
        None
    };

    // Find the counting param: stepped `+1`, with a bound test at the latch
    // or the header.
    let mut info: Option<LoopInfo> = None;
    'iv: for (idx, &p) in params.iter().enumerate() {
        let ty = func.dfg.value_type(p);
        if !ty.is_int() || ty.bits() > 64 {
            continue;
        }
        let Some(ba) = bargs[idx] else { continue };
        let Some(istep) = step_of(func, p, ba) else {
            continue;
        };
        // iv's entry value is a constant only when all entry edges agree.
        let mut iv0k = None;
        let mut same = true;
        for e in &entries {
            match edge_arg(func, e, idx).and_then(|v| iconst(func, v)) {
                Some(c) if iv0k.is_none_or(|k| k == c) => iv0k = Some(c),
                _ => {
                    same = false;
                    break;
                }
            }
        }
        if !same {
            iv0k = None;
        }
        for site in [latch, h] {
            let Some((bound, adj, _)) = bound_test(site, p, ba) else {
                continue;
            };
            let mut lo = LoopInfo {
                iv: p,
                iv0: idx,
                iv0k,
                bound,
                adj,
                stepped: FxHashMap::default(),
                g: None,
                h,
                bargs: bargs.clone(),
                eargs: eargs.clone(),
            };
            if !invariant(func, cfg, &lo, &body, bound)
                || matches!(istep, Step::V(sv) if !invariant(func, cfg, &lo, &body, sv))
            {
                continue;
            }
            for (j, &q) in params.iter().enumerate() {
                if j == idx {
                    continue;
                }
                let Some(qa) = bargs[j] else { continue };
                let Some(s) = step_of(func, q, qa) else {
                    continue;
                };
                let inv = match s {
                    Step::K(_) => true,
                    Step::V(sv) => invariant(func, cfg, &lo, &body, sv),
                };
                if inv {
                    lo.stepped.insert(q, (j, s));
                }
            }
            info = Some(lo);
            break 'iv;
        }
    }
    // No `+1` counting iv? The header's own `p < B` test still bounds `p`
    // on every in-body iteration, so an interior `(p + k) < len` check is
    // implied whenever `B - adj + k < len` — provable once in the guard
    // block with no trip count. `bsub_iv` marks the fallback: checks
    // matching `p`/`p + k` take the bound guard instead of affine `hi`.
    let (mut info, bsub_iv) = match info {
        Some(i) => (i, None),
        None => {
            let mut found = None;
            for (ix, &p) in params.iter().enumerate() {
                let ty = func.dfg.value_type(p);
                if !ty.is_int() || ty.bits() > 64 {
                    continue;
                }
                let Some((bound, adj, _)) = bound_test(h, p, bargs[ix].unwrap_or(p)) else {
                    continue;
                };
                let lo = LoopInfo {
                    iv: p,
                    iv0: ix,
                    iv0k: None,
                    bound,
                    adj,
                    stepped: FxHashMap::default(),
                    g: None,
                    h,
                    bargs: bargs.clone(),
                    eargs: eargs.clone(),
                };
                if invariant(func, cfg, &lo, &body, bound) {
                    found = Some(lo);
                    break;
                }
            }
            let Some(lo) = found else {
                bail!("no counting iv")
            };
            let iv = lo.iv;
            (lo, Some(iv))
        }
    };

    // The loop's own bound tests are never foldable: their "bad" edge is
    // the exit path, not a redundant panic — folding the test removes the
    // exit and turns the loop infinite even when the edge is dead.
    let mut trip_tests: FxHashSet<Inst> = FxHashSet::default();
    {
        let iv = info.iv;
        let ba = bargs[info.iv0].unwrap_or(iv);
        for site in [latch, h] {
            if let Some((_, _, ti)) = bound_test(site, iv, ba) {
                trip_tests.insert(ti);
            }
        }
    }

    // Foldable checks: `brif (icmp cc idx, len)` where idx is affine in the
    // counter, len invariant, and the unsafe edge is a cold (panic) block.
    let mut checks: Vec<Chk> = Vec::new();
    let mut total_checks = 0usize;
    'blk: for &b in &body {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        if func.dfg.insts[t].opcode() != Opcode::Brif || trip_tests.contains(&t) {
            continue;
        }
        let to_cold = func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .any(|bc| func.layout.is_cold(bc.block(&func.dfg.value_lists)));
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[t] else {
            if to_cold {
                total_checks += 1;
            }
            continue;
        };
        // A `brif iconst` to cold is already folded (e.g. by celim after the
        // count test proves the check); it costs nothing in the clone, so it
        // doesn't count as a residual check for partial versioning.
        if to_cold && iconst(func, c).is_none() {
            total_checks += 1;
        }
        let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst() else {
            continue;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: [x, y],
        } = func.dfg.insts[ci]
        else {
            continue;
        };
        for (a, bb, cc) in [(x, y, cond), (y, x, cond.swap_args())] {
            let (a, bb) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(bb));
            let (safe_true, le) = match cc {
                IntCC::UnsignedLessThan => (true, false),
                IntCC::UnsignedLessThanOrEqual => (true, true),
                IntCC::UnsignedGreaterThanOrEqual => (false, false),
                IntCC::UnsignedGreaterThan => (false, true),
                _ => continue,
            };
            if !invariant(func, cfg, &info, &body, bb) {
                if debug {
                    eprintln!(
                        "bcheck {name} block{}: blk{} len not invariant",
                        h.as_u32(),
                        b.as_u32()
                    );
                }
                continue;
            }
            // Bound-subsumption: `a` is `iv` or `iv + k` (k >= 0) — the
            // header's bound test caps it without a trip count.
            let mut bsub = None;
            if let Some(p) = bsub_iv {
                if a == p {
                    bsub = Some(0);
                } else if let Some(i) = func.dfg.value_def(a).inst()
                    && let InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args: [x, y],
                    } = func.dfg.insts[i]
                    && let Some(k) = (func.dfg.resolve_aliases(x) == p)
                        .then(|| iconst(func, y))
                        .or_else(|| (func.dfg.resolve_aliases(y) == p).then(|| iconst(func, x)))
                        .flatten()
                    && k >= 0
                {
                    bsub = Some(k);
                }
            }
            let aff = if bsub.is_some() {
                Aff {
                    m: Lin::K(0),
                    b: SmallVec::new(),
                }
            } else if bsub_iv.is_some() {
                // Fallback mode: `affine` assumes a unit-step iv, which `p`
                // is not — only `p + k` (handled above) and constant
                // `ubound` caps are sound here.
                match ubound(func, a).filter(|_| ubound_on()) {
                    Some(k) => Aff {
                        m: Lin::K(0),
                        b: [Term {
                            v: Lin::K(k as i64),
                            k: 1,
                        }]
                        .into_iter()
                        .collect(),
                    },
                    None => continue,
                }
            } else {
                match affine(func, cfg, &info, &body, MAX_AFFINE_DEPTH, a) {
                    Some(af) => af,
                    // (x64 wt note) data-dependent but range-limited index:
                    // `cnt[a[i]]` with a u8 `a` is `hi = 255`, not affine. A
                    // `ubound` cap synthesizes a constant Aff so the guard is
                    // `tymax < len` — hist's exact version.
                    None if ubound_on() => match ubound(func, a) {
                        Some(k) => Aff {
                            m: Lin::K(0),
                            b: [Term {
                                v: Lin::K(k as i64),
                                k: 1,
                            }]
                            .into_iter()
                            .collect(),
                        },
                        None => continue,
                    },
                    None => continue,
                }
            };
            let dests = func.dfg.insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            let safe_slot = if safe_true { 0 } else { 1 };
            let bad = dests[1 - safe_slot].block(&func.dfg.value_lists);
            if !dead_end(func, bad, throw_ret) {
                if debug {
                    eprintln!(
                        "bcheck {name} block{}: blk{} bad edge not dead (bsub={:?})",
                        h.as_u32(),
                        b.as_u32(),
                        bsub
                    );
                }
                continue;
            }
            checks.push(Chk {
                inst: t,
                safe_slot,
                aff,
                le,
                len: bb,
                bsub,
            });
            continue 'blk;
        }
        if checks.len() >= MAX_CHECKS {
            break;
        }
    }
    if checks.is_empty() {
        bail!("no foldable checks");
    }
    // A single foldable check that leaves other checks behind isn't always
    // worth a loop clone: the fast copy still pays a cold branch per
    // iteration, and the tighter loop can actually be slower — in an RMW
    // loop the removed branch's issue slots keep same-address store->load
    // forwarding from replaying (hist same-key: ~0.9ms unversioned vs
    // ~1.3ms with the check removed, though random-key hist improves
    // ~28%). The hazard needs a load that can re-read a slot a store
    // wrote, so it's only a risk when the body has BOTH a non-affine store
    // AND a non-affine load (hist's `cnt[idx]` on both sides). A scatter
    // (`out[idx[i]] = val[i]`) has a non-affine store but only affine
    // loads — in safe-Rust aliasing terms no iteration can read back a
    // scattered slot — so versioning it is safe. This is a perf heuristic,
    // not a correctness gate: PLIRON_BCHECK_PARTIAL overrides entirely.
    // (x64 wt note: widened the original all-stores-affine rule to this
    // store+load test — merge-flag for native's bcheck.rs.)
    if checks.len() < total_checks && checks.len() < 2 && !crate::env_set("PLIRON_BCHECK_PARTIAL") {
        let mut dyn_store = false;
        let mut dyn_load = false;
        for &b in &body {
            for i in func.layout.block_insts(b) {
                match func.dfg.insts[i] {
                    InstructionData::Store { args, .. } => {
                        let ok = affine(func, cfg, &info, &body, MAX_AFFINE_DEPTH, args[1])
                            .is_some_and(|af| !matches!(af.m, Lin::K(0)));
                        if !ok {
                            dyn_store = true;
                        }
                    }
                    InstructionData::Load { arg, .. } => {
                        let ok = affine(func, cfg, &info, &body, MAX_AFFINE_DEPTH, arg)
                            .is_some_and(|af| !matches!(af.m, Lin::K(0)));
                        if !ok {
                            dyn_load = true;
                        }
                    }
                    _ => {
                        let op = func.dfg.insts[i].opcode();
                        // Atomics/memcpy/etc.: treat any exotic memory op as
                        // both sides of a potential forwarding chain.
                        if op.can_store() {
                            dyn_store = true;
                        }
                        if op.can_load() {
                            dyn_load = true;
                        }
                    }
                }
            }
        }
        if dyn_store && dyn_load {
            bail!("single partial check with possible same-addr store->load chain");
        }
    }

    // Guard/preheader block g takes h's params verbatim; every entry edge
    // is retargeted to it, so its params are the canonical entry values.
    let ity = func.dfg.value_type(info.iv);
    let wty = if ity == types::I32 {
        types::I64
    } else {
        types::I128
    };
    let gtys: Vec<Type> = func
        .dfg
        .block_params(h)
        .iter()
        .map(|&p| func.dfg.value_type(p))
        .collect();
    // When every contribution to `hi` is a non-negative unsigned quantity,
    // the wrap-catching semantics of the wide guard fall out of native
    // add/sub/mul carry flags: emit it in `ity` and AND in `!carry` — a
    // fraction of the i128 dependency chain.
    let umax: u64 = if ity.bits() < 64 {
        (1u64 << ity.bits()) - 1
    } else {
        u64::MAX
    };
    let kfits = |k: i64| k >= 0 && (k as u64) <= umax;
    // `gval` rematerializes constants at the emit type but passes other
    // loop-invariant operands through at their own type — which can be
    // wider than the iv's when the check (or a stepped param) works in a
    // wider index type. The narrow emission types every runtime operand
    // `ity`; the wide one extends each to `wty`. A check whose operands
    // fit neither can't be emitted — drop it before the guard exists (it
    // stays counted in `total_checks`, so the partial-versioning
    // accounting still treats it as residual).
    let vty = |func: &Function, v: Value| func.dfg.value_type(func.dfg.resolve_aliases(v));
    let narrowable = |func: &Function, c: &Chk| -> bool {
        if ity == types::I128 {
            return false;
        }
        let l_ok = |l: &Lin| match l {
            Lin::K(_) => true,
            // A constant term is rematerialized masked to `ity`; require
            // it to fit so the emitted value equals the modeled one.
            Lin::V(v) | Lin::N(v) => match iconst(func, *v) {
                Some(k) => kfits(k),
                None => vty(func, *v) == ity,
            },
            Lin::P(i) => gtys[*i] == ity,
        };
        // `len` is masked-const or compared at `ity` verbatim.
        if iconst(func, c.len).is_none() && vty(func, c.len) != ity {
            return false;
        }
        if c.bsub.is_some() {
            return true;
        }
        let m_ok = match c.aff.m {
            Lin::K(k) => kfits(k),
            Lin::V(_) | Lin::P(_) => l_ok(&c.aff.m),
            Lin::N(_) => false,
        };
        m_ok && c.aff.b.iter().all(|t| {
            l_ok(&t.v)
                && match t.v {
                    Lin::K(kv) => kv
                        .checked_mul(t.k)
                        .is_some_and(|p| p >= 0 && (p as u64) <= umax),
                    Lin::V(_) | Lin::P(_) => kfits(t.k),
                    Lin::N(_) => false,
                }
        })
    };
    let fits_wide = |func: &Function, c: &Chk| -> bool {
        let fits = |v: Value| {
            iconst(func, v).is_some()
                || {
                    let t = vty(func, v);
                    t.is_int() && !t.is_vector() && t.bits() <= wty.bits()
                }
        };
        let l_fits = |l: &Lin| match l {
            Lin::K(_) => true,
            Lin::V(v) | Lin::N(v) => fits(*v),
            Lin::P(i) => gtys[*i].is_int() && gtys[*i].bits() <= wty.bits(),
        };
        fits(c.len) && l_fits(&c.aff.m) && c.aff.b.iter().all(|t| l_fits(&t.v))
    };
    checks.retain(|c| narrowable(func, c) || fits_wide(func, c));
    if checks.is_empty() {
        bail!("no emit-compatible checks");
    }
    let g = func.dfg.make_block();
    let pb = func.layout.inst_block(entries[0].inst).unwrap();
    func.layout.insert_block_after(g, pb);
    let gp: Vec<Value> = gtys
        .iter()
        .map(|&t| func.dfg.append_block_param(g, t))
        .collect();
    info.g = Some(g);

    // A loop-invariant value usable at g: constants are rematerialized, an
    // unchanged header param becomes its entry value (the uniform entry
    // arg when known — it dominates g — else g's opaque param).
    let gval = |info: &LoopInfo, gp: &[Value], pos: &mut FuncCursor, v: Value| -> Value {
        let v = pos.func.dfg.resolve_aliases(v);
        let v = if let Some(b) = def_block(pos.func, v)
            && b == info.h
            && let cranelift_codegen::ir::ValueDef::Param(_, n) = pos.func.dfg.value_def(v)
        {
            info.eargs.get(n).copied().flatten().unwrap_or(gp[n])
        } else {
            v
        };
        match iconst(pos.func, v) {
            Some(k) => iconst_masked(pos, ity, k),
            None => v,
        }
    };
    let mut pos = FuncCursor::new(func).at_bottom(g);
    let zext = |pos: &mut FuncCursor, v: Value| -> Value {
        // `fits_wide` already capped operands at `wty`; a value already
        // there passes through (`uextend` needs a strictly narrower src).
        if pos.func.dfg.value_type(v) == wty {
            v
        } else {
            pos.ins().uextend(wty, v)
        }
    };
    let boundv = gval(&info, &gp, &mut pos, info.bound);
    // Trip-count terms are only needed by affine checks. `t` is the last
    // counter value, in the index type; the wide path zextends it. `t_k`
    // is `t` when `bound`/`iv0` are entry constants — an `m*t` extremal
    // then folds to a literal and the `hi + s` combine needs no runtime
    // overflow flag (`s + C < len` ⇔ `s < len.saturating_sub(C)`).
    let need_affine = checks.iter().any(|c| c.bsub.is_none());
    let (t, t_k) = if need_affine {
        let iv0v = gval(&info, &gp, &mut pos, info.iv);
        let d = pos.ins().isub(boundv, iv0v);
        let t = match info.adj.cmp(&0) {
            core::cmp::Ordering::Equal => d,
            core::cmp::Ordering::Greater => {
                let k = pos.ins().iconst(ity, info.adj);
                pos.ins().isub(d, k)
            }
            core::cmp::Ordering::Less => {
                let k = pos.ins().iconst(ity, -info.adj);
                pos.ins().iadd(d, k)
            }
        };
        let tk = match (iconst(pos.func, boundv), iconst(pos.func, iv0v)) {
            (Some(b), Some(i)) => {
                Some((b.wrapping_sub(i).wrapping_sub(info.adj)) as u64 & umax)
            }
            _ => None,
        };
        (Some(t), tk)
    } else {
        (None, None)
    };

    // `x < len.saturating_sub(k)` for each `x + k < len` check: `len > k`
    // makes it `len - k`, `len <= k` makes the check fail unconditionally
    // just like `!of && x + k < len` (the sum can then only be < len by
    // wrapping). Emitted once at the outermost enclosing loop's preheader
    // when `len` dominates it — the alternative is a fresh usub+select in
    // the guard per inner-loop entry (matmul's k-loop head).
    let lim_pre = {
        let mut outer = None;
        let mut cur = la.loop_parent(lp);
        while let Some(p) = cur {
            outer = Some(p);
            cur = la.loop_parent(p);
        }
        outer.and_then(|o| {
            let oh = la.loop_header(o);
            edges_to(pos.func, cfg, oh)
                .iter()
                .map(|e| pos.func.layout.inst_block(e.inst).unwrap())
                .find(|&b| !la.is_in_loop(b, o))
        })
    };
    let mut lims: FxHashMap<(Value, u64), Value> = FxHashMap::default();
    let mut lim_of = |func: &mut Function, lenv: Value, k: u64| -> Value {
        *lims.entry((lenv, k)).or_insert_with(|| {
            let term = lim_pre
                .filter(|&pre| {
                    def_block(func, lenv)
                        .is_some_and(|lb| dt.dominates(lb, pre, &func.layout))
                })
                .and_then(|pre| func.layout.last_inst(pre));
            let mut at = match term {
                Some(t) => FuncCursor::new(func).at_inst(t),
                None => FuncCursor::new(func).at_bottom(g),
            };
            // `umax(len, k) - k` — saturating-sub without the select's
            // flag result to keep live across the inner loop.
            let kk = iconst_masked(&mut at, ity, k as i64);
            let hi = at.ins().umax(lenv, kk);
            at.ins().isub(hi, kk)
        })
    };

    // Wide values are only needed when some check can't go narrow.
    let any_wide = checks.iter().any(|c| !narrowable(pos.func, c));
    let (bw, tw, zero) = if any_wide {
        (
            Some(zext(&mut pos, boundv)),
            t.map(|t| zext(&mut pos, t)),
            Some(iconst_masked(&mut pos, wty, 0)),
        )
    } else {
        (None, None, None)
    };
    let mut flags: Vec<Value> = Vec::new();
    let mut ok: Option<Value> = None;
    for c in &checks {
        let lenv = gval(&info, &gp, &mut pos, c.len);
        let cc = if c.le {
            IntCC::UnsignedLessThanOrEqual
        } else {
            IntCC::UnsignedLessThan
        };
        if let Some(k) = c.bsub {
            // `iv + k < len` is implied by `B - adj + k < len`; `wty`
            // keeps the bound arithmetic wrap-free.
            let d = info.adj.wrapping_sub(k);
            let g = if narrowable(pos.func, c) {
                if let Some(bk) = iconst(pos.func, boundv) {
                    // Constant bound: the adjust is a fold — the compare
                    // alone remains (no overflow machinery to emit).
                    let kb = bk as u64 & umax;
                    let dd = d.unsigned_abs() & umax;
                    let (bhv, ovf) = if d >= 0 {
                        (kb.wrapping_sub(dd) & umax, kb < dd)
                    } else {
                        (kb.wrapping_add(dd) & umax, dd > umax - kb)
                    };
                    if ovf {
                        flags.push(pos.ins().iconst(types::I8, 1));
                    }
                    let bh = iconst_masked(&mut pos, ity, bhv as i64);
                    pos.ins().icmp(cc, bh, lenv)
                } else {
                    let (bh, of) = if d >= 0 {
                        let ka = iconst_masked(&mut pos, ity, d);
                        pos.ins().usub_overflow(boundv, ka)
                    } else {
                        let ka = iconst_masked(&mut pos, ity, d.wrapping_neg());
                        pos.ins().uadd_overflow(boundv, ka)
                    };
                    flags.push(of);
                    pos.ins().icmp(cc, bh, lenv)
                }
            } else {
                let ka = iconst_masked(&mut pos, wty, d);
                let bh = pos.ins().isub(bw.unwrap(), ka);
                let lenw = zext(&mut pos, lenv);
                pos.ins().icmp(cc, bh, lenw)
            };
            ok = Some(match ok {
                None => g,
                Some(o) => pos.ins().band(o, g),
            });
            continue;
        }
        if narrowable(pos.func, c) {
            let t = t.unwrap();
            // `m*t` as a machine constant: `s + C < len` then needs no
            // overflow flag — it's `s < len.saturating_sub(C)` — and the
            // multiply drops out entirely.
            let hi_k = t_k.and_then(|tk| match c.aff.m {
                Lin::K(m) => (m as u64).checked_mul(tk),
                _ => None,
            });
            let mut hi = match c.aff.m {
                Lin::K(0) => iconst_masked(&mut pos, ity, 0),
                Lin::K(_) if hi_k.is_some() => {
                    iconst_masked(&mut pos, ity, hi_k.unwrap() as i64)
                }
                Lin::K(1) => t,
                Lin::K(k) => {
                    let kv = iconst_masked(&mut pos, ity, k);
                    let (r, of) = pos.ins().umul_overflow(t, kv);
                    flags.push(of);
                    r
                }
                Lin::V(v) => {
                    let v = gval(&info, &gp, &mut pos, v);
                    let (r, of) = pos.ins().umul_overflow(v, t);
                    flags.push(of);
                    r
                }
                Lin::P(i) => {
                    let (r, of) = pos.ins().umul_overflow(gp[i], t);
                    flags.push(of);
                    r
                }
                Lin::N(_) => unreachable!("narrowable excludes N terms"),
            };
            let mut sum_b: Option<Value> = None;
            for tm in &c.aff.b {
                let term = match tm.v {
                    Lin::K(kv) => {
                        let p = kv.checked_mul(tm.k).unwrap_or(0);
                        if p == 0 {
                            continue;
                        }
                        iconst_masked(&mut pos, ity, p)
                    }
                    Lin::V(v) => {
                        let v = gval(&info, &gp, &mut pos, v);
                        if tm.k == 0 {
                            continue;
                        }
                        if tm.k == 1 {
                            v
                        } else {
                            let k = iconst_masked(&mut pos, ity, tm.k);
                            let (r, of) = pos.ins().umul_overflow(v, k);
                            flags.push(of);
                            r
                        }
                    }
                    Lin::P(i) => {
                        if tm.k == 0 {
                            continue;
                        }
                        if tm.k == 1 {
                            gp[i]
                        } else {
                            let k = iconst_masked(&mut pos, ity, tm.k);
                            let (r, of) = pos.ins().umul_overflow(gp[i], k);
                            flags.push(of);
                            r
                        }
                    }
                    Lin::N(_) => unreachable!("narrowable excludes N terms"),
                };
                sum_b = Some(match sum_b {
                    None => term,
                    Some(s) => {
                        let (r, of) = pos.ins().uadd_overflow(s, term);
                        flags.push(of);
                        r
                    }
                });
            }
            let g = match (sum_b, hi_k) {
                // `s + 0 <cmp> len` — the add is identity, no flag.
                (Some(s), Some(0)) => pos.ins().icmp(cc, s, lenv),
                // `s + C < len` ⇔ `s < len.saturating_sub(C)`; `ule` folds
                // into the limit: `s + C <= len` ⇔ `s < len - (C - 1)`.
                (Some(s), Some(ck)) => {
                    let lim = lim_of(&mut pos.func, lenv, ck - u64::from(c.le));
                    pos.ins().icmp(IntCC::UnsignedLessThan, s, lim)
                }
                _ => {
                    if let Some(s) = sum_b {
                        let (r, of) = pos.ins().uadd_overflow(hi, s);
                        flags.push(of);
                        hi = r;
                    }
                    pos.ins().icmp(cc, hi, lenv)
                }
            };
            ok = Some(match ok {
                None => g,
                Some(o) => pos.ins().band(o, g),
            });
            continue;
        }
        let tw = tw.unwrap();
        let zero = zero.unwrap();
        let dec = match c.aff.m {
            // idx(i) = m*i + sum_b over true ints. m < 0 (K<0 or N) makes the
            // sequence decrease: the max is the entry value sum_b and the min
            // is the last-iteration hi. Otherwise the max is hi and the min
            // is sum_b.
            Lin::K(k) => k < 0,
            Lin::N(_) => true,
            Lin::V(_) | Lin::P(_) => false,
        };
        let m = match c.aff.m {
            Lin::K(k) => iconst_masked(&mut pos, wty, k),
            Lin::V(v) | Lin::N(v) => {
                let v = gval(&info, &gp, &mut pos, v);
                let v = zext(&mut pos, v);
                if let Lin::N(_) = c.aff.m {
                    pos.ins().ineg(v)
                } else {
                    v
                }
            }
            Lin::P(i) => zext(&mut pos, gp[i]),
        };
        let mut hi = pos.ins().imul(m, tw);
        let mut sum_b = zero;
        for tm in &c.aff.b {
            let v = match tm.v {
                Lin::K(k) => iconst_masked(&mut pos, wty, k),
                Lin::V(v) | Lin::N(v) => {
                    let v = gval(&info, &gp, &mut pos, v);
                    let v = zext(&mut pos, v);
                    if let Lin::N(_) = tm.v {
                        pos.ins().ineg(v)
                    } else {
                        v
                    }
                }
                Lin::P(i) => zext(&mut pos, gp[i]),
            };
            let term = if tm.k == 1 {
                v
            } else {
                let k = iconst_masked(&mut pos, wty, tm.k);
                pos.ins().imul(v, k)
            };
            sum_b = pos.ins().iadd(sum_b, term);
        }
        hi = pos.ins().iadd(hi, sum_b);
        let lenw = zext(&mut pos, lenv);
        let (minv, maxv) = if dec { (hi, sum_b) } else { (sum_b, hi) };
        // The max over the trip range must be < len; the min must be >= 0
        // (a dip below 0 wraps to a huge machine value that would panic).
        // Skip the min test when no term can go negative — the common
        // `i < len` guard stays a single compare.
        let may_neg = dec
            || c.aff.b.iter().any(|t| match t.v {
                Lin::V(_) | Lin::P(_) => t.k < 0,
                Lin::N(_) => t.k > 0,
                Lin::K(kv) => kv.checked_mul(t.k).is_none_or(|p| p < 0),
            });
        let g1 = pos.ins().icmp(cc, maxv, lenw);
        let g = if may_neg {
            let g2 = pos.ins().icmp(IntCC::SignedGreaterThanOrEqual, minv, zero);
            pos.ins().band(g1, g2)
        } else {
            g1
        };
        ok = Some(match ok {
            None => g,
            Some(o) => pos.ins().band(o, g),
        });
    }
    let mut ok = ok.unwrap();
    if !flags.is_empty() {
        let mut b = flags[0];
        for &f in &flags[1..] {
            b = pos.ins().bor(b, f);
        }
        let z8 = pos.ins().iconst(types::I8, 0);
        let nb = pos.ins().icmp(IntCC::Equal, b, z8);
        ok = pos.ins().band(ok, nb);
    }

    // Clone the region blocks in RPO so in-loop defs precede their uses;
    // each check's brif folds to its safe slot in the clone.
    let fold: FxHashMap<Inst, usize> = checks.iter().map(|c| (c.inst, c.safe_slot)).collect();
    let bmap = clone_region(func, h, &region, &fold);
    // Redirect every entry edge through the guard, keeping its args.
    retarget_entries(func, &entries, g);
    let gpargs: Vec<BlockArg> = gp.iter().map(|&v| BlockArg::from(v)).collect();
    let mut pos = FuncCursor::new(func).at_bottom(g);
    let hc = bmap[&h];
    pos.ins().brif(ok, hc, &gpargs, h, &gpargs);

    // Cloned panic blocks are unreachable now; drop anything that lost all
    // predecessors so stale blocks don't keep phantom CFG edges.
    let cfg2 = ControlFlowGraph::with_function(func);
    let dead: Vec<Block> = bmap
        .values()
        .copied()
        .filter(|&b| cfg2.pred_iter(b).next().is_none())
        .collect();
    for b in dead {
        func.layout.remove_block_and_insts(b);
    }
    seen.insert(hc);
    1
}

pub fn run(
    func: &mut Function,
    an: &mut crate::pipeline::Analysis,
    name: &str,
    throw_ret: bool,
) -> usize {
    let debug = crate::env_set("PLIRON_BCHECK_DEBUG");
    let mut n = 0;
    // Analyses go stale after each versioning, so recompute per iteration:
    // find the first loop that versions, then rescan.
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    for _ in 0..MAX_VERSIONS {
        let crate::pipeline::CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
        let mut did = 0;
        let lps = deepest_first(la);
        if debug {
            eprintln!(
                "bcheck {name}: {} loops: {:?}",
                lps.len(),
                lps.iter().map(|&lp| la.loop_header(lp)).collect::<Vec<_>>()
            );
        }
        for lp in lps.into_iter().take(MAX_LOOPS) {
            let k = run_loop(func, cfg, dt, la, lp, name, throw_ret, &mut seen);
            did += k;
            if k > 0 {
                an.invalidate();
                break; // cfg/dt/la now stale; rescan
            }
        }
        n += did;
        if did == 0 {
            break;
        }
        if debug {
            eprintln!("bcheck {name}: versioned");
        }
    }
    n
}

/// Loop unswitching on an invariant `brif`.
///
/// A `brif` inside a loop whose condition is loop-invariant (defined
/// outside the body, or a header param the backedges pass through
/// unchanged) runs the same test every iteration — rustc's inlined
/// `HashMap::get` carries `capacity == 0` like this. Clone the loop so
/// each version sees a constant outcome: the clone's copy of the brif
/// folds to its taken edge, the original's to the fallthrough, and one
/// `brif` in a new guard block picks between them. Only a brif that
/// dominates every backedge is worth a whole-loop clone — anything
/// shallower may not run each iteration.
const MAX_UNSWITCH_BODY: usize = 320;

fn unswitch_loop(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    name: &str,
    seen: &mut FxHashSet<Block>,
) -> usize {
    let debug = crate::env_set("PLIRON_UNSWITCH_DEBUG");
    let h = la.loop_header(lp);
    if !seen.insert(h) {
        return 0;
    }
    macro_rules! bail {
        ($why:expr) => {{
            if debug {
                eprintln!("unswitch {name} block{}: {}", h.as_u32(), $why);
            }
            return 0;
        }};
    }
    if !dt.is_reachable(h) {
        bail!("unreachable");
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    let n_insts: usize = body
        .iter()
        .map(|&b| func.layout.block_insts(b).count())
        .sum();
    if n_insts > MAX_UNSWITCH_BODY {
        bail!("too big");
    }
    let edges = edges_to(func, cfg, h);
    let mut entries: Vec<Edge> = Vec::new();
    let mut backs: Vec<Edge> = Vec::new();
    for e in edges {
        let pb = func.layout.inst_block(e.inst).unwrap();
        if body.contains(&pb) {
            backs.push(e);
        } else {
            entries.push(e);
        }
    }
    if backs.is_empty() || entries.is_empty() {
        bail!("no entry/back edge");
    }
    let params = func.dfg.block_params(h).to_vec();
    let Some(region) = closed_region(func, cfg, &body) else {
        bail!("region too big");
    };
    // A `brif c` at a body-block end with loop-invariant `c`, dominating
    // every backedge source (guaranteed per-iteration). `c` must also be
    // evaluable in the guard: defined outside the whole clone region (a
    // def in an enclosed non-body block is still inside the loop's
    // reach) or a header param the backedges pass through unchanged.
    let invc = |func: &Function, v: Value| -> bool {
        let v = func.dfg.resolve_aliases(v);
        if iconst(func, v).is_some() {
            return false; // an iconst brif is jumpthread's fold, not ours
        }
        match def_block(func, v) {
            Some(b) if b == h => {
                if let cranelift_codegen::ir::ValueDef::Param(_, n) = func.dfg.value_def(v) {
                    backs
                        .iter()
                        .all(|e| edge_arg(func, e, n).is_some_and(|a| a == v))
                } else {
                    false
                }
            }
            Some(b) => !region.contains(&b),
            None => true,
        }
    };
    let mut cond: Option<Value> = None;
    'blk: for b in func.layout.blocks().filter(|b| body.contains(b)) {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        if !backs
            .iter()
            .all(|e| dt.block_dominates(b, func.layout.inst_block(e.inst).unwrap()))
        {
            continue;
        }
        let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else {
            continue;
        };
        if invc(func, arg) {
            cond = Some(func.dfg.resolve_aliases(arg));
            break 'blk;
        }
    }
    let Some(cond) = cond else {
        bail!("no invariant brif");
    };
    // Fold every brif on the same condition in the region, not just the
    // candidate — duplicated checks are common after inlining.
    let mut fold: FxHashMap<Inst, usize> = FxHashMap::default();
    for &b in &region {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        if let InstructionData::Brif { arg, .. } = func.dfg.insts[t]
            && func.dfg.resolve_aliases(arg) == cond
        {
            fold.insert(t, 0);
        }
    }
    // `cond` usable in the guard block: a header param becomes its entry
    // value (g's param at the same index).
    let g = func.dfg.make_block();
    let pb = func.layout.inst_block(entries[0].inst).unwrap();
    func.layout.insert_block_after(g, pb);
    let gtys: Vec<Type> = params.iter().map(|&p| func.dfg.value_type(p)).collect();
    let gp: Vec<Value> = gtys
        .iter()
        .map(|&t| func.dfg.append_block_param(g, t))
        .collect();
    let gcond = match def_block(func, cond) {
        Some(b) if b == h => {
            if let cranelift_codegen::ir::ValueDef::Param(_, n) = func.dfg.value_def(cond) {
                gp[n]
            } else {
                cond
            }
        }
        _ => cond,
    };
    // Clone with every matched brif folded to slot 0 (cond true); the
    // original folds them to slot 1.
    let bmap = clone_region(func, h, &region, &fold);
    for &b in &region {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        if !fold.contains_key(&t) {
            continue;
        }
        let bc = func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[1];
        let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
        let d = bc.block(&func.dfg.value_lists);
        func.layout.remove_inst(t);
        let mut pos = FuncCursor::new(func).at_bottom(b);
        pos.ins().jump(d, &args);
    }
    retarget_entries(func, &entries, g);
    let gpargs: Vec<BlockArg> = gp.iter().map(|&v| BlockArg::from(v)).collect();
    let mut pos = FuncCursor::new(func).at_bottom(g);
    let hc = bmap[&h];
    pos.ins().brif(gcond, hc, &gpargs, h, &gpargs);
    // Drop region blocks (either side) the folds made unreachable. A
    // no-preds sweep isn't enough: a dead fragment can be a self-
    // referencing cycle (each member keeping a pred from a sibling)
    // while sharing defs with live blocks — mark what's still reachable
    // from the entry and remove the rest.
    let cfg2 = ControlFlowGraph::with_function(func);
    let mut live: FxHashSet<Block> = FxHashSet::default();
    if let Some(e) = func.layout.entry_block() {
        let mut stack = vec![e];
        while let Some(b) = stack.pop() {
            if !live.insert(b) {
                continue;
            }
            for s in cfg2.succ_iter(b) {
                stack.push(s);
            }
        }
    }
    let mut dead: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|b| !live.contains(b))
        .collect();
    // Sweep everything unreachable from the entry, not just region
    // blocks and clones: a pre-existing unreachable block (e.g. a cold
    // try_call landing pad) can branch to or use values defined in a
    // fold-orphaned region block, so removing only the region leaves a
    // dangling BlockCall (`rustc_borrowck` `kill_all`: `invalid block
    // reference`). Keep anything a surviving block still references —
    // belt-and-braces in case cfg misses an edge type.
    let uses = crate::uses::Uses::new(func);
    if debug {
        eprintln!("unswitch {name}: dead candidates: {dead:?}");
    }
    loop {
        let keep: Vec<Block> = dead
            .iter()
            .copied()
            .filter(|d| {
                cfg2.pred_iter(*d).any(|p| !dead.contains(&p.block))
                    || func
                        .dfg
                        .block_params(*d)
                        .iter()
                        .chain(
                            func.layout
                                .block_insts(*d)
                                .flat_map(|i| func.dfg.inst_results(i)),
                        )
                        .any(|&v| {
                            uses.sites
                                .get(&func.dfg.resolve_aliases(v))
                                .is_some_and(|ss| {
                                    ss.iter().any(|(b, _)| !dead.contains(b))
                                })
                        })
            })
            .collect();
        if keep.is_empty() {
            break;
        }
        for d in keep {
            dead.remove(&d);
        }
    }
    if debug {
        eprintln!("unswitch {name}: dead after fold: {dead:?}");
    }
    for b in dead {
        func.layout.remove_block_and_insts(b);
    }
    seen.insert(hc);
    if debug {
        eprintln!("unswitch {name}: block{} unswitched", h.as_u32());
    }
    1
}

/// `PLIRON_UNSWITCH=0` disables invariant-brif loop unswitching.
pub fn unswitch(func: &mut Function, an: &mut crate::pipeline::Analysis, name: &str) -> usize {
    let debug = crate::env_set("PLIRON_UNSWITCH_DEBUG");
    let mut n = 0;
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    for _ in 0..MAX_VERSIONS {
        let crate::pipeline::CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
        let mut did = 0;
        for lp in la.loops().take(MAX_LOOPS) {
            let k = unswitch_loop(func, cfg, dt, la, lp, name, &mut seen);
            did += k;
            if k > 0 {
                an.invalidate();
                break; // cfg/dt/la now stale; rescan
            }
        }
        n += did;
        if did == 0 {
            break;
        }
        if debug {
            eprintln!("unswitch {name}: versioned");
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::{AbiParam, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;

    fn verify_ok(func: &Function) {
        let flags =
            cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(func, &flags) {
            panic!("verify failed: {e}\n{func}");
        }
    }

    /// Smoke test: unswitch a loop with an unreachable chain referencing
    /// the fold-orphaned fragment. The real regression lives in
    /// `filetests/unswitch_unreachable_refs.clif` — reproducing the
    /// exact `kill_all` shape needs nested-loop `is_in_loop` tagging the
    /// referencer escapes, which a small synthetic can't produce.
    #[test]
    fn unswitch_keeps_dead_target_of_unreachable_edge() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);

        let entry = func.dfg.make_block();
        let u1 = func.dfg.make_block();
        let u2 = func.dfg.make_block();
        let h = func.dfg.make_block();
        let m = func.dfg.make_block();
        let t = func.dfg.make_block();
        let latch = func.dfg.make_block();
        let leak = func.dfg.make_block();
        for b in [entry, u1, u2, h, m, t, latch, leak] {
            func.layout.append_block(b);
        }

        let (cond, v0);
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(entry);
            let c1 = pos.ins().iconst(types::I64, 1);
            let c2 = pos.ins().iconst(types::I64, 2);
            cond = pos.ins().icmp(IntCC::NotEqual, c1, c2);
            v0 = pos.ins().iconst(types::I64, 7);
            pos.ins().jump(h, &[BlockArg::from(v0)]);
        }
        let p = func.dfg.append_block_param(h, types::I64);
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(h);
            pos.ins().jump(m, &[BlockArg::from(p)]);
        }
        let pm = func.dfg.append_block_param(m, types::I64);
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(m);
            pos.ins().brif(cond, t, &[], latch, &[BlockArg::from(pm)]);
        }
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(t);
            pos.ins().jump(leak, &[]);
        }
        let p2 = func.dfg.append_block_param(latch, types::I64);
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(latch);
            pos.ins().jump(h, &[BlockArg::from(p2)]);
        }
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(leak);
            pos.ins().return_(&[v0]);
        }
        // Unreachable chain into `t`: u1 keeps u2's preds outside the
        // region, and the fold orphans `t` — u2's BlockCall to it must
        // not dangle.
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(u1);
            pos.ins().jump(u2, &[]);
        }
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(u2);
            pos.ins().jump(t, &[]);
        }

        let mut an = crate::pipeline::Analysis::default();
        let n = unswitch(&mut func, &mut an, "t");
        eprintln!("after unswitch:\n{func}");
        assert!(n >= 1, "unswitch did not fire");
        verify_ok(&func);
    }
}
