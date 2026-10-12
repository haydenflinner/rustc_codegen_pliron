//! pliron-IR helpers shared by the passes and lowering.

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
    for (v, c) in &st.consts {
        if let ConstVal::Sym { sym, .. } = c {
            if v.is_used(ctx) || escaped.contains(v) {
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
