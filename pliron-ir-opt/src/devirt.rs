//! Indirect-call promotion from a value profile (`PLIRON_DEVIRT`, on by
//! default in profile builds; docs/pgo.md).
//!
//! The front end marks every virtual call (and, with
//! `PLIRON_DEVIRT_FNPTR=1`, every call through a fn pointer) in
//! `State::icall_sites`. The instrumented build (`pgo::instrument`) puts a
//! `__pliron_prof_icall(site, target)` call in front of each; the runtime
//! keeps the first `pgo::SLOTS` distinct targets per site with counts, and
//! `pliron-prof` names them through the address tables every instrumented
//! CGU registers. The profile-use build emits the same plain indirect
//! calls — so the CFG fingerprint never depends on this — and after
//! `pgo::load` [`promote`] rewrites each hot site into
//!
//! ```text
//! if fnptr == &target1 -> direct call target1
//! else if fnptr == &target2 -> direct call target2
//! else -> the original indirect call
//! ```
//!
//! The guard and the call are two roles with two symbols. The guard must
//! name the address the profile saw, the *home* definition: the target's
//! own symbol when the home is this CGU, otherwise the home's alias
//! ([`home_alias`], which the home CGU defines for every profiled target
//! it holds as a mono item — [`define_aliases`]). The call goes to a body
//! in this CGU when there is one (the home itself, a cross-CGU copy, or
//! this crate's instantiation of a dependency's function), so the inliner
//! can take it; otherwise it calls the home through the alias. The
//! indirect arm is always kept, so a wrong or stale profile costs a
//! compare, never correctness — provided the alias links, which is what
//! `Icp::resolve` answers.

use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::irbuild::inserter::OpInsertionPoint;
use pliron::irbuild::listener::DummyListener;
use pliron::irbuild::rewriter::{IRRewriter, Rewriter};
use pliron::linked_list::ContainsLinkedList;
use pliron::location::Located;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed, TypedHandle};
use pliron::value::Value;
use pliron_llvm::attributes::ICmpPredicateAttr;
use pliron_llvm::ops::{BrOp, CallOp, CondBrOp, ICmpOp};
use pliron_llvm::types::{FuncType, PointerType, VoidType};

use crate::has_body;
use crate::inline::blocks;
use crate::pgo::ProfTarget;
use crate::state::{CallInfo, ConstVal, Exts, State};

/// The runtime entry the instrumented build calls before each site.
pub const ICALL_SYM: &str = "__pliron_prof_icall";

/// A profile build is being made (generate or use).
pub fn profile_build() -> bool {
    crate::env_set("PLIRON_PGO") || crate::env_set("PLIRON_PGO_USE")
}

/// Value-profiled promotion is on: any profile build, unless
/// `PLIRON_DEVIRT=0`. The instrumented and the profile-use build must
/// agree (they also make the same symbol-promotion decisions from it).
pub fn icp_on() -> bool {
    profile_build() && !crate::env_is("PLIRON_DEVIRT", "0")
}

