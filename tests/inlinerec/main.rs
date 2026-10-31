// r[verify iropt.inline.recursion]
// A recursive visitor (`visit` <-> `walk`) whose `visit` calls a big
// single-caller helper (`check`). Inlining `check` into `visit` is "free"
// in code size, but every level of the recursion then carries its frame:
// stage-2 `walk_expr::<TailCallCkVisitor>` was 1,936 bytes against LLVM's
// 80 that way. Under PLIRON_INLINE_RECSTACK the bonus doesn't apply into a
// recursive caller, so `check` stays a call; the output must not change.
use std::hint::black_box;

pub enum E {
    Leaf(u64),
    Add(Box<E>, Box<E>),
    Neg(Box<E>),
    Become(Box<E>),
    Many(Vec<E>),
}

pub trait V {
    fn visit(&mut self, e: &E);
}

pub fn walk<T: V>(v: &mut T, e: &E) {
    match e {
        E::Leaf(_) => {}
        E::Add(a, b) => {
            v.visit(a);
            v.visit(b);
        }
        E::Neg(a) | E::Become(a) => v.visit(a),
        E::Many(xs) => {
            for x in xs {
                v.visit(x)
            }
        }
    }
}

pub struct Ck {
    pub errs: Vec<String>,
    pub depth: u32,
}

impl Ck {
    fn check(&mut self, e: &E) {
        let mut parts = Vec::new();
        for k in 0..black_box(3) {
            parts.push(format!("part {k} of {}", self.depth));
        }
        if black_box(parts.len()) > 2 {
            self.errs.push(format!("become at depth {}: {:?} {}", self.depth, parts, parts.len()));
        }
        if let E::Leaf(n) = e {
            self.errs.push(format!("leaf {n} {:?}", parts));
        }
    }
}

impl V for Ck {
    fn visit(&mut self, e: &E) {
        if let E::Become(inner) = e {
            self.check(inner);
        }
        self.depth += 1;
        walk(self, e);
        self.depth -= 1;
    }
}

fn tree(n: u64) -> E {
    match n % 5 {
        _ if n < 3 => E::Leaf(n),
        0 => E::Add(Box::new(tree(n / 2)), Box::new(tree(n - 1))),
        1 => E::Neg(Box::new(tree(n - 1))),
        2 => E::Become(Box::new(tree(n - 2))),
        3 => E::Many(vec![tree(n / 3), tree(n - 3), E::Become(Box::new(E::Leaf(n)))]),
        _ => E::Become(Box::new(tree(n - 1))),
    }
}

fn main() {
    let e = tree(black_box(40));
    let mut c = Ck { errs: Vec::new(), depth: 0 };
    c.visit(&e);
    println!("{} errors", c.errs.len());
    for s in c.errs.iter().take(4) {
        println!("{s}");
    }
}
