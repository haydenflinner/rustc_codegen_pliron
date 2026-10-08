use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

/// Emulated EH on wasm32-unknown-unknown (llvm.wasm.throw path).
/// Returns a bitmask of passed checks; 15 = all pass.
#[unsafe(no_mangle)]
pub extern "C" fn eh_test() -> i32 {
    let mut ok = 0;
    // 1: catch_unwind catches; &str payload survives.
    let r = catch_unwind(|| panic!("deep"));
    if r.is_err() && r.unwrap_err().downcast_ref::<&'static str>() == Some(&"deep") {
        ok |= 1;
    }
    // 2: non-panicking closure returns Ok.
    if matches!(catch_unwind(|| 7u32), Ok(7)) {
        ok |= 2;
    }
    // 3: drop guard runs during unwind.
    struct D(*mut i32);
    impl Drop for D {
        fn drop(&mut self) {
            unsafe { *self.0 += 1 }
        }
    }
    let dropped = Box::into_raw(Box::new(0i32));
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _g = D(dropped);
        panic!("x")
    }));
    unsafe {
        if *dropped == 1 {
            ok |= 4;
        }
        drop(Box::from_raw(dropped));
    }
    // 4: resume_unwind rethrows to the outer handler.
    let r = catch_unwind(|| {
        let p = catch_unwind(|| panic!("inner")).unwrap_err();
        resume_unwind(p);
    })
    .unwrap_err();
    if r.downcast_ref::<&'static str>() == Some(&"inner") {
        ok |= 8;
    }
    ok
}
