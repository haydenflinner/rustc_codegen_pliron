//! A ratio-1 pointer whose base is a table address stays carried on
//! wasm. A base that fits a 12-bit load offset still refolds. Native
//! (`amode_scale`) refolds both; that path was not re-timed.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn body(base: i64) -> String {
    format!(
        "function %f(i32) -> i32 {{
    block0(v0: i32):
        v1 = iconst.i32 0
        v2 = iconst.i32 {base}
        jump block1(v1, v2)

    block1(v3: i32, v4: i32):
        v5 = icmp ult v3, v0
        brif v5, block2, block3

    block2:
        v6 = uload8.i32 notrap v4
        v7 = iconst.i32 1
        v8 = iadd.i32 v3, v7
        v9 = iadd.i32 v4, v7
        jump block1(v8, v9)

    block3:
        return v6
}}"
    )
}

fn run(src: &str, amode_scale: bool) -> String {
    let mut f = parse(src);
    pliron_clif_opt::ivrefold::run(&mut f, &mut Analysis::default(), amode_scale);
    f.display().to_string()
}

#[test]
fn wasm_keeps_a_table_address_pointer() {
    let text = run(&body(1052972), false);
    assert!(
        text.contains("block1(v3: i32, v4: i32)") || text.contains("uload8.i32 notrap v4"),
        "table pointer was refolded\n{text}"
    );
    assert!(
        !text.contains("iadd") || text.contains("iadd.i32 v4"),
        "load address rebuilt from the index\n{text}"
    );
}

#[test]
fn wasm_refolds_a_small_constant_base() {
    let text = run(&body(40), false);
    assert!(
        !text.contains("block1(v3: i32, v4: i32)"),
        "small base stayed a carried pointer\n{text}"
    );
}

#[test]
fn native_refolds_a_table_address() {
    let text = run(&body(1052972), true);
    assert!(
        !text.contains("block1(v3: i32, v4: i32)"),
        "native kept the pointer\n{text}"
    );
}
