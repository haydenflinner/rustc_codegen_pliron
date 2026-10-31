//! pliron-IR helpers shared by the passes and lowering.

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::operation::Operation;
use pliron::value::Value;
use pliron_llvm::ops::{AddressOfOp, FuncOp};

use crate::state::{ConstVal, State};

/// Functions whose address is used other than as a direct callee: symbol
/// constants that are used or sit in global/aggregate initializers, and names
/// mentioned in asm.
pub fn address_taken(ctx: &Context, st: &State) -> rustc_hash::FxHashSet<String> {
    address_taken_ex(ctx, st, false)
}

/// `skip_dead`: refs inside `dead_fns`/`dead_unopt` bodies don't count —
/// those callers are strip/emit-only, so their references needn't keep a
/// callee optimized (it still emits — this is only used for the
/// `dead_unopt` mark, never for `dead_fns` stripping).
pub fn address_taken_ex(
    ctx: &Context,
    st: &State,
    skip_dead: bool,
) -> rustc_hash::FxHashSet<String> {
    use rustc_hash::FxHashSet;
    let mut escaped: FxHashSet<Value> = st.globals.values().filter_map(|g| g.init).collect();
    for c in st.consts.values() {
        if let ConstVal::Agg(vs) = c {
            escaped.extend(vs.iter().copied());
        }
    }
    let mut taken = FxHashSet::default();
    for (v, c) in st.consts.iter() {
        if let ConstVal::Sym { sym, .. } = c {
            if v.is_used(ctx) || escaped.contains(&v) {
                taken.insert(sym.clone());
            }
        }
    }
    // `llvm.addressof` ops produce fn-ptr values the codegen-time
    // `st.consts` never saw (they're created by iropt rewrites or fed
    // through aliases `is_used` can't see on the original const).
    for (n, f) in st.funcs.iter() {
        if !has_body(ctx, f.op)
            || (skip_dead
                && (st.dead_fns.contains(n.as_str()) || st.dead_unopt.contains(n.as_str())))
        {
            continue;
        }
        for b in crate::inline::blocks_iter(ctx, f.op) {
            for op in b.deref(ctx).iter(ctx) {
                if let Some(a) = Operation::get_op::<AddressOfOp>(op, ctx) {
                    let id = a.get_global_name(ctx);
                    if let Some(sym) =
                        st.ident_to_sym.get(AsRef::<str>::as_ref(id.as_ref()))
                    {
                        taken.insert(sym.clone());
                    }
                }
            }
        }
    }
    taken.extend(
        st.funcs
            .keys()
            .filter(|n| st.asm.contains(n.as_str()))
            .cloned(),
    );
    taken
}

/// `has_body` and selected by `PLIRON_OPT_FNS` — the filter for passes
/// that rewrite a function's own body (never for call-site scans).
/// `dead_unopt` bodies are emit-only: passes skip them like opt-out fns.
// r[impl iropt.optfns]
pub fn opt_body(ctx: &Context, st: &State, f: Ptr<Operation>) -> bool {
    has_body(ctx, f) && !st.opt_skip.contains(&f) && !st.dead_unopt_ops.contains(&f)
}

/// Fill `st.opt_skip` from `PLIRON_OPT_FNS` (no-op when unset).
pub fn opt_skip_init(ctx: &Context, st: &mut State) {
    if pliron_cfg::env_get("PLIRON_OPT_FNS").is_none() {
        return;
    }
    st.opt_skip = st
        .funcs
        .iter()
        .filter(|(n, f)| has_body(ctx, f.op) && !pliron_cfg::fn_opt_selected(n))
        .map(|(_, f)| f.op)
        .collect();
}

/// `f`'s symbol for `bisect_in` logs — only looked up while
/// `PLIRON_OPT_BISECT` is set (a linear scan of `st.funcs`).
pub fn bisect_name(st: &State, f: Ptr<Operation>) -> String {
    if pliron_cfg::bisect_limit().is_none() {
        return String::new();
    }
    st.funcs
        .iter()
        .find(|(_, i)| i.op == f)
        .map(|(n, _)| n.clone())
        .unwrap_or_default()
}

pub fn has_body(ctx: &Context, f: Ptr<Operation>) -> bool {
    Operation::get_op::<FuncOp>(f, ctx)
        .unwrap()
        .get_entry_block(ctx)
        .is_some()
}

/// Functions the bottom-up analyses infer facts for: bodied and not
/// preemptible (an interposed definition could do anything). ODR-linked
/// bodies (`f.odr`) are the exception: every emitted copy of the symbol
/// is semantically identical, so facts inferred here apply to whichever
/// copy a call binds.
// r[impl iropt.ipo.datalog]
pub fn bodied(ctx: &Context, st: &State) -> Vec<String> {
    st.funcs
        .iter()
        .filter(|(n, f)| {
            (f.linkage != cranelift_module::Linkage::Preemptible || f.odr)
                && has_body(ctx, f.op)
                && !st.dead_unopt.contains(n.as_str())
        })
        .map(|(s, _)| s.clone())
        .collect()
}

