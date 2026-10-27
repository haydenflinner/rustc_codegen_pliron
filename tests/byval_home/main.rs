// PLIRON_BYVALHOME: `memcpy(tmp, local); call f(tmp)` — the by-value
// argument copy rustc makes of a local — becomes `call f(local)` when the
// local gets a whole new value before anything reads it again, and the
// local then stays one stack object through SROA instead of being split
// into scalars that are stored into the temp at every such call. The
// shapes follow regex-syntax's parser loop (`concat =
// self.push_group(concat)?`): an aggregate carried around a loop with
// calls, moved into non-inlined calls that write their copy (so the
// caller's slot holds garbage until the result is stored back), rebuilt
// from an sret result, read between the moves, and dropped on the
// unwind path only when still initialized. Every printed value must
// match the default build (and stock rustc's).

use std::hint::black_box;

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    start: u64,
    end: u64,
}

// 72 bytes: three Vec headers, like regex-syntax's `Concat`.
#[derive(Clone, Debug, PartialEq)]
pub struct Concat {
    span: Vec<u64>,
    asts: Vec<u64>,
    tags: Vec<u32>,
}

pub struct Group {
    concat: Concat,
    span: Span,
    kind: u32,
}

pub enum State {
    Group(Group),
    Alt { alt: Vec<Concat>, depth: u32 },
}

pub struct Parser {
    pos: usize,
    text: Vec<u8>,
    stack: Vec<State>,
    out: Vec<u64>,
}

#[derive(Debug)]
pub struct Error(u32);

impl Parser {
    #[inline(never)]
    fn bump(&mut self) -> Option<u8> {
        let b = self.text.get(self.pos).copied();
        self.pos += 1;
        b
    }

    // By-value argument the callee mutates in place before storing it:
    // the caller's slot must not be read after the move.
    #[inline(never)]
    fn push_group(&mut self, mut concat: Concat) -> Result<Concat, Error> {
        concat.tags.push(self.pos as u32);
        if self.stack.len() > 40 {
            return Err(Error(1));
        }
        let span = Span {
            start: self.pos as u64,
            end: 0,
        };
        self.stack.push(State::Group(Group {
            concat,
            span,
            kind: 7,
        }));
        Ok(Concat {
            span: vec![self.pos as u64],
            asts: Vec::new(),
            tags: Vec::new(),
        })
    }

    #[inline(never)]
    fn pop_group(&mut self, mut inner: Concat) -> Result<Concat, Error> {
        inner.asts.push(inner.tags.len() as u64);
        let st = self.stack.pop().ok_or(Error(2))?;
        match st {
            State::Group(Group {
                concat,
                span,
                kind,
            }) => {
                // Field-by-field moves out of an sret aggregate, each one
                // moved on into another call.
                let mut prev = self.finish_group(concat, span.start, kind)?;
                let inner_sum = self.sum(inner);
                prev.asts.push(inner_sum);
                Ok(prev)
            }
            State::Alt { alt, depth } => {
                let mut c = self.finish_alt(alt, depth)?;
                c.asts.push(self.sum(inner));
                Ok(c)
            }
        }
    }

    #[inline(never)]
    fn push_alternate(&mut self, concat: Concat) -> Result<Concat, Error> {
        let s = self.sum(concat);
        match self.stack.last_mut() {
            Some(State::Alt { alt, depth }) => {
                *depth += 1;
                alt.push(Concat {
                    span: vec![s],
                    asts: vec![],
                    tags: vec![],
                });
            }
            _ => self.stack.push(State::Alt {
                alt: vec![Concat {
                    span: vec![s],
                    asts: vec![],
                    tags: vec![],
                }],
                depth: 1,
            }),
        }
        Ok(Concat {
            span: vec![self.pos as u64 + 1000],
            asts: Vec::new(),
            tags: Vec::new(),
        })
    }

    #[inline(never)]
    fn finish_group(&mut self, mut concat: Concat, start: u64, kind: u32) -> Result<Concat, Error> {
        concat.span.push(start ^ kind as u64);
        if concat.span.len() > 1000 {
            return Err(Error(3));
        }
        Ok(concat)
    }

