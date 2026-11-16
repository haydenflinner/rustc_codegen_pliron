//! Two copies of a small region become one body and a tag dispatch.

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Flags};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src)
        .unwrap_or_else(|e| panic!("{e}\n{src}"))
        .remove(0)
}

fn flags() -> Flags {
    settings::Flags::new(settings::builder())
}

const TWO: &str = "function %f(i32) -> i32 {
    block0(v0: i32):
        brif v0, block1(v0), block2(v0)

    block1(v1: i32):
        v2 = iconst.i32 1
        v3 = iadd v1, v2
        jump block3(v3)

    block3(v4: i32):
        v5 = iconst.i32 3
        v6 = imul v4, v5
        jump block5(v6)

    block2(v7: i32):
        v8 = iconst.i32 1
        v9 = iadd v7, v8
        jump block4(v9)

    block4(v10: i32):
        v11 = iconst.i32 3
        v12 = imul v10, v11
        jump block6(v12)

    block5(v13: i32):
        return v13

    block6(v14: i32):
        v15 = iconst.i32 1
        v16 = iadd v14, v15
        return v16
}";

#[test]
fn two_copies_share_one_body() {
    let mut f = parse(TWO);
    let n = pliron_clif_opt::regionfold::run(&mut f);
    let text = f.display().to_string();
    assert_eq!(n, 1, "{text}");
    assert_eq!(text.matches("imul").count(), 1, "{text}");
    cranelift_codegen::verify_function(&f, &flags()).unwrap();
}

/// The exit blocks use a value defined on only that copy's path into
/// the region. The merge has to pass it as an exit parameter; leaving
/// it as a concrete use is a dominance break once every stub reaches
/// the dispatcher.
#[test]
fn exit_value_becomes_param() {
    let src = "function %f(i32) -> i32 {
    block0(v0: i32):
        brif v0, block7, block8

    block7:
        v20 = iconst.i32 7
        jump block1(v0)

    block1(v1: i32):
        v2 = iconst.i32 1
        v3 = iadd v1, v2
        jump block3(v3)

    block3(v4: i32):
        v5 = iconst.i32 3
        v6 = imul v4, v5
        jump block5(v6)

    block8:
        v22 = iconst.i32 9
        jump block2(v0)

    block2(v7: i32):
        v8 = iconst.i32 1
        v9 = iadd v7, v8
        jump block4(v9)

    block4(v10: i32):
        v11 = iconst.i32 3
        v12 = imul v10, v11
        jump block6(v12)

    block5(v13: i32):
        v21 = iadd v13, v20
        return v21

    block6(v14: i32):
        v23 = iadd v14, v22
        return v23
}";
    let mut f = parse(src);
    let n = pliron_clif_opt::regionfold::run(&mut f);
    let text = f.display().to_string();
    assert_eq!(n, 1, "{text}");
    assert_eq!(text.matches("imul").count(), 1, "{text}");
    cranelift_codegen::verify_function(&f, &flags()).unwrap();
}

/// The exit uses the region's `imul` result by dominance, not as a
/// block argument. The shared body has to pass that result on the exit.
#[test]
fn region_result_passed_on_exit() {
    let src = "function %f(i32) -> i32 {
    block0(v0: i32):
        brif v0, block1, block2

    block1:
        v2 = iconst.i32 1
        v3 = iadd v0, v2
        jump block3(v3)

    block3(v4: i32):
        v5 = iconst.i32 3
        v6 = imul v4, v5
        jump block5

    block2:
        v8 = iconst.i32 1
        v9 = iadd v0, v8
        jump block4(v9)

    block4(v10: i32):
        v11 = iconst.i32 3
        v12 = imul v10, v11
        jump block6

    block5:
        return v6

    block6:
        v15 = iconst.i32 1
        v16 = iadd v12, v15
        return v16
}";
    let mut f = parse(src);
    let n = pliron_clif_opt::regionfold::run(&mut f);
    let text = f.display().to_string();
    assert_eq!(n, 1, "{text}");
    assert_eq!(text.matches("imul").count(), 1, "{text}");
    cranelift_codegen::verify_function(&f, &flags()).unwrap();
}

#[test]
fn different_opcodes_stay() {
    let src = TWO.replace("v12 = imul v10, v11", "v12 = iadd v10, v11");
    let mut f = parse(&src);
    let n = pliron_clif_opt::regionfold::run(&mut f);
    assert_eq!(n, 0, "{}", f.display());
}