/// Symbol interner: Datalog relations hold `u32` function ids, not strings.
#[derive(Default)]
pub struct Syms(rustc_hash::FxHashMap<String, u32>);

impl Syms {
    pub fn id(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.0.get(s) {
            return i;
        }
        let i = self.0.len() as u32;
        self.0.insert(s.to_string(), i);
        i
    }
}

/// Erase every block no longer reachable from the entry (over CFG + invoke
/// edges) after an edge-removing transform: drops every dead block's held
/// uses first (killing dead→dead and dead→live edges and operand uses),
/// cleans op-keyed side tables, then erases the blocks. Dead blocks' results
/// can't be used by live code (dominance), so after the use drop `erase`
/// is safe.
pub fn erase_unreachable(
    ctx: &mut Context,
    st: &mut State,
    blocks: &[Ptr<BasicBlock>],
    ops_of: &dyn Fn(Ptr<BasicBlock>) -> Vec<Ptr<Operation>>,
) {
    let Some(&entry) = blocks.first() else {
        return;
    };
    // Live = reachable over real successors plus invoke pads of live ops
    // (pads have no real incoming edges).
    let mut live = rustc_hash::FxHashSet::default();
    let mut wl = vec![entry];
    while let Some(b) = wl.pop() {
        if !live.insert(b) {
            continue;
        }
        for op in ops_of(b) {
            wl.extend(op.deref(ctx).successors(ctx));
            if let Some(&(l, _)) = st.invokes.get(op) {
                wl.push(l);
            }
        }
    }
    let dead: Vec<Ptr<BasicBlock>> = blocks
        .iter()
        .copied()
        .filter(|b| !live.contains(b))
        .collect();
    for &b in &dead {
        for op in ops_of(b) {
            st.calls.remove(op);
            st.invokes.remove(op);
            st.intrinsics.remove(op);
            st.volatile.remove(op);
            st.rmw.remove(op);
            st.nonnull.remove(op);
        }
        BasicBlock::drop_all_uses(b, ctx);
    }
    for b in dead {
        BasicBlock::erase(b, ctx);
    }
}

/// `PLIRON_FACTS=<substr>`: print what a bottom-up analysis inferred for
/// each function whose symbol contains the substring.
pub fn dump_fact(analysis: &str, sym: &str, fact: &dyn std::fmt::Debug) {
    if let Some(f) = crate::env_get("PLIRON_FACTS")
        && sym.contains(f)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::facts", "{analysis} {sym}: {fact:?}")
        };
    }
}

/// Hash-backed stand-in for [`ColMap`](crate::state::ColMap) for
/// function-local scratch: a `ColMap`'s index spans every ordinal in the
/// CGU, so a per-function (or per-query) map paid an allocation, zero
/// fill and walk proportional to the whole CGU. Same verbs; iteration is in
/// ordinal order, as `ColMap`'s is.
#[derive(Clone, Debug)]
pub struct SparseMap<K: pliron::cols::ColKey + std::hash::Hash, V>(rustc_hash::FxHashMap<K, V>);

impl<K: pliron::cols::ColKey + std::hash::Hash, V> Default for SparseMap<K, V> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<K: pliron::cols::ColKey + std::hash::Hash, V> SparseMap<K, V> {
    #[inline]
    pub fn get(&self, k: K) -> Option<&V> {
        self.0.get(&k)
    }
    #[inline]
    pub fn contains(&self, k: K) -> bool {
        self.0.contains_key(&k)
    }
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.0.insert(k, v)
    }
    pub fn remove(&mut self, k: K) -> Option<V> {
        self.0.remove(&k)
    }
    pub fn or_default(&mut self, k: K) -> &mut V
    where
        V: Default,
    {
        self.0.entry(k).or_default()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Entries in ordinal order.
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        let mut v: Vec<(K, &V)> = self.0.iter().map(|(k, v)| (*k, v)).collect();
        v.sort_unstable_by_key(|(k, _)| k.ord());
        v.into_iter()
    }
}

impl<K: pliron::cols::ColKey + std::hash::Hash, V> std::ops::Index<K> for SparseMap<K, V> {
    type Output = V;
    #[inline]
    fn index(&self, k: K) -> &V {
        &self.0[&k]
    }
}

impl<K: pliron::cols::ColKey + std::hash::Hash, V> FromIterator<(K, V)> for SparseMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(it: I) -> Self {
        Self(it.into_iter().collect())
    }
}

/// Set form of [`SparseMap`].
#[derive(Clone, Debug)]
pub struct SparseSet<K: pliron::cols::ColKey + std::hash::Hash>(SparseMap<K, ()>);

impl<K: pliron::cols::ColKey + std::hash::Hash> Default for SparseSet<K> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<K: pliron::cols::ColKey + std::hash::Hash> SparseSet<K> {
    #[inline]
    pub fn contains(&self, k: K) -> bool {
        self.0.contains(k)
    }
    /// True if newly inserted.
    pub fn insert(&mut self, k: K) -> bool {
        self.0.insert(k, ()).is_none()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Members in ordinal order.
    pub fn iter(&self) -> impl Iterator<Item = K> + '_ {
        self.0.iter().map(|(k, _)| k)
    }
}
