//! `guardsink`: values computed ahead of a rotated loop's zero-trip guard
//! and read only on the loop side move past the guard's `brif`; a value
//! the guard block itself uses, and a load below a store, stay.

use cranelift_codegen::ir::{Function, Opcode};

const SRC: &str = r#"
function %f(i64, i64) -> i64 {
    gv0 = symbol %seed
block0(v0: i64, v1: i64):
    v2 = symbol_value.i64 gv0
    v3 = load.i64 notrap v2
    v4 = iconst.i64 64
    brif v1, block1, block3

block1:
    jump block2(v0, v1)

block2(v5: i64, v6: i64):
    v7 = bxor v5, v3
    v8 = ishl v7, v4
    v9 = iconst.i64 1
    v10 = isub v6, v9
    brif v10, block2(v8, v10), block4(v8)

block3:
    return v0

block4(v11: i64):
    return v11
}
"#;

fn block_of(f: &Function, op: Opcode) -> String {
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if f.dfg.insts[i].opcode() == op {
                return b.to_string();
            }
        }
    }
    String::new()
}

#[test]
fn invariants_move_past_the_guard() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let n = pliron_clif_opt::guardsink::run(&mut f);
    assert_eq!(n, 3, "{}", f.display());
    assert_eq!(block_of(&f, Opcode::SymbolValue), "block1", "{}", f.display());
    assert_eq!(block_of(&f, Opcode::Load), "block1", "{}", f.display());
}

#[test]
fn a_load_below_a_store_stays() {
    let src = SRC.replace("    v4 = iconst.i64 64\n", "    v4 = iconst.i64 64\n    store.i64 notrap v1, v0\n");
    let mut f = cranelift_reader::parse_functions(&src).unwrap().remove(0);
    pliron_clif_opt::guardsink::run(&mut f);
    assert_eq!(block_of(&f, Opcode::Load), "block0", "{}", f.display());
}
