//! SROA + mem2reg. codegen_ssa puts every non-SSA local in a byte-array
//! alloca; this splits each non-escaping alloca into one alloca per accessed
//! slice (expanding constant-size memcpy/memmove/memset into per-slice
//! loads/stores) and marks the slices in `State::promoted`. Lowering turns a
//! promoted alloca into Cranelift `Variable`s, so SSA construction (including
//! across `try_call` exception edges) is done by cranelift-frontend.
//! `PLIRON_SROA=0` disables it.

use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::builtin::types::{IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed, TypedHandle};
use pliron::value::Value;
use pliron_llvm::attributes::IntegerOverflowFlagsAttr;
use pliron_llvm::op_interfaces::{
    BinArithOp, CastOpInterface, CastOpWithNNegInterface, IntBinArithOpWithOverflowFlag,
};
use pliron_llvm::ops::{
    AllocaOp, AndOp, BrOp, CallIntrinsicOp, CallOp, GepIndex, GetElementPtrOp, ICmpOp, IntToPtrOp,
    LShrOp, LoadOp, MulOp, OrOp, PtrToIntOp, ShlOp, StoreOp, TruncOp, UndefOp, UnreachableOp,
    ZExtOp,
};
use pliron_llvm::types::{ArrayType, FuncType};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::has_body;
use crate::state::{ConstVal, State, mask};
use crate::types::{TyK, classify, leaves, size_align, struct_offsets};

const MAX_SIZE: u64 = 512;
const MAX_SLICES: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Load,
    Store,
    /// memcpy/memmove; `dst` says which side the alloca is on.
    Copy {
        dst: bool,
        n: u64,
    },
    Set {
        n: u64,
        /// `Some(b)` for a constant byte; `None` keeps the memset operand
        /// (`bval`) so the rewrite can splat it into covered int slices.
        byte: Option<u8>,
        bval: Value,
    },
    /// Operand `idx` of a call straight before `unreachable`: gets a fresh
    /// copy of the whole alloca, so the rest can still be split.
    Escape {
        idx: usize,
    },
    /// Operand `idx` of a call the callee may legally read/write within
    /// `n` bytes of this pointer (the arg's pointee extent, recorded in
    /// `CallInfo::arg_extent`). The slices covering `[off, off+n)` stay
    /// real stack memory and the operand is retargeted into them — sound
    /// for any callee, unlike the copy `Escape` makes.
    Pin {
        idx: usize,
        n: u64,
    },
    /// The pointer value itself escapes as an operand — `store *q <- p`,
    /// `ptrtoint p`, `icmp p, x`: its target byte must stay a real stack
    /// address (`off` inside the pin span), and operand `idx` is retargeted
    /// into `pin_a`. Accesses discovered downstream of a loaded-back
    /// pointer are only *maybe* ours, so they pin instead of rewriting.
    Flow {
        idx: usize,
    },
}

#[derive(Clone, Copy)]
struct Access {
    op: Ptr<Operation>,
    off: u64,
    kind: Kind,
    /// Reached through a pointer read back out of a temp slot: the op's
    /// operand isn't provably a pointer into this alloca, so it isn't
    /// rewritten — it just pins the bytes it may touch.
    flow: bool,
}

/// The function op an alloca sits in (debug output only).
fn alloca_fn(ctx: &Context, alloca: Ptr<Operation>) -> Ptr<Operation> {
    let b = alloca.deref(ctx).get_parent_block().unwrap();
    let r = b.deref(ctx).get_parent_region().unwrap();
    r.deref(ctx).get_parent_op()
}

struct Slice {
    off: u64,
    size: u64,
    ty: TypeHandle,
    /// True when a call operand reaches into this slice (a `Pin` access):
    /// the bytes must stay real memory, so the slice is alloca'd but not
    /// promoted.
    pinned: bool,
}

pub fn const_int(ctx: &Context, st: &State, v: Value) -> Option<i128> {
    let b = match st.consts.get(v)? {
        ConstVal::Bits(b) => *b,
        ConstVal::Zero => return Some(0),
        _ => return None,
    };
    let w = match classify(ctx, v.get_type(ctx)) {
        TyK::Int(w) => w,
        _ => 64,
    };
    Some(if w < 128 && (b >> (w - 1)) & 1 == 1 {
        (b | (!0u128 << w)) as i128
    } else {
        b as i128
    })
}

/// Constant byte offset of a GEP, if all indices are constant.
pub fn gep_offset(ctx: &Context, st: &State, op: Ptr<Operation>) -> Option<i64> {
    let gep = Operation::get_op::<GetElementPtrOp>(op, ctx)?;
    let mut cur = gep.src_elem_type(ctx);
    let mut off: i64 = 0;
    for (k, idx) in gep.indices(ctx).iter().enumerate() {
        let c = match idx {
            GepIndex::Constant(c) => *c as i128,
            GepIndex::Value(v) => const_int(ctx, st, *v)?,
        } as i64;
        if k == 0 {
            off = off.wrapping_add(c.wrapping_mul(size_align(ctx, cur).0 as i64));
            continue;
        }
        match classify(ctx, cur) {
            TyK::Struct(fs, packed) => {
                let offs = struct_offsets(ctx, &fs, packed).0;
                off += *offs.get(c as usize)? as i64;
                cur = fs[c as usize];
            }
            TyK::Array(e, _) | TyK::Vector(e, _) => {
                cur = e;
                off = off.wrapping_add(c.wrapping_mul(size_align(ctx, e).0 as i64));
            }
            _ => return None,
        }
    }
    Some(off)
}

/// `p` is an argument (not the callee) of `op`, a plain call that diverges:
/// nothing after it can read the alloca, and an unwind leaves the frame.
fn diverging_arg(ctx: &Context, op: Ptr<Operation>, p: Value) -> bool {
    let Some(c) = Operation::get_op::<CallOp>(op, ctx) else {
        return false;
    };
    op.get_next(ctx)
        .is_some_and(|n| Operation::is_op::<UnreachableOp>(n, ctx))
        && !matches!(c.callee(ctx), CallOpCallable::Indirect(f) if f == p)
        && crate::pass_enabled("PLIRON_SROA_ESCAPE")
}

/// `p` is argument `idx` of `op`, a call whose callee can't observe that it
/// got a fresh copy of the alloca rather than the real slot — either rustc
/// deduced `captures(none)` on a by-value (Indirect) param (the callee gets
/// a private copy and can't retain the pointer), or our own inference shows
/// arg `idx` isn't retained and the callee writes nothing a path returning
/// to us could see. A panic-path writer is fine for a plain call — unwinding
/// skips our frame — but not an invoke, whose catch pad must see pre-panic
/// writes made through a shared pointer.
fn copyable_arg(ctx: &Context, st: &State, op: Ptr<Operation>, _p: Value, idx: usize) -> bool {
    let Some(c) = Operation::get_op::<CallOp>(op, ctx) else {
        return false;
    };
    let direct = c.get_attr_llvm_call_callee(ctx).is_some();
    // Operand 0 of an indirect call is the callee, not an argument.
    if !direct && idx == 0 {
        return false;
    }
    let ai = idx - usize::from(!direct);
    if st
        .calls
        .get(op)
        .and_then(|i| i.nocap.as_ref())
        .is_some_and(|n| n.get(ai) == Some(&true))
    {
        return true;
    }
    let Some(id) = c.get_attr_llvm_call_callee(ctx) else {
        return false;
    };
    let Some(sym) = st.ident_to_sym.get(AsRef::<str>::as_ref(id.as_ref())) else {
        return false;
    };
    let Some(&strict) = st.nowrite.get(sym) else {
        return false;
    };
    if !strict && st.invokes.contains(op) {
        return false;
    }
    st.nocapture
        .get(sym)
        .is_some_and(|s| s.contains(&(ai as u32)))
        && crate::pass_enabled("PLIRON_SROA_ESCAPE")
}

/// `p` is operand `idx` of `op`: if `op` is a call whose `idx`-th argument
/// has a known pointee extent (a ref arg or a sized Indirect param —
/// `CallInfo::arg_extent`), the callee may legally touch `(size, align)`
/// bytes from `p`. The slices covering them must stay memory; anything
/// else in the alloca can still promote.
fn pin_extent(ctx: &Context, st: &State, op: Ptr<Operation>, idx: usize) -> Option<(u64, u64)> {
    if !crate::pass_enabled("PLIRON_SROA_PIN") {
        return None;
    }
    let c = Operation::get_op::<CallOp>(op, ctx)?;
    let indirect = c.get_attr_llvm_call_callee(ctx).is_none();
    if indirect && idx == 0 {
        // Operand 0 of an indirect call is the callee, not an argument.
        return None;
    }
    let ai = idx - usize::from(indirect);
    let r = st
        .calls
        .get(op)
        .and_then(|i| i.arg_extent.as_ref())
        .and_then(|e| e.get(ai))
        .copied()
        .filter(|&e| e.0 > 0);
    if r.is_none()
        && pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::sroa", tracing::Level::DEBUG)
        && let CallOpCallable::Direct(id) = c.callee(ctx)
    {
        let got = st
            .calls
            .get(op)
            .map(|i| format!("extent={:?}", i.arg_extent))
            .unwrap_or_else(|| "no CallInfo".into());
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::sroa", "pinmiss {id} #{idx}: {got}")
        };
    }
    r
}

