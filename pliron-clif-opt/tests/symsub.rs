//! clifpeep `PLIRON_SYMSUB`: `(sym + n) - sym` folds to `n` even when
//! the two `symbol_value`s are separate instructions (GVN doesn't merge
//! them). A `&'static str`'s length arrives that way, so `push_str("<p>")`
//! kept a runtime length and its memcpy stayed a libc call (pulldown's
//! `HtmlWriter::run`). Offsets from different symbols, or from an
//! unrelated value, must not fold. Differentially checked.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i64) -> i64 {
    gv0 = symbol colocated %a
    gv1 = symbol colocated %b
block0(v0: i64):
    v1 = symbol_value.i64 gv0
    v2 = iconst.i64 37
    v3 = iadd v1, v2
    v4 = symbol_value.i64 gv0
    v5 = isub v3, v4
    v6 = symbol_value.i64 gv1
    v7 = isub v3, v6
    v8 = isub v3, v0
    v9 = iadd v5, v7
    v10 = iadd v9, v8
    return v10
}";

fn isubs(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Isub)
        .count()
}

#[test]
fn same_symbol_offsets_fold() {
    // Opt-in; this test binary holds only this test.
    unsafe { std::env::set_var("PLIRON_SYMSUB", "1") };
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    pliron_clif_opt::clifpeep::run(&mut after, false);
    // Only `(gv0 + 37) - gv0` folds; the gv1 and v0 differences stay.
    assert_eq!(isubs(&after), 2, "{}", after.display());
    if let Err(m) = check(&before, &after, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}
