// r[verify native.hot.limits]
// Hot-reload layout guard: `tests/hot/run.sh` edits `State` after one
// body-only patch; the second patch must be refused.
use std::sync::Mutex;

struct State { n: u64 }

static STATE: Mutex<State> = Mutex::new(State { n: 7 });

#[inline(never)]
fn value() -> u64 {
    STATE.lock().unwrap().n
}

fn main() {
    pliron_hot::start();
    for _ in 0..60 {
        println!("value {}", value());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
