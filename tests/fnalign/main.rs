// `#[rustc_align(N)]` sets a function's minimum alignment — on the
// function and on the `dyn` shims rustc codegens for it
// (codegen_instance_attrs(..).alignment). The backend defined every
// function at Cranelift's code alignment only (rust-lang/rust
// tests/ui/attributes/fn-align-dyn.rs failed under the selfhost UI run).
#![feature(rustc_attrs)]
#![feature(fn_align)]

trait Test {
    #[rustc_align(4096)]
    fn foo(&self);

    #[rustc_align(4096)]
    fn foo1(&self);
}

#[rustc_align(256)]
#[inline(never)]
fn plain(x: u32) -> u32 {
    x.wrapping_mul(3)
}

fn main() {
    assert_eq!((<dyn Test>::foo as fn(_) as usize & !1) % 4096, 0);
    assert_eq!((<dyn Test>::foo1 as fn(_) as usize & !1) % 4096, 0);
    assert_eq!(plain as fn(u32) -> u32 as usize % 256, 0);
    assert_eq!(std::hint::black_box(plain)(7), 21);
    println!("ok");
}
