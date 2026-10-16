//! Batch use index: one O(fn) scan answering every "where is `v`
//! used" for the rest of a pass phase, instead of a `layout.blocks()`
//! walk per query — the O(candidate × fn) pattern that made lcse's
//! `uses_of`, `reap_dead`, `split_niche_tests`, `rebind_exports`, and
//! `memspec` each go quadratic independently.
//!
//! Snapshot semantics: mutating the function invalidates it — rebuild
//! alongside cfg/domtree, at the same point in the pass.

use cranelift_codegen::entity::EntityRef;
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
    /// CSR over resolved values: uses of `v` are
    /// `flat[off[v.index()]..off[v.index() + 1]]` in layout order —
    /// a dense index beats hashing per lookup and per-use `Vec`
    /// allocation during build.
    off: Vec<u32>,
    flat: Vec<(Block, Inst)>,
}

impl Uses {
    /// Plain index — use `build` when the same walk must feed extra
    /// per-use tables (memspec's def-site map).
    pub fn new(func: &Function) -> Self {
        Self::build(func, |_, _, _| {})
    }

    /// `(block, inst)` sites using `v`, in layout order. `v` must be
    /// resolved — an unresolved alias reads as unused, same as a hash
    /// lookup miss did.
    pub fn sites(&self, v: Value) -> &[(Block, Inst)] {
        let i = v.index();
        if i + 1 < self.off.len() {
            &self.flat[self.off[i] as usize..self.off[i + 1] as usize]
        } else {
            &[]
        }
    }

    /// One `inst_values` walk producing the index plus caller-side
    /// per-use work — the histogram is folded into the walk over a
    /// dense (idx, site) pairs vec, then a stable counting-sort
    /// scatter fills `flat`.
    pub fn build(func: &Function, mut extra: impl FnMut(Value, Block, Inst)) -> Self {
        let n = func.dfg.num_values();
        let mut off = vec![0u32; n + 1];
        let mut pairs: Vec<(u32, Block, Inst)> = Vec::new();
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    off[v.index() + 1] += 1;
                    pairs.push((v.index() as u32, b, i));
                    extra(v, b, i);
                }
            }
        }
        for i in 1..=n {
            off[i] += off[i - 1];
        }
        let mut flat = vec![(Block::new(0), Inst::new(0)); off[n] as usize];
        let mut cur = off.clone();
        for (idx, b, i) in pairs {
            flat[cur[idx as usize] as usize] = (b, i);
            cur[idx as usize] += 1;
        }
        Self { off, flat }
    }

    /// `v` still referenced anywhere. Removals only delete uses, so a
    /// snapshot taken before a dead-code sweep stays conservative.
    pub fn is_used(&self, v: Value) -> bool {
        !self.sites(v).is_empty()
    }

    /// Any use of `v` outside block `home`.
    pub fn escapes(&self, v: Value, home: Block) -> bool {
        self.sites(v).iter().any(|&(b, _)| b != home)
    }
}
