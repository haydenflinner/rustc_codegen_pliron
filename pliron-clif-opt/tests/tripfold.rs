//! The hot scan's zero counter is `index < start + bound` once the
//! entry proved `start + (bound - 1)` does not wrap. The cold side of
//! that proof keeps its counter.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const SCAN: &str = "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v2 = iconst.i32 3
        v3 = iconst.i32 1
        v4 = isub.i32 v1, v3
        v5 = isub.i32 v4, v3
        v6, v7 = uadd_overflow v5, v2
        v8 = iconst.i32 100
        v9 = icmp ult v6, v8
        v10 = iconst.i8 0
        v11 = icmp eq v7, v10
        v12 = band v9, v11
        v13 = iconst.i32 0
        brif v12, block1(v13, v2, v13), block2(v13, v2, v13)

    block1(v20: i32, v21: i32, v22: i32):
        v23 = iadd.i32 v0, v21
        v24 = uload8.i32 v23
        v25 = iadd.i32 v22, v24
        v26 = icmp ult v20, v4
        brif v26, block3, block4

    block3:
        v27 = iconst.i32 1
        v28 = iadd.i32 v20, v27
        v29 = iadd.i32 v21, v27
        jump block1(v28, v29, v25)

    block4:
        v30 = iconst.i32 1
        v31 = band.i32 v21, v30
        return v31

    block2(v40: i32, v41: i32, v42: i32):
        v43 = icmp ult v40, v4
        brif v43, block5, block6

    block5:
        v44 = iconst.i32 1
        v45 = iadd.i32 v40, v44
        v46 = iadd.i32 v41, v44
        jump block2(v45, v46, v42)

    block6:
        v47 = iconst.i32 1
        v48 = band.i32 v41, v47
        return v48
}";

#[test]
fn hot_loop_drops_the_zero_counter() {
    let mut f = parse(SCAN);
    let n = pliron_clif_opt::tripfold::run(&mut f, &mut Analysis::default());
    let text = f.display().to_string();
    assert_eq!(n, 1, "{text}");
    assert!(
        text.contains("icmp ult v21, v20"),
        "trip test did not move onto the index\n{text}"
    );
    let hot = text.split("block3:").nth(1).unwrap().split("block4:").next().unwrap();
    assert!(
        !hot.contains("iadd.i32 v20") && !hot.contains("iadd v20"),
        "counter increment survived\n{hot}"
    );
    let cold = text.split("block5:").nth(1).unwrap();
    assert!(
        cold.contains("iadd.i32 v40, v44") || cold.contains("iadd v40, v44"),
        "cold counter was removed\n{text}"
    );
}

/// The real scan proves `i0 + (length - 2)` and the bound is
/// `length - 1`, so the predecessor is not literally `bound - 1`.
const LATE: &str = "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v193 = iconst.i32 9
        v535 = iconst.i32 3
        v511 = iconst.i32 -2
        v512 = iadd.i32 v193, v511
        v349, v350 = uadd_overflow v512, v535
        v536 = iconst.i32 1547
        v351 = icmp ult v349, v536
        v473 = iconst.i8 0
        v353 = icmp eq v350, v473
        v354 = band v351, v353
        v538 = iconst.i32 -1
        v331 = iadd.i32 v193, v538
        v245 = iconst.i32 1
        v537 = iconst.i32 0
        brif v354, block1(v537, v535, v537), block2(v537, v535, v537)

    block1(v355: i32, v356: i32, v357: i32):
        v359 = icmp ult v355, v331
        brif v359, block3, block4

    block3:
        v440 = iadd.i32 v355, v245
        v441 = iadd.i32 v356, v245
        v362 = iadd.i32 v357, v0
        v363 = icmp ule v362, v1
        brif v363, block1(v440, v441, v362), block4

    block2(v40: i32, v41: i32, v42: i32):
        v43 = icmp ult v40, v331
        brif v43, block5, block4

    block5:
        v45 = iadd.i32 v40, v245
        v46 = iadd.i32 v41, v245
        jump block2(v45, v46, v42)

    block4:
        return v356
}";

#[test]
fn length_minus_two_still_drops_the_counter() {
    let mut f = parse(LATE);
    let n = pliron_clif_opt::tripfold::run(&mut f, &mut Analysis::default());
    let text = f.display().to_string();
    assert_eq!(n, 1, "{text}");
    assert!(text.contains("icmp ult v356, v355"), "{text}");
}
