//! Shapes the wasm byte peepholes got wrong, for `harness/wasm/peepdiff.mjs`
//! (tests/wasm/smoke.sh compiles this to an object and runs every body
//! before and after `wpeep`; the object is not linked). Each function is
//! the smallest source that put the miscompiled shape into its body.
#![no_std]

#[panic_handler]
fn p(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

/// Store-to-load forwarding across a loop entry (`154456d`): `[b] = v`
/// before the loop, `[b]` loaded at its top, `[c]` stored in its body.
/// `b` and `c` may be the same address, so from the second iteration on
/// the load sees what the body stored. The entry filter kept the
/// `(b, 0) -> v` entry because the store went through another local
/// (rapier2d's `symmetric_eigen`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn peep_loop_alias(b: *mut u32, c: *mut u32, v: u32, n: u32) -> u32 {
    unsafe {
        *b = v;
        let mut acc = 0u32;
        let mut i = n;
        loop {
            acc = acc.wrapping_add(*b);
            *c = acc;
            i = i.wrapping_sub(1);
            if i == 0 {
                break;
            }
        }
        acc ^ v
    }
}

/// The same with a computed value (forwarded through a minted local) and
/// with f32, the type the original iterated on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn peep_loop_alias_computed(b: *mut u32, c: *mut u32, v: u32, n: u32) -> u32 {
    unsafe {
        *b = v.wrapping_mul(7).wrapping_add(n);
        let mut acc = 0u32;
        let mut i = n;
        loop {
            acc = acc.wrapping_add(*b);
            *c = acc;
            i = i.wrapping_sub(1);
            if i == 0 {
                break;
            }
        }
        acc
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn peep_loop_alias_f32(b: *mut f32, c: *mut f32, v: f32, n: u32) -> f32 {
    unsafe {
        *b = v;
        let mut acc = 0.5f32;
        let mut i = n;
        loop {
            acc = acc * 0.5 + *b;
            *c = acc;
            i = i.wrapping_sub(1);
            if i == 0 {
                break;
            }
        }
        acc - v
    }
}

const IV: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];
const MSG_SCHEDULE: [[usize; 16]; 7] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8],
    [3, 4, 10, 12, 13, 2, 7, 14, 6, 5, 9, 0, 11, 15, 8, 1],
    [10, 7, 12, 9, 14, 3, 13, 15, 4, 0, 11, 2, 5, 8, 1, 6],
    [12, 13, 9, 11, 15, 10, 14, 8, 7, 2, 5, 3, 0, 1, 6, 4],
    [9, 14, 11, 5, 8, 12, 15, 1, 13, 3, 0, 10, 2, 6, 4, 7],
    [11, 15, 5, 0, 1, 9, 8, 6, 14, 10, 2, 12, 3, 4, 7, 13],
];

#[inline(always)]
fn g(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(x);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(y);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

#[inline(always)]
fn round(state: &mut [u32; 16], msg: &[u32; 16], round: usize) {
    let schedule = MSG_SCHEDULE[round];
    g(state, 0, 4, 8, 12, msg[schedule[0]], msg[schedule[1]]);
    g(state, 1, 5, 9, 13, msg[schedule[2]], msg[schedule[3]]);
    g(state, 2, 6, 10, 14, msg[schedule[4]], msg[schedule[5]]);
    g(state, 3, 7, 11, 15, msg[schedule[6]], msg[schedule[7]]);
    g(state, 0, 5, 10, 15, msg[schedule[8]], msg[schedule[9]]);
    g(state, 1, 6, 11, 12, msg[schedule[10]], msg[schedule[11]]);
    g(state, 2, 7, 8, 13, msg[schedule[12]], msg[schedule[13]]);
    g(state, 3, 4, 9, 14, msg[schedule[14]], msg[schedule[15]]);
}

#[inline(always)]
fn word(block: &[u8; 64], i: usize) -> u32 {
    u32::from_le_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]])
}

#[inline(always)]
fn compress_pre(cv: &[u32; 8], block: &[u8; 64], block_len: u8, counter: u64, flags: u8) -> [u32; 16] {
    let msg = [
        word(block, 0), word(block, 1), word(block, 2), word(block, 3), word(block, 4), word(block, 5),
        word(block, 6), word(block, 7), word(block, 8), word(block, 9), word(block, 10), word(block, 11),
        word(block, 12), word(block, 13), word(block, 14), word(block, 15),
    ];
    let mut state = [
        cv[0], cv[1], cv[2], cv[3], cv[4], cv[5], cv[6], cv[7], IV[0], IV[1], IV[2], IV[3],
        counter as u32, (counter >> 32) as u32, block_len as u32, flags as u32,
    ];
    round(&mut state, &msg, 0);
    round(&mut state, &msg, 1);
    round(&mut state, &msg, 2);
    round(&mut state, &msg, 3);
    round(&mut state, &msg, 4);
    round(&mut state, &msg, 5);
    round(&mut state, &msg, 6);
    state
}

/// blake3's portable `compress_in_place`: seven rounds over sixteen state
/// words kept in locals. Keep-on-stack hoists a window's `local.get`s
/// below the producer; a later rewrite of `set t` then saw no get of `t`
/// left in the list and deleted a write the hoisted get still read
/// (`b60ceb0`, `blake3::hash(b"abc")` wrong).
#[unsafe(no_mangle)]
pub extern "C" fn peep_compress(cv: &mut [u32; 8], block: &[u8; 64], block_len: u8, counter: u64, flags: u8) {
    let state = compress_pre(cv, block, block_len, counter, flags);
    cv[0] = state[0] ^ state[8];
    cv[1] = state[1] ^ state[9];
    cv[2] = state[2] ^ state[10];
    cv[3] = state[3] ^ state[11];
    cv[4] = state[4] ^ state[12];
    cv[5] = state[5] ^ state[13];
    cv[6] = state[6] ^ state[14];
    cv[7] = state[7] ^ state[15];
}