    #[inline(never)]
    fn finish_alt(&mut self, alt: Vec<Concat>, depth: u32) -> Result<Concat, Error> {
        let mut c = Concat {
            span: vec![depth as u64],
            asts: vec![],
            tags: vec![],
        };
        for a in alt {
            c.asts.push(self.sum(a));
        }
        Ok(c)
    }

    #[inline(never)]
    fn sum(&mut self, c: Concat) -> u64 {
        let mut h = 0x9e3779b97f4a7c15u64;
        for v in c.span.iter().chain(c.asts.iter()) {
            h = h.rotate_left(7) ^ v.wrapping_mul(0x100000001b3);
        }
        for t in &c.tags {
            h = h.rotate_left(3) ^ *t as u64;
        }
        self.out.push(h);
        h
    }

    // The parser loop: `concat` lives across iterations and is moved into
    // a call and rebuilt from its result in three of the arms; literals
    // touch its fields in place.
    #[inline(never)]
    pub fn parse(&mut self) -> Result<Concat, Error> {
        let mut concat = Concat {
            span: vec![0],
            asts: Vec::new(),
            tags: Vec::new(),
        };
        loop {
            let Some(b) = self.bump() else { break };
            match b {
                b'(' => {
                    concat = self.push_group(concat)?;
                }
                b')' => {
                    concat = self.pop_group(concat)?;
                }
                b'|' => {
                    concat = self.push_alternate(concat)?;
                }
                b' ' => {
                    // A read of the carried local between the moves.
                    let n = concat.asts.len() as u64;
                    concat.tags.push(n as u32);
                }
                b'#' => {
                    // A by-value move into a call whose result is not
                    // written back into the local.
                    let probe = concat.clone();
                    let s = self.sum(probe);
                    concat.span.push(s & 0xff);
                }
                c => {
                    concat.asts.push(c as u64);
                }
            }
        }
        while !self.stack.is_empty() {
            concat = self.pop_group(concat)?;
        }
        Ok(concat)
    }
}

// Straight-line shape: an sret result moved into a local, moved into a
// call, with a scalar read in between.
#[inline(never)]
fn make(n: u64) -> Concat {
    Concat {
        span: vec![n; (n % 3) as usize + 1],
        asts: vec![n * 2],
        tags: vec![n as u32],
    }
}

#[inline(never)]
fn consume(mut c: Concat) -> u64 {
    c.span.push(1);
    c.span.iter().sum::<u64>() + c.asts.len() as u64 + c.tags.len() as u64
}

#[inline(never)]
fn straight(n: u64) -> u64 {
    let c = make(n);
    let k = c.asts.len() as u64;
    consume(c) + k
}

fn run(text: &str) -> (u64, Vec<u64>, Result<Concat, u32>) {
    let mut p = Parser {
        pos: 0,
        text: text.as_bytes().to_vec(),
        stack: Vec::new(),
        out: Vec::new(),
    };
    let r = p.parse().map_err(|e| e.0);
    let h = p.out.iter().fold(0u64, |a, v| a.rotate_left(5) ^ v);
    (h, p.out, r)
}

fn main() {
    let inputs = [
        "ab(cd|ef)g",
        "((a)(b c)|d)#(e|f)",
        "a(b(c(d(e(f(g)))))) #x",
        "(((((((((((((((((((((((((((((((((((((((((((((a)))))))))))))))))))))))))))))))))))))))))))))",
        "a)b",
        "|||| a||b# c",
        "",
    ];
    let mut total = 0u64;
    for t in inputs {
        let (h, out, r) = run(black_box(t));
        println!("{t:?}: h={h:#x} n={} r={:?}", out.len(), r.as_ref().map(|c| (c.span.len(), c.asts.len(), c.tags.len())));
        total = total.rotate_left(9)
            ^ h
            ^ r.map(|c| c.asts.iter().fold(0u64, |a, v| a.wrapping_add(*v)))
                .unwrap_or_else(|e| e as u64);
    }
    for n in 0..20u64 {
        total = total.rotate_left(1) ^ straight(black_box(n));
    }
    println!("total={total:#x}");
}
