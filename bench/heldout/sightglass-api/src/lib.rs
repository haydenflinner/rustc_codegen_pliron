//! Stand-in for the `sightglass-api` crate. The real one imports
//! `bench.start`/`bench.end` from the sightglass wasm host; this one times
//! the region with `Instant` and prints it to stderr, so the benchmark runs
//! as a native binary (or under any WASI runtime).

use std::sync::Mutex;
use std::time::Instant;

static START: Mutex<Option<Instant>> = Mutex::new(None);

/// Start the measured region.
pub fn start() {
    *START.lock().unwrap() = Some(Instant::now());
}

/// End the measured region and report it as `heldout-ns <nanoseconds>`.
pub fn end() {
    let t = START.lock().unwrap().take().expect("bench::end without start");
    eprintln!("heldout-ns {}", t.elapsed().as_nanos());
}
