//! Per-step and whole-pipeline memoization (`PLIRON_STEPCACHE=1`).
//!
//! A pipeline step is a pure function of `(step name, backend build id,
//! input function state, the FnJob input maps, the `PLIRON_*` vars it
//! reads)`, so each step's output is content-addressed on disk. Two
//! files per function input, under `$PLIRON_CACHE_DIR` (or
//! `~/.cache/pliron-steps`):
//!
//! - `pipe/<key>` — `Vec<PipeVariant>`: whole-pipeline results. A hit
//!   replays the output func + `derived` + stats + bisect draws in one
//!   read. Each variant carries the env reads its run made *and* the
//!   list of steps that ran — a gate flip that changes the step list
//!   misses even when every recorded read happens to match.
//! - `step/<key>` — `Vec<StepEntry>`: per-step results for the
//!   expensive (`Step.cached`) passes, loaded once per function and
//!   written once at pipeline end. This is the partial-invalidation
//!   fallback: a gate flip misses the pipe variants, runs the
//!   pipeline, and still hits every step whose input hash and read-set
//!   are unchanged.
//!
//! Function identity for keys is a hash of `(stencil, params-walk,
//! derived)` — `params` is walked (`PrimaryMap` order is stable), not
//! serialized, because its internal `HashMap` serializes in
//! per-process-random order. Steps chain: each stored `out_hash` is
//! the next step's input hash, so a hit never re-hashes the function.
//! The FnJob fact maps hash once per function (`fnh`). A step that
//! leaves the state byte-identical stores no `func` — hits then
//! replay as a tiny `derived`/stats update.
//!
//! Correctness contract: all IO/serde failure degrades to a miss —
//! the cache must never break a build. Concurrent writers can lose
//! variants (last-writer-wins per file); that costs hit rate, never
//! correctness, because every variant is a real output for its
//! recorded `(ran, reads)` pair.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::function::{FunctionParameters, FunctionStencil};
use cranelift_codegen::ir::{Function, Value};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::config::{config, reads_begin, reads_take};
use crate::pipeline::{Analysis, Pipe, Stats, Step};

#[derive(Serialize, Deserialize)]
struct Variant {
    /// `(var, value)` env reads the producing run made. A hit requires
    /// every recorded read to equal the current config — vars not read
    /// cannot have affected the output, so their current value is
    /// irrelevant (that is what makes gate flips surgical).
    reads: Vec<(String, Option<String>)>,
    /// Hash of the post-step function state — the chain link.
    out_hash: [u8; 32],
    /// `None` when the step left the function byte-identical.
    func: Option<(FunctionStencil, FunctionParameters)>,
    /// Whole post-step `derived` map (only ifconv writes it today).
    derived: Vec<(Value, Value)>,
    /// `(d, w)` pairs `carry_one` applied during the step. Replayed on
    /// hit so carried facts land exactly as the real run left them.
    /// Never needed in `in_hash`: fact entries are claims about the
    /// identical func state the hash already covers — a variant made
    /// under a different fact set still produces a valid output (the
    /// run's facts stayed true, unused extra facts only cost opts).
    facts: Vec<(Value, Value)>,
    stats: StatDelta,
    k: usize,
}

/// All cached results for one `(step, input-state)` in a function's
/// step file — one entry per distinct input the step ever saw.
#[derive(Serialize, Deserialize)]
pub(crate) struct StepEntry {
    step: String,
    in_hash: [u8; 32],
    variants: Vec<Variant>,
}

#[derive(Serialize, Deserialize)]
struct PipeVariant {
    reads: Vec<(String, Option<String>)>,
    /// Step names that ran, in order. Gate flips that only change
    /// *which* steps run (`Gate::Flag` reads `p.flags`, not the env,
    /// so it isn't in `reads`) are caught by comparing this against a
    /// fresh `selection()` at lookup time.
    ran: Vec<String>,
    func: (FunctionStencil, FunctionParameters),
    derived: Vec<(Value, Value)>,
    /// Carry pairs applied across the pipeline (see `Variant::facts`).
    facts: Vec<(Value, Value)>,
    stats: StatDelta,
    /// `bisect` units this function drew — replayed on hit so
    /// `PLIRON_OPT_BISECT` numbering matches an uncached run.
    draws: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Default)]
struct StatDelta {
    threaded: usize,
    dead_params: usize,
    dominated_folds: usize,
    unreached: usize,
    peeped: usize,
    merged: usize,
    dupd: usize,
    forwarded: usize,
    slot_dse: usize,
}

