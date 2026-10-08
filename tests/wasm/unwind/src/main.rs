//! panic=unwind on wasm32-wasip1 via emulated EH (`__pliron_eh` flag+exn).
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

fn check(name: &str, ok: bool) {
    println!("{} {name}", if ok { "ok  " } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }
}

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        println!("drop guard");
    }
}

fn deep(n: u32) -> u32 {
    let _g = Guard;
    if n == 0 {
        panic!("boom {n}");
    }
    deep(n - 1) + 1
}

fn may_throw(flag: bool) -> Result<u32, Box<dyn std::any::Any + Send>> {
    catch_unwind(AssertUnwindSafe(|| if flag { deep(3) } else { 7 }))
}

fn main() {
    check("no panic returns ok", may_throw(false).ok() == Some(7));
    let r = may_throw(true);
    check("deep panic caught", r.is_err());
    let e = r.unwrap_err();
    check("payload", e.downcast_ref::<String>().map(|s| s.as_str()) == Some("boom 0"));

    // panic message + drop order through several frames
    let r = catch_unwind(|| {
        let _g1 = Guard;
        deep(2)
    });
    check("nested drop+panic", r.is_err());

    // resume_unwind propagates to the next handler / top
    let r = catch_unwind(|| {
        let r = catch_unwind(|| panic!("inner"));
        resume_unwind(r.unwrap_err());
    });
    check("resume_unwind", r.unwrap_err().downcast_ref::<&str>() == Some(&"inner"));

    // panic payload boxing: downcast<String>
    let r = catch_unwind(|| panic!("{}", 42));
    check("int payload", r.unwrap_err().downcast_ref::<String>().map(|s| s.as_str()) == Some("42"));

    println!("unwind test done");
}