/// Operand `idx` of call `op` cannot be retained by the callee past the
/// call's end: rustc's `nocapture` bit on the arg (`nocap`) or our own
/// `nocapture` inference. `noalias` alone does NOT suffice: `f(&'a mut
/// self) -> Entry<'a>`-style reborrows legally hand a pointer into this
/// alloca back through the return value or an out-param, and a later call
/// writing through it would bypass the `pin_a` sync below.
/// A slice pinned only through such operands can promote after all: the
/// callee sees real memory in `pin_a`, and `split` writes the SSA value
/// back before the call and reloads it on the call's return edges.
fn nonretaining(ctx: &Context, st: &State, op: Ptr<Operation>, idx: usize) -> bool {
    if !crate::pass_enabled("PLIRON_SROA_SYNC") {
        return false;
    }
    let Some(c) = Operation::get_op::<CallOp>(op, ctx) else {
        return false;
    };
    let direct = c.get_attr_llvm_call_callee(ctx).is_some();
    // Operand 0 of an indirect call is the callee, not an argument.
    if !direct && idx == 0 {
        return false;
    }
    // A call with no following op has no normal edge to reload on; an
    // invoke whose pad takes arguments can't take a reload trampoline —
    // treat both as retaining.
    if op.get_next(ctx).is_none() {
        return false;
    }
    if let Some(&(pad, _)) = st.invokes.get(op)
        && pad.deref(ctx).arguments(ctx).next().is_some()
    {
        return false;
    }
    // Invokes are retaining unless `PLIRON_SROA_SYNC_INVOKE=1`: syncing a
    // pinned slot across a `try_call` miscompiled cranelift-codegen's
    // `constructor_simplify` (the 24-byte `ContextIter::next` result read
    // back stale on the normal edge; sightglass-cli's
    // `instrumented_component_validates` crashed). The IR after the split
    // reads correct and the trampoline isn't involved; the defect is
    // downstream of sroa and not yet found — see
    // docs/history/2026-10-24-cranelift-sightglass.md.
    if st.invokes.contains(op) && !crate::env_is("PLIRON_SROA_SYNC_INVOKE", "1") {
        return false;
    }
    let ai = idx - usize::from(!direct);
    let info = st.calls.get(op);
    if info
        .and_then(|i| i.nocap.as_ref())
        .is_some_and(|n| n.get(ai) == Some(&true))
    {
        return true;
    }
    let Some(id) = c.get_attr_llvm_call_callee(ctx) else {
        return false;
    };
    st.ident_to_sym
        .get(AsRef::<str>::as_ref(id.as_ref()))
        .and_then(|sym| st.nocapture.get(sym))
        .is_some_and(|s| s.contains(&(ai as u32)))
}

fn intrinsic<'a>(st: &'a State, op: Ptr<Operation>) -> Option<&'a str> {
    st.intrinsics.get(op).map(|s| s.as_str())
}

/// Clone `a`'s `llvm.lifetime` markers onto escape copy `c`. The copy's
/// bytes back a call operand: a callee retaining the pointer can only
/// legally dereference it inside the original local's storage window —
/// the parent's markers therefore bound the copy's escape the same way
/// they bound `a` — without them the copy's slot reads as live to
/// function end and poisons stack-slot coloring.
fn inherit_markers(ctx: &mut Context, st: &mut State, a: Value, c: Value) {
    let markers: Vec<Ptr<Operation>> = a
        .uses_iter(ctx)
        .map(|u| u.user_op())
        .filter(|&m| {
            matches!(
                intrinsic(st, m),
                Some("llvm.lifetime.start" | "llvm.lifetime.end")
            ) && m.deref(ctx).operands(ctx).any(|v| v == a)
        })
        .collect();
    for m in markers {
        let name = intrinsic(st, m).unwrap().to_string();
        let ci = Operation::get_op::<CallIntrinsicOp>(m, ctx).unwrap();
        let iname = ci.get_attr_llvm_intrinsic_name(ctx).unwrap().clone();
        let fty = TypedHandle::<FuncType>::from_handle(
            ci.get_attr_llvm_intrinsic_type(ctx).unwrap().clone().into(),
            ctx,
        )
        .unwrap();
        let args: Vec<Value> = m
            .deref(ctx)
            .operands(ctx)
            .map(|v| if v == a { c } else { v })
            .collect();
        let new = CallIntrinsicOp::new(ctx, iname, fty, args).get_operation();
        new.insert_before(ctx, m);
        st.intrinsics.insert(new, name);
    }
}

/// Deepest `store *q <- p` chain the flow walk will chase (a pointer stored
/// into a temp, loaded back, then stored into another temp, ...).
const MAX_FLOW_DEPTH: u32 = 6;
/// Total (value, offset, flow) states one `accesses` walk may visit.
/// A flow-dense graph — a temp slot stored through K offsets, each
/// store pushing every load of its copy-out closure under that base —
/// otherwise re-walks the same use-subgraphs per base (the stage2
/// `aho_corasick` build spent 20+ minutes here). Past the cap the
/// alloca simply stays unpromoted.
const MAX_ACC_WALK: usize = 1 << 14;

/// `accesses` results shared across one analysis pass's nested walks: a
/// `store *q <- p` use runs `accesses` on `q`, and K stores into `q` (or a
/// load-back store chain) would otherwise re-walk `q`'s whole use graph K
/// times — the failure mode that made stage2's `aho_corasick` spin for
/// hours. Keyed `(root, depth)` so results computed at different flow
/// depths don't mix. Valid only while the function's use-lists are
/// untouched — never carry it across a rewrite (sroa's `split` mutates, so
/// each `accesses` call gets a fresh memo; pure passes like `nocapture`
/// may share one for the whole body).
pub(crate) struct AccMemo {
    acc: FxHashMap<(Value, u32), Option<(Vec<Access>, Vec<Ptr<Operation>>)>>,
    /// `slot_loads` results per (resolved alloca, depth) — a hot temp
    /// slot otherwise re-walks its whole copy-out closure for every
    /// `store *q <- p` use.
    loads: FxHashMap<(Value, u32, Option<i64>), Option<Vec<Value>>>,
    /// `gep_offset` per op — a GEP re-visited under another root or
    /// depth re-decodes its index attr otherwise.
    geps: FxHashMap<Ptr<Operation>, Option<i64>>,
    /// Shared exploration budget across the memo's nested walks: the
    /// per-walk `visited` cap alone still lets K distinct (root,depth)
    /// subgraphs each burn `MAX_ACC_WALK`. `accesses` caps one
    /// top-level query's total work at `MAX_ACC_WALK`; long-lived memos
    /// (nocapture's whole-body pass) keep the default unbounded budget
    /// and rely on the per-walk cap as before.
    budget: usize,
}

impl Default for AccMemo {
    fn default() -> Self {
        Self {
            acc: Default::default(),
            loads: Default::default(),
            geps: Default::default(),
            budget: usize::MAX,
        }
    }
}

/// `gep_offset` through an `AccMemo` — a GEP's decoded offset is
/// use-independent, so it survives across the memo's whole lifetime.
fn gep_offset_m(ctx: &Context, st: &State, op: Ptr<Operation>, memo: &mut AccMemo) -> Option<i64> {
    if let Some(&r) = memo.geps.get(&op) {
        return r;
    }
    let r = gep_offset(ctx, st, op);
    memo.geps.insert(op, r);
    r
}

/// `dst` is the address operand of `store *dst <- p` that carried a pointer
/// derived from the alloca being collected. If `dst` resolves to a temp
/// slot whose bytes never leave this walk — every use is a load, a store
/// into it, or a memset/copy *into* it — return the results of its loads:
/// each may carry `p` again, so their uses are walked as accesses on the
/// original alloca. Anything else (a call handed the bytes, a memcpy *out*
/// of them, a nested pointer store) means the pointer can roam where this
/// pass can't see — the escape stands.
pub(crate) fn slot_loads(
    ctx: &Context,
    st: &State,
    dst: Value,
    depth: u32,
    memo: &mut AccMemo,
) -> Option<Vec<Value>> {
    slot_loads_at(ctx, st, dst, Some(0), depth, memo)
}

/// `slot_loads` with the pointer stored at `at` bytes past `dst` (None =
/// unknown): integer loads overlapping those bytes carry it too.
fn slot_loads_at(
    ctx: &Context,
    st: &State,
    dst: Value,
    at: Option<i64>,
    depth: u32,
    memo: &mut AccMemo,
) -> Option<Vec<Value>> {
    // `off`: where in the slot the pointer lands (None = unknown).
    let mut q = dst;
    let mut off = at;
    while !st.allocas.contains(q) {
        let op = q.defining_op()?;
        if !Operation::is_op::<GetElementPtrOp>(op, ctx) {
            return None;
        }
        off = off.zip(gep_offset_m(ctx, st, op, memo)).map(|(a, b)| a + b);
        q = op.deref(ctx).get_operand(ctx, 0);
    }
    if let Some(r) = memo.loads.get(&(q, depth, off)) {
        return r.clone();
    }
    let r = slot_loads_inner(ctx, st, q, depth, off, memo);
    memo.loads.insert((q, depth, off), r.clone());
    r
}

