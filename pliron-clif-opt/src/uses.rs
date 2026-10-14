//! Batch use index: one O(fn) scan answering every "where is `v`
//! used" for the rest of a pass phase, instead of a `layout.blocks()`
//! walk per query — the O(candidate × fn) pattern that made lcse's
//! `uses_of`, `reap_dead`, `split_niche_tests`, `rebind_exports`, and
//! `memspec` each go quadratic independently.
//!
//! Snapshot semantics: mutating the function invalidates it — rebuild
//! alongside cfg/domtree, at the same point in the pass.

use cranelift_codegen::ir::{Block, Function, Inst, Value, ValueDef};
use rustc_hash::FxHashMap;

/// Batch dead-inst elimination: one use-count walk, then each kill
/// decrements its operands' counts and enqueues any def whose last use
/// just died — O(fn + killed×args) instead of the O(dead-chain-depth ×
/// fn) of rescanning the function per round (`sweep_dead`/
/// `remove_dead_insts` both did that, from ~10 call sites).
/// `killable` is the caller's keep-if-predicate (e.g. `pure_op`,
/// `removable`); results are cleared on removal.
pub fn worklist_dce(func: &mut Function, killable: impl Fn(&Function, Inst) -> bool) -> usize {
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *uses.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    let mut work: Vec<Inst> = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .filter(|&i| {
            func.dfg.inst_results(i).iter().all(|&r| {
                uses.get(&func.dfg.resolve_aliases(r))
                    .copied()
                    .unwrap_or(0)
                    == 0
            }) && killable(func, i)
        })
        .collect();
    let mut n = 0;
    while let Some(i) = work.pop() {
        // Skip insts already removed by an earlier wave, and defs pushed
        // when one result died while a sibling result stayed live.
        if func.layout.inst_block(i).is_none()
            || func.dfg.inst_results(i).iter().any(|&r| {
                uses.get(&func.dfg.resolve_aliases(r))
                    .copied()
                    .unwrap_or(0)
                    > 0
            })
        {
            continue;
        }
        for v in func.dfg.inst_values(i).collect::<Vec<_>>() {
            let rv = func.dfg.resolve_aliases(v);
            let Some(u) = uses.get_mut(&rv) else {
                continue;
            };
            *u -= 1;
            if *u == 0 {
                if let ValueDef::Result(d, _) = func.dfg.value_def(rv) {
                    if killable(func, d) {
                        work.push(d);
                    }
                }
            }
        }
        func.layout.remove_inst(i);
        func.dfg.clear_results(i);
        n += 1;
    }
    n
}

pub struct Uses {
    /// Resolved value -> (block, inst) of each `inst_values` use
    /// (args, branch-edge args, EH contexts — the same set).
    pub sites: FxHashMap<Value, Vec<(Block, Inst)>>,
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
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    sites.entry(v).or_default().push((b, i));
                    extra(v, b, i);
                }
            }
        }
        Self { sites }
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
}
