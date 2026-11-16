//! On wasm a bare `load (base + iv)` is an add on the address path.
//! The base staying live for another access must not keep that add.
//! Native (`amode_scale`) still requires the base's last in-loop use
//! to leave with the add; that path was not re-timed.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const SCAN: &str = "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v2 = iconst.i32 0
        v3 = iconst.i32 0
        jump block1(v2, v3)

    block1(v4: i32, v5: i32):
        v6 = icmp ult v4, v1
        brif v6, block2, block3

    block2:
        v7 = load.i32 notrap v0+8
        v8 = iadd.i32 v0, v5
        v9 = load.i32 notrap v8
        v10 = iconst.i32 1
        v11 = iadd.i32 v4, v10
        v12 = iconst.i32 4
        v13 = iadd.i32 v5, v12
        jump block1(v11, v13)

    block3:
        return v4
}";

fn run(amode_scale: bool) -> Function {
    let mut f = parse(SCAN);
    pliron_clif_opt::indvars::run(
        &mut f,
        &mut Analysis::default(),
        amode_scale,
        &mut pliron_clif_opt::vmap::VMap::default(),
    );
    f
}

#[test]
fn wasm_steps_the_pointer_while_base_stays_live() {
    let f = run(false);
    let text = f.display().to_string();
    assert!(
        !text.contains("iadd.i32 v0, v5") && !text.contains("iadd.i32 v5, v0"),
        "address add survived\n{text}"
    );
}

#[test]
fn native_keeps_the_add_when_base_stays_live() {
    let f = run(true);
    let text = f.display().to_string();
    assert!(
        text.contains("iadd.i32 v0, v5") || text.contains("iadd.i32 v5, v0"),
        "native rewrote a live base\n{text}"
    );
}

/// `base + ((len - i) << 3)` is a pointer that steps backward. Wasm
/// wants that pointer; native keeps the shift for scaled addressing.
const REV: &str = "function %f(i32, i32, i32) -> i32 {
    block0(v0: i32, v1: i32, v2: i32):
        v3 = iconst.i32 0
        jump block1(v3)

    block1(v4: i32):
        v5 = icmp ult v4, v2
        brif v5, block2, block3

    block2:
        v6 = isub.i32 v1, v4
        v7 = iconst.i32 3
        v8 = ishl.i32 v6, v7
        v9 = iadd.i32 v0, v8
        v10 = load.i64 notrap v9
        v11 = iconst.i32 1
        v12 = iadd.i32 v4, v11
        jump block1(v12)

    block3:
        return v4
}";

#[test]
fn wasm_steps_a_reversed_index() {
    let mut f = parse(REV);
    pliron_clif_opt::indvars::run(
        &mut f,
        &mut Analysis::default(),
        false,
        &mut pliron_clif_opt::vmap::VMap::default(),
    );
    let text = f.display().to_string();
    let body = text.split("block2:").nth(1).unwrap_or("");
    let body = body.split("block3:").next().unwrap_or("");
    assert!(!body.contains("ishl") && !body.contains("isub"), "shift survived\n{text}");
    assert!(text.contains("v9 ->") || text.contains("load.i64 notrap v13"), "load missed the pointer\n{text}");
}

#[test]
fn native_keeps_a_reversed_index() {
    let mut f = parse(REV);
    pliron_clif_opt::indvars::run(
        &mut f,
        &mut Analysis::default(),
        true,
        &mut pliron_clif_opt::vmap::VMap::default(),
    );
    let text = f.display().to_string();
    assert!(text.contains("ishl"), "native dropped the shift\n{text}");
}
