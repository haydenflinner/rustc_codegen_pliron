//! `PLIRON_UNROLL2_BUNROLL=<insts>` (opt-in, with `PLIRON_UNROLL2`): the
//! late round's `bunroll` takes loops that contain loops. The data file is
//! the wasm32 CLIF of rapier2d's assembly nest in miniature:
//!
//! ```text
//! for q in 0..3 { for p in 0..=q {
//!     let slot = blocks[p * 3 + q];
//!     if slot == u32::MAX { continue }
//!     x[slot % 9] += s * (p + 2 * q) as f32;
//! } }
//! ```
//!
//! `unroll`'s trip walk cannot take the data-dependent `continue`, and the
//! inner bound `p <= q` is constant only per outer copy. The outer loop is
//! copied three times (its IV `q + 1` relays through the inner loop's
//! header, a cycle the late `rec_expr` resolves), then each copy's inner
//! loop with its exact trip count (`q + 1`, so no tail survives).
//! Every step is checked by the interpreter (no panic path in this one).

use std::process::Command;

/// (loops, MISMATCH lines) of the final function.
fn run(env: &[(&str, &str)]) -> (usize, usize) {
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/unroll2_bunroll_tri.clif");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clif-check"));
    cmd.args(["pipeline", data, "--trials", "40", "--print"]);
    cmd.env("PLIRON_CLIF_WASM", "1");
    for k in ["PLIRON_FCMPFOLD", "PLIRON_FMUL1", "PLIRON_UNROLL2", "PLIRON_UNROLL2_BUNROLL"] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run clif-check");
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "clif-check failed:\n{text}");
    assert!(!text.contains("VERIFIER ERROR"), "{text}");
    let last = text.rsplit("=== after ").next().expect("no step changed anything");
    let body = last.split("\n}").next().unwrap();
    (back_edges(body), text.matches("MISMATCH").count())
}

/// Natural back edges (`b -> h` with `h` dominating `b`) of a printed
/// function body.
fn back_edges(body: &str) -> usize {
    use std::collections::HashMap;
    let mut order: Vec<String> = Vec::new();
    let mut succ: HashMap<String, Vec<String>> = HashMap::new();
    let mut cur = String::new();
    for l in body.lines().map(str::trim) {
        if l.starts_with("block") && l.ends_with(':') {
            cur = l.split(|c| c == '(' || c == ':').next().unwrap().to_string();
            order.push(cur.clone());
            succ.entry(cur.clone()).or_default();
            continue;
        }
        if l.starts_with("jump") || l.starts_with("brif") || l.starts_with("br_table") {
            let words: Vec<String> = l
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.starts_with("block"))
                .map(str::to_string)
                .collect();
            succ.entry(cur.clone()).or_default().extend(words);
        }
    }
    let idx: HashMap<&str, usize> = order.iter().enumerate().map(|(i, b)| (b.as_str(), i)).collect();
    let n = order.len();
    let mut preds = vec![Vec::new(); n];
    for (b, ss) in &succ {
        for s in ss {
            if let (Some(&i), Some(&j)) = (idx.get(b.as_str()), idx.get(s.as_str())) {
                preds[j].push(i);
            }
        }
    }
    // dom[i] = set of dominators, iterative (small functions).
    let all: Vec<bool> = vec![true; n];
    let mut dom = vec![all; n];
    dom[0] = (0..n).map(|k| k == 0).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for i in 1..n {
            let mut d = vec![true; n];
            let mut any = false;
            for &p in &preds[i] {
                any = true;
                for k in 0..n {
                    d[k] &= dom[p][k];
                }
            }
            if !any {
                d = vec![false; n];
            }
            d[i] = true;
            if d != dom[i] {
                dom[i] = d;
                changed = true;
            }
        }
    }
    let mut back = 0;
    for (b, ss) in &succ {
        let Some(&i) = idx.get(b.as_str()) else { continue };
        for s in ss {
            if let Some(&j) = idx.get(s.as_str()) {
                if dom[i][j] {
                    back += 1;
                }
            }
        }
    }
    back
}

#[test]
fn the_nest_is_taken_apart() {
    let (loops, bad) = run(&[("PLIRON_UNROLL2", "1"), ("PLIRON_UNROLL2_BUNROLL", "256")]);
    assert_eq!(bad, 0);
    assert_eq!(loops, 0, "back edges left");
}

#[test]
fn without_the_late_bunroll_the_nest_stays() {
    let (loops, bad) = run(&[("PLIRON_UNROLL2", "1")]);
    assert_eq!(bad, 0);
    assert!(loops >= 2, "{loops} back edges");
}

/// Below the copies' total the outer loop is too big to copy.
#[test]
fn a_small_budget_leaves_it() {
    let (loops, bad) = run(&[("PLIRON_UNROLL2", "1"), ("PLIRON_UNROLL2_BUNROLL", "20")]);
    assert_eq!(bad, 0);
    assert!(loops >= 2, "{loops} back edges");
}
