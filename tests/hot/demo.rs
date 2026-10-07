use std::sync::atomic::{AtomicU32, Ordering};

static TICKS: AtomicU32 = AtomicU32::new(0);

#[inline(never)]
fn value() -> u32 {
    1
}

fn main() {
    pliron_hot::start();
    for _ in 0..500 {
        let t = TICKS.fetch_add(1, Ordering::Relaxed);
        let v = value();
        println!("tick {t} value {v}");
        if v != 1 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::process::exit(1);
}
