// Regression: post-allocation pair fusion must not merge a load with a
// second load whose address uses the first load's result. Regalloc may
// reuse one physical register for two vregs with disjoint live ranges
// (`ldr x0, [x0]` then `ldr x1, [x0, #8]`); fusing that to
// `ldp x0, x1, [x0]` reads the pre-load base for both slots. The trigger
// shape is pointer chasing in a tight loop — the iterator/closures here
// produced it reliably (`EXC_BAD_ACCESS` in `chars().filter().count()`).
//
// The `match` arms double as a small-switch (`br_table` → compare chain)
// sanity check: every arm must produce the same values as sequential
// compares would.

use std::hint::black_box;

#[inline(never)]
fn chase(nodes: &[usize], mut cur: usize, steps: usize) -> usize {
    // cur = nodes[cur] each step: every load's address depends on the
    // previous load's result. `nodes` is laid out so the chase cycles
    // through every index.
    for _ in 0..steps {
        cur = nodes[cur];
    }
    cur
}

#[inline(never)]
fn env_chain(e: &Box<(usize, usize, usize)>) -> usize {
    // A load of the box pointer feeding loads off that pointer — the
    // closure-environment shape from the original crash.
    let t = &**e;
    t.0 + t.1 * 10 + t.2 * 100
}

#[inline(never)]
fn disc(x: u8) -> usize {
    match x & 3 {
        0 => 10,
        1 => 20,
        2 => 30,
        _ => 40,
    }
}

fn main() {
    // A cycle over all indices: nodes[i] = i+1 mod n.
    let n = 4096usize;
    let nodes: Vec<usize> = (0..n).map(|i| (i + 1) % n).collect();
    // After n steps the chase returns to its start regardless of where
    // it began; after n-1 steps it is one short.
    assert_eq!(chase(&nodes, 0, n), 0);
    assert_eq!(chase(&nodes, 5, n), 5);
    assert_eq!(chase(&nodes, 0, n - 1), n - 1);
    assert_eq!(chase(&nodes, 7, n - 1), 6);

    // Self-referential-ish: values that are also valid indices into a
    // small table so two consecutive loads share a base register easily.
    let idx: Vec<usize> = vec![2, 0, 3, 1];
    let vals: Vec<usize> = vec![11, 22, 33, 44];
    let mut acc = 0usize;
    for &i in &idx {
        acc += vals[i];
    }
    assert_eq!(acc, 11 + 22 + 33 + 44);

    let e = Box::new((3usize, 4usize, 5usize));
    assert_eq!(env_chain(&e), 543);

    // The original trigger: chars().filter().count() inside an inlined
    // closure over a heap string miscompiled to a dependent ldp.
    let s: String = "the quick brown fox\n".repeat(4096);
    let got = black_box(&s).chars().filter(|c| c.is_alphabetic()).count();
    let want = s.chars().filter(|c| c.is_alphabetic()).count();
    assert_eq!(got, want);
    let words = black_box(&s).split_whitespace().count();
    assert_eq!(words, 4 * 4096);

    for x in 0u8..=255 {
        assert_eq!(disc(x), [10usize, 20, 30, 40][(x & 3) as usize]);
    }

    println!("pairfuse ok");
}