fn knob(name: &str, default: u64) -> u64 {
    crate::env_get(name).and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// A function's sites in profile order: the marked call ops that are
/// still indirect, by position, with the called pointer.
pub fn sites(ctx: &Context, st: &State, bs: &[Ptr<BasicBlock>]) -> Vec<(Ptr<Operation>, Value)> {
    let mut out = Vec::new();
    if st.icall_sites.is_empty() {
        return out;
    }
    for b in bs {
        for op in b.deref(ctx).iter(ctx) {
            if st.icall_sites.get(op).is_some()
                && let Some(c) = Operation::get_op::<CallOp>(op, ctx)
                && let CallOpCallable::Indirect(callee) = c.callee(ctx)
            {
                out.push((op, callee));
            }
        }
    }
    out
}

/// Whether `pgo::instrument` will record any site in this CGU (and so
/// call [`ICALL_SYM`]). An import nothing references must not be
/// declared: the registration ctor's asm is spliced against the
/// object's symbols.
pub fn any_sites(ctx: &Context, st: &State) -> bool {
    !st.icall_sites.is_empty()
        && st.funcs.iter().any(|(n, f)| {
            has_body(ctx, f.op) && !st.dead_fns.contains(n) && !sites(ctx, st, &blocks(ctx, f.op)).is_empty()
        })
}

fn plain_call(fn_ty: TypeHandle, callee: &str) -> CallInfo {
    CallInfo {
        fn_ty,
        exts: Exts::default(),
        foreign: false,
        cc: None,
        tail: false,
        nocap: None,
        sret_nocap: false,
        arg_extent: None,
        arg_noalias: None,
        arg_byval_ro: None,
        arg_ro: None,
        bb: false,
        callee: Some(callee.to_string()),
        noret: false,
    }
}

/// Instrumented build: `__pliron_prof_icall(site, target)` right before
/// the call `op`. The caller of `pgo::instrument` declares the import.
pub(crate) fn record_call(
    ctx: &mut Context,
    st: &mut State,
    op: Ptr<Operation>,
    site: Value,
    target: Value,
) {
    let ptr_t: TypeHandle = PointerType::get(ctx, 0).into();
    let void: TypeHandle = VoidType::get(ctx).into();
    let fty = FuncType::get(ctx, void, vec![ptr_t, ptr_t], false);
    let id = crate::pgo::ident(st, ICALL_SYM);
    let c = CallOp::new(ctx, CallOpCallable::Direct(id), fty, vec![site, target]).get_operation();
    let loc = op.deref(ctx).loc();
    c.deref_mut(ctx).set_loc(loc);
    c.insert_before(ctx, op);
    st.calls.insert(c, plain_call(fty.into(), ICALL_SYM));
}

/// The symbol a home CGU defines next to a profiled target, for guards
/// in other objects: `<target>.icp.<hash of the home CGU's name>`. A
/// second name is needed because the target's own name can be taken in
/// the guarding CGU by a local copy (cross-CGU copies keep the symbol
/// name, with internal linkage), and because the target itself may be
/// private to its CGU.
pub fn home_alias(sym: &str, cgu: u64) -> String {
    format!("{sym}.icp.{cgu:016x}")
}

/// How one profiled target is guarded and called from the current CGU.
pub struct Resolved {
    /// The symbol whose address the profile saw.
    pub guard: String,
    /// The symbol the hit arm calls: a body in this CGU, or `guard`.
    pub call: String,
    /// `L`: the home definition is in this CGU. `C`: guard on the home's
    /// alias, call to a body in this CGU. `X`: guard and call through
    /// the alias (no body here).
    pub tag: &'static str,
}

/// A direct call on a guard's hit arm (`State::icp_calls`).
#[derive(Clone, Debug)]
pub struct IcpCall {
    /// The function holding the original site, the profiled target, and
    /// the symbol called.
    pub caller: String,
    pub target: String,
    pub callee: String,
    /// Profiled calls of this target at the site.
    pub count: u64,
    pub tag: &'static str,
}

/// What the rustc side knows that this crate does not.
pub struct Icp<'a> {
    /// Hash of this CGU's name (`pgo::cgu_hash`), as the instrumented
    /// build stored it with every target it defined (CGU names carry the
    /// crate's name and disambiguator).
    pub cgu: u64,
    /// The guard and call symbols for a target seen from this CGU, or
    /// the reason (for the remark) it cannot be guarded.
    pub resolve: &'a dyn Fn(&State, &ProfTarget) -> Result<Resolved, &'static str>,
    /// Declare an import of `sym` with the given signature.
    pub declare: &'a mut dyn FnMut(&mut Context, &mut State, &str, TypeHandle, Exts),
}

