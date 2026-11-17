// `PLIRON_INLINE_LEANCOST=2`: a small matrix product written the way
// nalgebra writes it (generic storage, slices, `gemm` -> `gemv` -> `axcpy`
// helpers, `from_fn` temporaries) is hundreds of raw IR ops, mostly jumps,
// slots and markers, and a few dozen instructions once simplified. With
// the knob its call in `assemble` is inlined (no `inline` remark for
// `Mat::mul`); without it, it is not. Outputs must match stock rustc.
use std::hint::black_box;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Mat<const R: usize, const C: usize> {
    d: [[f32; R]; C], // column major
}

impl<const R: usize, const C: usize> Mat<R, C> {
    #[inline]
    fn from_fn(mut f: impl FnMut(usize, usize) -> f32) -> Self {
        Mat { d: core::array::from_fn(|c| core::array::from_fn(|r| f(r, c))) }
    }
    #[inline]
    fn col(&self, c: usize) -> &[f32] {
        &self.d[c]
    }
}

#[inline]
fn axcpy(y: &mut [f32], a: f32, x: &[f32], c: f32, beta: f32) {
    assert_eq!(y.len(), x.len());
    if beta != 0.0 {
        for i in 0..y.len() {
            y[i] = a * x[i] * c + beta * y[i];
        }
    } else {
        for i in 0..y.len() {
            y[i] = a * x[i] * c;
        }
    }
}

#[inline]
fn gemv<const R: usize, const K: usize>(y: &mut [f32], alpha: f32, a: &Mat<R, K>, x: &[f32], beta: f32) {
    assert_eq!(x.len(), K);
    axcpy(y, alpha, a.col(0), x[0], beta);
    for k in 1..K {
        axcpy(y, alpha, a.col(k), x[k], 1.0);
    }
}

impl<const R: usize, const K: usize> Mat<R, K> {
    fn mul<const C: usize>(&self, b: &Mat<K, C>) -> Mat<R, C> {
        let mut out = Mat::<R, C>::from_fn(|_, _| 0.0);
        for j in 0..C {
            gemv(&mut out.d[j], 1.0, self, b.col(j), 0.0);
        }
        out
    }
}

#[inline(never)]
fn assemble(js: &[Mat<2, 3>], h: &Mat<3, 3>, scale: f32) -> f32 {
    let mut acc = 0.0f32;
    for q in 0..js.len() {
        let jt = Mat::<3, 2>::from_fn(|r, c| js[q].d[r][c]);
        let hq = h.mul(&jt);
        for p in 0..=q {
            let b = js[p].mul(&hq);
            acc += (b.d[0][0] + b.d[1][1] - b.d[0][1] * b.d[1][0]) * scale;
        }
    }
    acc
}

fn main() {
    let js: Vec<Mat<2, 3>> = (0..3)
        .map(|k| Mat::from_fn(|r, c| (k * 7 + r * 3 + c) as f32 * 0.25 - 1.0))
        .collect();
    let h = Mat::<3, 3>::from_fn(|r, c| if r == c { 2.0 } else { 0.5 * (r + c) as f32 });
    let mut s = 0.0f32;
    for i in 0..1000 {
        s += assemble(black_box(&js), black_box(&h), black_box(1.0 + i as f32 * 1e-3));
    }
    println!("{s:?}");
    println!("{:?}", black_box(js[1]).mul(&black_box(h)));
}
