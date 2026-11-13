// NEON pairwise ops and lane-narrowing (`PLIRON_NEON_PAIRWISE`,
// `PLIRON_VTRUNC_UZP`): the inline lowerings against a scalar model, on
// pseudo-random vectors. Prints one line per op; the output must equal
// stock rustc's.
#[cfg(target_arch = "aarch64")]
mod t {
    use std::arch::aarch64::*;
    use std::hint::black_box;

    fn rng(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }
    fn bytes(s: &mut u64) -> [u8; 16] {
        let (a, b) = (rng(s), rng(s));
        let mut o = [0u8; 16];
        o[..8].copy_from_slice(&a.to_le_bytes());
        o[8..].copy_from_slice(&b.to_le_bytes());
        o
    }

    macro_rules! pair {
        ($name:ident, $lane:ty, $n:expr, $load:ident, $store:ident, $op:ident, $f:expr) => {
            #[inline(never)]
            fn $name(a: [u8; 16], b: [u8; 16]) -> ([u8; 16], [u8; 16]) {
                let la: [$lane; $n] = unsafe { std::mem::transmute(a) };
                let lb: [$lane; $n] = unsafe { std::mem::transmute(b) };
                let mut want = [0 as $lane; $n];
                let f: fn($lane, $lane) -> $lane = $f;
                for i in 0..$n / 2 {
                    want[i] = f(la[2 * i], la[2 * i + 1]);
                    want[$n / 2 + i] = f(lb[2 * i], lb[2 * i + 1]);
                }
                let mut got = [0 as $lane; $n];
                unsafe {
                    let r = $op($load(black_box(la).as_ptr()), $load(black_box(lb).as_ptr()));
                    $store(got.as_mut_ptr(), r);
                }
                unsafe { (std::mem::transmute(got), std::mem::transmute(want)) }
            }
        };
    }
    pair!(umaxp8, u8, 16, vld1q_u8, vst1q_u8, vpmaxq_u8, |x, y| x.max(y));
    pair!(uminp8, u8, 16, vld1q_u8, vst1q_u8, vpminq_u8, |x, y| x.min(y));
    pair!(smaxp8, i8, 16, vld1q_s8, vst1q_s8, vpmaxq_s8, |x, y| x.max(y));
    pair!(sminp8, i8, 16, vld1q_s8, vst1q_s8, vpminq_s8, |x, y| x.min(y));
    pair!(addp8, u8, 16, vld1q_u8, vst1q_u8, vpaddq_u8, |x, y| x.wrapping_add(y));
    pair!(umaxp16, u16, 8, vld1q_u16, vst1q_u16, vpmaxq_u16, |x, y| x.max(y));
    pair!(sminp16, i16, 8, vld1q_s16, vst1q_s16, vpminq_s16, |x, y| x.min(y));
    pair!(addp16, u16, 8, vld1q_u16, vst1q_u16, vpaddq_u16, |x, y| x.wrapping_add(y));
    pair!(umaxp32, u32, 4, vld1q_u32, vst1q_u32, vpmaxq_u32, |x, y| x.max(y));
    pair!(smaxp32, i32, 4, vld1q_s32, vst1q_s32, vpmaxq_s32, |x, y| x.max(y));
    pair!(addp32, u32, 4, vld1q_u32, vst1q_u32, vpaddq_u32, |x, y| x.wrapping_add(y));

    // memchr's match mask: `vshrn_n_u16(x, 4)` keeps the low byte of each
    // shifted lane; `vmovn_*` is the plain narrowing.
    #[inline(never)]
    fn shrn(a: [u8; 16]) -> (u64, u64) {
        let l: [u16; 8] = unsafe { std::mem::transmute(a) };
        let mut want = [0u8; 8];
        for i in 0..8 {
            want[i] = (l[i] >> 4) as u8;
        }
        let got = unsafe { vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vld1q_u16(black_box(l).as_ptr()), 4)), 0) };
        (got, u64::from_le_bytes(want))
    }
    #[inline(never)]
    fn movn32(a: [u8; 16]) -> (u64, u64) {
        let l: [u32; 4] = unsafe { std::mem::transmute(a) };
        let mut want = [0u16; 4];
        for i in 0..4 {
            want[i] = l[i] as u16;
        }
        let got = unsafe { vget_lane_u64(vreinterpret_u64_u16(vmovn_u32(vld1q_u32(black_box(l).as_ptr()))), 0) };
        (got, unsafe { std::mem::transmute(want) })
    }
    #[inline(never)]
    fn movn64(a: [u8; 16]) -> (u64, u64) {
        let l: [u64; 2] = unsafe { std::mem::transmute(a) };
        let want = [l[0] as u32, l[1] as u32];
        let got = unsafe { vget_lane_u64(vreinterpret_u64_u32(vmovn_u64(vld1q_u64(black_box(l).as_ptr()))), 0) };
        (got, unsafe { std::mem::transmute(want) })
    }

    pub fn run() {
        let fs: [(&str, fn([u8; 16], [u8; 16]) -> ([u8; 16], [u8; 16])); 11] = [
            ("umaxp8", umaxp8),
            ("uminp8", uminp8),
            ("smaxp8", smaxp8),
            ("sminp8", sminp8),
            ("addp8", addp8),
            ("umaxp16", umaxp16),
            ("sminp16", sminp16),
            ("addp16", addp16),
            ("umaxp32", umaxp32),
            ("smaxp32", smaxp32),
            ("addp32", addp32),
        ];
        for (name, f) in fs {
            let (mut s, mut bad, mut sum) = (0x9E3779B97F4A7C15u64, 0u32, 0u64);
            for _ in 0..2000 {
                let (a, b) = (bytes(&mut s), bytes(&mut s));
                let (got, want) = f(a, b);
                bad += (got != want) as u32;
                sum = sum.rotate_left(5) ^ u64::from_le_bytes(got[..8].try_into().unwrap()) ^ u64::from_le_bytes(got[8..].try_into().unwrap());
            }
            println!("{name}: mismatches {bad} sum {sum:016x}");
        }
        let gs: [(&str, fn([u8; 16]) -> (u64, u64)); 3] = [("shrn", shrn), ("movn32", movn32), ("movn64", movn64)];
        for (name, g) in gs {
            let (mut s, mut bad, mut sum) = (0xD1B54A32D192ED03u64, 0u32, 0u64);
            for _ in 0..2000 {
                let (got, want) = g(bytes(&mut s));
                bad += (got != want) as u32;
                sum = sum.rotate_left(7) ^ got;
            }
            println!("{name}: mismatches {bad} sum {sum:016x}");
        }
    }
}

fn main() {
    #[cfg(target_arch = "aarch64")]
    t::run();
    #[cfg(not(target_arch = "aarch64"))]
    println!("neonpair: aarch64 only");
}
