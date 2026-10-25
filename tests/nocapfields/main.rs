// PLIRON_NOCAP_FIELDS: nocapture reads an `extract_value` as the value
// inserted at that index, not the whole aggregate. test.sh checks the
// facts PLIRON_FACTS=fx_ prints against the comments here.

/// `Enumerate::next` yields `{i64 index, ptr}`; the index is an integer
/// that never held the pointer, so storing `seed * (i + 1)` doesn't
/// capture `p`: [0] (integer params never count as uncaptured).
#[inline(never)]
fn fx_enum(p: &mut [u64; 8], seed: u64) {
    for (i, w) in p.iter_mut().enumerate() {
        *w = seed.wrapping_mul(i as u64 + 1);
    }
}

/// The pointer field of the same pair is stored: `p` is captured, `out`
/// is not: [1].
#[inline(never)]
fn fx_enum_leak(p: &mut [u64; 8], out: &mut *mut u64) {
    for (i, w) in p.iter_mut().enumerate() {
        if i == 3 {
            *out = w;
        }
    }
}

/// Reads two words through `p`; returns them as an `{i64, i64}` pair.
#[inline(never)]
fn fx_layout(p: &[u64; 8]) -> (usize, usize) {
    (p[0] as usize, p[1] as usize)
}

/// Hands its integer args to an opaque sink: they're "captured".
#[inline(never)]
fn fx_report(a: usize, b: usize) {
    std::hint::black_box((a, b));
}

/// SmallVec::extend's shape: `p` goes to a call returning an all-integer
/// pair, and those integers go on to a capturing callee. An all-int
/// struct can't hold `p`, so `p` stays uncaptured: [0].
#[inline(never)]
fn fx_pair(p: &[u64; 8]) {
    let (a, b) = fx_layout(p);
    fx_report(a, b);
}

pub struct Sv {
    len: usize,
    cap: usize,
    heap: *mut u64,
    inline: [u64; 4],
}

/// SmallVec::try_grow's shape: the buffer is the heap block or `s`'s own
/// inline array; the spilled path reallocs it, and the new block is
/// stored back into `s`. The allocator shims keep no argument and return
/// a fresh block, so `s` stays uncaptured: [0].
#[inline(never)]
fn fx_grow(s: &mut Sv) {
    use std::alloc::{Layout, alloc, realloc};
    let spilled = s.cap > 4;
    let ptr = if spilled { s.heap } else { s.inline.as_mut_ptr() };
    let new_cap = s.cap * 2;
    let layout = Layout::array::<u64>(new_cap).unwrap();
    // SAFETY: `ptr` is the heap block when spilled; sizes match the layouts.
    let p = unsafe {
        if spilled {
            realloc(ptr as *mut u8, Layout::array::<u64>(s.cap).unwrap(), layout.size())
        } else {
            let p = alloc(layout);
            std::ptr::copy_nonoverlapping(ptr as *const u8, p, s.len * 8);
            p
        }
    } as *mut u64;
    s.heap = p;
    s.cap = new_cap;
}

fn main() {
    let mut a = [0u64; 8];
    fx_enum(&mut a, std::hint::black_box(3));
    let mut q: *mut u64 = std::ptr::null_mut();
    fx_enum_leak(&mut a, &mut q);
    // SAFETY: q points into a, which is still live.
    unsafe { *q += 1 };
    fx_pair(&a);
    let mut sv = Sv { len: 4, cap: 4, heap: std::ptr::null_mut(), inline: [1, 2, 3, 4] };
    fx_grow(&mut sv);
    fx_grow(&mut sv);
    // SAFETY: fx_grow copied the 4 inline words into the heap block.
    a[0] += unsafe { std::slice::from_raw_parts(sv.heap, 4) }.iter().sum::<u64>() + sv.cap as u64;
    println!("{}", a.iter().sum::<u64>());
}
