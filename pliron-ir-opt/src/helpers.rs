//! pliron-IR helpers shared by the passes and lowering.

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::operation::Operation;
use pliron::value::Value;
use pliron_llvm::ops::FuncOp;

use crate::state::{ConstVal, State};

/// Functions whose address is used other than as a direct callee: symbol
/// constants that are used or sit in global/aggregate initializers, and names
/// mentioned in asm.
pub fn address_taken(ctx: &Context, st: &State) -> rustc_hash::FxHashSet<String> {
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
    taken.extend(
        st.funcs
            .keys()
            .filter(|n| st.asm.contains(n.as_str()))
            .cloned(),
    );
    taken
}

pub fn has_body(ctx: &Context, f: Ptr<Operation>) -> bool {
    Operation::get_op::<FuncOp>(f, ctx)
        .unwrap()
        .get_entry_block(ctx)
        .is_some()
}

/// Functions the bottom-up analyses infer facts for: bodied and not
/// preemptible (an interposed definition could do anything).
pub fn bodied(ctx: &Context, st: &State) -> Vec<String> {
    st.funcs
        .iter()
        .filter(|(_, f)| f.linkage != cranelift_module::Linkage::Preemptible && has_body(ctx, f.op))
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
            wl.extend(op.deref(ctx).successors());
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
