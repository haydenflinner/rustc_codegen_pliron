//! `xorcancel`: SipHash's last-round tail, where `v0` occurs twice in the
//! final xor tree and cancels.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

/// `v3' = rotl(v3, 21) ^ v0; h = ((v1 ^ v0) ^ rotl(v2, 32)) ^ v3'`, the
/// shape the backend lowers std's `finish` to.
const TAIL: &str = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v10 = iconst.i64 21
    v11 = rotl v3, v10
    v12 = bxor v11, v0
    v13 = iconst.i64 32
    v14 = rotl v2, v13
    v15 = bxor v1, v0
    v16 = bxor v15, v14
    v17 = bxor v16, v12
    return v17
}";

/// A tree with no repeated leaf.
const PLAIN: &str = "function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v10 = bxor v0, v1
    v11 = bxor v10, v2
    return v11
}";

fn count(f: &Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

/// Uses of `v` (parameter 0) by live instructions: after the rewrite the
/// dead tree nodes still name it, so count those reachable from the
/// return through the root.
fn root_mentions_v0(f: &Function) -> bool {
    let ret = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| f.dfg.insts[i].opcode() == Opcode::Return)
        .unwrap();
    let p0 = f.dfg.block_params(f.layout.entry_block().unwrap())[0];
    let mut stack = vec![f.dfg.resolve_aliases(f.dfg.inst_args(ret)[0])];
    let mut seen = std::collections::HashSet::new();
    while let Some(v) = stack.pop() {
        if v == p0 {
            return true;
        }
        if !seen.insert(v) {
            continue;
        }
        if let Some(d) = f.dfg.value_def(v).inst() {
            stack.extend(f.dfg.inst_args(d).iter().map(|&a| f.dfg.resolve_aliases(a)));
        }
    }
    false
}

#[test]
fn siphash_tail_v0_cancels() {
    let before = cranelift_reader::parse_functions(TAIL).unwrap().remove(0);
    let mut f = before.clone();
    assert_eq!(pliron_clif_opt::xorcancel::run(&mut f), 1, "{}", f.display());
    assert!(!root_mentions_v0(&f), "v0 still feeds the hash:\n{}", f.display());
    check(&before, &f, &Facts::default(), &Config::default()).unwrap_or_else(|m| panic!("{m}"));
}

#[test]
fn tree_without_repeats_is_left_alone() {
    let mut f = cranelift_reader::parse_functions(PLAIN).unwrap().remove(0);
    let n = count(&f, Opcode::Bxor);
    assert_eq!(pliron_clif_opt::xorcancel::run(&mut f), 0);
    assert_eq!(count(&f, Opcode::Bxor), n);
}