fn slot_loads_inner(
    ctx: &Context,
    st: &State,
    q: Value,
    depth: u32,
    off: Option<i64>,
    memo: &mut AccMemo,
) -> Option<Vec<Value>> {
    let &(qsize, _) = st.allocas.get(q)?;
    let (qacc, _) = accesses_m(ctx, st, q, qsize, depth + 1, memo)?;
    let mut loads = Vec::new();
    for qa in &qacc {
        match qa.kind {
            Kind::Load => {
                let r = qa.op.deref(ctx).get_result(ctx, 0);
                // An integer load overlapping the pointer's bytes carries it
                // too: stored elsewhere it is a byte copy of the pointer
                // (`extract_if` returning `&mut self.length` through i64
                // scratch slots, serde_json test_retain, 2026-10-21).
                let overlaps = || {
                    let n = size_align(ctx, r.get_type(ctx)).0 as i64;
                    off.is_none_or(|o| (qa.off as i64) < o + 8 && o < qa.off as i64 + n)
                };
                if matches!(classify(ctx, r.get_type(ctx)), TyK::Ptr) || overlaps() {
                    loads.push(r);
                } else if r
                    .uses_iter(ctx)
                    .any(|u| Operation::is_op::<IntToPtrOp>(u.user_op(), ctx))
                {
                    // A load of the pointer bytes as an integer fed to
                    // inttoptr would re-materialize the pointer unseen.
                    return None;
                }
            }
            Kind::Store | Kind::Set { .. } => {}
            Kind::Copy { dst: true, .. } => {}
            Kind::Copy { dst: false, .. } => {
                // The bytes were memcpy'd into another slot — the stored
                // pointer may come back at the destination's loads.
                let d = qa.op.deref(ctx).get_operand(ctx, 0);
                let at = off.map(|o| o - qa.off as i64);
                loads.extend(slot_loads_at(ctx, st, d, at, depth + 1, memo)?);
            }
            _ => return None,
        }
    }
    Some(loads)
}

/// Collect every access to `a` (through constant-offset GEPs), or None if it escapes.
fn accesses(
    ctx: &Context,
    st: &State,
    a: Value,
    size: u64,
    depth: u32,
) -> Option<(Vec<Access>, Vec<Ptr<Operation>>)> {
    // `split` mutates use-lists between `run`'s per-alloca calls — the
    // memo's scope is one top-level walk.
    accesses_m(
        ctx,
        st,
        a,
        size,
        depth,
        &mut AccMemo {
            budget: MAX_ACC_WALK,
            ..Default::default()
        },
    )
}

fn accesses_m(
    ctx: &Context,
    st: &State,
    a: Value,
    size: u64,
    depth: u32,
    memo: &mut AccMemo,
) -> Option<(Vec<Access>, Vec<Ptr<Operation>>)> {
    if let Some(r) = memo.acc.get(&(a, depth)) {
        return r.clone();
    }
    let r = accesses_inner(ctx, st, a, size, depth, memo);
    memo.acc.insert((a, depth), r.clone());
    r
}

fn accesses_inner(
    ctx: &Context,
    st: &State,
    a: Value,
    size: u64,
    depth: u32,
    memo: &mut AccMemo,
) -> Option<(Vec<Access>, Vec<Ptr<Operation>>)> {
    if depth > MAX_FLOW_DEPTH {
        return None;
    }
    let mut out = Vec::new();
    let mut geps = Vec::new();
    let mut work = vec![(a, 0i64, false)];
    let mut visited = FxHashSet::default();
    let mut ubuf = Vec::new();
    while let Some((p, base, flow)) = work.pop() {
        if !visited.insert((p, base, flow)) {
            continue;
        }
        memo.budget = memo.budget.saturating_sub(1);
        if memo.budget == 0 || visited.len() > MAX_ACC_WALK {
            return None;
        }
        p.uses_into(ctx, &mut ubuf);
        for u in ubuf.iter().copied() {
            let op = u.user_op();
            let idx = u.find_index(ctx);
            if st.volatile.contains(op) {
                return None;
            }
            let in_range = |off: i64, n: u64| {
                off >= 0 && (off as u64).checked_add(n).is_some_and(|e| e <= size)
            };
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                if idx != 0 {
                    return None;
                }
                let o = base.checked_add(gep_offset_m(ctx, st, op, memo)?)?;
                if !flow {
                    geps.push(op);
                }
                work.push((op.deref(ctx).get_result(ctx, 0), o, flow));
            } else if Operation::is_op::<LoadOp>(op, ctx) {
                let ty = op.deref(ctx).get_result(ctx, 0).get_type(ctx);
                if !in_range(base, size_align(ctx, ty).0) {
                    return None;
                }
                out.push(Access {
                    op,
                    off: base as u64,
                    kind: Kind::Load,
                    flow,
                });
            } else if Operation::is_op::<StoreOp>(op, ctx) {
                let ty = op.deref(ctx).get_operand(ctx, 0).get_type(ctx);
                if idx == 1 {
                    if !in_range(base, size_align(ctx, ty).0) {
                        return None;
                    }
                    out.push(Access {
                        op,
                        off: base as u64,
                        kind: Kind::Store,
                        flow,
                    });
                    continue;
                }
                if idx != 0 {
                    return None;
                }
                // `store *q <- p`: the pointer itself goes through memory.
                // Sound to chase only if `q`'s bytes stay inside this walk;
                // the loaded-back value keeps this pointer's offset.
                let loads = slot_loads(ctx, st, op.deref(ctx).get_operand(ctx, 1), depth, memo)?;
                if !flow {
                    out.push(Access {
                        op,
                        off: base as u64,
                        kind: Kind::Flow { idx },
                        flow: false,
                    });
                }
                for l in loads {
                    work.push((l, base, true));
                }
            } else if Operation::is_op::<PtrToIntOp>(op, ctx) || Operation::is_op::<ICmpOp>(op, ctx)
            {
                // The pointer is only observed as a number or compared —
                // sound as long as the int never becomes a pointer again.
                // The operand still needs a live address, so a direct use
                // retargets into `pin_a` like a flow store.
                if !flow {
                    out.push(Access {
                        op,
                        off: base as u64,
                        kind: Kind::Flow { idx },
                        flow: false,
                    });
                }
                if Operation::is_op::<PtrToIntOp>(op, ctx)
                    && op
                        .deref(ctx)
                        .get_result(ctx, 0)
                        .uses_iter(ctx)
                        .any(|u| Operation::is_op::<IntToPtrOp>(u.user_op(), ctx))
                {
                    return None;
                }
            } else if let Some(name) = intrinsic(st, op) {
                // Liveness markers touch no bytes — neither an access nor an escape.
                if matches!(name, "llvm.lifetime.start" | "llvm.lifetime.end") {
                    continue;
                }
                let opnds: Vec<Value> = op.deref(ctx).operands(ctx).collect();
                let kind = match name {
                    "llvm.memcpy" | "llvm.memmove" if idx < 2 => {
                        let n = const_int(ctx, st, opnds[2])?;
                        Kind::Copy {
                            dst: idx == 0,
                            n: u64::try_from(n).ok()?,
                        }
                    }
                    "llvm.memset" if idx == 0 => {
                        let n = const_int(ctx, st, opnds[2])?;
                        Kind::Set {
                            n: u64::try_from(n).ok()?,
                            byte: const_int(ctx, st, opnds[1]).map(|b| b as u8),
                            bval: opnds[1],
                        }
                    }
                    _ => return None,
                };
                let n = match kind {
                    Kind::Copy { n, .. } | Kind::Set { n, .. } => n,
                    _ => unreachable!(),
                };
                if !in_range(base, n) {
                    return None;
                }
                out.push(Access {
                    op,
                    off: base as u64,
                    kind,
                    flow,
                });
            } else if (diverging_arg(ctx, op, p) || copyable_arg(ctx, st, op, p, idx))
                && in_range(base, 0)
            {
                out.push(Access {
                    op,
                    off: base as u64,
                    kind: Kind::Escape { idx },
                    flow,
                });
            } else if let Some((n, _)) = pin_extent(ctx, st, op, idx)
                && in_range(base, n)
            {
                out.push(Access {
                    op,
                    off: base as u64,
                    kind: Kind::Pin { idx, n },
                    flow,
                });
            } else {
                return None;
            }
        }
    }
    // A copy with both ends in this alloca would need overlap handling.
    // Two pinned or escaping args on one call are fine — each access
    // retargets its own operand.
    let mut seen = FxHashSet::default();
    if out
        .iter()
        .any(|a| matches!(a.kind, Kind::Copy { .. }) && !seen.insert(a.op))
    {
        return None;
    }
    Some((out, geps))
}

fn leaf_compatible(ctx: &Context, a: TypeHandle, b: TypeHandle) -> bool {
    if a == b {
        return true;
    }
    let (la, lb) = (leaves(ctx, a), leaves(ctx, b));
    if la.len() != lb.len() || size_align(ctx, a).0 != size_align(ctx, b).0 {
        return false;
    }
    la.iter()
        .zip(&lb)
        .all(|((oa, ta), (ob, tb))| oa == ob && ta.bits() == tb.bits())
}

fn access_ty(ctx: &Context, a: &Access) -> Option<TypeHandle> {
    match a.kind {
        Kind::Load => Some(a.op.deref(ctx).get_result(ctx, 0).get_type(ctx)),
        Kind::Store => Some(a.op.deref(ctx).get_operand(ctx, 0).get_type(ctx)),
        _ => None,
    }
}

