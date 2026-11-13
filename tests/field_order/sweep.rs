// Why does packing the hot fields make an out-of-cache sequential sweep
// slower (docs/pgo.md "Field order")? A layout-only experiment: elements
// are `stride` bytes of a flat u64 buffer, the four hot words sit at the
// given offsets, and the same loop runs over
//   packed     hot words at 0, 8, 16, 24
//   scattered  hot words spread evenly over the element
//   packed + software prefetch N elements ahead
//   packed + one cold read per 64 bytes of the element (touch every line)
// Usage: sweep [megabytes per array, default 256] [repetitions, default 5]
// Plain rustc is enough (no backend involved): rustc -O sweep.rs
use std::hint::black_box;
use std::time::Instant;

#[inline(always)]
fn prefetch(p: *const u64) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{p}]", p = in(reg) p, options(nostack, preserves_flags, readonly));
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!("prefetcht0 [{p}]", p = in(reg) p, options(nostack, preserves_flags, readonly));
    }
    let _ = p;
}

/// The hot loop of tests/field_order/cache.rs `sweep`, offsets in words.
/// `pf`: prefetch that many elements ahead (0 = none). `touch`: also read
/// one word per 64 bytes of the element.
#[inline(never)]
fn sweep(buf: &mut [u64], words: usize, o: [usize; 4], pf: usize, touch: bool) -> f64 {
    let n = buf.len() / words;
    let base = buf.as_mut_ptr();
    let mut live = 0u64;
    let mut e = 0.0;
    let mut cold = 0u64;
    for i in 0..n {
        unsafe {
            let p = base.add(i * words);
            if pf != 0 {
                prefetch(p.wrapping_add(pf * words));
            }
            let x = f64::from_bits(*p.add(o[0])) + f64::from_bits(*p.add(o[1])) * f64::from_bits(*p.add(o[2]));
            *p.add(o[0]) = x.to_bits();
            live += *p.add(o[3]);
            e += x;
            if touch {
                let mut w = 0;
                while w < words {
                    cold = cold.wrapping_add(std::ptr::read_volatile(p.add(w)));
                    w += 8;
                }
            }
        }
    }
    black_box(cold);
    e + live as f64
}

fn fill(buf: &mut [u64], words: usize, o: [usize; 4]) {
    let mut s = 0x9e3779b97f4a7c15u64;
    for (i, el) in buf.chunks_exact_mut(words).enumerate() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        for w in el.iter_mut() {
            *w = s;
        }
        el[o[0]] = (i as f64).to_bits();
        el[o[1]] = ((s % 1000) as f64 / 1000.0).to_bits();
        el[o[2]] = (1.0 + (s % 7) as f64).to_bits();
        el[o[3]] = s & 1;
    }
}

/// Lines of `line` bytes: (distinct lines the hot words touch per
/// element, share of all the array's lines that are touched, longest run
/// of untouched lines), over a long aligned array.
fn lines(stride: usize, o: [usize; 4], line: usize) -> (f64, f64, usize) {
    let n = 4096;
    let total = n * stride / line;
    let mut hit = vec![false; total + 1];
    let mut per = 0usize;
    for i in 0..n {
        let mut mine: Vec<usize> = o.iter().map(|w| (i * stride + w * 8) / line).collect();
        mine.dedup();
        per += mine.len();
        for l in mine {
            hit[l] = true;
        }
    }
    let touched = hit[..total].iter().filter(|&&h| h).count();
    let (mut gap, mut cur) = (0, 0);
    for &h in &hit[..total] {
        cur = if h { 0 } else { cur + 1 };
        gap = gap.max(cur);
    }
    (per as f64 / n as f64, touched as f64 / total as f64, gap)
}

fn time(buf: &mut [u64], words: usize, o: [usize; 4], pf: usize, touch: bool, reps: usize) -> (f64, f64) {
    let n = buf.len() / words;
    let mut ts = Vec::new();
    let mut sum = 0.0;
    for _ in 0..reps {
        let t = Instant::now();
        sum += sweep(buf, words, o, pf, touch);
        ts.push(t.elapsed().as_nanos() as f64 / n as f64);
    }
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Best of `reps`: the machine is rarely quiet, the minimum is.
    (ts[0], sum)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mb: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(256);
    let reps: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(5);
    println!("lines touched by the four hot words: per element / share of all lines / longest untouched run");
    println!("{:>6} {:>9}  {:>22}  {:>22}", "stride", "layout", "64-byte lines", "128-byte lines");
    let layouts = |words: usize| {
        let packed = [0usize, 1, 2, 3];
        let w = words - 1;
        let scat = [0, w / 3, 2 * w / 3, w];
        [("packed", packed), ("scattered", scat)]
    };
    let strides = [64usize, 128, 160, 192, 256, 320];
    for &stride in &strides {
        for (name, o) in layouts(stride / 8) {
            let (a, b) = (lines(stride, o, 64), lines(stride, o, 128));
            println!(
                "{stride:>6} {name:>9}  {:>5.2} / {:>4.0}% / {:>2}       {:>5.2} / {:>4.0}% / {:>2}",
                a.0, a.1 * 100.0, a.2, b.0, b.1 * 100.0, b.2
            );
        }
    }
    println!();
    println!("ns/element, {mb} MB per array, best of {reps}");
    println!(
        "{:>6} {:>8} {:>9} {:>9} | {:>7} {:>7} {:>7} {:>7} | {:>9}",
        "stride", "elements", "scattered", "packed", "pf 8", "pf 32", "pf 64", "pf 128", "touch all"
    );
    for &stride in &strides {
        let words = stride / 8;
        let n = black_box(mb * 1024 * 1024 / stride);
        let mut buf = vec![0u64; n * words];
        let [(_, packed), (_, scat)] = layouts(words);
        let mut check = Vec::new();
        fill(&mut buf, words, scat);
        let (ts, c) = time(&mut buf, words, scat, 0, false, reps);
        check.push(c);
        fill(&mut buf, words, packed);
        let (tp, c) = time(&mut buf, words, packed, 0, false, reps);
        check.push(c);
        let mut pf = Vec::new();
        for d in [8, 32, 64, 128] {
            fill(&mut buf, words, packed);
            let (t, c) = time(&mut buf, words, packed, d, false, reps);
            pf.push(t);
            check.push(c);
        }
        fill(&mut buf, words, packed);
        let (tt, c) = time(&mut buf, words, packed, 0, true, reps);
        check.push(c);
        assert!(check.iter().all(|&c| c == check[0]), "checksums differ: {check:?}");
        println!(
            "{stride:>6} {n:>8} {ts:>9.2} {tp:>9.2} | {:>7.2} {:>7.2} {:>7.2} {:>7.2} | {tt:>9.2}",
            pf[0], pf[1], pf[2], pf[3]
        );
    }
}
