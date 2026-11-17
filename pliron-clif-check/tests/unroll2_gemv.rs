//! `PLIRON_UNROLL2=1` with `PLIRON_FCMPFOLD=1`: the small-trip loop nest
//! of nalgebra's `gemv` (`tests/data/gemv_beta_slot.clif`, written after
//! the CLIF of rapier2d's 2x3 by 3x2 `Matrix::mul`). Each column tests
//! `!beta.is_zero()` on a `beta` that sits in a stack slot holding the
//! constant 0.0, so the early `unroll` finds the column loop undecided.
//! Once `loadfwd` forwards the 0.0 and the egraph folds the `fcmp`, the
//! late `unroll2` takes the whole nest apart. `bunroll` is off in these
//! runs: on a body this small it would unroll the loop by itself, and
//! rapier's real bodies are past its cap.

use std::process::Command;

/// `clif-check pipeline` on the data file under `env`; the number of
/// `brif`s in the final function, after checking every step.
fn final_brifs(env: &[(&str, &str)]) -> usize {
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/gemv_beta_slot.clif");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clif-check"));
    cmd.args(["pipeline", data, "--trials", "20", "--print"]);
    cmd.env("PLIRON_BUNROLL", "0");
    for k in ["PLIRON_FCMPFOLD", "PLIRON_FMUL1", "PLIRON_UNROLL2"] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run clif-check");
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "clif-check failed:\n{text}");
    assert!(!text.contains("MISMATCH") && !text.contains("VERIFIER ERROR"), "{text}");
    let last = text.rsplit("=== after ").next().expect("no step changed anything");
    let body = last.split("\n}").next().unwrap();
    body.lines().filter(|l| l.trim_start().starts_with("brif")).count()
}

#[test]
fn late_unroll_takes_the_gemv_nest_apart() {
    assert_eq!(final_brifs(&[("PLIRON_FCMPFOLD", "1"), ("PLIRON_UNROLL2", "1")]), 0);
}

/// Without the late step the column loop stays (the beta test folds, the
/// loop does not).
#[test]
fn constant_fcmp_alone_keeps_the_loop() {
    assert!(final_brifs(&[("PLIRON_FCMPFOLD", "1")]) >= 1);
}

/// Without the fold the late step has no decided trip either.
#[test]
fn late_unroll_alone_keeps_the_loop() {
    assert!(final_brifs(&[("PLIRON_UNROLL2", "1")]) >= 1);
}

#[test]
fn default_keeps_the_loops() {
    assert!(final_brifs(&[]) >= 1);
}