/// Byte width of a plain integer type.
fn int_bytes(ctx: &Context, t: TypeHandle) -> Option<u64> {
    match classify(ctx, t) {
        TyK::Int(b) if b % 8 == 0 && b <= 128 => Some(b as u64 / 8),
        _ => None,
    }
}

/// Partition the alloca into slices; None if typed accesses overlap
/// inconsistently or a copy/set cuts through a typed slice. An integer access
/// inside a wider integer slice (an enum tag byte of a register-returned
/// `Result`, say) is allowed and rewritten as shift/mask of the slice.
fn slices(ctx: &mut Context, acc: &[Access]) -> Result<Vec<Slice>, &'static str> {
    let subint = crate::pass_enabled("PLIRON_SROA_SUBINT");
    let wide = crate::pass_enabled("PLIRON_SROA_GAPWIDE");
    let mut order: Vec<(&Access, TypeHandle, u64)> = acc
        .iter()
        .filter_map(|a| access_ty(ctx, a).map(|t| (a, t, size_align(ctx, t).0)))
        .collect();
    if subint {
        order.sort_by_key(|x| std::cmp::Reverse(x.2));
    }
    let mut typed: Vec<Slice> = Vec::new();
    for (a, ty, _) in order {
        let size = size_align(ctx, ty).0;
        if size == 0 {
            return Err("zero-size access");
        }
        match typed
            .iter()
            .find(|s| s.off < a.off + size && a.off < s.off + s.size)
        {
            Some(s) if s.off == a.off && s.size == size && leaf_compatible(ctx, s.ty, ty) => {}
            Some(s)
                if subint
                    && s.off <= a.off
                    && a.off + size <= s.off + s.size
                    && int_bytes(ctx, ty) == Some(size)
                    && int_bytes(ctx, s.ty) == Some(s.size) => {}
            Some(_) => return Err("typed accesses overlap"),
            None => typed.push(Slice {
                off: a.off,
                size,
                ty,
                pinned: false,
            }),
        }
    }
    let mut cuts: Vec<u64> = Vec::new();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for a in acc {
        match a.kind {
            Kind::Copy { n, .. } | Kind::Set { n, .. } => {
                if n == 0 {
                    continue;
                }
                let (s, e) = (a.off, a.off + n);
                if typed
                    .iter()
                    .any(|t| (t.off < s && s < t.off + t.size) || (t.off < e && e < t.off + t.size))
                {
                    return Err("copy/set cuts a typed slice");
                }
                cuts.extend([s, e]);
                ranges.push((s, e));
            }
            Kind::Pin { n, .. } if n > 0 => {
                // Pinned bytes need slice coverage but don't cut existing
                // slices: whatever the range overlaps stays memory anyway.
                cuts.extend([a.off, a.off + n]);
                ranges.push((a.off, a.off + n));
            }
            _ => {}
        }
    }
    cuts.extend(typed.iter().flat_map(|t| [t.off, t.off + t.size]));
    cuts.sort_unstable();
    cuts.dedup();
    let covered = |x: u64| typed.iter().any(|t| t.off <= x && x < t.off + t.size);
    let mut gaps = Vec::new();
    for w in cuts.windows(2) {
        let (s, e) = (w[0], w[1]);
        if covered(s) || !ranges.iter().any(|&(rs, re)| rs <= s && e <= re) {
            continue;
        }
        let mut o = s;
        while o < e {
            let mut k = 8;
            while (!wide && o % k != 0) || o + k > e {
                k /= 2;
            }
            gaps.push((o, k));
            o += k;
        }
    }
    for (off, k) in gaps {
        let ty = IntegerType::get(ctx, (k * 8) as u32, Signedness::Signless).into();
        typed.push(Slice {
            off,
            size: k,
            ty,
            pinned: false,
        });
    }
    let pins: Vec<(u64, u64)> = acc
        .iter()
        .filter_map(|a| {
            let n = match a.kind {
                Kind::Pin { n, .. } => n,
                Kind::Copy { n, .. } | Kind::Set { n, .. } if a.flow => n,
                Kind::Load | Kind::Store if a.flow => size_align(ctx, access_ty(ctx, a)?).0,
                _ => return None,
            };
            (n > 0).then_some((a.off, a.off + n))
        })
        .collect();
    for s in &mut typed {
        s.pinned = pins
            .iter()
            .any(|&(lo, hi)| lo < s.off + s.size && s.off < hi);
    }
    typed.sort_by_key(|s| s.off);
    if typed.is_empty() {
        return Err("no typed slices");
    }
    if typed.len() > MAX_SLICES {
        return Err("too many slices");
    }
    Ok(typed)
}

pub fn mk_const(ctx: &mut Context, st: &mut State, ty: TypeHandle, cv: ConstVal) -> Value {
    let op = UndefOp::new(ctx, ty).get_operation();
    let v = op.deref(ctx).get_result(ctx, 0);
    st.consts.insert(v, cv);
    v
}

pub fn ptr_plus(
    ctx: &mut Context,
    st: &mut State,
    p: Value,
    off: u64,
    before: Ptr<Operation>,
) -> Value {
    if off == 0 {
        return p;
    }
    let i64t = IntegerType::get(ctx, 64, Signedness::Signless).into();
    let i8t = IntegerType::get(ctx, 8, Signedness::Signless).into();
    let c = mk_const(ctx, st, i64t, ConstVal::Bits(off as u128));
    let g = GetElementPtrOp::new(ctx, p, vec![GepIndex::Value(c)], i8t).get_operation();
    g.insert_before(ctx, before);
    g.deref(ctx).get_result(ctx, 0)
}

/// `pin_a` → SSA reload for one call's return edge: every op is anchored
/// before `at` (the op after the call on the normal edge, or the
/// trampoline's `br` on an unwind edge).
#[allow(clippy::too_many_arguments)]
fn reload_sync(
    ctx: &mut Context,
    st: &mut State,
    pa: Value,
    pin_lo: u64,
    sl: &[Slice],
    new: &[Value],
    ss: &[usize],
    at: Ptr<Operation>,
) {
    for &i in ss {
        let p = ptr_plus(ctx, st, pa, sl[i].off - pin_lo, at);
        let l = LoadOp::new(ctx, p, sl[i].ty).get_operation();
        l.insert_before(ctx, at);
        let lv = l.deref(ctx).get_result(ctx, 0);
        StoreOp::new(ctx, lv, new[i])
            .get_operation()
            .insert_before(ctx, at);
    }
}

fn erase_dead(ctx: &mut Context, mut ops: Vec<Ptr<Operation>>) {
    loop {
        let before = ops.len();
        ops.retain(|&op| {
            if op.deref(ctx).results(ctx).any(|r| r.is_used(ctx)) {
                return true;
            }
            Operation::erase(op, ctx);
            false
        });
        if ops.len() == before {
            break;
        }
    }
}

/// Is `a` already a promotable single slice (only whole-size direct loads/stores)?
fn trivially_promotable(
    ctx: &Context,
    acc: &[Access],
    geps: &[Ptr<Operation>],
    size: u64,
) -> Option<TypeHandle> {
    if !geps.is_empty() {
        return None;
    }
    let mut ty: Option<TypeHandle> = None;
    for x in acc {
        let t = access_ty(ctx, x)?;
        if x.off != 0 || size_align(ctx, t).0 != size {
            return None;
        }
        match ty {
            Some(t0) if !leaf_compatible(ctx, t0, t) => return None,
            None => ty = Some(t),
            _ => {}
        }
    }
    ty
}

