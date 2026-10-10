// LICM must not speculate a `notrap` load past the enum discriminant guard:
// `has_any_name`'s `iter().any` loop derefs `n` only on the `Unparsed` arm,
// but `n`'s load is `notrap`, so a plain "invariant + notrap" hoist moves the
// deref into the preheader and it runs for `Parsed` payloads too (garbage
// pointer deref — segfault on rustc_passes::check_unused_attribute in
// self-hosted rustc).

pub struct Item {
    path: Path,
    #[allow(dead_code)]
    pad: [u64; 4],
}
pub struct Path {
    #[allow(dead_code)]
    pad: [u64; 5],
    segs: Box<[u32]>,
}
pub enum Attr {
    Parsed([u64; 4]),
    Unparsed(Box<Item>),
}
impl Attr {
    #[inline]
    pub fn name(&self) -> Option<u32> {
        match self {
            Attr::Unparsed(n) => {
                if let [id] = n.path.segs.as_ref() {
                    Some(*id)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    #[inline]
    pub fn has_any_name(&self, names: &[u32]) -> bool {
        names.iter().any(|&n| self.name() == Some(n))
    }
}

#[inline(never)]
fn check_unused(a: &Attr) -> u32 {
    if a.has_any_name(&[1, 2, 3, 4, 5]) {
        1
    } else {
        0
    }
}

fn main() {
    // The Parsed payload is zeros — a speculative box deref reads 0+0x30.
    let p = Attr::Parsed([0, 0, 0, 0]);
    assert_eq!(check_unused(&p), 0);
    let mk = |seg: u32| {
        Attr::Unparsed(Box::new(Item {
            path: Path {
                pad: [0; 5],
                segs: vec![seg].into_boxed_slice(),
            },
            pad: [0; 4],
        }))
    };
    assert_eq!(check_unused(&mk(3)), 1);
    assert_eq!(check_unused(&mk(9)), 0);
    // Alternate variants inside one loop so hoisting is exercised repeatedly.
    let attrs = [p, mk(2), Attr::Parsed([0, 0, 0, 0]), mk(8)];
    let hits: u32 = attrs.iter().map(check_unused).sum();
    assert_eq!(hits, 1);
    println!("licm guard ok");
}