/// Profile-use build, in the home CGU: define the alias of every
/// profiled target in `homes` (symbols this CGU holds as mono items, by
/// the crate-wide rule the guarding CGUs also apply). The function stays
/// as it is — private if it was — but counts as address-taken from here
/// on (`address_taken` reads `State::aliases`), and it is never turned
/// back into an import (`xcgu_home`). Returns the aliases defined.
pub fn define_aliases<'s>(
    ctx: &Context,
    st: &mut State,
    cgu: u64,
    homes: impl Iterator<Item = &'s str>,
) -> usize {
    let mut n = 0;
    for sym in homes {
        let body = st.funcs.get(sym).is_some_and(|f| {
            f.linkage != cranelift_module::Linkage::Import && has_body(ctx, f.op)
        });
        if !body {
            // The crate-wide rule said this CGU defines it: a guard
            // elsewhere may reference the alias, and the link would say
            // so. Make the mismatch findable.
            if pliron_cfg::tracing_on() {
                tracing::warn!(target: "pliron::pgo", "pliron-pgo: {}: no body for profiled target {sym}: home alias not defined", st.cgu);
            }
            continue;
        }
        if st.xcgu_home.contains(sym) {
            st.xcgu_home = st.xcgu_home.iter().filter(|s| s.as_str() != sym).cloned().collect();
        }
        st.aliases.push((home_alias(sym, cgu), sym.to_string(), false));
        n += 1;
    }
    n
}

/// Profile-use build, after `pgo::load`: guard the hot targets of every
/// indirect-call site. Per site, at most `PLIRON_DEVIRT_MAX` (default 2)
/// targets with at least `PLIRON_DEVIRT_MIN_SHARE` percent (default 10)
/// of the site's calls, hottest first. Returns the number of sites
/// rewritten.
///
/// Which targets are usable, and through which symbols, is
/// `icp.resolve`'s answer (see the module comment).
pub fn promote(ctx: &mut Context, st: &mut State, icp: &mut Icp<'_>) -> usize {
    if st.icall_sites.is_empty() {
        return 0;
    }
    let max = knob("PLIRON_DEVIRT_MAX", 2) as usize;
    let share = knob("PLIRON_DEVIRT_MIN_SHARE", 10) as u128;
    let funcs: Vec<(String, Ptr<Operation>)> = st
        .funcs
        .iter()
        .filter(|(_, f)| has_body(ctx, f.op))
        .map(|(n, f)| (n.clone(), f.op))
        .collect();
    let mut rw = IRRewriter::<DummyListener>::default();
    let mut done = 0;
    for (caller, fop) in funcs {
        let Some(psites) = st.prof.get(fop).map(|p| p.sites.clone()) else {
            continue;
        };
        let fsites = sites(ctx, st, &blocks(ctx, fop));
        if fsites.is_empty() {
            continue;
        }
        if psites.len() != fsites.len() {
            pliron_cfg::remark!("devirt", caller, "";
                "profile has {} sites, function has {}: not promoted", psites.len(), fsites.len());
            continue;
        }
        for (k, ((op, callee), ps)) in fsites.into_iter().zip(psites).enumerate() {
            let what = st.icall_sites.get(op).cloned().unwrap_or_default();
            let total: u64 = ps.targets.iter().fold(ps.other, |a, t| a.saturating_add(t.count));
            if total == 0 {
                continue;
            }
            let Some(info) = st.calls.get(op).cloned() else {
                continue;
            };
            if info.tail {
                continue;
            }
            let mut order: Vec<&ProfTarget> = ps.targets.iter().collect();
            order.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
            let mut kept: Vec<(&ProfTarget, Resolved)> = Vec::new();
            let mut notes: Vec<String> = Vec::new();
            for t in order {
                let pct = t.count as u128 * 100 / total as u128;
                if t.count == 0 || (t.count as u128) * 100 < share * total as u128 {
                    continue;
                }
                if kept.len() >= max {
                    notes.push(format!("{pct}% over the arm limit"));
                    continue;
                }
                let ok = (icp.resolve)(st, t).and_then(|r| {
                    let sig = |s: &str| st.funcs.get(s).is_none_or(|f| f.ty == info.fn_ty);
                    if !sig(&r.call) || !sig(&t.name) {
                        return Err("signature differs");
                    }
                    // `PLIRON_DEVIRT_LINKED=0`: only targets with a body in
                    // this CGU. A guard for a linked target buys a direct
                    // call (no inlining); this isolates what that is worth.
                    if r.tag == "X" && crate::env_is("PLIRON_DEVIRT_LINKED", "0") {
                        return Err("linked targets off");
                    }
                    Ok(r)
                });
                match ok {
                    Ok(r) => {
                        notes.push(format!("{pct}%{} [{}]", r.tag, t.name));
                        kept.push((t, r));
                    }
                    Err(why) => notes.push(format!("{pct}% unusable ({why}) [{}]", t.name)),
                }
            }
            let other_pct = ps.other as u128 * 100 / total as u128;
            pliron_cfg::remark!("devirt", caller, what;
                "icp: site {k} kept {} of {} targets, calls {total} (shares {}; unnamed {other_pct}%; L = home body in this CGU, C = copy in this CGU, X = linked)",
                kept.len(), ps.targets.len(), notes.join(" "));
            if kept.is_empty() {
                continue;
            }
            let kept: Vec<(ProfTarget, Resolved)> = kept.into_iter().map(|(t, r)| (t.clone(), r)).collect();
            guard(ctx, st, &mut rw, icp, &caller, op, callee, &info, &kept, total);
            done += 1;
        }
    }
    done
}

