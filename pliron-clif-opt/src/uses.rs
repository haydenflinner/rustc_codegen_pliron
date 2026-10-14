//! Batch use index: one O(fn) scan answering every "where is `v`
//! used" for the rest of a pass phase, instead of a `layout.blocks()`
//! walk per query — the O(candidate × fn) pattern that made lcse's
//! `uses_of`, `reap_dead`, `split_niche_tests`, `rebind_exports`, and
//! `memspec` each go quadratic independently.
//!
//! Snapshot semantics: mutating the function invalidates it — rebuild
//! alongside cfg/domtree, at the same point in the pass.

use cranelift_codegen::ir::{Block, Function, Inst, Value};
use rustc_hash::{FxHashMap, FxHashSet};

pub struct Uses {
    /// Resolved value -> (block, inst) of each `inst_values` use
    /// (args, branch-edge args, EH contexts — the same set).
    pub sites: FxHashMap<Value, Vec<(Block, Inst)>>,
    /// Block -> resolved values used inside it.
    pub by_block: FxHashMap<Block, FxHashSet<Value>>,
}

impl Uses {
    /// Plain index — use `build` when the same walk must feed extra
    /// per-use tables (memspec's def-site map).
    pub fn new(func: &Function) -> Self {
        Self::build(func, |_, _, _| {})
    }

    /// One `inst_values` walk producing the index plus caller-side
    /// per-use work — splitting into two scans just to share the type
    /// would double the walk.
    pub fn build(func: &Function, mut extra: impl FnMut(Value, Block, Inst)) -> Self {
        let mut sites: FxHashMap<Value, Vec<(Block, Inst)>> = FxHashMap::default();
        let mut by_block: FxHashMap<Block, FxHashSet<Value>> = FxHashMap::default();
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    sites.entry(v).or_default().push((b, i));
                    by_block.entry(b).or_default().insert(v);
                    extra(v, b, i);
                }
            }
        }
        Self { sites, by_block }
    }

    /// `v` still referenced anywhere. Removals only delete uses, so a
    /// snapshot taken before a dead-code sweep stays conservative.
    pub fn is_used(&self, v: Value) -> bool {
        self.sites.contains_key(&v)
    }

    /// Any use of `v` outside block `home`.
    pub fn escapes(&self, v: Value, home: Block) -> bool {
        self.sites
            .get(&v)
            .is_some_and(|ss| ss.iter().any(|&(b, _)| b != home))
    }

    /// Resolved values used inside `b`.
    pub fn in_block(&self, b: Block) -> impl Iterator<Item = Value> + '_ {
        self.by_block.get(&b).into_iter().flatten().copied()
    }
}