impl StatDelta {
    fn of(s: &Stats) -> Self {
        Self {
            threaded: s.threaded,
            dead_params: s.dead_params,
            dominated_folds: s.dominated_folds,
            unreached: s.unreached,
            peeped: s.peeped,
            merged: s.merged,
            dupd: s.dupd,
            forwarded: s.forwarded,
            slot_dse: s.slot_dse,
        }
    }
    fn delta(before: &Stats, after: &Stats) -> Self {
        let (b, a) = (Self::of(before), Self::of(after));
        Self {
            threaded: a.threaded.wrapping_sub(b.threaded),
            dead_params: a.dead_params.wrapping_sub(b.dead_params),
            dominated_folds: a.dominated_folds.wrapping_sub(b.dominated_folds),
            unreached: a.unreached.wrapping_sub(b.unreached),
            peeped: a.peeped.wrapping_sub(b.peeped),
            merged: a.merged.wrapping_sub(b.merged),
            dupd: a.dupd.wrapping_sub(b.dupd),
            forwarded: a.forwarded.wrapping_sub(b.forwarded),
            slot_dse: a.slot_dse.wrapping_sub(b.slot_dse),
        }
    }
    fn apply(&self, s: &mut Stats) {
        s.threaded += self.threaded;
        s.dead_params += self.dead_params;
        s.dominated_folds += self.dominated_folds;
        s.unreached += self.unreached;
        s.peeped += self.peeped;
        s.merged += self.merged;
        s.dupd += self.dupd;
        s.forwarded += self.forwarded;
        s.slot_dse += self.slot_dse;
    }
}

fn enabled() -> bool {
    // `PLIRON_STEPCACHE` is process-fixed and excluded from read
    // recording (it configures the cache, not the computation), so a
    // one-time parse is safe — called per cached step per function.
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| config().is("PLIRON_STEPCACHE", "1"));
    *ON
}

fn dir() -> Option<&'static Path> {
    // Both inputs are process-fixed record-excluded cache knobs; the
    // fallback allocates a PathBuf per call otherwise.
    static DIR: std::sync::LazyLock<Option<PathBuf>> = std::sync::LazyLock::new(|| {
        config()
            .get("PLIRON_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|h| h.join(".cache/pliron-steps")))
    });
    DIR.as_deref()
}

fn log(what: &str) {
    if pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::stepcache", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stepcache", "stepcache {what}")
        };
    }
}

/// `Hasher` over `Sha256`: strong collision resistance is correctness —
/// a wrong hit silently substitutes a different function's output.
struct Sha(sha2::Sha256);