/// Rewrite `op` (an indirect call through `callee`) into the guard chain
/// for `kept`, the original call staying as the last arm.
fn guard(
    ctx: &mut Context,
    st: &mut State,
    rw: &mut IRRewriter<DummyListener>,
    icp: &mut Icp<'_>,
    caller: &str,
    op: Ptr<Operation>,
    callee: Value,
    info: &CallInfo,
    kept: &[(ProfTarget, Resolved)],
    total: u64,
) {
    let block = op.deref(ctx).get_parent_block().unwrap();
    let region = block.deref(ctx).get_parent_region().unwrap();
    let loc = op.deref(ctx).loc();
    let args = Operation::get_op::<CallOp>(op, ctx).unwrap().args(ctx);
    let res = (op.deref(ctx).get_num_results() > 0).then(|| op.deref(ctx).get_result(ctx, 0));
    let unwind = st.invokes.get(op).copied();
    let fty = TypedHandle::<FuncType>::from_handle(info.fn_ty, ctx).unwrap();
    let ptr_t: TypeHandle = PointerType::get(ctx, 0).into();
    // Everything after the call continues in `cont`, which takes the
    // call's result as a block argument.
    let cont = rw.split_block(ctx, block, OpInsertionPoint::AfterOperation(op), None);
    let site_c = st.prof_bcount.get(block).copied();
    if let Some(c) = site_c {
        st.prof_bcount.insert(cont, c);
    }
    if let Some(res) = res {
        let ty = res.get_type(ctx);
        BasicBlock::push_argument(cont, ctx, ty);
        let arg = cont.deref(ctx).get_argument(ctx, 0);
        res.replace_all_uses_with(ctx, &arg);
    }
    let new_block = |ctx: &mut Context| {
        let b = BasicBlock::new(ctx, None, vec![]);
        b.insert_at_back(region, ctx);
        b
    };
    let leave = |ctx: &mut Context, from: Ptr<BasicBlock>, call: Ptr<Operation>| {
        let rv: Vec<Value> = res.map(|_| call.deref(ctx).get_result(ctx, 0)).into_iter().collect();
        let br = BrOp::new(ctx, cont, rv).get_operation();
        br.deref_mut(ctx).set_loc(loc.clone());
        br.insert_at_back(from, ctx);
    };
    // The original call moves to the fallback block.
    let fallback = new_block(ctx);
    op.unlink(ctx);
    op.insert_at_back(fallback, ctx);
    leave(ctx, fallback, op);
    // Counts: the site's block count split by the profiled shares.
    let scale = |c: u64| match site_c {
        Some(s) => (c as u128 * s as u128 / total.max(1) as u128) as u64,
        None => c,
    };
    let mut remaining = site_c.unwrap_or(total);
    let guards: Vec<Ptr<BasicBlock>> = kept.iter().map(|_| new_block(ctx)).collect();
    let br = BrOp::new(ctx, guards[0], vec![]).get_operation();
    br.deref_mut(ctx).set_loc(loc.clone());
    br.insert_at_back(block, ctx);
    for (k, (t, r)) in kept.iter().enumerate() {
        if r.guard != t.name {
            st.icp_aliases.insert(r.guard.clone());
        }
        for s in [&r.guard, &r.call] {
            if st.funcs.get(s).is_none() {
                (icp.declare)(ctx, st, s, info.fn_ty, info.exts.clone());
            }
        }
        let hit = new_block(ctx);
        let next = guards.get(k + 1).copied().unwrap_or(fallback);
        let addr = crate::sroa::mk_const(ctx, st, ptr_t, ConstVal::Sym { sym: r.guard.clone(), off: 0 });
        let eq = ICmpOp::new(ctx, ICmpPredicateAttr::EQ, callee, addr).get_operation();
        eq.deref_mut(ctx).set_loc(loc.clone());
        eq.insert_at_back(guards[k], ctx);
        let cond = eq.deref(ctx).get_result(ctx, 0);
        let cbr = CondBrOp::new(ctx, cond, hit, vec![], next, vec![]).get_operation();
        cbr.deref_mut(ctx).set_loc(loc.clone());
        cbr.insert_at_back(guards[k], ctx);
        let id = crate::pgo::ident(st, &r.call);
        let call = CallOp::new(ctx, CallOpCallable::Direct(id), fty, args.clone()).get_operation();
        call.deref_mut(ctx).set_loc(loc.clone());
        call.insert_at_back(hit, ctx);
        let mut ci = info.clone();
        ci.callee = Some(r.call.clone());
        st.calls.insert(call, ci);
        if pliron_cfg::remarks_on() {
            st.icp_calls.insert(
                call,
                IcpCall {
                    caller: caller.to_string(),
                    target: t.name.clone(),
                    callee: r.call.clone(),
                    count: t.count,
                    tag: r.tag,
                },
            );
        }
        if let Some(u) = unwind {
            st.invokes.insert(call, u);
        }
        leave(ctx, hit, call);
        if site_c.is_some() || st.prof.get(region.deref(ctx).get_parent_op()).is_some() {
            let c = scale(t.count).min(remaining);
            let rest = remaining - c;
            st.prof_bcount.insert(guards[k], remaining);
            st.prof_bcount.insert(hit, c);
            st.prof_edges.insert(cbr, vec![c, rest]);
            if remaining >= 8 && (c >= 4 * rest || rest >= 4 * c) && !crate::env_is("PLIRON_PGO_EXPECT", "0") {
                st.expect.insert(cbr, c >= rest);
            }
            remaining = rest;
        }
    }
    if site_c.is_some() {
        st.prof_bcount.insert(fallback, remaining);
    }
    st.icall_sites.remove(op);
}

