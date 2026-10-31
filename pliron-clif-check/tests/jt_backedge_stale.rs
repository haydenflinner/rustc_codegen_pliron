//! indexmap's `Core::<usize, ()>::erase_indices` (its `PLIRON_CORPUS` dump,
//! facts included) through the whole pipeline. Its shifted-index loop
//! `for ((new, old), e) in (start..).zip(end..).zip(shifted)` merges the
//! zip's `Option` in `block379`, which rotates its params each trip; once
//! the niche test there folds, jtround threads the latch's back edge past
//! `block379` onto `block25`. That edge named `block379`'s own params, so
//! the loop carried the entry's `start + 1` / `end + 1` forever and
//! `update_index` panicked "index not found" (crates suite, 2026-10-31).
//! The final back edge into `block25` must carry the loop's own counters,
//! not values computed before the loop.

use std::process::Command;

#[test]
fn erase_indices_backedge_carries_counters() {
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/indexmap_erase_indices.clif");
    let out = Command::new(env!("CARGO_BIN_EXE_clif-check"))
        .args(["vcode", data])
        .env("PLIRON_PRECOMPILE", "1")
        // The (opt-in) second-level niche fold makes the edge threadable.
        .env("PLIRON_NULLSRC", "1")
        .output()
        .expect("run clif-check");
    let err = String::from_utf8_lossy(&out.stderr);
    let clif = err.split("precompile ====").nth(1).expect("no precompile dump");
    let lines: Vec<&str> = clif.lines().map(str::trim).collect();
    // Blocks as (header line, body lines).
    let mut blocks: Vec<(&str, Vec<&str>)> = Vec::new();
    for l in &lines {
        if l.starts_with("block") && l.ends_with(':') {
            blocks.push((l, Vec::new()));
        } else if !l.is_empty() && let Some(b) = blocks.last_mut() {
            b.1.push(l);
        }
    }
    let name = |h: &str| h.split(['(', ':', ' ']).next().unwrap().to_string();
    // The latch: the block storing `*index = new` (`store.i64 .., vN-8`).
    let latch = blocks
        .iter()
        .find(|(_, b)| b.iter().any(|l| l.starts_with("store.i64 notrap") && l.ends_with("-8")))
        .expect("update_index latch");
    let back = *latch.1.iter().rev().find(|l| l.starts_with("brif") || l.starts_with("jump")).unwrap();
    // Its loop target: the destination that has another, outside edge.
    let mut checked = false;
    for t in back.split(' ').filter_map(|w| w.split_once('(').map(|(b, _)| b)).filter(|b| b.starts_with("block")) {
        let pat = format!("{t}(");
        let entries: Vec<&(&str, Vec<&str>)> = blocks
            .iter()
            .filter(|(h, b)| name(h) != name(latch.0) && b.last().is_some_and(|l| l.contains(&pat)))
            .collect();
        // Values defined in the blocks entering the loop from outside.
        let pre: Vec<&str> = entries
            .iter()
            .flat_map(|(_, b)| b.iter().filter_map(|l| l.split_once(" = ").map(|(v, _)| v)))
            .collect();
        if pre.is_empty() {
            continue;
        }
        let args = back.split(&pat).nth(1).unwrap().split(')').next().unwrap();
        for a in args.split(", ") {
            assert!(!pre.contains(&a), "back edge passes pre-loop value {a}: {back}\n{clif}");
        }
        checked = true;
    }
    assert!(checked, "no loop entry found for {back}\n{clif}");
}