/// Split one alloca into slices; `made` collects the ops it creates —
/// all `insert_before(alloca)`, so pushing them in creation order keeps
/// a caller-maintained alloca list in exact body-scan order.
fn split(
    ctx: &mut Context,
    st: &mut State,
    alloca: Ptr<Operation>,
    made: &mut Vec<Ptr<Operation>>,
    cfg_dirty: &mut bool,
) -> bool {
    let a = alloca.deref(ctx).get_result(ctx, 0);
    let Some(&(size, align)) = st.allocas.get(a) else {
        return false;
    };
    if st.escape_copies.contains(a) {
        return false;
    }
    if st.promoted.contains(a) {
        validate(ctx, st, a);
        if st.promoted.contains(a) {
            return false;
        }
    }
    if size == 0 || size > MAX_SIZE {
        return false;
    }
    let Some((acc, geps)) = accesses(ctx, st, a, size, 0) else {
        return false;
    };
    if acc.is_empty() {
        return false;
    }
    let escapes = acc.iter().any(|x| matches!(x.kind, Kind::Escape { .. }));
    if !escapes && let Some(t) = trivially_promotable(ctx, &acc, &geps, size) {
        st.promoted.insert(a, t);
        return true;
    }
    let Ok(sl) = slices(ctx, &acc) else {
        return false;
    };

    // A runtime-byte memset scalarizes to an integer splat, which the
    // rewrite arm only knows how to build for int slices of <=16 bytes —
    // if a covered slice is anything else, refuse the whole scalarization
    // (the alloca keeps its real memset).
    for x in &acc {
        if let Kind::Set { n, byte: None, .. } = x.kind {
            for s in sl
                .iter()
                .filter(|s| x.off <= s.off && s.off + s.size <= x.off + n)
            {
                if !matches!(classify(ctx, s.ty), TyK::Int(_)) || s.size > 16 {
                    return false;
                }
            }
        }
    }

    // A pinned slice can still promote when every access covering it is a
    // `Pin` on a nonretaining call operand: the callee sees real memory in
    // `pin_a`, and the SSA view is synced with a writeback before the call
    // plus a reload on each return edge. Opaque uses — loads/stores through
    // a pointer read back out of memory (`flow`), a `store *q <- p` parking
    // a pointer where any later call could pick it up, or a retaining pin —
    // keep their slices memory-only.
    let mut pin_rngs: FxHashMap<Ptr<Operation>, Vec<(u64, u64)>> = FxHashMap::default();
    let mut covered = vec![false; sl.len()];
    let mut bad = vec![false; sl.len()];
    for x in &acc {
        let n = match x.kind {
            Kind::Pin { n, .. } | Kind::Copy { n, .. } | Kind::Set { n, .. } => n,
            Kind::Load | Kind::Store if x.flow => {
                access_ty(ctx, x).map(|t| size_align(ctx, t).0).unwrap_or(0)
            }
            _ => continue,
        };
        if n == 0 {
            continue;
        }
        let ok = match x.kind {
            Kind::Pin { idx, .. } => nonretaining(ctx, st, x.op, idx),
            // A flow access is an opaque touch — no call boundary to sync at.
            _ => !x.flow,
        };
        for (i, s) in sl.iter().enumerate() {
            if x.off < s.off + s.size && s.off < x.off + n {
                if ok {
                    covered[i] = true;
                } else {
                    bad[i] = true;
                }
            }
        }
        if let Kind::Pin { .. } = x.kind
            && ok
        {
            pin_rngs.entry(x.op).or_default().push((x.off, x.off + n));
        }
    }
    // `store *q <- p` (a non-flow Flow access) parks a pointer into this
    // alloca in memory a callee could pick back up — nothing is syncable.
    if acc
        .iter()
        .any(|x| matches!(x.kind, Kind::Flow { .. }) && Operation::is_op::<StoreOp>(x.op, ctx))
    {
        bad.iter_mut().for_each(|b| *b = true);
    }
    let syncable: Vec<bool> = (0..sl.len())
        .map(|i| sl[i].pinned && covered[i] && !bad[i])
        .collect();
    // `PLIRON_SROA_DEBUG=<fn substr>`: the access list and slices of each
    // split attempted in matching functions (eprintln, for bisect repros).
    if let Some(want) = crate::env_get("PLIRON_SROA_DEBUG")
        && crate::bisect_name(st, alloca_fn(ctx, alloca)).contains(want)
    {
        let kinds: Vec<String> = acc
            .iter()
            .map(|x| {
                let k = match x.kind {
                    Kind::Load => "load".to_string(),
                    Kind::Store => "store".to_string(),
                    Kind::Pin { idx, n } => format!("pin(arg{idx},n={n})"),
                    Kind::Copy { n, .. } => format!("copy(n={n})"),
                    Kind::Set { n, .. } => format!("set(n={n})"),
                    _ => "other".to_string(),
                };
                let w = access_ty(ctx, x).map(|t| size_align(ctx, t).0).unwrap_or(0);
                format!("{}@{}w{}{}", k, x.off, w, if x.flow { "F" } else { "" })
            })
            .collect();
        let sls: Vec<String> = sl
            .iter()
            .enumerate()
            .map(|(i, s)| format!("[{}+{} pin={} sync={}]", s.off, s.size, s.pinned, syncable[i]))
            .collect();
        eprintln!("SROADBG size={size} acc={} slices={}", kinds.join(" "), sls.join(" "));
    }

    let count = alloca.deref(ctx).get_operand(ctx, 0);
    let i8t: TypeHandle = IntegerType::get(ctx, 8, Signedness::Signless).into();
    // `PLIRON_LTUNDEF=1`: each promoted slice keeps the parent's
    // `llvm.lifetime` markers, which lowering turns into "contents
    // undefined" defs of the slice's variables. Without them a slice's
    // value reaches around a loop to the next iteration: a `Result<(),
    // E>` local whose `Ok` writes only the niche word carries E's other
    // fields as loop-carried block params from one iteration's `Err` to
    // the next, which spill in call-heavy loops (NestLimiter::check:
    // 45 such params, 624 B of spill slots).
    let ltundef = crate::env_is("PLIRON_LTUNDEF", "1");
    // All pinned slices share one stack object: a callee handed a pointer
    // into the pin range may legally roam the whole extent, and per-slice
    // allocas aren't guaranteed to sit contiguously. The object covers
    // every pinned slice (a typed slice can extend past its pin range),
    // not just the ranges themselves.
    let (mut pin_lo, mut pin_hi, mut pin_al) = (u64::MAX, 0u64, 1u64);
    for x in &acc {
        match x.kind {
            Kind::Pin { idx, .. } => {
                if let Some((_, al)) = pin_extent(ctx, st, x.op, idx) {
                    pin_al = pin_al.max(al);
                }
            }
            // The flow-stored pointer keeps `a + x.off`'s alignment only if
            // `x.off - pin_lo` preserves its low bits — fold its alignment
            // into the pin_lo rounding below.
            Kind::Flow { .. } => {
                let low = 1u64 << x.off.trailing_zeros().min(6);
                pin_al = pin_al.max(low.min(align));
            }
            _ => {}
        }
    }
    for s in &sl {
        if s.pinned {
            pin_lo = pin_lo.min(s.off);
            pin_hi = pin_hi.max(s.off + s.size);
        }
    }
    // A valid `&T` arg means `x.off` was a multiple of `T`'s align in the
    // original alloca. Rounding `pin_lo` down to the widest pointee align
    // keeps `x.off - pin_lo` a multiple of it too, so the retargeted
    // pointer into `pin_a` stays correctly aligned for the callee.
    pin_lo &= !(pin_al - 1);
    // A `store *q <- ptr` (Flow) retargets the stored value to `pin_a`, so
    // the pointer's target offset must land inside the pinned span.
    if acc
        .iter()
        .any(|x| matches!(x.kind, Kind::Flow { .. }) && !(x.off >= pin_lo && x.off < pin_hi))
    {
        return false;
    }
    let pin_a = if pin_lo < pin_hi {
        let arr = ArrayType::get(ctx, i8t, pin_hi - pin_lo).into();
        let op = AllocaOp::new(ctx, arr, count, 0).get_operation();
        op.insert_before(ctx, alloca);
        made.push(op);
        let v = op.deref(ctx).get_result(ctx, 0);
        st.allocas.insert(v, (pin_hi - pin_lo, align.max(1)));
        // Its bytes back a live call operand — re-splitting would just
        // re-pin the same range.
        st.escape_copies.insert(v);
        inherit_markers(ctx, st, a, v);
        Some(v)
    } else {
        None
    };
    let new: Vec<Value> = sl
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if s.pinned && !syncable[i] {
                return ptr_plus(ctx, st, pin_a.unwrap(), s.off - pin_lo, alloca);
            }
            let arr = ArrayType::get(ctx, i8t, s.size).into();
            let op = AllocaOp::new(ctx, arr, count, 0).get_operation();
            op.insert_before(ctx, alloca);
            made.push(op);
            let v = op.deref(ctx).get_result(ctx, 0);
            let al = if s.off == 0 {
                align
            } else {
                align.min(1 << s.off.trailing_zeros())
            };
            st.allocas.insert(v, (s.size, al.max(1)));
            st.promoted.insert(v, s.ty);
            if ltundef {
                inherit_markers(ctx, st, a, v);
            }
            v
        })
        .collect();
    let find = |off: u64| {
        sl.iter()
            .position(|s| s.off <= off && off < s.off + s.size)
            .unwrap()
    };
    for x in &acc {
        if x.flow {
            // Reached through a loaded-back pointer: keep the operand (it
            // resolves to a `pin_a` address once the temp slot promotes)
            // and only pin the touched slice.
            continue;
        }
        match x.kind {
            Kind::Load | Kind::Store => {
                let (i, ty) = (find(x.off), access_ty(ctx, x).unwrap());
                let (s, n) = (&sl[i], size_align(ctx, ty).0);
                if s.off == x.off && s.size == n {
                    let k = if x.kind == Kind::Load { 0 } else { 1 };
                    Operation::replace_operand(x.op, ctx, k, new[i]);
                    continue;
                }
                let sh = (x.off - s.off) * 8;
                let shc = mk_const(ctx, st, s.ty, ConstVal::Bits(sh as u128));
                let ins = |ctx: &mut Context, o: Ptr<Operation>| {
                    o.insert_before(ctx, x.op);
                    o.deref(ctx).get_result(ctx, 0)
                };
                let w = LoadOp::new(ctx, new[i], s.ty).get_operation();
                let w = ins(ctx, w);
                if x.kind == Kind::Load {
                    let v = LShrOp::new(ctx, w, shc).get_operation();
                    let v = ins(ctx, v);
                    let v = TruncOp::new(ctx, v, ty).get_operation();
                    let v = ins(ctx, v);
                    let r = x.op.deref(ctx).get_result(ctx, 0);
                    r.replace_all_uses_with(ctx, &v);
                } else {
                    let flags = IntegerOverflowFlagsAttr {
                        nsw: false,
                        nuw: false,
                    };
                    let val = x.op.deref(ctx).get_operand(ctx, 0);
                    let z = ZExtOp::new_with_nneg(ctx, val, s.ty, false).get_operation();
                    let z = ins(ctx, z);
                    let z = ShlOp::new_with_overflow_flag(ctx, z, shc, flags).get_operation();
                    let z = ins(ctx, z);
                    let keep = mask(
                        !(mask(u128::MAX, (n * 8) as u32) << sh),
                        (s.size * 8) as u32,
                    );
                    let kc = mk_const(ctx, st, s.ty, ConstVal::Bits(keep));
                    let m = AndOp::new(ctx, w, kc).get_operation();
                    let m = ins(ctx, m);
                    let o = OrOp::new(ctx, m, z).get_operation();
                    let o = ins(ctx, o);
                    StoreOp::new(ctx, o, new[i])
                        .get_operation()
                        .insert_before(ctx, x.op);
                }
                Operation::erase(x.op, ctx);
            }
            Kind::Copy { dst, n } => {
                let other = x.op.deref(ctx).get_operand(ctx, if dst { 1 } else { 0 });
                let mut stores = Vec::new();
                for (i, s) in sl
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| x.off <= s.off && s.off + s.size <= x.off + n)
                {
                    let p = ptr_plus(ctx, st, other, s.off - x.off, x.op);
                    let (from, to) = if dst { (p, new[i]) } else { (new[i], p) };
                    let l = LoadOp::new(ctx, from, s.ty).get_operation();
                    l.insert_before(ctx, x.op);
                    stores.push((l.deref(ctx).get_result(ctx, 0), to));
                }
                for (v, to) in stores {
                    StoreOp::new(ctx, v, to)
                        .get_operation()
                        .insert_before(ctx, x.op);
                }
                st.intrinsics.remove(x.op);
                Operation::erase(x.op, ctx);
            }
            Kind::Set { n, byte, bval } => {
                for (i, s) in sl
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| x.off <= s.off && s.off + s.size <= x.off + n)
                {
                    if byte == Some(0) {
                        let c = mk_const(ctx, st, s.ty, ConstVal::Zero);
                        StoreOp::new(ctx, c, new[i])
                            .get_operation()
                            .insert_before(ctx, x.op);
                        continue;
                    }
                    let Some(byte) = byte else {
                        // Runtime byte: splat it — `zext(bval) * 0x0101..`
                        // fills the whole slice (LLVM does the same fold
                        // for `u64::from_ne_bytes([b; 8])`-style code, e.g.
                        // hashbrown's tag broadcast). Coverage is confined
                        // to int slices by the preflight in `split`.
                        let (bsize, _) = size_align(ctx, bval.get_type(ctx));
                        let v = if bsize < s.size {
                            let o = ZExtOp::new_with_nneg(ctx, bval, s.ty, false).get_operation();
                            o.insert_before(ctx, x.op);
                            o.deref(ctx).get_result(ctx, 0)
                        } else {
                            bval
                        };
                        let rep = (0..s.size).fold(0u128, |acc, k| acc | (1u128 << (8 * k)));
                        let c = mk_const(
                            ctx,
                            st,
                            s.ty,
                            ConstVal::Bits(mask(rep, (s.size * 8) as u32)),
                        );
                        let m = MulOp::new_with_overflow_flag(
                            ctx,
                            v,
                            c,
                            IntegerOverflowFlagsAttr {
                                nsw: false,
                                nuw: false,
                            },
                        )
                        .get_operation();
                        m.insert_before(ctx, x.op);
                        let m = m.deref(ctx).get_result(ctx, 0);
                        StoreOp::new(ctx, m, new[i])
                            .get_operation()
                            .insert_before(ctx, x.op);
                        continue;
                    };
                    if s.size <= 16 {
                        let rep =
                            (0..s.size).fold(0u128, |acc, k| acc | ((byte as u128) << (8 * k)));
                        let c = mk_const(
                            ctx,
                            st,
                            s.ty,
                            ConstVal::Bits(mask(rep, (s.size * 8) as u32)),
                        );
                        StoreOp::new(ctx, c, new[i])
                            .get_operation()
                            .insert_before(ctx, x.op);
                        continue;
                    }
                    // The repeated byte doesn't fit one u128 const:
                    // decompose into power-of-2-aligned integer stores.
                    let mut o = 0;
                    while o < s.size {
                        let mut k = 16;
                        while o % k != 0 || o + k > s.size {
                            k /= 2;
                        }
                        let ty = IntegerType::get(ctx, (k * 8) as u32, Signedness::Signless).into();
                        let rep = (0..k).fold(0u128, |acc, j| acc | ((byte as u128) << (8 * j)));
                        let c = mk_const(ctx, st, ty, ConstVal::Bits(rep));
                        let p = ptr_plus(ctx, st, new[i], o, x.op);
                        StoreOp::new(ctx, c, p)
                            .get_operation()
                            .insert_before(ctx, x.op);
                        o += k;
                    }
                }
                st.intrinsics.remove(x.op);
                Operation::erase(x.op, ctx);
            }
            Kind::Escape { idx } => {
                let arr = ArrayType::get(ctx, i8t, size).into();
                let tmp_op = AllocaOp::new(ctx, arr, count, 0).get_operation();
                tmp_op.insert_before(ctx, alloca);
                made.push(tmp_op);
                let tmp = tmp_op.deref(ctx).get_result(ctx, 0);
                st.allocas.insert(tmp, (size, align));
                st.escape_copies.insert(tmp);
                inherit_markers(ctx, st, a, tmp);
                for (i, s) in sl.iter().enumerate() {
                    let l = LoadOp::new(ctx, new[i], s.ty).get_operation();
                    l.insert_before(ctx, x.op);
                    let v = l.deref(ctx).get_result(ctx, 0);
                    let to = ptr_plus(ctx, st, tmp, s.off, x.op);
                    StoreOp::new(ctx, v, to)
                        .get_operation()
                        .insert_before(ctx, x.op);
                }
                let p = ptr_plus(ctx, st, tmp, x.off, x.op);
                Operation::replace_operand(x.op, ctx, idx, p);
            }
            Kind::Pin { idx, .. } => {
                // The callee always sees a `pin_a` address: pinned slices
                // live there, and syncable slices mirror into it around
                // each call.
                let p = ptr_plus(ctx, st, pin_a.unwrap(), x.off - pin_lo, x.op);
                Operation::replace_operand(x.op, ctx, idx, p);
            }
            Kind::Flow { idx } => {
                // The operand was `a + x.off`; its target byte moved
                // to `pin_a`, so the pointer value becomes a pin_a
                // address (`x.off - pin_lo` is nonneg — checked above).
                let p = ptr_plus(ctx, st, pin_a.unwrap(), x.off - pin_lo, x.op);
                Operation::replace_operand(x.op, ctx, idx, p);
            }
        }
    }
    // Sync the dual view: before each nonretaining pin call, write the
    // syncable slices' SSA values into `pin_a`; on its normal-return edge
    // (and, for an invoke, on a fresh unwind trampoline) reload them so
    // the callee's writes land back in SSA.
    //
    // Every syncable slice is synced for every call, not just the slices
    // overlapping that call's pinned arg extents: `pin_a` is a single
    // contiguous object, so a callee can legally roam all of it — a
    // slice pinned only by a different call's arg would otherwise expose
    // uninitialized mirror bytes to a roaming read and silently drop a
    // roaming write.
    if let Some(pa) = pin_a {
        let ss: Vec<usize> = (0..sl.len()).filter(|&i| syncable[i]).collect();
        for cop in pin_rngs.keys() {
            if ss.is_empty() {
                break;
            }
            for &i in &ss {
                let l = LoadOp::new(ctx, new[i], sl[i].ty).get_operation();
                l.insert_before(ctx, *cop);
                let lv = l.deref(ctx).get_result(ctx, 0);
                let p = ptr_plus(ctx, st, pa, sl[i].off - pin_lo, *cop);
                StoreOp::new(ctx, lv, p)
                    .get_operation()
                    .insert_before(ctx, *cop);
            }
            let next = cop.get_next(ctx);
            if let Some(next) = next {
                reload_sync(ctx, st, pa, pin_lo, &sl, &new, &ss, next);
            }
            let padc = st.invokes.get(*cop).copied();
            if let Some((pad, catch)) = padc {
                let region = cop
                    .deref(ctx)
                    .get_parent_block()
                    .unwrap()
                    .deref(ctx)
                    .get_parent_region()
                    .unwrap();
                let t = BasicBlock::new(ctx, None, vec![]);
                t.insert_at_back(region, ctx);
                let br = BrOp::new(ctx, pad, vec![]).get_operation();
                br.insert_at_back(t, ctx);
                reload_sync(ctx, st, pa, pin_lo, &sl, &new, &ss, br);
                st.invokes.insert(*cop, (t, catch));
                // The normal-path reloads went right after the invoke.
                crate::inline::end_block_at_invoke(ctx, st, *cop);
                *cfg_dirty = true;
            }
        }
    }
    erase_dead(ctx, geps.into_iter().rev().collect());
    if !a.is_used(ctx) {
        st.allocas.remove(a);
        Operation::erase(alloca, ctx);
    }
    true
}

