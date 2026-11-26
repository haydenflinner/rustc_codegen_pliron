// r[verify iropt.inline.recself]
// regex's backpatch shape: `fill` recurses through `Hole::Many` and calls
// the chunky single-callsite helper `patch` for each `Hole::One`. The
// single-caller bonus normally skips a caller on a recursion cycle
// (PLIRON_INLINE_RECSTACK: every level would carry the callee's frame) —
// but `fill` is a lone self-loop, not a mutual cycle like inlinerec's
// visit<->walk. Under PLIRON_INLINE_RECSELF=1 the bonus fires and `patch`
// inlines; the output must not change.
use std::hint::black_box;

enum Hole {
    None,
    One(usize),
    Many(Vec<Hole>),
}

enum Slot {
    Empty,
    Save(usize),
    Split1(usize),
    Split2(usize),
    Done(String),
}

impl Slot {
    // A single-callsite helper that is too big for the flat threshold:
    // a match over the cell doing allocation + formatting per arm. With
    // the bonus it still inlines — its one out-of-line copy dies.
    fn patch(&mut self, goto: usize) {
        let next = match self {
            Slot::Empty => Slot::Split1(goto),
            Slot::Save(slot) => {
                let tag = format!("save slot {slot} -> {goto}");
                Slot::Done(tag)
            }
            Slot::Split1(goto1) => {
                let mut arms = Vec::new();
                for k in 0..black_box(2) {
                    arms.push(format!("arm {k}: {goto1} | {goto}"));
                }
                Slot::Done(format!("split {{{}, {}}} [{}]", goto1, goto, arms.len()))
            }
            Slot::Split2(goto2) => {
                let mut parts = Vec::new();
                for k in 0..black_box(3) {
                    parts.push(format!("part {k} of {goto}"));
                }
                if black_box(parts.len()) > 2 {
                    Slot::Done(format!("rsplit {{{goto}, {goto2}}} {:?}", parts))
                } else {
                    Slot::Done(format!("rsplit {{{goto}, {goto2}}}"))
                }
            }
            Slot::Done(tag) => Slot::Done(format!("{tag}+fin{goto}")),
        };
        *self = next;
    }
}

struct Prog {
    insts: Vec<Slot>,
}

impl Prog {
    fn fill(&mut self, hole: &Hole, goto: usize) {
        match hole {
            Hole::None => {}
            Hole::One(pc) => {
                self.insts[*pc].patch(goto);
            }
            Hole::Many(hs) => {
                for h in hs {
                    self.fill(h, goto);
                }
            }
        }
    }
}

fn holes(n: usize, depth: usize) -> Hole {
    if depth == 0 {
        return Hole::One(black_box(n) % 8);
    }
    let mut hs = Vec::new();
    for k in 0..black_box(2usize) {
        hs.push(holes(n + k, depth - 1));
    }
    if black_box(n) % 3 == 0 {
        hs.push(Hole::One(black_box(n) % 8));
    }
    Hole::Many(hs)
}

fn main() {
    let mut p = Prog {
        insts: vec![
            Slot::Empty,
            Slot::Save(3),
            Slot::Split1(5),
            Slot::Split2(7),
            Slot::Empty,
            Slot::Save(11),
            Slot::Split1(13),
            Slot::Done("seed".to_string()),
        ],
    };
    for i in 0..black_box(8usize) {
        let h = holes(i, black_box(3usize));
        p.fill(&h, i + 1);
    }
    let mut done = 0;
    let mut other = 0;
    for s in &p.insts {
        match s {
            Slot::Done(t) => {
                done += 1;
                println!("{t}");
            }
            _ => other += 1,
        }
    }
    println!("done {done} other {other}");
}
