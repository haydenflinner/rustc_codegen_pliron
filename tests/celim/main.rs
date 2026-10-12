// celim's unpeeled-icmp retry must use that normalization's own operand
// pair for range queries — pairing it with the peeled `raw` ranged the
// wrong value: `v223 == 0` (v223 = depth - 1) was decided false from a
// `depth != 0` fact (the base's range), so BTreeMap's leaf-edge walk in
// IntoIter::dying_next followed a nonexistent next pointer and segfaulted
// on drop of any tree deeper than one level (12+ entries).

use std::collections::BTreeMap;
use std::process::Command;

fn main() {
    // A leaf split at the 12th insert makes a two-level tree; iterating
    // and dropping it walks internal-edge descent + leaf links.
    for n in [12usize, 20, 40, 200] {
        let mut m: BTreeMap<u32, u32> = BTreeMap::new();
        for j in 0..n as u32 {
            m.insert(j, j * 3);
        }
        assert_eq!(m.iter().count(), n);
        assert_eq!(
            m.iter().map(|(k, _)| *k).sum::<u32>(),
            (0..n as u32).sum::<u32>()
        );
        // Consuming iteration frees internal nodes mid-walk.
        assert_eq!(m.into_iter().count(), n);
    }
    // The shape the blake3/cc build-script crash took: Command's env map.
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/c", "echo"]);
        c
    } else {
        Command::new("/bin/echo")
    };
    for j in 0..30 {
        c.env(format!("VAR_{j}"), format!("v{j}"));
    }
    c.env("UNSET_ME", "x");
    c.env_remove("UNSET_ME");
    c.output().unwrap();

    // off_implied must not fold the *weak* reverse compare: `x < y` implies
    // `x + 1 <= y`, so `x + 1 > y` is dead but `x + 1 >= y` is live at
    // `x = y - 1`. Folding it false dropped ceil_char_boundary's `i >= len`
    // exit inside trim_start_matches("0"), turning demangle of any
    // `[T; Nusize]` array type into an out-of-bounds panic — and stage2
    // rustc hangs on backtrace demangling.
    let mut hits = 0;
    for (x, y) in [(0usize, 1usize), (0, 5), (3, 4), (10, 11), (5, 3)] {
        if x < y && x + 1 >= y {
            hits += 1;
        }
    }
    assert_eq!(hits, 3);
    // Same fold on the loop-carried increment path (the searcher shape).
    assert_eq!("4".trim_start_matches("0"), "4");
    assert_eq!("4usize".trim_start_matches("0"), "4usize");
    assert_eq!("000a4".trim_start_matches("0"), "a4");
    assert_eq!("0".trim_start_matches("0"), "");
    println!("celim ok");
}