/// Drop promotion for any alloca that ended up with a use lowering can't
/// turn into a variable access (it then stays a stack slot).
fn validate(ctx: &Context, st: &mut State, a: Value) {
    let Some(&ty) = st.promoted.get(a) else {
        return;
    };
    let ok = a.uses_iter(ctx).all(|u| {
        let op = u.user_op();
        let idx = u.find_index(ctx);
        if Operation::is_op::<LoadOp>(op, ctx) {
            leaf_compatible(ctx, ty, op.deref(ctx).get_result(ctx, 0).get_type(ctx))
        } else if Operation::is_op::<StoreOp>(op, ctx) {
            idx == 1 && leaf_compatible(ctx, ty, op.deref(ctx).get_operand(ctx, 0).get_type(ctx))
        } else {
            matches!(
                intrinsic(st, op),
                Some("llvm.lifetime.start" | "llvm.lifetime.end")
            )
        }
    });
    if !ok {
        st.promoted.remove(a);
    }
}

/// Allocas anywhere in `f` (inlined callees' allocas live outside the entry block).
pub fn allocas(ctx: &Context, f: Ptr<Operation>) -> Vec<Ptr<Operation>> {
    let r = f.deref(ctx).get_region(ctx, 0);
    r.deref(ctx)
        .iter(ctx)
        .flat_map(|b| {
            b.deref(ctx)
                .iter(ctx)
                .filter(|&op| Operation::is_op::<AllocaOp>(op, ctx))
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn run(ctx: &mut Context, st: &mut State) {
    let funcs: Vec<Ptr<Operation>> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| crate::opt_body(ctx, st, f))
        .collect();
    for f in funcs {
        // No alloca anywhere: `forward_single_store`, the split rounds
        // and the trailing cleanup all have nothing to do.
        let mut wl = allocas(ctx, f);
        if wl.is_empty() {
            continue;
        }
        // The `Dom` survives both `forward_single_store` calls — neither
        // it nor `split` moves edges; only a spliced invoke-pad
        // trampoline (flagged by `cfg_dirty`) forces a rebuild.
        let mut dom: Option<crate::domcheck::Dom> = None;
        let mut dom_stale = false;
        // Again after `phisimp`: loads that went through trivial block args
        // now name the slot directly.
        if crate::pass_enabled("PLIRON_SROA_FWD") {
            forward_single_store_d(ctx, st, f, &mut dom);
        }
        // Retry worklist instead of rescanning the body each round:
        // `split` erases its alloca only when the rewrite left it
        // unused — anything still in `st.allocas` (failed splits and
        // partial splits alike) is what the next round's rescan would
        // have retried, plus the ops `split` created, which sat just
        // before their parent in the scan. Order is preserved exactly.
        let bname = crate::bisect_name(st, f);
        for _round in 0..3 {
            let mut changed = false;
            let mut retry = Vec::new();
            let mut made = Vec::new();
            for op in wl.drain(..) {
                let a = op.deref(ctx).get_result(ctx, 0);
                let mut cfg_dirty = false;
                // One PLIRON_OPT_BISECT unit per split attempt.
                changed |= crate::bisect_in("sroa", &bname)
                    && split(ctx, st, op, &mut made, &mut cfg_dirty);
                dom_stale |= cfg_dirty;
                retry.extend(made.drain(..));
                if st.allocas.contains(a) {
                    retry.push(op);
                }
            }
            if !changed {
                break;
            }
            wl = retry;
        }
        // And after splitting: a slice left holding one stored value.
        if crate::pass_enabled("PLIRON_SROA_FWD") {
            if dom_stale {
                dom = None;
            }
            forward_single_store_d(ctx, st, f, &mut dom);
        }
        for op in allocas(ctx, f) {
            let a = op.deref(ctx).get_result(ctx, 0);
            if !a.is_used(ctx) {
                st.allocas.remove(a);
                st.promoted.remove(a);
                Operation::erase(op, ctx);
                continue;
            }
            validate(ctx, st, a);
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        let total = st
            .funcs
            .values()
            .filter(|f| has_body(ctx, f.op))
            .map(|f| allocas(ctx, f.op).len())
            .sum::<usize>();
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats", "sroa {}: {} of {total} allocas promoted",
                st.cgu,
                st.promoted.len()
            )
        };
        {
            let (mut n, mut k, mut kk) = (0, 0, 0);
            for f in st.funcs.values().filter(|f| has_body(ctx, f.op)) {
                for b in f.op.deref(ctx).get_region(ctx, 0).deref(ctx).iter(ctx) {
                    for op in b.deref(ctx).iter(ctx) {
                        if Operation::is_op::<pliron_llvm::ops::CondBrOp>(op, ctx) {
                            n += 1;
                            let c = op.deref(ctx).get_operand(ctx, 0);
                            if st.consts.contains(c) {
                                k += 1;
                            } else if let Some(d) = c.defining_op() {
                                if d.deref(ctx).operands(ctx).all(|o| st.consts.contains(o)) {
                                    kk += 1;
                                }
                            }
                        }
                    }
                }
            }
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::stats", "  cond_br {n}: {k} const cond, {kk} cond of all-const operands")
            };
        }
        if pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
        {
            let mut h: std::collections::BTreeMap<String, usize> = Default::default();
            let kind = |v: Value| -> String {
                let Some(d) = v.defining_op() else {
                    return "arg".into();
                };
                if Operation::is_op::<AllocaOp>(d, ctx) {
                    let uses: Vec<String> = v
                        .uses_iter(ctx)
                        .map(|u| {
                            let o = u.user_op();
                            st.intrinsics
                                .get(o)
                                .cloned()
                                .unwrap_or_else(|| Operation::get_opid(o, ctx).to_string())
                                + &format!("#{}", u.find_index(ctx))
                        })
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    format!("alloca[{}]", uses.join(","))
                } else {
                    Operation::get_opid(d, ctx).to_string()
                }
            };
            for f in st.funcs.values().filter(|f| has_body(ctx, f.op)) {
                for b in f.op.deref(ctx).get_region(ctx, 0).deref(ctx).iter(ctx) {
                    for op in b.deref(ctx).iter(ctx) {
                        if intrinsic(st, op) == Some("llvm.memcpy") {
                            let o: Vec<Value> = op.deref(ctx).operands(ctx).collect();
                            let c = if const_int(ctx, st, o[2]).is_some() {
                                "const"
                            } else {
                                "dyn"
                            };
                            *h.entry(format!("{c} {} <- {}", kind(o[0]), kind(o[1])))
                                .or_default() += 1;
                        }
                    }
                }
            }
            let mut v: Vec<_> = h.into_iter().collect();
            v.sort_by_key(|x| std::cmp::Reverse(x.1));
            for (n, c) in v.iter().take(25) {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::stats", "  memcpy {c:5} {n}")
                };
            }
        }
        if pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
        {
            let mut h: std::collections::BTreeMap<String, usize> = Default::default();
            let fs: Vec<_> = st
                .funcs
                .values()
                .map(|f| f.op)
                .filter(|&f| has_body(ctx, f))
                .collect();
            for f in fs {
                for op in allocas(ctx, f) {
                    let a = op.deref(ctx).get_result(ctx, 0);
                    if !st.promoted.contains(a) {
                        *h.entry(why2(ctx, st, a)).or_default() += 1;
                    }
                }
            }
            // Per-function breakdown for a name filter: `PLIRON_SROA_WHYFN`.
            if let Some(filt) = crate::env_get("PLIRON_SROA_WHYFN") {
                for (name, f) in &st.funcs {
                    if !name.contains(&filt) || !has_body(ctx, f.op) {
                        continue;
                    }
                    for op in allocas(ctx, f.op) {
                        let a = op.deref(ctx).get_result(ctx, 0);
                        if !st.promoted.contains(a) {
                            let sz = st.allocas.get(a).map(|x| x.0).unwrap_or(0);
                            if pliron_cfg::tracing_on() {
                                tracing::info!(target: "pliron::stats", "  whyfn {sz:5}B {} {name}", why2(ctx, st, a))
                            };
                        }
                    }
                }
            }
            let mut v: Vec<_> = h.into_iter().collect();
            v.sort_by_key(|x| std::cmp::Reverse(x.1));
            for (n, c) in v.iter().take(20) {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::stats", "  why {c:6} {n}")
                };
            }
        }
        if pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
        {
            let mut h: std::collections::BTreeMap<String, usize> = Default::default();
            for f in st.funcs.values().filter(|f| has_body(ctx, f.op)) {
                for b in f.op.deref(ctx).get_region(ctx, 0).deref(ctx).iter(ctx) {
                    for op in b.deref(ctx).iter(ctx) {
                        let n = Operation::get_opid(op, ctx).to_string();
                        let n = st.intrinsics.get(op).map_or(n, |i| i.clone());
                        *h.entry(n).or_default() += 1;
                    }
                }
            }
            let mut v: Vec<_> = h.into_iter().collect();
            v.sort_by_key(|x| std::cmp::Reverse(x.1));
            for (n, c) in v.iter().take(30) {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::stats", "  {c:7} {n}")
                };
            }
        }
    }
}