/// After `constload`, before the static devirtualization
/// (`inline::devirt`): a guard whose called pointer has become a
/// constant function address decides nothing — inlining exposed the
/// vtable, so the call is resolved without a profile. Comparing that
/// constant with a home alias cannot be folded by address (two symbols,
/// one address, known only to the linker; or a local copy's address,
/// which differs), and it does not have to be: when the constant names
/// the guarded target itself, the guard's arm calls — or by now is —
/// that very function, whichever copy of it the pointer holds, so the
/// guard is taken as true and the indirect arm goes. When it names
/// another function the guard is false and its arm goes. Left alone,
/// every such site kept two address materializations and a compare for
/// nothing (lol_html's `&mut dyn FnMut(&[u8])` forwarder, 62k calls);
/// taking the indirect arm instead would lose the body, because a
/// cross-CGU copy is an import again by the time `inline::devirt` makes
/// the call direct (`home_copies`). Returns the guards removed.
pub fn fold_static(ctx: &mut Context, st: &mut State) -> usize {
    if st.icp_aliases.is_empty() || crate::env_is("PLIRON_DEVIRT_FOLD", "0") {
        return 0;
    }
    let fns: Vec<Ptr<Operation>> =
        st.funcs.iter().filter(|(_, f)| crate::opt_body(ctx, st, f.op)).map(|(_, f)| f.op).collect();
    let sym = |st: &State, v: Value| match st.consts.get(v) {
        Some(ConstVal::Sym { sym, off: 0 }) => Some(sym.clone()),
        _ => None,
    };
    let mut n = 0;
    for f in fns {
        let mut hits: Vec<(Ptr<Operation>, bool)> = Vec::new();
        for b in blocks(ctx, f) {
            for op in b.deref(ctx).iter(ctx) {
                if Operation::get_op::<ICmpOp>(op, ctx).is_some()
                    && let Some(a) = sym(st, op.deref(ctx).get_operand(ctx, 1))
                    && st.icp_aliases.contains(&a)
                    && let Some(x) = sym(st, op.deref(ctx).get_operand(ctx, 0))
                {
                    let same = a.strip_prefix(x.as_str()).is_some_and(|r| r.starts_with(".icp."));
                    hits.push((op, same));
                }
            }
        }
        if hits.is_empty() {
            continue;
        }
        for (op, same) in hits {
            let ty = op.deref(ctx).get_result(ctx, 0).get_type(ctx);
            let no = crate::sroa::mk_const(ctx, st, ty, ConstVal::Bits(same as u128));
            op.deref(ctx).get_result(ctx, 0).replace_all_uses_with(ctx, &no);
            Operation::erase(op, ctx);
            n += 1;
        }
        crate::cleanup::fold_branches(ctx, st, f);
    }
    n
}

