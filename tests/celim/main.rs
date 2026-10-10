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
        assert_eq!(m.iter().map(|(k, _)| *k).sum::<u32>(), (0..n as u32).sum::<u32>());
        // Consuming iteration frees internal nodes mid-walk.
        assert_eq!(m.into_iter().count(), n);
    }
    // The shape the blake3/cc build-script crash took: Command's env map.
    let mut c = Command::new("/bin/echo");
    for j in 0..30 {
        c.env(format!("VAR_{j}"), format!("v{j}"));
    }
    c.env("UNSET_ME", "x");
    c.env_remove("UNSET_ME");
    c.output().unwrap();
    println!("celim ok");
}
