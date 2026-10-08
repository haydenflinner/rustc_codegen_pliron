//! SROA + mem2reg. codegen_ssa puts every non-SSA local in a byte-array
//! alloca; this splits each non-escaping alloca into one alloca per accessed
//! slice (expanding constant-size memcpy/memmove/memset into per-slice
//! loads/stores) and marks the slices in `State::promoted`. Lowering turns a
//! promoted alloca into Cranelift `Variable`s, so SSA construction (including
//! across `try_call` exception edges) is done by cranelift-frontend.
//! `PLIRON_SROA=0` disables it.

use pliron::builtin::types::{IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;
use pliron_llvm::ops::{AllocaOp, GepIndex, GetElementPtrOp, LoadOp, StoreOp, UndefOp};
use pliron_llvm::types::ArrayType;
use rustc_data_structures::fx::FxHashSet;

use crate::context::{ConstVal, State, mask};
use crate::lower::has_body;
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
        byte: u8,
    },
}

#[derive(Clone, Copy)]
struct Access {
    op: Ptr<Operation>,
    off: u64,
    kind: Kind,
}

struct Slice {
    off: u64,
    size: u64,
    ty: TypeHandle,
}

pub(crate) fn const_int(ctx: &Context, st: &State<'_>, v: Value) -> Option<i128> {
    let b = match st.consts.get(&v)? {
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
pub(crate) fn gep_offset(ctx: &Context, st: &State<'_>, op: Ptr<Operation>) -> Option<i64> {
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

fn intrinsic<'a>(st: &'a State<'_>, op: Ptr<Operation>) -> Option<&'a str> {
    st.intrinsics.get(&op).map(|s| s.as_str())
}

/// Collect every access to `a` (through constant-offset GEPs), or None if it escapes.
fn accesses(
    ctx: &Context,
    st: &State<'_>,
    a: Value,
    size: u64,
) -> Option<(Vec<Access>, Vec<Ptr<Operation>>)> {
    let mut out = Vec::new();
    let mut geps = Vec::new();
    let mut work = vec![(a, 0i64)];
    while let Some((p, base)) = work.pop() {
        for u in p.uses(ctx) {
            let op = u.user_op();
            let idx = u.find_index(ctx);
            if st.volatile.contains(&op) {
                return None;
            }
            let in_range = |off: i64, n: u64| {
                off >= 0 && (off as u64).checked_add(n).is_some_and(|e| e <= size)
            };
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                if idx != 0 {
                    return None;
                }
                let o = base.checked_add(gep_offset(ctx, st, op)?)?;
                geps.push(op);
                work.push((op.deref(ctx).get_result(0), o));
            } else if Operation::is_op::<LoadOp>(op, ctx) {
                let ty = op.deref(ctx).get_result(0).get_type(ctx);
                if !in_range(base, size_align(ctx, ty).0) {
                    return None;
                }
                out.push(Access {
                    op,
                    off: base as u64,
                    kind: Kind::Load,
                });
            } else if Operation::is_op::<StoreOp>(op, ctx) {
                let ty = op.deref(ctx).get_operand(0).get_type(ctx);
                if idx != 1 || !in_range(base, size_align(ctx, ty).0) {
                    return None;
                }
                out.push(Access {
                    op,
                    off: base as u64,
                    kind: Kind::Store,
                });
            } else if let Some(name) = intrinsic(st, op) {
                let opnds: Vec<Value> = op.deref(ctx).operands().collect();
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
                        let byte = const_int(ctx, st, opnds[1])? as u8;
                        Kind::Set {
                            n: u64::try_from(n).ok()?,
                            byte,
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
                });
            } else {
                return None;
            }
        }
    }
    // A copy with both ends in this alloca would need overlap handling.
    let mut seen = FxHashSet::default();
    if out.iter().any(|a| !seen.insert(a.op)) {
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
        Kind::Load => Some(a.op.deref(ctx).get_result(0).get_type(ctx)),
        Kind::Store => Some(a.op.deref(ctx).get_operand(0).get_type(ctx)),
        _ => None,
    }
}

/// Partition the alloca into slices; None if typed accesses overlap
/// inconsistently or a copy/set cuts through a typed slice.
fn slices(ctx: &mut Context, acc: &[Access]) -> Result<Vec<Slice>, &'static str> {
    let mut typed: Vec<Slice> = Vec::new();
    for a in acc {
        let Some(ty) = access_ty(ctx, a) else {
            continue;
        };
        let size = size_align(ctx, ty).0;
        if size == 0 {
            return Err("zero-size access");
        }
        match typed
            .iter()
            .find(|s| s.off < a.off + size && a.off < s.off + s.size)
        {
            Some(s) if s.off == a.off && s.size == size && leaf_compatible(ctx, s.ty, ty) => {}
            Some(_) => return Err("typed accesses overlap"),
            None => typed.push(Slice {
                off: a.off,
                size,
                ty,
            }),
        }
    }
    let mut cuts: Vec<u64> = Vec::new();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for a in acc {
        if let Kind::Copy { n, .. } | Kind::Set { n, .. } = a.kind {
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
            while o % k != 0 || o + k > e {
                k /= 2;
            }
            gaps.push((o, k));
            o += k;
        }
    }
    for (off, k) in gaps {
        let ty = IntegerType::get(ctx, (k * 8) as u32, Signedness::Signless).into();
        typed.push(Slice { off, size: k, ty });
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

pub(crate) fn mk_const(
    ctx: &mut Context,
    st: &mut State<'_>,
    ty: TypeHandle,
    cv: ConstVal,
) -> Value {
    let op = UndefOp::new(ctx, ty).get_operation();
    let v = op.deref(ctx).get_result(0);
    st.consts.insert(v, cv);
    v
}

pub(crate) fn ptr_plus(
    ctx: &mut Context,
    st: &mut State<'_>,
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
    g.deref(ctx).get_result(0)
}

fn erase_dead(ctx: &mut Context, mut ops: Vec<Ptr<Operation>>) {
    loop {
        let before = ops.len();
        ops.retain(|&op| {
            if op.deref(ctx).results().any(|r| r.is_used(ctx)) {
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

fn split(ctx: &mut Context, st: &mut State<'_>, alloca: Ptr<Operation>) -> bool {
    let a = alloca.deref(ctx).get_result(0);
    let Some(&(size, align)) = st.allocas.get(&a) else {
        return false;
    };
    if st.promoted.contains_key(&a) {
        validate(ctx, st, a);
        if st.promoted.contains_key(&a) {
            return false;
        }
    }
    if size == 0 || size > MAX_SIZE {
        return false;
    }
    let Some((acc, geps)) = accesses(ctx, st, a, size) else {
        return false;
    };
    if acc.is_empty() {
        return false;
    }
    if let Some(t) = trivially_promotable(ctx, &acc, &geps, size) {
        st.promoted.insert(a, t);
        return true;
    }
    let Ok(sl) = slices(ctx, &acc) else {
        return false;
    };

    let count = alloca.deref(ctx).get_operand(0);
    let i8t: TypeHandle = IntegerType::get(ctx, 8, Signedness::Signless).into();
    let new: Vec<Value> = sl
        .iter()
        .map(|s| {
            let arr = ArrayType::get(ctx, i8t, s.size).into();
            let op = AllocaOp::new(ctx, arr, count, 0).get_operation();
            op.insert_before(ctx, alloca);
            let v = op.deref(ctx).get_result(0);
            let al = if s.off == 0 {
                align
            } else {
                align.min(1 << s.off.trailing_zeros())
            };
            st.allocas.insert(v, (s.size, al.max(1)));
            st.promoted.insert(v, s.ty);
            v
        })
        .collect();
    let find = |off: u64| sl.iter().position(|s| s.off == off).unwrap();
    for x in &acc {
        match x.kind {
            Kind::Load => Operation::replace_operand(x.op, ctx, 0, new[find(x.off)]),
            Kind::Store => Operation::replace_operand(x.op, ctx, 1, new[find(x.off)]),
            Kind::Copy { dst, n } => {
                let other = x.op.deref(ctx).get_operand(if dst { 1 } else { 0 });
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
                    stores.push((l.deref(ctx).get_result(0), to));
                }
                for (v, to) in stores {
                    StoreOp::new(ctx, v, to)
                        .get_operation()
                        .insert_before(ctx, x.op);
                }
                st.intrinsics.remove(&x.op);
                Operation::erase(x.op, ctx);
            }
            Kind::Set { n, byte } => {
                for (i, s) in sl
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| x.off <= s.off && s.off + s.size <= x.off + n)
                {
                    let cv = if byte == 0 {
                        ConstVal::Zero
                    } else {
                        let rep = (0..s.size.min(16))
                            .fold(0u128, |acc, k| acc | ((byte as u128) << (8 * k)));
                        ConstVal::Bits(mask(rep, (s.size * 8) as u32))
                    };
                    let c = mk_const(ctx, st, s.ty, cv);
                    StoreOp::new(ctx, c, new[i])
                        .get_operation()
                        .insert_before(ctx, x.op);
                }
                st.intrinsics.remove(&x.op);
                Operation::erase(x.op, ctx);
            }
        }
    }
    erase_dead(ctx, geps.into_iter().rev().collect());
    if !a.is_used(ctx) {
        st.allocas.remove(&a);
        Operation::erase(alloca, ctx);
    }
    true
}

/// Drop promotion for any alloca that ended up with a use lowering can't
/// turn into a variable access (it then stays a stack slot).
fn validate(ctx: &Context, st: &mut State<'_>, a: Value) {
    let Some(&ty) = st.promoted.get(&a) else {
        return;
    };
    let ok = a.uses(ctx).iter().all(|u| {
        let op = u.user_op();
        let idx = u.find_index(ctx);
        if Operation::is_op::<LoadOp>(op, ctx) {
            leaf_compatible(ctx, ty, op.deref(ctx).get_result(0).get_type(ctx))
        } else if Operation::is_op::<StoreOp>(op, ctx) {
            idx == 1 && leaf_compatible(ctx, ty, op.deref(ctx).get_operand(0).get_type(ctx))
        } else {
            false
        }
    });
    if !ok {
        st.promoted.remove(&a);
    }
}

/// Allocas anywhere in `f` (inlined callees' allocas live outside the entry block).
pub(crate) fn allocas(ctx: &Context, f: Ptr<Operation>) -> Vec<Ptr<Operation>> {
    let r = f.deref(ctx).get_region(0);
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

pub fn run(ctx: &mut Context, st: &mut State<'_>) {
    let funcs: Vec<Ptr<Operation>> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| has_body(ctx, f))
        .collect();
    for f in funcs {
        // Again after `phisimp`: loads that went through trivial block args
        // now name the slot directly.
        if crate::pass_enabled("PLIRON_SROA_FWD") {
            forward_single_store(ctx, st, f);
        }
        for _round in 0..3 {
            let allocas = allocas(ctx, f);
            let mut changed = false;
            for op in allocas {
                changed |= split(ctx, st, op);
            }
            if !changed {
                break;
            }
        }
        // And after splitting: a slice left holding one stored value.
        if crate::pass_enabled("PLIRON_SROA_FWD") {
            forward_single_store(ctx, st, f);
        }
        for op in allocas(ctx, f) {
            let a = op.deref(ctx).get_result(0);
            if !a.is_used(ctx) {
                st.allocas.remove(&a);
                st.promoted.remove(&a);
                Operation::erase(op, ctx);
                continue;
            }
            validate(ctx, st, a);
        }
    }
    if std::env::var_os("PLIRON_STATS").is_some() {
        let total = st
            .funcs
            .values()
            .filter(|f| has_body(ctx, f.op))
            .map(|f| allocas(ctx, f.op).len())
            .sum::<usize>();
        eprintln!(
            "sroa {}: {} of {total} allocas promoted",
            st.cgu,
            st.promoted.len()
        );
        {
            let (mut n, mut k, mut kk) = (0, 0, 0);
            for f in st.funcs.values().filter(|f| has_body(ctx, f.op)) {
                for b in f.op.deref(ctx).get_region(0).deref(ctx).iter(ctx) {
                    for op in b.deref(ctx).iter(ctx) {
                        if Operation::is_op::<pliron_llvm::ops::CondBrOp>(op, ctx) {
                            n += 1;
                            let c = op.deref(ctx).get_operand(0);
                            if st.consts.contains_key(&c) {
                                k += 1;
                            } else if let Some(d) = c.defining_op() {
                                if d.deref(ctx).operands().all(|o| st.consts.contains_key(&o)) {
                                    kk += 1;
                                }
                            }
                        }
                    }
                }
            }
            eprintln!("  cond_br {n}: {k} const cond, {kk} cond of all-const operands");
        }
        if std::env::var_os("PLIRON_STATS_MEMCPY").is_some() {
            let mut h: std::collections::BTreeMap<String, usize> = Default::default();
            let kind = |v: Value| -> String {
                let Some(d) = v.defining_op() else {
                    return "arg".into();
                };
                if Operation::is_op::<AllocaOp>(d, ctx) {
                    let uses: Vec<String> = v
                        .uses(ctx)
                        .iter()
                        .map(|u| {
                            let o = u.user_op();
                            st.intrinsics
                                .get(&o)
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
                for b in f.op.deref(ctx).get_region(0).deref(ctx).iter(ctx) {
                    for op in b.deref(ctx).iter(ctx) {
                        if intrinsic(st, op) == Some("llvm.memcpy") {
                            let o: Vec<Value> = op.deref(ctx).operands().collect();
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
                eprintln!("  memcpy {c:5} {n}");
            }
        }
        if std::env::var_os("PLIRON_STATS_WHY").is_some() {
            let mut h: std::collections::BTreeMap<String, usize> = Default::default();
            let fs: Vec<_> = st
                .funcs
                .values()
                .map(|f| f.op)
                .filter(|&f| has_body(ctx, f))
                .collect();
            for f in fs {
                for op in allocas(ctx, f) {
                    let a = op.deref(ctx).get_result(0);
                    if !st.promoted.contains_key(&a) {
                        *h.entry(why2(ctx, st, a)).or_default() += 1;
                    }
                }
            }
            let mut v: Vec<_> = h.into_iter().collect();
            v.sort_by_key(|x| std::cmp::Reverse(x.1));
            for (n, c) in v.iter().take(20) {
                eprintln!("  why {c:6} {n}");
            }
        }
        if std::env::var_os("PLIRON_STATS_OPS").is_some() {
            let mut h: std::collections::BTreeMap<String, usize> = Default::default();
            for f in st.funcs.values().filter(|f| has_body(ctx, f.op)) {
                for b in f.op.deref(ctx).get_region(0).deref(ctx).iter(ctx) {
                    for op in b.deref(ctx).iter(ctx) {
                        let n = Operation::get_opid(op, ctx).to_string();
                        let n = st.intrinsics.get(&op).map_or(n, |i| i.clone());
                        *h.entry(n).or_default() += 1;
                    }
                }
            }
            let mut v: Vec<_> = h.into_iter().collect();
            v.sort_by_key(|x| std::cmp::Reverse(x.1));
            for (n, c) in v.iter().take(30) {
                eprintln!("  {c:7} {n}");
            }
        }
    }
}

/// First use that blocks promotion of alloca `a` (PLIRON_STATS_WHY).
fn why(ctx: &Context, st: &State<'_>, a: Value) -> String {
    let mut work = vec![a];
    let mut uses = Vec::new();
    while let Some(p) = work.pop() {
        for u in p.uses(ctx) {
            let op = u.user_op();
            let idx = u.find_index(ctx);
            if Operation::is_op::<GetElementPtrOp>(op, ctx) && idx == 0 {
                if gep_offset(ctx, st, op).is_none() {
                    return "dynamic gep".into();
                }
                work.push(op.deref(ctx).get_result(0));
            } else if Operation::is_op::<LoadOp>(op, ctx) {
                uses.push("load");
            } else if Operation::is_op::<StoreOp>(op, ctx) && idx == 1 {
                uses.push("store");
            } else if matches!(
                intrinsic(st, op),
                Some("llvm.memcpy" | "llvm.memmove" | "llvm.memset")
            ) && (idx == 0 || (idx == 1 && intrinsic(st, op) != Some("llvm.memset")))
                && const_int(ctx, st, op.deref(ctx).get_operand(2)).is_some()
            {
                uses.push("mem");
            } else {
                let n = st
                    .intrinsics
                    .get(&op)
                    .cloned()
                    .unwrap_or_else(|| Operation::get_opid(op, ctx).to_string());
                return format!("{n} #{idx}");
            }
        }
    }
    format!("layout ({} accesses)", uses.len().min(10))
}

/// `why`, refined with the accesses/slices verdict (PLIRON_STATS_WHY).
fn why2(ctx: &mut Context, st: &State<'_>, a: Value) -> String {
    let w = why(ctx, st, a);
    if !w.starts_with("layout") {
        return w;
    }
    let Some(&(size, _)) = st.allocas.get(&a) else {
        return "not in st.allocas".into();
    };
    if size == 0 || size > MAX_SIZE {
        return format!("size {}", if size == 0 { "0" } else { "> MAX_SIZE" });
    }
    let Some((acc, _)) = accesses(ctx, st, a, size) else {
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
pub fn forward(ctx: &mut Context, st: &State<'_>) {
    let funcs: Vec<Ptr<Operation>> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| has_body(ctx, f))
        .collect();
    let n: usize = funcs
        .into_iter()
        .map(|f| forward_single_store(ctx, st, f))
        .sum();
    if std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!("sroa-fwd {}: {n} single-store allocas forwarded", st.cgu);
    }
}

/// mem2reg's single-store case: an alloca whose only uses are one store and
/// same-typed loads that the store dominates (directly or through zero-offset
/// GEPs) is replaced by the stored value. This frees allocas whose address
/// was parked in such a slot (e.g. a closure's captured `&&K`), so `split`
/// can promote them too.
fn forward_single_store(ctx: &mut Context, st: &State<'_>, f: Ptr<Operation>) -> usize {
    let Some(dom) = crate::domcheck::Dom::new(ctx, st, f) else {
        return 0;
    };
    let pos = |ctx: &Context, o: Ptr<Operation>| {
        let b = o.deref(ctx).get_parent_block().unwrap();
        crate::inline::ops(ctx, b).iter().position(|&x| x == o)
    };
    let mut n = 0;
    for op in allocas(ctx, f) {
        let (mut store, mut loads, mut geps, mut ok) = (None, Vec::new(), Vec::new(), true);
        let mut work = vec![op.deref(ctx).get_result(0)];
        while let Some(p) = work.pop() {
            for u in p.uses(ctx) {
                let o = u.user_op();
                ok &= !st.volatile.contains(&o)
                    && if Operation::is_op::<StoreOp>(o, ctx) && u.find_index(ctx) == 1 {
                        store.replace(o).is_none()
                    } else if Operation::is_op::<LoadOp>(o, ctx) {
                        loads.push(o);
                        true
                    } else if Operation::is_op::<GetElementPtrOp>(o, ctx)
                        && u.find_index(ctx) == 0
                        && (gep_offset(ctx, st, o) == Some(0)
                            || o.deref(ctx).get_result(0).uses(ctx).is_empty())
                    {
                        geps.push(o);
                        work.push(o.deref(ctx).get_result(0));
                        true
                    } else {
                        false
                    };
            }
        }
        let Some(s) = store.filter(|_| ok && !loads.is_empty()) else {
            continue;
        };
        let v = s.deref(ctx).get_operand(0);
        let vt = v.get_type(ctx);
        let sb = s.deref(ctx).get_parent_block().unwrap();
        let fwd = dom.idx.contains_key(&sb)
            && loads.iter().all(|&l| {
                let lb = l.deref(ctx).get_parent_block().unwrap();
                // Unreachable loads never run (lowering traps those blocks).
                l.deref(ctx).get_result(0).get_type(ctx) == vt
                    && (!dom.idx.contains_key(&lb)
                        || if lb == sb {
                            pos(ctx, s) < pos(ctx, l)
                        } else {
                            dom.dominates(sb, lb)
                        })
            });
        if !fwd {
            continue;
        }
        for l in loads {
            let r = l.deref(ctx).get_result(0);
            r.replace_all_uses_with(ctx, &v);
            Operation::erase(l, ctx);
        }
        Operation::erase(s, ctx);
        erase_dead(ctx, geps);
        n += 1;
    }
    n
}
