// `PLIRON_ELABKEEP_C2` (compile-path egraph elaboration): a constant
// needing two or more aarch64 instructions — here the bit-test mask
// `0x0001_0000_3600` (`movz`+`movk`, no logical-immediate encoding) —
// keeps the position the pipeline's licm gave it ahead of the loop.
// Without it, elaboration re-materializes the mask inside the loop
// (`ELABKEEP`'s operand rule breaks on a constant's empty operand list
// and `ELABKEEP_WIDE` covers only three-plus-instruction constants),
// two extra instructions per iteration. The loop has the byte-dispatch
// shape of lol_html's `tag_name_state` (several exits and backedges),
// which is what pops the loop entry before the body is elaborated.
#![crate_type = "bin"]

const MASK: u64 = 0x0001_0000_3600;

#[inline(never)]
#[unsafe(no_mangle)]
pub fn masklex(bytes: &[u8]) -> u64 {
    let mut i = 0usize;
    let mut count = 0u64;
    while i < bytes.len() {
        let b = bytes[i];
        if (MASK >> (b & 63)) & 1 != 0 {
            count += 1;
            i += 1;
            continue;
        }
        match b {
            b'<' => return count,
            b'>' => count += 2,
            b'/' => count += 3,
            0 => return count + 100,
            b'=' => count += 5,
            _ => count += 4,
        }
        i += 1;
    }
    count
}

fn main() {
    let input: Vec<u8> = (0..2000u32).map(|i| (i * 7 + 3) as u8).collect();
    let r = std::hint::black_box(masklex(std::hint::black_box(&input)));
    println!("{r}");
    assert_eq!(r, masklex(&input));
}
