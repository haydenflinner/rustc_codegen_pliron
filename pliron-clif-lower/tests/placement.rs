
//! Unit tests for the vendored Cranelift block placement
//! (`machinst/placement.rs`, patch 98): the crate cannot be tested on its
//! own (its build script needs the workspace feature set).
use cranelift_codegen::pliron_place as place;

fn run(entry: usize, succs: Vec<Vec<usize>>, cold: Vec<bool>, empty: Vec<bool>, unlikely: Vec<Vec<bool>>) -> Vec<usize> {
    let (order, _) = place(entry, &succs, &cold, &empty, &unlikely).expect("placed");
    let n = succs.len();
    let hot: Vec<usize> = (0..n).filter(|&b| !cold[b]).collect();
    let mut seen = order.clone();
    seen.sort_unstable();
    assert_eq!(seen, hot, "every hot block exactly once");
    assert_eq!(order[0], entry);
    order
}

fn no_hint(succs: &[Vec<usize>]) -> Vec<Vec<bool>> {
    succs.iter().map(|s| vec![false; s.len()]).collect()
}

/// `0 -> 1(header: 2 body | 3 exit); 2 -> 1`: the body follows the
/// header and the exit follows the loop.
#[test]
fn top_tested_loop() {
    let succs = vec![vec![1], vec![2, 3], vec![1], vec![]];
    let u = no_hint(&succs);
    let order = run(0, succs, vec![false; 4], vec![false; 4], u);
    assert_eq!(order, vec![0, 1, 2, 3]);
}

/// A cold panic arm goes last; the hot continuation is the fallthrough.
#[test]
fn cold_last() {
    let succs = vec![vec![1, 2], vec![], vec![3], vec![]];
    let u = no_hint(&succs);
    let order = run(0, succs, vec![false, true, false, false], vec![false; 4], u);
    assert_eq!(order, vec![0, 2, 3]);
}

/// Compare cascade `0 -> (3 match | 1); 1 -> (4 match | 2)` with the
/// match arms hinted unlikely: the chain 0,1,2 stays contiguous.
#[test]
fn cascade_with_hints() {
    let succs = vec![vec![3, 1], vec![4, 2], vec![], vec![], vec![]];
    let mut u = no_hint(&succs);
    u[0][0] = true;
    u[1][0] = true;
    let order = run(0, succs, vec![false; 5], vec![false; 5], u);
    assert_eq!(&order[..3], &[0, 1, 2]);
}

/// An empty back-edge block (`3: jump 1`) is placed last and the exit
/// is the loop's fallthrough: `[0, 1, 2, 4, 3]`.
#[test]
fn empty_latch_last() {
    let succs = vec![vec![1], vec![2, 4], vec![3], vec![1], vec![]];
    let u = no_hint(&succs);
    let order = run(0, succs, vec![false; 5], vec![false, false, false, true, false], u);
    assert_eq!(order, vec![0, 1, 2, 4, 3]);
}

/// Irreducible (two entries into a cycle) keeps the old order.
#[test]
fn irreducible_bails() {
    let succs = vec![vec![1, 2], vec![2], vec![1], vec![]];
    let u = no_hint(&succs);
    assert!(place(0, &succs, &[false; 4], &[false; 4], &u).is_none());
}
