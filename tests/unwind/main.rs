use std::panic;

struct D(&'static str);
impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

#[inline(never)]
fn boom(n: u32) -> u32 {
    let _d = D("inner");
    if n > 2 {
        panic!("boom {n}");
    }
    n
}

fn main() {
    panic::set_hook(Box::new(|i| {
        println!(
            "hook: {}",
            i.payload()
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .unwrap_or("?")
        )
    }));
    let r = panic::catch_unwind(|| {
        let _o = D("outer");
        boom(std::hint::black_box(5))
    });
    println!("caught = {}", r.is_err());
    let msg = r.unwrap_err().downcast::<String>().unwrap();
    println!("payload = {msg}");
    println!("ok = {:?}", panic::catch_unwind(|| boom(1)));
}
