// A derived `Clone` of a recursive AST is a call-graph cycle of many
// small functions (`Expr::clone`, `Kind::clone`, `to_vec::<Expr>`,
// `to_vec::<Stmt>`, `Box<Expr>::clone`, ...). A caller outside the cycle
// that clones a slice enters it once; under PLIRON_INLINE_RECX the sites
// cloned out of that first inline do not inline further members of the
// cycle (dsa's `Matrix::defaulted` took 317 of them, 732 bytes under LLVM
// against 721 KB). The output must not change.
use std::hint::black_box;

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub id: u32,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Lit(u64),
    Name(String),
    Unary(Box<Expr>),
    Binary(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Arg>),
    Block(Vec<Stmt>),
    Match(Box<Expr>, Vec<Case>),
    Tuple(Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Let(String, Expr),
    Expr(Expr),
    Return(Option<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    pub pats: Vec<Expr>,
    pub body: Expr,
}

fn build(depth: u32, seed: &mut u64) -> Expr {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let pick = (*seed >> 33) % 8;
    let id = (*seed >> 20) as u32 & 0xffff;
    let kind = if depth == 0 {
        if pick & 1 == 0 { Kind::Lit(*seed >> 40) } else { Kind::Name(format!("n{}", pick)) }
    } else {
        match pick {
            0 => Kind::Lit(*seed >> 40),
            1 => Kind::Name(format!("v{}", depth)),
            2 => Kind::Unary(Box::new(build(depth - 1, seed))),
            3 => Kind::Binary(Box::new(build(depth - 1, seed)), Box::new(build(depth - 1, seed))),
            4 => Kind::Call(
                Box::new(build(depth - 1, seed)),
                vec![Arg { name: None, value: build(depth - 1, seed) }, Arg { name: Some("k".into()), value: build(0, seed) }],
            ),
            5 => Kind::Block(vec![
                Stmt::Let("x".into(), build(depth - 1, seed)),
                Stmt::Expr(build(depth - 1, seed)),
                Stmt::Return(Some(build(0, seed))),
            ]),
            6 => Kind::Match(
                Box::new(build(depth - 1, seed)),
                vec![Case { pats: vec![build(0, seed)], body: build(depth - 1, seed) }],
            ),
            _ => Kind::Tuple(vec![build(depth - 1, seed), build(depth - 1, seed)]),
        }
    };
    Expr { id, kind }
}

fn weight(e: &Expr) -> u64 {
    let k = match &e.kind {
        Kind::Lit(v) => *v & 0xff,
        Kind::Name(s) => s.len() as u64,
        Kind::Unary(a) => 1 + weight(a),
        Kind::Binary(a, b) => 2 + weight(a) + 3 * weight(b),
        Kind::Call(f, args) => weight(f) + args.iter().map(|a| weight(&a.value) + a.name.is_some() as u64).sum::<u64>(),
        Kind::Block(ss) => ss
            .iter()
            .map(|s| match s {
                Stmt::Let(n, e) => n.len() as u64 + weight(e),
                Stmt::Expr(e) => weight(e),
                Stmt::Return(e) => e.as_ref().map_or(7, weight),
            })
            .sum(),
        Kind::Match(s, cs) => weight(s) + cs.iter().map(|c| c.pats.iter().map(weight).sum::<u64>() + weight(&c.body)).sum::<u64>(),
        Kind::Tuple(xs) => xs.iter().map(weight).sum(),
    };
    k.wrapping_mul(31).wrapping_add(e.id as u64)
}

/// Outside the cycle: keeps the rows that start with a literal, dropping
/// the first column (the shape of `Matrix::defaulted`).
#[inline(never)]
pub fn defaulted(rows: &[Vec<Expr>]) -> Vec<Vec<Expr>> {
    let mut out = Vec::new();
    for row in rows {
        let Some((first, rest)) = row.split_first() else { continue };
        if let Kind::Lit(_) | Kind::Name(_) = first.kind {
            out.push(rest.to_vec());
        }
    }
    out
}

fn main() {
    let mut seed = 0x5eed_u64;
    let rows: Vec<Vec<Expr>> = (0..40)
        .map(|r| (0..(r % 4) + 1).map(|c| build(if c == 0 { r % 2 } else { 4 }, &mut seed)).collect())
        .collect();
    let d = defaulted(black_box(&rows));
    let kept: usize = d.iter().map(|r| r.len()).sum();
    let w = d.iter().flat_map(|r| r.iter()).fold(0u64, |a, e| a.wrapping_mul(1099511628211) ^ weight(e));
    let same = d.iter().zip(rows.iter().filter(|r| matches!(r[0].kind, Kind::Lit(_) | Kind::Name(_)))).all(|(a, b)| a[..] == b[1..]);
    println!("{} rows, {} exprs, weight {:016x}, equal to the source: {}", d.len(), kept, w, same);
}
