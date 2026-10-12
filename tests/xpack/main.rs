// xpack pairs isomorphic scalar insts in one block into two-lane vector
// ops. Two independent `sqrt(x)+sqrt(y)` halves share the fadd root's
// operand columns, so the sqrts pack into `sqrtpd` and their results are
// extracted for the scalar adds.
#[inline(never)]
fn quad_roots(p: &[f64; 4]) -> f64 {
    p[0].sqrt() + p[1].sqrt() + p[2].sqrt() + p[3].sqrt()
}

#[inline(never)]
fn quad_norm(p: &[f64; 4], d: f64) -> f64 {
    p[0].sqrt() / d + p[1].sqrt() / d + p[2].sqrt() / d + p[3].sqrt() / d
}

fn main() {
    let p = [4.0f64, 9.0, 16.0, 25.0];
    let s = quad_roots(&p);
    let n = quad_norm(&p, 2.0);
    assert_eq!(s, 14.0);
    assert_eq!(n, 7.0);
    println!("{s} {n}");
}
