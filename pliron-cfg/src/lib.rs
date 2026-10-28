//! One-shot parse of the `PLIRON_*` environment.
//!
//! `std::env::var` takes a global lock per call, and passes gate
//! per-function — reads here are HashMap lookups after a single parse.
//! Semantics match the old inline reads exactly: gate vars default on
//! unless `=0`, debug flags are presence-checked, opt-ins compare `=1`.

pub mod jearena;

use rustc_hash::FxHashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct PassConfig {
    /// Every `PLIRON_*` var present in the environment (raw values).
    values: FxHashMap<String, String>,
    /// `PLIRON_PIPELINE=a,b,c` — explicit CLIF-step order/selection.
    pub pipeline: Option<Vec<String>>,
}

/// Opt-in flags that are on by default: an unset variable reads as `1`.
/// `PLIRON_X=0` (any value but `1`) turns one off. Applied once, when the
/// config is parsed, so every `env_is(X, "1")` read site sees the same
/// value: the table is the single source of the default, and the kill
/// switch reaches every site by construction. Read with `env_is`/`get`,
/// never `env_set` (presence would count `=0` as on). The vendored
/// Cranelift reads `PLIRON_REMAT_SADDR`, `PLIRON_REMATCALL`,
/// `PLIRON_EDGEDEDUP`, `PLIRON_NARROWEXT`, `PLIRON_EDGEMERGE`,
/// `PLIRON_EPIMERGE` and `PLIRON_JT_NARROW` itself and defaults them the
/// same way (booleans on unless `=0`; EDGEMERGE/EPIMERGE mode `1` when
/// unset). Validation: docs/history/2026-10-27-defaults.md,
/// 2026-10-28-overnight.md.
pub const DEFAULT_ON: &[&str] = &[
    // Stack-slot merging (docs/history/2026-10-25-slot-coloring.md,
    // 2026-10-26-slot-interference.md).
    "PLIRON_SLOTMERGE_MARK",
    "PLIRON_SM_MARKFLOW",
    "PLIRON_SM_COALESCE",
    "PLIRON_SM_PRECISE",
    "PLIRON_SM_DROPNC",
    "PLIRON_SM_BYSIZE",
    "PLIRON_SM_FLOWIV",
    "PLIRON_SM_AFTERKILL",
    "PLIRON_SM_STARTS",
    "PLIRON_SM_STARTONLY",
    "PLIRON_SM_ESCFLOW",
    "PLIRON_SM_COALCALL",
    "PLIRON_NOCAP_FIELDS",
    "PLIRON_NOCAP_CALLEE",
    // Inliner (2026-10-26-inline-recursion.md).
    "PLIRON_INLINE_RESUME_DOM",
    "PLIRON_INLINE_RESUME_HOIST",
    "PLIRON_INLINE_ONCE1",
    "PLIRON_INLINE_RECSTACK",
    "PLIRON_INLINE_SCCLIMIT",
    // Memory (2026-10-26-memory.md, 2026-10-27-memory2.md, memory3.md).
    "PLIRON_DEADFREE",
    "PLIRON_LOWERFREE",
    "PLIRON_JEARENA",
    // Lifetime-undef on SROA-promoted locals (2026-10-27-spills.md;
    // side-table form in 922d8f7 keeps RSS flat).
    "PLIRON_LTUNDEF",
    // Cold-call args as plain copies (2026-10-27-exact-div.md).
    "PLIRON_COLDCOPY",
    // parse_only / eqbug digit-loop shape (2026-10-27-slow-kernels.md):
    // madd form, pointer step compare, select-chain peels.
    "PLIRON_SELCHAIN",
    "PLIRON_MULCARRY",
    "PLIRON_STEPCMP",
    // Wasm size (2026-10-27-wasm-size.md): refuse irreducible/recursive
    // loop-rule splices, GC dead locals, keep CLIF CFGs reducible, and
    // stop IR inlining along call-graph cycles in small/wasm mode.
    "PLIRON_WASM_NOIRRED",
    "PLIRON_WASM_NOREC",
    "PLIRON_WASM_GC",
    "PLIRON_REDUCIBLE",
    "PLIRON_INLINE_SMALLSCC",
    // Parity card B leftovers (2026-10-27-parity-card.md): jump-table
    // shape, Ordering folds through joins, drop trap after noreturn.
    "PLIRON_JT_SHAPE",
    "PLIRON_CMP3",
    "PLIRON_NORETTRAP",
];