/// First use that blocks promotion of alloca `a` (PLIRON_STATS_WHY).
fn why(ctx: &Context, st: &State, a: Value) -> String {
    let mut memo = AccMemo::default();
    let mut work = vec![a];
    // The store->slot_loads->work re-entry path can cycle through shared
    // GEP/load chains; without a visited set the worklist grew without
    // bound and OOMed the backend under PLIRON_STATS_WHY.
    let mut seen: rustc_hash::FxHashSet<Value> = Default::default();
    let mut uses = Vec::new();
    let mut ubuf = Vec::new();
    while let Some(p) = work.pop() {
        if !seen.insert(p) {
            continue;
        }
        p.uses_into(ctx, &mut ubuf);
        for u in ubuf.iter().copied() {
            let op = u.user_op();
            let idx = u.find_index(ctx);
            if Operation::is_op::<GetElementPtrOp>(op, ctx) && idx == 0 {
                if gep_offset_m(ctx, st, op, &mut memo).is_none() {
                    return "dynamic gep".into();
                }
                work.push(op.deref(ctx).get_result(ctx, 0));
            } else if Operation::is_op::<LoadOp>(op, ctx) {
                uses.push("load");
            } else if Operation::is_op::<StoreOp>(op, ctx) && idx == 1 {
                uses.push("store");
            } else if Operation::is_op::<StoreOp>(op, ctx) && idx == 0 {
                match slot_loads(ctx, st, op.deref(ctx).get_operand(ctx, 1), 0, &mut memo) {
                    Some(ls) => work.extend(ls),
                    None => return "store-ptr escapes".into(),
                }
            } else if Operation::is_op::<PtrToIntOp>(op, ctx) {
                if op
                    .deref(ctx)
                    .get_result(ctx, 0)
                    .uses_iter(ctx)
                    .any(|u| Operation::is_op::<IntToPtrOp>(u.user_op(), ctx))
                {
                    return "ptrtoint roundtrip".into();
                }
                uses.push("ptrtoint");
            } else if Operation::is_op::<ICmpOp>(op, ctx) {
                uses.push("icmp");
            } else if matches!(
                intrinsic(st, op),
                Some("llvm.lifetime.start" | "llvm.lifetime.end")
            ) {
                // Markers are skipped by `accesses` — not the blocker.
            } else if matches!(
                intrinsic(st, op),
                Some("llvm.memcpy" | "llvm.memmove" | "llvm.memset")
            ) && (idx == 0 || (idx == 1 && intrinsic(st, op) != Some("llvm.memset")))
                && const_int(ctx, st, op.deref(ctx).get_operand(ctx, 2)).is_some()
            {
                uses.push("mem");
            } else if diverging_arg(ctx, op, p) || copyable_arg(ctx, st, op, p, idx) {
                uses.push("escape");
            } else if pin_extent(ctx, st, op, idx).is_some() {
                uses.push("pin");
            } else {
                let n = st
                    .intrinsics
                    .get(op)
                    .cloned()
                    .unwrap_or_else(|| Operation::get_opid(op, ctx).to_string());
                let callee = Operation::get_op::<CallOp>(op, ctx)
                    .and_then(|c| match c.callee(ctx) {
                        CallOpCallable::Direct(id) => Some(format!("({id})")),
                        CallOpCallable::Indirect(_) => Some("(indirect)".into()),
                    })
                    .unwrap_or_default();
                return format!("{n} #{idx}{callee}");
            }
        }
    }
    format!("layout ({} accesses)", uses.len().min(10))
}

