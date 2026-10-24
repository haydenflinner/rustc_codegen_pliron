//! `asm!` calling C functions by their raw Mach-O names (`bl _malloc`).
//! objmerge strips one `_` from raw asm names on Mach-O and relies on the
//! object writer to add it back, which it only does for Text/Data/Tls
//! symbols: the undefined `_malloc` reference came out as `malloc` and
//! the link failed (rustc tests/ui/allocator/
//! regression-abort-on-free-issue-150898, 2026-10-24).

fn main() {
    let p: usize;
    unsafe {
        core::arch::asm!(
            "mov x0, #18",
            "bl _malloc",
            "mov x21, x0",
            "bl _free",
            out("x21") p,
            clobber_abi("C"),
        )
    }
    assert!(p != 0);
    println!("ok");
}
