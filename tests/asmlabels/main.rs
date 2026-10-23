// rust-lang/rust tests/ui/asm/binary_asm_labels_allowed.rs (build-pass on
// aarch64): `01: bl 01b` was rejected by the vendored assembler as an
// invalid octal literal. Built only — every block branches to itself.


use std::arch::asm;

fn main() {
    unsafe {
        asm!("0: bl 0b");
        asm!("1: bl 1b");
        asm!("10: bl 10b");
        asm!("01: bl 01b");
        asm!("1001101: bl 1001101b");
    }
}