/// Valued defaults: an unset variable reads as the given string.
/// `PLIRON_X=0` (or any other explicit value) overrides. Used for caps
/// that are not boolean `=1` gates — `env_get` sites see the default
/// the same way `env_is` sees `DEFAULT_ON`. Kill switch for a cap is
/// usually `=0` (off).
pub const DEFAULT_VALUES: &[(&str, &str)] = &[
    // At most 8 CGUs past the start of stage A (2026-10-27-memory2.md).
    // Tuned on a 14-core Mac; `=0` disables the cap.
    ("PLIRON_CGU_ACAP", "8"),
    // Waffle loop-rule leaf filter (`1` ⇒ ≤2 calls in the callee).
    // 5.73x → 2.17x on regex-wasm CODE; hash_one-style leaves still inline.
    ("PLIRON_WASM_HOTLEAF", "1"),
    // Edge/epilogue merge modes (vendored Cranelift `pliron_mode` also
    // defaults unset → 1; `=0` off, `=2` merge-everything).
    ("PLIRON_EDGEMERGE", "1"),
    ("PLIRON_EPIMERGE", "1"),
    // Cap inlining of high-fan-in callees (2026-10-27-ir-size.md). lockf
    // ctime: FANIN=8 cut syn text 1.94→1.46x / RSS 1.81→1.52x. `=0` off.
    ("PLIRON_INLINE_FANIN", "8"),
    // Waffle single-callsite inline cap (src/wasm.rs). 100: regex-wasm
    // CODE 440→401 KiB; hashmap/tokenize runtime neutral. Prior unwrap 300.
    ("PLIRON_WASM_SOLO", "100"),
];

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
        for k in DEFAULT_ON {
            values.entry(k.to_string()).or_insert_with(|| "1".to_string());
        }
        for &(k, v) in DEFAULT_VALUES {
            values.entry(k.to_string()).or_insert_with(|| v.to_string());
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
    static READS: std::cell::RefCell<Vec<rustc_hash::FxHashMap<String, Option<String>>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Read windows open on THIS thread. A global flag would gate on
    /// other threads' windows: while any worker is inside a pipeline
    /// frame, every `config()` read on every other thread (lowering,
    /// emission) falls out of the no-window fast path and pays a TLS
    /// borrow + frame walk — measured at ~1 s per million reads.
    static ARMED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// True while a read-recording window is open on this thread. Callers
/// memoizing an env-derived value must take the real `config()` path
/// under this so the reads still land in the open windows.
#[inline]
pub fn reads_open() -> bool {
    ARMED.with(|a| a.get() != 0)
}

/// Open a read-recording frame. Always succeeds — frames nest, and each
/// frame sees only the reads that happen inside it.
pub fn reads_begin() -> bool {
    ARMED.with(|a| a.set(a.get() + 1));
    READS.with(|r| {
        r.borrow_mut().push(Default::default());
        true
    })
}

/// Close the innermost frame and return its `(var, value)` pairs,
/// sorted so equal read-sets compare equal.
pub fn reads_take() -> std::collections::BTreeMap<String, Option<String>> {
    if mcprof() {
        // Merge this thread's per-key counts — a window close is the
        // natural batch point, so the global map sees one lock per
        // window instead of one per recorded read.
        KEY_COUNTS.with(|m| {
            let mut m = m.borrow_mut();
            if !m.is_empty() {
                let mut g = RECORD_KEYS.lock().unwrap_or_else(|e| e.into_inner());
                for (k, n) in m.drain() {
                    *g.entry(k).or_default() += n;
                }
            }
        });
    }
    let m = READS.with(|r| r.borrow_mut().pop());
    if m.is_some() {
        ARMED.with(|a| a.set(a.get() - 1));
    }
    m.unwrap_or_default().into_iter().collect()
}

pub static RECORD_COUNT: AtomicU64 = AtomicU64::new(0);
static RECORD_KEYS: std::sync::LazyLock<
    std::sync::Mutex<rustc_hash::FxHashMap<String, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(rustc_hash::FxHashMap::default()));
