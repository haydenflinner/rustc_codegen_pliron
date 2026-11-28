//! edgesel: a `select c`/`brif c` inside a block whose every incoming
//! edge pins `c` folds to the known arm. `str::chars`'s
//! `next().unwrap_or(0)` clamps reach CLIF as `select(at_end, 0, cur)`
//! in the `!at_end` target — isel emits a dup `cmp` + `cmov` pair per
//! char on tokenize's ASCII scan unless the fold fires.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn dcheck(before: &Function, after: &Function) {
    match check(before, after, &Facts::default(), &Config::default()) {
        Ok(s) => assert!(s.compared > 0, "{s:?}"),
        Err(m) => panic!("{m}
---- after
{}", after.display()),
    }
}

#[test]
fn else_edge_folds_selects() {
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = icmp eq v0, v1
            brif v2, block2, block1

        block1:
            v3 = iconst.i64 0
            v4 = select.i64 v2, v3, v0
            v5 = iadd.i64 v4, v0
            v6 = select.i64 v2, v0, v5
            brif v2, block2, block3(v6)

        block2:
            return v0

        block3(v7: i64):
            return v7
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::run(&mut after);
    // Both selects fold to the known arm; the redundant `brif v2`
    // becomes a `jump`.
    assert_eq!(n, 3);
    let s = after.to_string();
    assert!(!s.contains("select"), "{s}");
    assert!(s.contains("v4 -> v0") && s.contains("v6 -> v5"), "{s}");
    assert!(s.contains("jump block3(v6)"), "{s}");
    dcheck(&before, &after);
}

#[test]
fn then_edge_folds_selects() {
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = icmp eq v0, v1
            brif v2, block1, block2

        block1:
            v3 = iconst.i64 9
            v4 = select.i64 v2, v3, v0
            return v4

        block2:
            return v0
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::run(&mut after);
    assert_eq!(n, 1);
    let s = after.to_string();
    assert!(!s.contains("select"), "{s}");
    assert!(s.contains("v4 -> v3"), "{s}");
    dcheck(&before, &after);
}

#[test]
fn mixed_edges_keep_select() {
    // One `jump` pred establishes nothing — the select must survive.
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = icmp eq v0, v1
            brif v2, block3, block1

        block1:
            jump block2

        block2:
            v3 = iconst.i64 9
            v4 = select.i64 v2, v3, v0
            return v4

        block3:
            jump block2
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::run(&mut after);
    assert_eq!(n, 0);
    dcheck(&before, &after);
}

#[test]
fn both_arms_same_target_keeps_select() {
    // `brif c, b, b` reaches b on either polarity — no fold.
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = icmp eq v0, v1
            brif v2, block1, block1

        block1:
            v3 = iconst.i64 9
            v4 = select.i64 v2, v3, v0
            return v4
        }",
    );
    let mut after = before.clone();
    let _ = pliron_clif_opt::edgesel::run(&mut after);
    let s = after.to_string();
    assert!(s.contains("select"), "{s}");
    dcheck(&before, &after);
}

#[test]
fn equality_edge_cancels_select_hidden_sub() {
    // Reduced `slice::range` validation: success on `tag == 2` proves both
    // validation selects took their false arms, so the copy count is
    // `(x + 4) - x` even though armmerge hid the endpoints behind selects.
    let before = parse(
        "function %f(i64) -> i64 {
        block0(v0: i64):
            v1 = iconst.i64 3
            v2 = iadd v0, v1
            v3 = icmp uge v2, v0
            v4 = iconst.i64 4
            v5 = iadd v0, v4
            v6 = icmp ugt v0, v5
            v7 = iconst.i64 1
            v8 = iconst.i64 2
            v9 = select.i64 v6, v7, v8
            v10 = iconst.i64 0
            v11 = select.i64 v3, v10, v9
            v12 = select.i64 v3, v2, v0
            v13 = select.i64 v3, v10, v5
            jump block1(v11, v12, v13)

        block1(v20: i64, v21: i64, v22: i64):
            v23 = iconst.i64 2
            v24 = icmp eq v20, v23
            brif v24, block2, block3

        block2:
            v25 = isub v22, v21
            return v25

        block3:
            v26 = iconst.i64 0
            return v26
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::edge_rel(&mut after);
    assert_eq!(n, 1);
    let s = after.to_string();
    assert!(s.contains("v25 = iconst.i64 4"), "{s}");
    assert!(!s.contains("isub"), "{s}");
    dcheck(&before, &after);
}

#[test]
fn loop_reentry_rejects_select_hidden_sub() {
    // The soundness gate is the unique incoming edge: a success block that
    // can re-enter itself without passing through the equality edge must not
    // use the edge-proof endpoints.
    let before = parse(
        "function %f(i64, i8) -> i64 {
        block0(v0: i64, v30: i8):
            v4 = iconst.i64 4
            v5 = iadd v0, v4
            v7 = iconst.i64 1
            v8 = iconst.i64 2
            v9 = select.i64 v30, v7, v8
            v10 = iconst.i64 0
            v11 = select.i64 v30, v10, v9
            v12 = select.i64 v30, v5, v0
            jump block1(v11, v0, v12)

        block1(v20: i64, v21: i64, v22: i64):
            v23 = iconst.i64 2
            v24 = icmp eq v20, v23
            brif v24, block2, block3

        block2:
            v25 = isub v22, v21
            brif v30, block2, block3

        block3:
            v26 = iconst.i64 0
            return v26
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::edge_rel(&mut after);
    assert_eq!(n, 0);
    assert!(after.to_string().contains("isub"));
    dcheck(&before, &after);
}

#[test]
fn narrow_delta_uses_in_bounds_iconst() {
    // Cranelift's verifier requires a narrow `iconst` to be zero-extended:
    // `(x - 1) - x` is `0xffffffff` in i32, not the sign-extended i64 -1.
    let before = parse(
        "function %f(i32, i8) -> i32 {
        block0(v0: i32, v30: i8):
            v4 = iconst.i32 -1
            v5 = iadd v0, v4
            v7 = iconst.i32 1
            v8 = iconst.i32 2
            v9 = select.i32 v30, v7, v8
            v10 = iconst.i32 0
            v11 = select.i32 v30, v10, v9
            v12 = select.i32 v30, v0, v5
            jump block1(v11, v0, v12)

        block1(v20: i32, v21: i32, v22: i32):
            v23 = iconst.i32 2
            v24 = icmp eq v20, v23
            brif v24, block2, block3

        block2:
            v25 = isub v22, v21
            return v25

        block3:
            v26 = iconst.i32 0
            return v26
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::edge_rel(&mut after);
    assert_eq!(n, 1);
    let inst = after
        .dfg
        .value_def(cranelift_codegen::ir::Value::from_u32(25))
        .inst()
        .expect("v25 is an instruction result");
    let cranelift_codegen::ir::InstructionData::UnaryImm {
        opcode: cranelift_codegen::ir::Opcode::Iconst,
        imm,
    } = after.dfg.insts[inst]
    else {
        panic!("v25 was not folded: {}", after.display())
    };
    let imm = imm.bits();
    assert_eq!(imm, u32::MAX as i64, "{}", after.display());
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&after, &flags).unwrap();
    dcheck(&before, &after);
}
