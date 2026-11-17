//! `PLIRON_WASM_SMALLFN=<insts>` (opt-in, wasm): a small function whose
//! loops are all search loops skips `looprot`, `looprot2` and `punroll`,
//! so it stays under V8's 500-byte inlining limit. The data files are the
//! wasm32 CLIF of `core::slice::memchr::memchr_aligned` (copied into a
//! scratch crate: three search loops, 205 instructions after the pipeline
//! with eight peeled copies of each byte loop, 75 without) and of a
//! filtered sum over a slice (one counted loop: `punroll` must still
//! unroll it).

use std::process::Command;

/// (instructions, loads) of the final function under `clif-check pipeline`.
fn shape(file: &str, env: &[(&str, &str)]) -> (usize, usize) {
    let data = format!("{}/tests/data/{file}", env!("CARGO_MANIFEST_DIR"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clif-check"));
    cmd.args(["pipeline", &data, "--trials", "20", "--print"]);
    cmd.env("PLIRON_CLIF_WASM", "1").env_remove("PLIRON_WASM_SMALLFN");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run clif-check");
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "clif-check failed:\n{text}");
    assert!(!text.contains("MISMATCH") && !text.contains("VERIFIER ERROR"), "{text}");
    let last = text.rsplit("=== after ").next().expect("no step changed anything");
    let body = last.split("\n}").next().unwrap();
    let insts = body
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.starts_with('v') && l.contains(" = ")
                || ["store", "jump", "brif", "return", "call", "br_table", "trap"].iter().any(|p| l.starts_with(p))
        })
        .count();
    let loads = body.lines().filter(|l| l.contains("load")).count();
    (insts, loads)
}

#[test]
fn search_loops_stay_rolled_in_a_small_function() {
    let (n0, l0) = shape("wasm_smallfn_memchr.clif", &[]);
    let (n1, l1) = shape("wasm_smallfn_memchr.clif", &[("PLIRON_WASM_SMALLFN", "120")]);
    assert!(n1 * 2 < n0, "insts {n0} -> {n1}");
    // Two byte loops and the two-word body: no peeled copies.
    assert!(l1 <= 4 && l0 > 8, "loads {l0} -> {l1}");
}

#[test]
fn a_counted_loop_is_still_unrolled() {
    assert_eq!(
        shape("wasm_smallfn_sum.clif", &[]),
        shape("wasm_smallfn_sum.clif", &[("PLIRON_WASM_SMALLFN", "120")])
    );
}

/// Over the size limit: nothing changes.
#[test]
fn a_bigger_function_is_left_alone() {
    assert_eq!(
        shape("wasm_smallfn_memchr.clif", &[]),
        shape("wasm_smallfn_memchr.clif", &[("PLIRON_WASM_SMALLFN", "40")])
    );
}