thread_local! {
    static KEY_COUNTS: std::cell::RefCell<rustc_hash::FxHashMap<String, u64>> =
        Default::default();
}
// std::env, not `config()` — the recorder must not re-enter `config()`.
fn mcprof() -> bool {
    static PROF: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("PLIRON_MCPROF").is_some());
    *PROF
}

fn record(k: &str, v: Option<&str>) {
    // Cache-machinery knobs are inputs to the cache itself, not to the
    // computation — recording them would tie variants to the cache dir
    // or profiling flags rather than to the work performed.
    if matches!(
        k,
        "PLIRON_STEPCACHE" | "PLIRON_CACHE_DIR" | "PLIRON_CACHE_LOG" | "PLIRON_MCPROF"
    ) {
        return;
    }
    if ARMED.with(|a| a.get() == 0) {
        return;
    }
    if mcprof() {
        RECORD_COUNT.fetch_add(1, Ordering::Relaxed);
        KEY_COUNTS.with(|m| *m.borrow_mut().entry(k.to_string()).or_default() += 1);
    }
    READS.with(|r| {
        for m in r.borrow_mut().iter_mut() {
            // Most calls re-record a key this frame already has — hash
            // once and skip the String allocs.
            if !m.contains_key(k) {
                m.insert(k.to_string(), v.map(str::to_string));
            }
        }
    });
}

/// PLIRON_MCPROF: top recorded keys (debug only).
pub fn record_keys_top() -> Vec<(String, u64)> {
    let mut v: Vec<_> = RECORD_KEYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, n)| (k.clone(), *n))
        .collect();
    v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    v
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

/// `run_step`'s per-step `Instant::now()`s are only observable through
/// `pliron::timing` logs or `PLIRON_TRACE_FILE` lines — harnesses that
/// consume `Stats::timing` directly (clif-check bench) mark this.
static STEP_TIMING: AtomicBool = AtomicBool::new(false);

pub fn mark_step_timing() {
    STEP_TIMING.store(true, Ordering::Relaxed);
}

/// Whether per-step wall-clock reads are worth paying. Off in a plain
/// rustc compile (no subscriber path can print `pliron::timing` and no
/// trace file is open): ~2 clock reads per step per function saved.
#[inline]
pub fn step_timing_on() -> bool {
    static DERIVED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        env_get("PLIRON_TRACE_FILE").is_some()
            || (tracing_on()
                && tracing::enabled!(target: "pliron::timing", tracing::Level::INFO))
    });
    STEP_TIMING.load(Ordering::Relaxed) || *DERIVED
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

/// `PLIRON_TRACE_FILE=<path>`: append `mark <pid> <t_ms> <label>` — a
/// wall-clock point event (ms since process start), for reconstructing
/// pipeline timelines that `trace`'s durations can't express.
/// The process's current physical footprint in MiB (macOS; 0 elsewhere).
pub fn footprint_mib() -> u64 {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
        }
        // rusage_info_v0: 16-byte uuid, then u64s; phys_footprint is the
        // eighth (user, system, pkg_idle, interrupt, pageins, wired,
        // resident, phys_footprint, ...).
        let mut buf = [0u64; 2 + 11];
        // SAFETY: a buffer as large as rusage_info_v0, flavor 0 (V0).
        if unsafe { proc_pid_rusage(std::process::id() as i32, 0, buf.as_mut_ptr()) } == 0 {
            return buf[2 + 7] >> 20;
        }
    }
    0
}

pub fn mark(label: &str) {
    let Some(path) = env_get("PLIRON_TRACE_FILE") else {
        return;
    };
    use std::io::Write;
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let t = T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1e3;
    // `PLIRON_MEM_MARK=1`: the process's current memory footprint too.
    let line = if env_set("PLIRON_MEM_MARK") {
        format!("mark {} {t:.3} {label} mem={}MiB\n", std::process::id(), footprint_mib())
    } else {
        format!("mark {} {t:.3} {label}\n", std::process::id())
    };
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

/// Stable 32-bit id of a symbol for `PLIRON_OPT_FNS` ranges: the high
/// half of its 64-bit FNV-1a hash. Unlike the `PLIRON_OPT_BISECT`
/// counter it depends on nothing but the name, so it agrees across
/// threads, CGUs, rustc processes and rebuilds — a whole bootstrap can
/// be bisected by function.
pub fn fn_hash(name: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h >> 32) as u32
}