/// `why`, refined with the accesses/slices verdict (PLIRON_STATS_WHY).
fn why2(ctx: &mut Context, st: &State, a: Value) -> String {
    let w = why(ctx, st, a);
    if !w.starts_with("layout") {
        return w;
    }
    let Some(&(size, _)) = st.allocas.get(a) else {
        return "not in st.allocas".into();
    };
    if size == 0 || size > MAX_SIZE {
        return format!("size {}", if size == 0 { "0" } else { "> MAX_SIZE" });
    }
    let Some((acc, _)) = accesses(ctx, st, a, size, 0) else {
        return "accesses: out of range / self-copy".into();
    };
    if acc.is_empty() {
        return "no accesses".into();
    }
    match slices(ctx, &acc) {
        Err(e) => e.into(),
        Ok(_) => "splittable (round limit?)".into(),
    }
}

/// `forward_single_store` on every body; run before `phisimp` so block args
/// that only ever carry the forwarded value collapse before `split`.
pub fn forward(ctx: &mut Context, st: &State) {
    let funcs: Vec<Ptr<Operation>> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| crate::opt_body(ctx, st, f))
        .collect();
    let n: usize = funcs
        .into_iter()
        .map(|f| forward_single_store(ctx, st, f))
        .sum();
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats", "sroa-fwd {}: {n} single-store allocas forwarded", st.cgu)
        };
    }
}

/// mem2reg's single-store case: an alloca whose only uses are one store and
/// same-typed loads that the store dominates (directly or through zero-offset
/// GEPs) is replaced by the stored value. This frees allocas whose address
/// was parked in such a slot (e.g. a closure's captured `&&K`), so `split`
/// can promote them too.
fn forward_single_store(ctx: &mut Context, st: &State, f: Ptr<Operation>) -> usize {
    let mut dom = None;
    forward_single_store_d(ctx, st, f, &mut dom)
}

/// `forward_single_store` over a caller-held `Dom` — `sroa::run` calls
/// this before and after its split rounds, and the dominator tree only
/// needs rebuilding when a split spliced in a trampoline block.
fn forward_single_store_d(
    ctx: &mut Context,
    st: &State,
    f: Ptr<Operation>,
    dom_slot: &mut Option<crate::domcheck::Dom>,
) -> usize {
    let allocs = allocas(ctx, f);
    // No alloca → no single-store pattern anywhere; skip the domtree.
    if allocs.is_empty() {
        return 0;
    }
    let dom = match dom_slot {
        Some(d) => d,
        slot @ None => match crate::domcheck::Dom::new(ctx, st, f) {
            Some(d) => slot.insert(d),
            None => return 0,
        },
    };
    // `pos` is asked per (store, same-block load) pair — a fresh
    // `ops().position()` scan each time is O(loads × block ops). Block
    // order is stable within this pass, so index each block once.
    let mut posm: FxHashMap<Ptr<BasicBlock>, FxHashMap<Ptr<Operation>, usize>> =
        FxHashMap::default();
    let mut pos = |ctx: &Context, o: Ptr<Operation>| {
        let b = o.deref(ctx).get_parent_block().unwrap();
        posm.entry(b)
            .or_insert_with(|| {
                crate::inline::ops_iter(ctx, b)
                    .enumerate()
                    .map(|(i, x)| (x, i))
                    .collect()
            })
            .get(&o)
            .copied()
    };
    // `PLIRON_LTUNDEF=1`: lifetime markers touch no bytes, so they don't
    // block forwarding (the slices `split` gives markers would otherwise
    // never forward). Sound: a load past a `lifetime.start` reads undefined
    // bytes, which the single stored value refines.
    let skip_markers = crate::env_is("PLIRON_LTUNDEF", "1");
    let mut n = 0;
    for op in allocs {
        let (mut store, mut loads, mut geps, mut ok) = (None, Vec::new(), Vec::new(), true);
        let mut work = vec![op.deref(ctx).get_result(ctx, 0)];
        let mut ubuf = Vec::new();
        while let Some(p) = work.pop() {
            p.uses_into(ctx, &mut ubuf);
            for u in ubuf.iter().copied() {
                let o = u.user_op();
                ok &= !st.volatile.contains(o)
                    && if Operation::is_op::<StoreOp>(o, ctx) && u.find_index(ctx) == 1 {
                        store.replace(o).is_none()
                    } else if Operation::is_op::<LoadOp>(o, ctx) {
                        loads.push(o);
                        true
                    } else if Operation::is_op::<GetElementPtrOp>(o, ctx)
                        && u.find_index(ctx) == 0
                        && (gep_offset(ctx, st, o) == Some(0)
                            || !o.deref(ctx).get_result(ctx, 0).is_used(ctx))
                    {
                        geps.push(o);
                        work.push(o.deref(ctx).get_result(ctx, 0));
                        true
                    } else {
                        skip_markers
                            && matches!(
                                intrinsic(st, o),
                                Some("llvm.lifetime.start" | "llvm.lifetime.end")
                            )
                    };
            }
        }
        let Some(s) = store.filter(|_| ok && !loads.is_empty()) else {
            continue;
        };
        let v = s.deref(ctx).get_operand(ctx, 0);
        let vt = v.get_type(ctx);
        let sb = s.deref(ctx).get_parent_block().unwrap();
        let fwd = dom.contains(sb)
            && loads.iter().all(|&l| {
                let lb = l.deref(ctx).get_parent_block().unwrap();
                // Unreachable loads never run (lowering traps those blocks).
                l.deref(ctx).get_result(ctx, 0).get_type(ctx) == vt
                    && (!dom.contains(lb)
                        || if lb == sb {
                            pos(ctx, s) < pos(ctx, l)
                        } else {
                            dom.dominates(sb, lb)
                        })
            });
        if !fwd || !crate::bisect_in("sroa-fwd", "") {
            continue;
        }
        for l in loads {
            let r = l.deref(ctx).get_result(ctx, 0);
            r.replace_all_uses_with(ctx, &v);
            Operation::erase(l, ctx);
        }
        Operation::erase(s, ctx);
        erase_dead(ctx, geps);
        n += 1;
    }
    n
}
