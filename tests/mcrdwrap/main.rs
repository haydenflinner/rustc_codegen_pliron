// memcpyopt: a temporary shared by two `String::new()` values, one built
// before a loop and appended to in it, one built in the loop. Forwarding
// the first copy rebased the temporary onto the outer string, so the
// inner one started as a copy of it on the second iteration: two owners
// of one heap block.
use std::fmt::{self, Write};

struct W(u32);

impl fmt::Display for W {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.0)
    }
}

#[inline(never)]
fn build(xs: &[W]) -> String {
    let mut body = String::new();
    for x in xs {
        let mut name = String::new();
        write!(name, "{x}").unwrap();
        body.push_str(&format!("{name},"));
    }
    body
}

fn main() {
    let xs: Vec<W> = (1..=6).map(|i| W(i * 111)).collect();
    let s = build(std::hint::black_box(&xs));
    assert_eq!(s, "<111>,<222>,<333>,<444>,<555>,<666>,");
    println!("{s}");
}