enum FnSel {
    /// `lo..hi` over `fn_hash` (`hi` exclusive, `u32::MAX + 1` allowed).
    Range(u64, u64),
    /// `~substr`: names containing it.
    Sub(String),
    /// `@file`: exact names, one per line (`#` comments; a leading
    /// `<hash>\t` column, as `PLIRON_OPT_FNS_LOG` writes, is skipped).
    Names(rustc_hash::FxHashSet<String>),
}

fn parse_num(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

fn parse_fn_sel(spec: &str) -> Option<(bool, FnSel)> {
    let (neg, s) = match spec.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, spec),
    };
    let sel = if let Some(sub) = s.strip_prefix('~') {
        FnSel::Sub(sub.to_string())
    } else if let Some(path) = s.strip_prefix('@') {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("PLIRON_OPT_FNS=@{path}: {e}"));
        FnSel::Names(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.rsplit('\t').next().unwrap_or(l).to_string())
                .collect(),
        )
    } else {
        let (lo, hi) = s.split_once("..")?;
        FnSel::Range(parse_num(lo)?, parse_num(hi)?)
    };
    Some((neg, sel))
}

/// `PLIRON_OPT_FNS=<sel>`: optimize only the selected functions — the
/// rest keep their unoptimized body through the per-function pliron-IR
/// transforms and skip the CLIF pipeline. Interprocedural rewrites
/// (signature changes, dead-fn sweeps) still see every function, so a
/// mixed build stays consistent. `<sel>` is `lo..hi` over [`fn_hash`]
/// (decimal or 0x hex, `hi` exclusive; `0..0x100000000` is everything),
/// `~substr`, or `@file`; a leading `!` inverts. Unset selects all.
///
/// `PLIRON_OPT_FNS_LOG=<path>` appends `<hash>\t<name>` for every
/// selected function — `harness/fnbisect.py` reads it to name the
/// culprits once the range is narrow.
pub fn fn_opt_selected(name: &str) -> bool {
    static SEL: LazyLock<Option<(bool, FnSel)>> = LazyLock::new(|| {
        let spec = env_get("PLIRON_OPT_FNS")?;
        Some(
            parse_fn_sel(spec)
                .unwrap_or_else(|| panic!("PLIRON_OPT_FNS: bad selector `{spec}`")),
        )
    });
    // Record the read so step-cache variants key on the selector.
    if reads_open() {
        let _ = env_get("PLIRON_OPT_FNS");
    }
    let Some((neg, sel)) = &*SEL else {
        return true;
    };
    let h = fn_hash(name);
    let hit = match sel {
        FnSel::Range(lo, hi) => (*lo..*hi).contains(&(h as u64)),
        FnSel::Sub(s) => name.contains(s.as_str()),
        FnSel::Names(set) => set.contains(name),
    };
    let on = hit != *neg;
    if on && let Some(path) = env_get("PLIRON_OPT_FNS_LOG") {
        use std::io::Write;
        // Every gated pass asks again; log each name once per process.
        static LOGGED: std::sync::Mutex<Option<rustc_hash::FxHashSet<String>>> =
            std::sync::Mutex::new(None);
        if !LOGGED
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .insert(name.to_string())
        {
            return on;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(format!("{h:08x}\t{name}\n").as_bytes());
        }
    }
    on
}

#[cfg(test)]
mod fn_sel_tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        // Pinned: bisect logs from older builds must stay comparable.
        assert_eq!(fn_hash(""), 0xcbf29ce4);
        assert_eq!(fn_hash("main"), fn_hash("main"));
        assert_ne!(fn_hash("main"), fn_hash("mains"));
    }

    #[test]
    fn selectors_parse() {
        assert!(matches!(parse_fn_sel("0..0x100000000"), Some((false, FnSel::Range(0, 0x1_0000_0000)))));
        assert!(matches!(parse_fn_sel("!~drop"), Some((true, FnSel::Sub(s))) if s == "drop"));
        assert!(parse_fn_sel("12").is_none());
    }
}