impl Hasher for Sha {
    fn finish(&self) -> u64 {
        unreachable!("Sha256Hasher doesn't support finish!")
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

/// Maps are iterated in nondeterministic order; hash each entry
/// separately, sort, then fold so iteration order can't split keys.
fn seq_hash<I: Hash>(h: &mut Sha, items: impl IntoIterator<Item = I>) {
    let mut es: Vec<u64> = items
        .into_iter()
        .map(|e| {
            let mut x = FxHasher::default();
            e.hash(&mut x);
            x.finish()
        })
        .collect();
    es.sort_unstable();
    (es.len() as u64).hash(h);
    for e in es {
        e.hash(h);
    }
}

/// Hash the whole mutable pipeline state: stencil + `params` +
/// `derived`. `params` is walked (`PrimaryMap` order is stable), not
/// serialized: its internal `HashMap` serializes in per-process-random
/// order, which would make keys differ across runs. `derived` is in
/// the chain because a step can rewrite it without touching the
/// function — downstream steps must not hit a variant produced for
/// different facts.
fn state_hash(p: &Pipe) -> [u8; 32] {
    let mut h = Sha(sha2::Sha256::new());
    p.clctx.func.stencil.hash(&mut h);
    let params = &p.clctx.func.params;
    params.base_srcloc().hash(&mut h);
    for name in params.user_named_funcs().values() {
        name.hash(&mut h);
    }
    seq_hash(&mut h, p.derived.iter());
    h.0.finalize().into()
}

/// The previous step's output hash is this step's input hash; the
/// first cached step hashes the post-`FnLower` state once per function.
fn cur_hash(p: &Pipe, an: &mut Analysis) -> [u8; 32] {
    if let Some(h) = an.clif_hash {
        return h;
    }
    let h = state_hash(p);
    an.clif_hash = Some(h);
    h
}

/// Everything a step can observe besides the function: the FnLower
/// fact maps plus target/config fields. Hashed once per function.
fn fn_inputs(p: &Pipe, an: &mut Analysis) -> [u8; 32] {
    if let Some(h) = an.fnh {
        return h;
    }
    let mut h = Sha(sha2::Sha256::new());
    seq_hash(&mut h, p.nonnull.iter());
    seq_hash(&mut h, p.frozen.iter());
    seq_hash(&mut h, p.noalias.iter());
    seq_hash(&mut h, p.deref.iter());
    seq_hash(&mut h, p.nowrite.iter());
    seq_hash(&mut h, p.call_frozen.iter());
    seq_hash(&mut h, p.scoped.iter());
    seq_hash(&mut h, p.invariant.iter());
    seq_hash(&mut h, p.operand_frozen.iter());
    seq_hash(&mut h, p.block_region.iter());
    seq_hash(&mut h, p.region_parent.iter());
    seq_hash(&mut h, p.lifetimes.iter());
    seq_hash(&mut h, p.nocap.iter());
    seq_hash(&mut h, p.prof.iter());
    // Steps can observe the function's identity: `tailrec` matches
    // self-calls on `func_id`, `loopidiom` refuses libcall-named `fname`s.
    // Two stencils may be identical across functions — without these a
    // hit replays a decision made for a different self.
    p.fname.hash(&mut h);
    p.func_id.as_u32().hash(&mut h);
    (
        p.tcfg.default_call_conv,
        p.tcfg.pointer_width,
        p.tcfg.page_size_align_log2,
    )
        .hash(&mut h);
    // `p.flags` deliberately excluded: flag fields are only read by
    // `Gate::ok` before a step runs — a flip changes *which* steps run
    // (caught by `PipeVariant::ran`), not what a running step sees, and
    // flag reads through the config accessors land in the read-set.
    (p.simd, p.x64, p.cmp64, p.has_eh, p.dump, p.verify_full).hash(&mut h);
    let r = h.0.finalize().into();
    an.fnh = Some(r);
    r
}

fn file_path(kind: &str, k: &[u8; 32]) -> Option<PathBuf> {
    let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
    Some(dir()?.join(kind).join(&hex[..2]).join(format!("{hex}.bin")))
}

fn load<T: for<'de> Deserialize<'de>>(p: &PathBuf) -> Vec<T> {
    std::fs::read(p)
        .ok()
        .and_then(|b| postcard::from_bytes::<Vec<T>>(&b).ok())
        .unwrap_or_default()
}

fn store<T: Serialize>(path: &PathBuf, variants: &[T]) {
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_ok()
        && let Ok(blob) = postcard::to_allocvec(variants)
    {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &blob).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn reads_ok(reads: &[(String, Option<String>)]) -> bool {
    reads
        .iter()
        .all(|(k, v)| config().get(k.as_str()) == v.as_deref())
}

/// Apply a cached func+derived to the pipe, mirroring `clctx.clear`'s
/// reset of the context's cached analyses.
fn apply_func(
    p: &mut Pipe,
    func: (FunctionStencil, FunctionParameters),
    derived: &[(Value, Value)],
) {
    let name = p.clctx.func.name.clone();
    p.clctx.func = Function {
        name,
        stencil: func.0,
        params: func.1,
    };
    p.clctx.cfg = ControlFlowGraph::new();
    p.clctx.domtree = DominatorTree::new();
    p.clctx.loop_analysis = LoopAnalysis::new();
    *p.derived = derived.iter().copied().collect();
    p.seed_derived_bits();
}

// ---------------------------------------------------------------------
// Whole-pipeline entries.

/// The pipeline key covers the input state plus the custom pipeline
/// list (`PLIRON_PIPELINE`) — everything else is captured by variants.
/// Also addresses the function's step file.
fn pipe_key(p: &Pipe, an: &mut Analysis) -> [u8; 32] {
    let mut h = Sha(sha2::Sha256::new());
    env!("PLIRON_BUILD_ID").hash(&mut h);
    fn_inputs(p, an).hash(&mut h);
    cur_hash(p, an).hash(&mut h);
    config().pipeline.hash(&mut h);
    h.0.finalize().into()
}

pub enum PipeLookup {
    Hit,
    Miss([u8; 32]),
    Off,
}

/// Try to replay a whole cached pipeline. Returns the function's key
/// on a miss so `pipeline_store` doesn't recompute it (the input hash
/// must be captured before any step mutates the function).
pub fn pipeline_lookup(p: &mut Pipe, an: &mut Analysis) -> PipeLookup {
    if !enabled() {
        return PipeLookup::Off;
    }
    let k = pipe_key(p, an);
    an.pk = Some(k);
    let Some(path) = file_path("pipe", &k) else {
        return PipeLookup::Off;
    };
    let variants: Vec<PipeVariant> = load(&path);
    let ran: Vec<&str> = crate::pipeline::selected(p, true)
        .iter()
        .map(|s| s.name)
        .collect();
    let Some(v) = variants
        .iter()
        .find(|v| v.ran.iter().map(String::as_str).eq(ran.iter().copied()) && reads_ok(&v.reads))
    else {
        log("pipe-miss");
        return PipeLookup::Miss(k);
    };
    log("pipe-hit");
    apply_func(p, v.func.clone(), &v.derived);
    for &(d, w) in &v.facts {
        crate::pipeline::carry_one(p, d, w);
    }
    v.stats.apply(p.stats);
    crate::config::bisect_skip(v.draws);
    PipeLookup::Hit
}

/// Store a whole-pipeline variant after a real run. `before` is the
/// stats snapshot taken before the pipeline ran; `ran` is the list of
/// step names that ran.
pub fn pipeline_store(
    k: &[u8; 32],
    p: &Pipe,
    before: &Stats,
    reads: std::collections::BTreeMap<String, Option<String>>,
    draws: u64,
    ran: impl Iterator<Item = &'static str>,
) {
    let Some(path) = file_path("pipe", k) else {
        return;
    };
    let variant = PipeVariant {
        reads: reads.into_iter().collect(),
        ran: ran.map(str::to_string).collect(),
        func: (p.clctx.func.stencil.clone(), p.clctx.func.params.clone()),
        derived: p.derived.iter().map(|(a, b)| (*a, *b)).collect(),
        facts: p.carry_log.clone(),
        stats: StatDelta::delta(before, p.stats),
        draws,
    };
    let mut variants: Vec<PipeVariant> = load(&path);
    match variants
        .iter_mut()
        .find(|v| v.reads == variant.reads && v.ran == variant.ran)
    {
        Some(v) => *v = variant,
        None => variants.push(variant),
    }
    store(&path, &variants);
}

// ---------------------------------------------------------------------
// Per-step entries. The step file loads lazily on the first cached
// step's lookup (skipped entirely on a pipe hit) and is written once
// by `flush` if any step stored.

fn stepfile<'x>(an: &'x mut Analysis) -> Option<&'x mut Vec<StepEntry>> {
    if an.stepfile.is_none() {
        let path = file_path("step", &an.pk?)?;
        an.stepfile = Some(load(&path));
    }
    an.stepfile.as_mut()
}

/// Try to replay a cached step result. Returns the step's `k` on a hit.
pub fn lookup(p: &mut Pipe, s: &Step, an: &mut Analysis) -> Option<usize> {
    if !enabled() {
        return None;
    }
    let cur = cur_hash(p, an);
    let (func, derived, facts, stats, out_hash, k) = {
        let ent = stepfile(an)?
            .iter()
            .find(|e| e.step == s.name && e.in_hash == cur)?;
        let v = ent.variants.iter().find(|v| reads_ok(&v.reads))?;
        (
            v.func.clone(),
            v.derived.clone(),
            v.facts.clone(),
            v.stats,
            v.out_hash,
            v.k,
        )
    };
    if let Some(func) = func {
        apply_func(p, func, &derived);
    } else {
        *p.derived = derived.iter().copied().collect();
        p.seed_derived_bits();
    }
    for (d, w) in &facts {
        crate::pipeline::carry_one(p, *d, *w);
    }
    stats.apply(p.stats);
    an.clif_hash = Some(out_hash);
    p.dump(s.name);
    Some(k)
}

/// Run the step for real, recording its env reads and appending the
/// variant to the in-memory step file (written back by `flush`).
pub fn compute(p: &mut Pipe, s: &Step, an: &mut Analysis) -> usize {
    if !enabled() {
        return (s.run)(p, an);
    }
    let in_hash = cur_hash(p, an);
    let before = p.stats.clone();
    let cmark = p.carry_log.len();
    let armed = reads_begin();
    let k = (s.run)(p, an);
    let reads = reads_take();
    if !armed {
        return k; // nested window — reads unattributable, don't store.
    }
    let out_hash = state_hash(p);
    an.clif_hash = Some(out_hash);
    let func =
        (out_hash != in_hash).then(|| (p.clctx.func.stencil.clone(), p.clctx.func.params.clone()));
    let variant = Variant {
        reads: reads.into_iter().collect(),
        out_hash,
        func,
        derived: p.derived.iter().map(|(a, b)| (*a, *b)).collect(),
        facts: p.carry_log[cmark..].to_vec(),
        stats: StatDelta::delta(&before, p.stats),
        k,
    };
    let Some(file) = stepfile(an) else {
        return k;
    };
    match file
        .iter_mut()
        .find(|e| e.step == s.name && e.in_hash == in_hash)
    {
        Some(e) => match e.variants.iter_mut().find(|v| v.reads == variant.reads) {
            Some(v) => *v = variant,
            None => e.variants.push(variant),
        },
        None => file.push(StepEntry {
            step: s.name.to_string(),
            in_hash,
            variants: vec![variant],
        }),
    }
    an.stepfile_dirty = true;
    k
}

/// Write the step file back if any step stored. Called at pipeline end.
pub fn flush(an: &mut Analysis) {
    if !an.stepfile_dirty {
        return;
    }
    if let (Some(k), Some(file)) = (an.pk, an.stepfile.take())
        && let Some(path) = file_path("step", &k)
    {
        store(&path, &file);
    }
}
