//! `PLIRON_NORETUDF=1`: the `NORETURN_CALL` trap ending a function drops
//! too — unwind lookups use `ret_addr - 1`, which still lands on the
//! call. Own test binary: the flag caches in a `OnceLock` at first
//! emission, so the env var must be set before any other test compiles.

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::TrapCode;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, isa};

#[test]
// r[verify clif.vendor.noretudf]
fn noreturn_trap_dropped_at_function_end() {
    // Single-threaded single-test binary: no other thread reads env.
    unsafe { std::env::set_var("PLIRON_NORETUDF", "1") };
    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    let isa = isa::lookup(target_lexicon::Triple::host())
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap();
    let func = cranelift_reader::parse_functions(
        "function %f(i32) {
    sig0 = (i32)
    fn0 = %die sig0
block0(v0: i32):
    call fn0(v0)
    trap user250
}",
    )
    .unwrap()
    .remove(0);
    let mut ctx = Context::for_function(func);
    let code = ctx
        .compile(&*isa, &mut ControlPlane::default())
        .map_err(|e| format!("{e:?}"))
        .unwrap();
    let traps: Vec<_> = code.buffer.traps().iter().map(|t| t.code).collect();
    assert_eq!(
        traps.iter().filter(|&&c| c == TrapCode::NORETURN_CALL).count(),
        0,
        "{traps:?}"
    );
}
