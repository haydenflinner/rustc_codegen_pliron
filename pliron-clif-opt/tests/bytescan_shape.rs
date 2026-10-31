//! bytescan on pulldown-cmark's `escape_html_scalar` scan (before
//! `punroll`): a pointer loop with an offset param, the byte load and the
//! table test split by an empty `jump`, `imul x, 1` in the table address.
//! It gets a 16-byte vector pre-loop over the table's 4 stop bytes; the
//! scalar loop stays as the exit path. A loop with a store is left alone.
//! End-to-end behavior: the `bytescan` stanza (gate job `optin:bytescan`).

use cranelift_codegen::ir::{Function, Opcode, types};
use cranelift_codegen::settings;
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashMap;
use std::sync::Arc;

const SCAN: &str = "function %f(i64, i64) -> i64 {
    gv0 = symbol colocated u1:0
block0(v0: i64, v1: i64):
    v2 = iadd v0, v1
    v3 = symbol_value.i64 gv0
    v4 = iconst.i64 0
    v5 = icmp eq v1, v4
    brif v5, block5, block1(v0, v4)
block1(v10: i64, v11: i64):
    v12 = iconst.i64 1
    v13 = iadd v10, v12
    v14 = uload8.i64 notrap v10
    jump block2
block2:
    v15 = imul.i64 v14, v12
    v16 = iadd.i64 v3, v15
    v17 = load.i8 notrap v16
    v18 = iconst.i8 0
    v19 = icmp ne v17, v18
    brif v19, block4(v11), block3
block3:
    v20 = isub.i64 v13, v0
    v21 = icmp ne v13, v2
    brif v21, block1(v13, v20), block5
block4(v30: i64):
    return v30
block5:
    v31 = iconst.i64 -1
    return v31
}";

fn tables() -> FxHashMap<u32, Arc<[u8]>> {
    let mut t = vec![0u8; 256];
    for (k, c) in b"\"&<>".iter().enumerate() {
        t[*c as usize] = k as u8 + 1;
    }
    [(0u32, Arc::from(t))].into_iter().collect()
}

fn count(f: &Function, op: Opcode) -> usize {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).filter(|&i| f.dfg.insts[i].opcode() == op).count()
}

#[test]
fn escape_scan_gets_vector_preloop() {
    let mut f = cranelift_reader::parse_functions(SCAN).unwrap().remove(0);
    let n = pliron_clif_opt::bytescan::run(&mut f, &mut Analysis::default(), &tables());
    assert_eq!(n, 1, "not vectorized:\n{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    let vloads = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load && f.dfg.value_type(f.dfg.first_result(i)) == types::I8X16)
        .count();
    assert_eq!(vloads, 1, "{}", f.display());
    // One splat per stop byte, plus the movemask's 0xff lane mask.
    assert_eq!(count(&f, Opcode::Splat), 5, "{}", f.display());
    assert_eq!(count(&f, Opcode::Uload8), 1, "scalar loop changed:\n{}", f.display());
}

#[test]
fn loop_with_store_is_left_alone() {
    let src = SCAN.replace("    jump block2\nblock2:\n", "    store.i64 notrap v11, v0\n    jump block2\nblock2:\n");
    let mut f = cranelift_reader::parse_functions(&src).unwrap().remove(0);
    assert_eq!(pliron_clif_opt::bytescan::run(&mut f, &mut Analysis::default(), &tables()), 0, "{}", f.display());
}