/// After the inline rounds: one `icp` remark per promoted call (and per
/// clone the inliner made of one): `inlined`, or `call` when it is still
/// a call in a function that is emitted (the `inline` remark of the same
/// caller and callee says why). Remarks only.
pub fn report(ctx: &Context, st: &mut State) {
    if (st.icp_calls.is_empty() && st.icp_inlined.is_empty()) || !pliron_cfg::remarks_on() {
        return;
    }
    let mut rows: Vec<String> = Vec::new();
    let row = |state: &str, c: &IcpCall, at: &str, cgu: &str| {
        format!(
            "{}\t{}\t{state} tag={} calls={} callee={} in={at} cgu={cgu}",
            c.caller, c.target, c.tag, c.count, c.callee
        )
    };
    for c in &st.icp_inlined {
        rows.push(row("inlined", c, "", &st.cgu));
    }
    for (n, f) in st.funcs.iter() {
        if !has_body(ctx, f.op) || st.dead_fns.contains(n.as_str()) || st.dead_unopt.contains(n.as_str()) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            for op in b.deref(ctx).iter(ctx) {
                if let Some(c) = st.icp_calls.get(&op) {
                    rows.push(row("call", c, n, &st.cgu));
                }
            }
        }
    }
    rows.sort();
    for r in rows {
        let mut f = r.splitn(3, '\t');
        let (func, what, why) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
        pliron_cfg::remark("icp", func, &what, &why);
    }
    st.icp_calls.clear();
    st.icp_inlined.clear();
}
