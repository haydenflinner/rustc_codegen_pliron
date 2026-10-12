//! One-shot parse of the `PLIRON_*` environment.
//!
//! `std::env::var` takes a global lock per call, and passes gate
//! per-function — reads here are HashMap lookups after a single parse.
//! Semantics match the old inline reads exactly: gate vars default on
//! unless `=0`, debug flags are presence-checked, opt-ins compare `=1`.

use rustc_hash::FxHashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

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
        self.values.get(var).map_or(true, |v| v != "0")
    }

    /// `PLIRON_X` present at all — presence-flag debug knobs.
    pub fn set(&self, var: &str) -> bool {
        self.values.contains_key(var)
    }

    /// `PLIRON_X == v` — opt-in gates (`=1` and model selects).
    pub fn is(&self, var: &str, v: &str) -> bool {
        self.values.get(var).is_some_and(|x| x == v)
    }

    pub fn get(&self, var: &str) -> Option<&str> {
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

/// `PLIRON_<PASS>=0` disables a pass at runtime (default on).
pub fn pass_enabled(var: &str) -> bool {
    config().enabled(var)
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

/// `PLIRON_OPT_BISECT=N`: allow only the first N optimization-pass
/// applications (across all passes and functions), like LLVM's
/// `-opt-bisect-limit`. Each gate call consumes one unit; with no env var
/// the budget is unlimited. `PLIRON_OPT_BISECT_DEBUG=1` logs each gate.
pub fn bisect(name: &str) -> bool {
    bisect_in(name, "")
}

/// [`bisect`] for a unit inside function `func`, which the debug log names
/// (`bisect <n> <pass> run|skip <func>`) so `harness/bisect.py` can say
/// which function the first bad unit touched.
pub fn bisect_in(name: &str, func: &str) -> bool {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let Some(limit) = bisect_limit() else { return true };
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let ok = n < limit;
    if env_is("PLIRON_OPT_BISECT_DEBUG", "1") {
        eprintln!("bisect {} {name} {} {func}", n + 1, if ok { "run" } else { "skip" });
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
