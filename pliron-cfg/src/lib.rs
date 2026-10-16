//! One-shot parse of the `PLIRON_*` environment.
//!
//! `std::env::var` takes a global lock per call, and passes gate
//! per-function — reads here are HashMap lookups after a single parse.
//! Semantics match the old inline reads exactly: gate vars default on
//! unless `=0`, debug flags are presence-checked, opt-ins compare `=1`.

use rustc_hash::FxHashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct PassConfig {
    /// Every `PLIRON_*` var present in the environment (raw values).
    values: FxHashMap<String, String>,
    /// `PLIRON_PIPELINE=a,b,c` — explicit CLIF-step order/selection.
    pub pipeline: Option<Vec<String>>,
}

pub fn config() -> &'static PassConfig {
    static CONFIG: LazyLock<PassConfig> = LazyLock::new(PassConfig::parse);
    &CONFIG
}

impl PassConfig {
    fn parse() -> Self {
        let mut values = FxHashMap::default();
        for (k, v) in std::env::vars_os() {
            if let Some(k) = k.to_str()
                && k.starts_with("PLIRON_")
            {
                values.insert(k.to_string(), v.to_string_lossy().into_owned());
            }
        }
        let pipeline = values.get("PLIRON_PIPELINE").map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        });
        Self { values, pipeline }
    }

    /// `PLIRON_X` unset or `!= "0"` — the default-on pass gate.
    pub fn enabled(&self, var: &str) -> bool {
        record(var, self.values.get(var).map(String::as_str));
        self.values.get(var).map_or(true, |v| v != "0")
    }

    /// `PLIRON_X` present at all — presence-flag debug knobs.
    pub fn set(&self, var: &str) -> bool {
        record(var, self.values.get(var).map(String::as_str));
        self.values.contains_key(var)
    }

    /// `PLIRON_X == v` — opt-in gates (`=1` and model selects).
    pub fn is(&self, var: &str, v: &str) -> bool {
        record(var, self.values.get(var).map(String::as_str));
        self.values.get(var).is_some_and(|x| x == v)
    }

    pub fn get(&self, var: &str) -> Option<&str> {
        record(var, self.values.get(var).map(String::as_str));
        self.values.get(var).map(String::as_str)
    }

    /// One-line summary for `PLIRON_STATS` headers: explicit pipeline or
    /// the default, plus every gate forced off.
    pub fn describe(&self) -> String {
        let mut off: Vec<&str> = self
            .values
            .iter()
            .filter(|(_, v)| v.as_str() == "0")
            .map(|(k, _)| k.as_str())
            .collect();
        off.sort();
        match &self.pipeline {
            Some(p) => format!("custom [{}], off: {}", p.join(","), off.join(",")),
            None => format!("default, off: {}", off.join(",")),
        }
    }
}

// Cache read tracking: a cached entry's validity depends only on the
// `PLIRON_*` vars the producing run actually consulted, so flipping a
// var invalidates only entries that read it. `READS` is a stack of
// windows: a read lands in every open frame, so an outer (pipeline-
// level) window accumulates the union of its inner (per-step) windows.
// Reads outside a tracking window cost a TLS borrow-check.
thread_local! {
    static READS: std::cell::RefCell<Vec<std::collections::BTreeMap<String, Option<String>>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Open a read-recording frame. Always succeeds — frames nest, and each
/// frame sees only the reads that happen inside it.
pub fn reads_begin() -> bool {
    READS.with(|r| {
        r.borrow_mut().push(Default::default());
        true
    })
}

/// Close the innermost frame and return its `(var, value)` pairs.
pub fn reads_take() -> std::collections::BTreeMap<String, Option<String>> {
    READS.with(|r| r.borrow_mut().pop().unwrap_or_default())
}

fn record(k: &str, v: Option<&str>) {
    // Cache-machinery knobs are inputs to the cache itself, not to the
    // computation — recording them would tie variants to the cache dir
    // or log flag rather than to the work performed.
    if matches!(
        k,
        "PLIRON_STEPCACHE" | "PLIRON_CACHE_DIR" | "PLIRON_CACHE_LOG"
    ) {
        return;
    }
    READS.with(|r| {
        for m in r.borrow_mut().iter_mut() {
            m.insert(k.to_string(), v.map(str::to_string));
        }
    });
}

/// `PLIRON_<PASS>=0` disables a pass at runtime (default on).
pub fn pass_enabled(var: &str) -> bool {
    config().enabled(var)
}

/// Whether any tracing subscriber is live. The `tracing` macros pay a
/// `get_default` TLS access + callsite-registry interest check per
/// invocation even with no dispatcher installed — measured at ~4% of
/// codegen samples under `finish_module`. Guarding per-fn/per-step span
/// construction with this is one relaxed atomic load. `tracing_on()` is
/// true once the backend's `tracing_init` installs its subscriber, or
/// when any caller (e.g. a test harness) installed a dispatcher itself.
static TRACING_ON: AtomicBool = AtomicBool::new(false);

/// Mark that a subscriber was installed. Called from the backend's
/// `tracing_init`; test harnesses that call `fmt::init()`/`set_default`
/// are caught by the `has_been_set` arm below.
pub fn mark_tracing_on() {
    TRACING_ON.store(true, Ordering::Relaxed);
}

#[inline]
pub fn tracing_on() -> bool {
    TRACING_ON.load(Ordering::Relaxed) || tracing::dispatcher::has_been_set()
}

/// `PLIRON_<X>` present — presence-flag debug knobs.
pub fn env_set(var: &str) -> bool {
    config().set(var)
}

/// `PLIRON_<X> == v` — opt-in gates.
pub fn env_is(var: &str, v: &str) -> bool {
    config().is(var, v)
}

/// `PLIRON_<X>` raw value.
pub fn env_get(var: &str) -> Option<&'static str> {
    config().get(var)
}

