//! Two reverse slices of the same length (`base[..sz].rev()`) share one
//! end test. A different length, or a different step, stays a zip.

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Flags};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap_or_else(|e| panic!("{e}\n{src}")).remove(0)
}

fn flags() -> Flags {
    settings::Flags::new(settings::builder())
}

fn rev(len_b: &str) -> String {
    format!(
        "function %f(i32, i32, i32) -> i8 {{
    block0(v0: i32, v1: i32, v2: i32):
        v3 = iconst.i32 2
        v4 = ishl v2, v3
        v5 = iadd v4, v0
        {len_b}
        v7 = iconst.i32 -4
        v8 = icmp eq v0, v5
        v9 = iadd v5, v7
        v10 = icmp ne v1, v6
        brif v8, block1(v10), block2(v6, v9)

    block2(v20: i32, v21: i32):
        v22 = load.i32 notrap v21
        v23 = icmp eq v1, v20
        v24 = iadd v20, v7
        v25 = select v23, v20, v24
        v26 = iconst.i8 1
        brif v23, block3(v26), block4(v20, v21, v22, v25)

    block4(v30: i32, v31: i32, v32: i32, v33: i32):
        v34 = iadd v30, v7
        v35 = load.i32 notrap v34
        v36 = icmp eq v32, v35
        brif v36, block5, block3(v26)

    block5:
        v37 = icmp eq v0, v31
        v38 = iadd v31, v7
        v39 = icmp ne v1, v33
        brif v37, block1(v39), block6

    block6:
        v40 = load.i32 notrap v38
        v41 = icmp eq v1, v33
        v42 = iadd v33, v7
        v43 = select v41, v33, v42
        brif v41, block3(v26), block4(v33, v38, v40, v43)

    block1(v50: i8):
        v51 = ineg v50
        return v51

    block3(v52: i8):
        return v52
}}"
    )
}

#[test]
fn equal_lengths_drop_the_second_end_test() {
    let mut f = parse(&rev("v6 = iadd v4, v1"));
    let n = pliron_clif_opt::revcmp::run(&mut f);
    let text = f.display().to_string();
    assert_eq!(n, 1, "{text}");
    let latch = text.split("block6:").nth(1).unwrap_or("");
    assert!(
        !latch.contains("select"),
        "freeze select survived\n{text}"
    );
    assert!(
        latch.contains("jump"),
        "latch still branches on the exhausted side\n{text}"
    );
    assert!(
        text.contains("iconst.i8 0"),
        "equal exit did not become Ordering::Equal\n{text}"
    );
    cranelift_codegen::verify_function(&f, &flags()).unwrap();
}

#[test]
fn different_lengths_stay() {
    let mut f = parse(&rev("v6 = iadd v2, v1"));
    let n = pliron_clif_opt::revcmp::run(&mut f);
    assert_eq!(n, 0, "{}", f.display());
    assert!(f.display().to_string().contains("select"));
}
