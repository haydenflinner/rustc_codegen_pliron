//! `PLIRON_IVMERGE=1`: loop params in lockstep share one register.
//!
//! The shapes are `core::num::bignum::Big32x40::cmp` (`a.iter().rev()
//! .cmp(b.iter().rev())`) as the late pipeline leaves it: natively two
//! pointer IVs, a third param holding `b`'s previous value, `b`'s step
//! behind `select(b == b_start, b, b - 4)` and both end tests; on wasm32
//! (after `REVCMP`) one end test, but `b` and a duplicate of it carried.
//! Run with `PLIRON_IVMERGE=0` to see the tests fail.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::ivmerge::{Amode, run};
use pliron_clif_opt::pipeline::Analysis;

fn knob() {
    if std::env::var_os("PLIRON_IVMERGE").is_none() {
        unsafe { std::env::set_var("PLIRON_IVMERGE", "1") };
    }
}

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const AARCH64: Amode = Amode {
    reg_reg: true,
    reg_reg_disp: false,
    neg_off: true,
};
const WASM: Amode = Amode {
    reg_reg: false,
    reg_reg_disp: false,
    neg_off: false,
};

fn count(f: &Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

fn params_of(f: &Function, name: &str) -> usize {
    let b = f
        .layout
        .blocks()
        .find(|b| b.to_string() == name)
        .unwrap_or_else(|| panic!("no {name}"));
    f.dfg.num_block_params(b)
}

fn same(before: &Function, after: &Function) {
    let s = check(
        before,
        after,
        &Facts::default(),
        &Config {
            trials: 256,
            ..Config::default()
        },
    )
    .unwrap_or_else(|m| panic!("mismatch: {m:?}\n{}", after.display()));
    assert!(s.compared > 0, "no trial compared: {:?}", s.why);
}

/// Native `big_cmp` after `retest`.
const NATIVE: &str = "function %big_cmp(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v5 = load.i64 notrap v0+160
    v8 = load.i64 notrap v1+160
    v361 = umax v5, v8
    v379 = iconst.i64 41
    v15 = icmp ult v361, v379
    brif v15, block4, block3

block4:
    v406 = iconst.i64 2
    v407 = ishl v361, v406
    v509 = iconst.i64 0
    v505 = icmp eq v407, v509
    v515 = icmp ne v407, v509
    brif v505, block33(v515), block186

block186:
    v37 = iadd v407, v1
    v26 = iadd v407, v0
    v65 = iconst.i64 -4
    v434 = iadd v26, v65
    jump block56(v37, v434)

block56(v441: i64, v445: i64):
    v87 = load.i32 notrap v445
    v511 = iadd v441, v65
    v439 = select v505, v441, v511
    v516 = iconst.i8 1
    jump block118(v441, v445, v87, v439)

block118(v490: i64, v491: i64, v492: i32, v493: i64):
    v125 = load.i32 notrap v490-4
    v414 = icmp eq v492, v125
    brif v414, block182, block183

block183:
    v410 = icmp ugt v492, v125
    v411 = icmp ult v492, v125
    v503 = isub v410, v411
    jump block40(v503)

block182:
    v446 = icmp eq v0, v491
    brif v446, block188, block184

block188:
    v487 = icmp ne v1, v493
    jump block33(v487)

block184:
    v504 = iadd v491, v65
    v494 = load.i32 notrap v504
    v495 = icmp eq v1, v493
    v496 = iadd v493, v65
    v497 = select v495, v493, v496
    brif v495, block189, block118(v493, v504, v494, v497)

block189:
    jump block40(v516)

block33(v488: i8):
    v426 = ineg v488
    return v426

block40(v292: i8):
    return v292

block3 cold:
    trap user1
}";

/// `b` (two params: its previous and its next value) becomes `a + (v1 -
/// v0)`; `b`'s end test is `a`'s and folds, with its `select`.
#[test]
fn native_reverse_cmp_keeps_one_pointer() {
    knob();
    let before = parse(NATIVE);
    let mut f = before.clone();
    let n = run(&mut f, &mut Analysis::default(), AARCH64);
    assert!(n >= 2, "nothing merged:\n{}", f.display());
    assert_eq!(params_of(&f, "block118"), 2, "{}", f.display());
    assert_eq!(count(&f, Opcode::Select), 0, "{}", f.display());
    // The loop's two `brif`s on `v0 == a` and `v1 == b` are one.
    assert_eq!(count(&f, Opcode::Brif), count(&before, Opcode::Brif) - 1, "{}", f.display());
    // `b`'s load is `[a + (v1 - v0)]`, offset folded to 0.
    let loads: Vec<String> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .map(|i| f.dfg.display_inst(i).to_string())
        .collect();
    assert!(loads.iter().all(|l| !l.contains("-4")), "{loads:?}");
    same(&before, &f);
}

/// wasm32 `big_cmp` after `retest` (pointers widened to `i64`: the
/// differential checker's pointer width is per process): `v447` is `b`,
/// `v448` its duplicate.
const DUP: &str = "function %big_cmp(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v5 = load.i64 notrap v0+160
    v8 = load.i64 notrap v1+160
    v355 = umax v5, v8
    v375 = iconst.i64 41
    v15 = icmp ult v355, v375
    brif v15, block4, block3