/// `PLIRON_TRACE_FILE=<path>`: append one timing line per pipeline step (and per
/// `*pipeline*`/`*compile*` CGU summary) the moment it is measured, so a
/// watcher can read live progress instead of waiting for the process to
/// exit. Line: `trace <pid> <step> <ms> <name>`.
pub fn trace(step: &str, ms: f64, name: &str) {
    let Some(path) = env_get("PLIRON_TRACE_FILE") else {
        return;
    };
    use std::io::Write;
    let line = format!("trace {} {step} {ms:.3} {name}\n", std::process::id());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `PLIRON_OPT_BISECT=N`: allow only the first N optimization-pass
/// applications (across all passes and functions), like LLVM's
/// `-opt-bisect-limit`. Each gate call consumes one unit; with no env var
/// the budget is unlimited. `PLIRON_OPT_BISECT_DEBUG=1` logs each gate.
static BISECT_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn bisect(name: &str) -> bool {
    bisect_in(name, "")
}

/// [`bisect`] for a unit inside function `func`, which the debug log names
/// (`bisect <n> <pass> run|skip <func>`) so `harness/bisect.py` can say
/// which function the first bad unit touched.
pub fn bisect_in(name: &str, func: &str) -> bool {
    let Some(limit) = bisect_limit() else {
        return true;
    };
    let n = BISECT_COUNT.fetch_add(1, Ordering::Relaxed);
    let ok = n < limit;
    if env_is("PLIRON_OPT_BISECT_DEBUG", "1")
        && tracing::enabled!(target: "pliron::cfg", tracing::Level::DEBUG)
    {
        let rss = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1).map(|x| x.to_string()))
            .unwrap_or_default();
        tracing::debug!(target: "pliron::cfg", "bisect {} {name} {} {func} rss={}",
            n + 1,
            if ok { "run" } else { "skip" },
            rss
        );
    }
    ok
}

/// The `PLIRON_OPT_BISECT` budget, parsed once. When `Some`, pass
/// applications must run in a deterministic order — the counter numbers
/// each `bisect` call, which is what makes bisection reproducible.
pub fn bisect_limit() -> Option<u64> {
    static LIMIT: LazyLock<Option<u64>> =
        LazyLock::new(|| env_get("PLIRON_OPT_BISECT").and_then(|v| v.parse().ok()));
    *LIMIT
}

/// Current bisect position and a way to replay skipped draws. A
/// whole-pipeline cache hit bypasses the per-step `bisect` calls, so it
/// advances the counter by the recorded amount to keep downstream
/// numbering identical to an uncached run.
pub fn bisect_count() -> u64 {
    BISECT_COUNT.load(Ordering::Relaxed)
}

pub fn bisect_skip(n: u64) {
    BISECT_COUNT.fetch_add(n, Ordering::Relaxed);
}