block4:
    v378 = iconst.i64 2
    v379 = ishl v355, v378
    v465 = iconst.i64 0
    v463 = icmp eq v379, v465
    v471 = iconst.i8 0
    brif v463, block23(v471), block152

block152:
    v37 = iadd v379, v1
    v26 = iadd v379, v0
    v60 = iconst.i64 -4
    v401 = iadd v26, v60
    jump block39(v37, v401)

block39(v408: i64, v411: i64):
    v79 = load.i32 notrap v411
    v467 = iadd v408, v60
    jump block83(v411, v79, v467, v467)

block83(v445: i64, v446: i32, v447: i64, v448: i64):
    v115 = load.i32 notrap v447
    v386 = icmp eq v446, v115
    brif v386, block148, block149

block149:
    v382 = icmp ugt v446, v115
    v383 = icmp ult v446, v115
    v459 = isub v382, v383
    jump block28(v459)

block148:
    v412 = icmp eq v0, v445
    brif v412, block154, block150

block154:
    jump block23(v471)

block150:
    v460 = iadd v445, v60
    v449 = load.i32 notrap v460
    v451 = iadd v448, v60
    jump block83(v460, v449, v451, v451)

block23(v443: i8):
    v393 = ineg v443
    return v393

block28(v265: i8):
    return v265

block3 cold:
    trap user1
}";

#[test]
fn wasm_duplicate_and_lockstep_pointer_go() {
    knob();
    let before = parse(DUP);
    let mut f = before.clone();
    let n = run(&mut f, &mut Analysis::default(), WASM);
    assert_eq!(n, 2, "{}", f.display());
    assert_eq!(params_of(&f, "block83"), 2, "{}", f.display());
    // `b`'s step is gone; its load reads `a + (v1 - v0)`.
    assert!(count(&f, Opcode::Iadd) <= count(&before, Opcode::Iadd), "{}", f.display());
    assert_eq!(count(&f, Opcode::Isub), count(&before, Opcode::Isub) + 1, "{}", f.display());
    same(&before, &f);
}

/// Different steps: no relation holds after the first back edge.
#[test]
fn different_steps_stay() {
    knob();
    let src = DUP.replace(
        "    v451 = iadd v448, v60\n",
        "    v61 = iconst.i64 -8\n    v451 = iadd v448, v61\n",
    );
    let before = parse(&src);
    let mut f = before.clone();
    run(&mut f, &mut Analysis::default(), WASM);
    // The duplicate still merges (`v448 = v447` holds), `b` does not.
    assert_eq!(params_of(&f, "block83"), 3, "{}", f.display());
    same(&before, &f);
}

/// A step behind a `select` the back edge does not decide: `b` is not
/// in lockstep with `a`.
#[test]
fn undecided_select_stays() {
    knob();
    let src = NATIVE.replace(
        "    v497 = select v495, v493, v496\n",
        "    v497 = select v414, v493, v496\n",
    );
    let before = parse(&src);
    let mut f = before.clone();
    run(&mut f, &mut Analysis::default(), AARCH64);
    assert!(params_of(&f, "block118") >= 3, "{}", f.display());
    same(&before, &f);
}

/// wasm has no register-register address: a member whose only use is an
/// address and whose step does not die costs an add per iteration, and
/// stays.
#[test]
fn wasm_cost_rule_keeps_live_step() {
    knob();
    // `b`'s step `v451` also feeds a store: it does not die.
    let src = DUP.replace(
        "    jump block83(v460, v449, v451, v451)\n",
        "    store notrap v449, v451+64\n    jump block83(v460, v449, v451, v451)\n",
    );
    let before = parse(&src);
    let mut f = before.clone();
    run(&mut f, &mut Analysis::default(), WASM);
    // The duplicate goes (free), `b` stays.
    assert_eq!(params_of(&f, "block83"), 3, "{}", f.display());
    same(&before, &f);
}

fn mem_offsets(f: &Function) -> Vec<i64> {
    use cranelift_codegen::ir::InstructionData;
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter_map(|i| match f.dfg.insts[i] {
            InstructionData::Load { offset, .. } | InstructionData::Store { offset, .. } => {
                Some(i64::from(offset))
            }
            _ => None,
        })
        .collect()
}

/// wasm's memarg offset is unsigned: a rebased address whose folded
/// constant goes below zero keeps it in the address (the emitter would
/// read `p+0xffff_fff8`). `b`'s load at `b - 8` is `a + (v1 - v0) + 4 - 8`.
#[test]
fn wasm_keeps_offsets_nonnegative() {
    knob();
    let src = NATIVE.replace("v490-4\n", "v490-8\n");
    let before = parse(&src);
    assert!(mem_offsets(&before).contains(&-8));
    let mut f = before.clone();
    run(&mut f, &mut Analysis::default(), WASM);
    assert_eq!(params_of(&f, "block118"), 2, "{}", f.display());
    // No negative offset the input did not have (it keeps its `-8`; the
    // rewrite would have made it `-4`).
    let neg = |g: &Function| -> Vec<i64> { mem_offsets(g).into_iter().filter(|&o| o < 0).collect() };
    assert_eq!(neg(&f), neg(&before), "{}", f.display());
    let mut g = before.clone();
    run(&mut g, &mut Analysis::default(), AARCH64);
    assert!(mem_offsets(&g).contains(&-4), "{}", g.display());
    same(&before, &f);
}
