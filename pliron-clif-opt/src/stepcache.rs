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
use cranelift_codegen::ir::entities::UserExternalNameRef;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::{Function, UserExternalName, Value};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::{FxHashMap, FxHasher};
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
    /// Per-position symbol for each `user_named_funcs` entry —
    /// lets a consumer CGU remap `FuncId` indices (empty = verbatim,
    /// shared numbering only). See `uen_names`/`uen_remaps`.
    names: Vec<Option<String>>,
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

#[derive(Serialize, Deserialize, Clone)]
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
    /// Same contract as `Variant::names`.
    names: Vec<Option<String>>,
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

/// A bisect budget makes cache replay unsound: a hit replays the
/// producer's `ran` list, but the consumer's remaining budget may
/// require fewer transforms — `bisect_skip` only renumbers, it cannot
/// un-run a step. Bisection is a debugging tool; keep it exact by
/// turning both cache tiers off while `PLIRON_OPT_BISECT` is set.
fn bisecting() -> bool {
    static B: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| crate::config::bisect_limit().is_some());
    *B
}

pub(crate) fn enabled() -> bool {
    // `PLIRON_STEPCACHE` is process-fixed and excluded from read
    // recording (it configures the cache, not the computation), so a
    // one-time parse is safe — called per cached step per function.
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| config().is("PLIRON_STEPCACHE", "1") && !bisecting());
    *ON
}

/// In-process whole-pipeline tier (`PLIRON_MEMCACHE=1`, opt-in):
/// identical functions recur across CGUs in one compile — xcgu copies
/// and rustc's own shared-generic duplicates — and each otherwise
/// re-runs every step. Keys, variant matching and replay are exactly
/// the disk tier's. Entries are decoded `PipeVariant`s — a disk hit
/// promotes its freshly-decoded list and a mem-store blob could be
/// written to disk unchanged (not done: the disk tier stays opt-in).
/// Determinism is the disk tier's contract — a hit replays an output
/// produced from the same `(key, reads, ran)`, and parallel CGUs make
/// producer choice nondeterministic only over identical results.
fn mem_on() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| config().is("PLIRON_MEMCACHE", "1") && !bisecting());
    *ON
}

/// Any cache tier on — callers can skip work (like symbol-table
/// construction) that only feeds cache keys.
pub fn cache_on() -> bool {
    enabled() || mem_on()
}

/// Cap on serialized bytes held in `MEM` (`PLIRON_MEMCACHE_MB`,
/// default 1 GiB). Storing blobs keeps the accounting exact; past the
/// cap new keys stop inserting — a partial cache is still a win, and a
/// miss only costs the lookup.
fn mem_cap() -> usize {
    static CAP: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        config()
            .get("PLIRON_MEMCACHE_MB")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1024)
            << 20
    });
    *CAP
}

/// One pipeline result or an in-flight computation of it. A copy in
/// CGU B that looks up while CGU A is still optimizing the same body
/// waits on `Pending` instead of recomputing — without this, parallel
/// CGUs would run the duplicates concurrently and never hit. Entries
/// are decoded `PipeVariant`s: postcard round-trips on this tier cost
/// more than the lookups save.
enum MemEnt {
    Pending(std::sync::Arc<Pending>),
    Ready(std::sync::Arc<Vec<PipeVariant>>),
}

/// `Open` while the claim's owner computes; `Done` once it published
/// (`Some`) or dropped without storing (`None` — waiters then produce
/// the result themselves).
enum Slot {
    Open,
    Done(Option<std::sync::Arc<Vec<PipeVariant>>>),
}

struct Pending {
    slot: std::sync::Mutex<Slot>,
    cv: std::sync::Condvar,
}

impl Pending {
    fn new() -> Self {
        Self {
            slot: std::sync::Mutex::new(Slot::Open),
            cv: std::sync::Condvar::new(),
        }
    }
}

static MEM: std::sync::LazyLock<std::sync::Mutex<rustc_hash::FxHashMap<[u8; 32], MemEnt>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(rustc_hash::FxHashMap::default()));
static MEM_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Returned by `pipeline_lookup` on a miss: while held this thread
/// owns the key's pending slot (`pend`, when it claimed one).
/// `pipeline_store` fills it; a drop without storing wakes waiters
/// with `None` so they compute for themselves — a dropped claim must
/// never leave a waiter asleep.
pub struct Claim {
    key: [u8; 32],
    pend: Option<std::sync::Arc<Pending>>,
    filled: bool,
}

impl Claim {
    /// No pending slot owned — used when a blob existed but no variant
    /// in it matched this run.
    fn bare(key: [u8; 32]) -> Self {
        Self {
            key,
            pend: None,
            filled: false,
        }
    }

    fn holding(key: [u8; 32], pend: std::sync::Arc<Pending>) -> Self {
        Self {
            key,
            pend: Some(pend),
            filled: false,
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.filled {
            return;
        }
        if let Some(p) = &self.pend {
            let mut s = p.slot.lock().unwrap_or_else(|e| e.into_inner());
            *s = Slot::Done(None);
            p.cv.notify_all();
        }
    }
}

/// Rough per-variant byte estimate for `MEM_BYTES` accounting —
/// decoded variants aren't measured exactly, and the cap only needs
/// to keep this tier bounded, not precise.
fn est_bytes(v: &PipeVariant) -> usize {
    let (st, params) = &v.func;
    1024 + 96 * st.dfg.num_insts()
        + 64 * (st.layout.blocks().count()
            + params.user_named_funcs().len()
            + v.derived.len()
            + v.facts.len())
        + 16 * v.ran.len()
}

/// The Ready variant list under `k`, if any — for merges on store.
fn mem_ready(k: &[u8; 32]) -> Option<std::sync::Arc<Vec<PipeVariant>>> {
    match MEM.lock().ok()?.get(k) {
        Some(MemEnt::Ready(v)) => Some(v.clone()),
        _ => None,
    }
}

/// Insert a Ready variant list under `k` if it fits the byte cap.
/// Retention only — waiters must already have been fed by `mem_fill`.
fn mem_retain(k: [u8; 32], vs: std::sync::Arc<Vec<PipeVariant>>) {
    let Ok(mut m) = MEM.lock() else {
        return;
    };
    let old = match m.get(&k) {
        Some(MemEnt::Ready(v)) => v.iter().map(est_bytes).sum::<usize>(),
        _ => 0,
    };
    let new: usize = vs.iter().map(est_bytes).sum();
    let cur = MEM_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    if cur + new > old + mem_cap() {
        // Over cap: keep nothing new — later lookups just miss.
        if matches!(m.get(&k), Some(MemEnt::Pending(_))) {
            m.remove(&k);
        }
        return;
    }
    if new >= old {
        MEM_BYTES.fetch_add(new - old, std::sync::atomic::Ordering::Relaxed);
    } else {
        MEM_BYTES.fetch_sub(old - new, std::sync::atomic::Ordering::Relaxed);
    }
    m.insert(k, MemEnt::Ready(vs));
}

/// Wake the pending slot's waiters with `vs`, then retain it.
fn mem_fill(k: [u8; 32], t: &std::sync::Arc<Pending>, vs: Vec<PipeVariant>) {
    let vs = std::sync::Arc::new(vs);
    {
        let mut s = t.slot.lock().unwrap_or_else(|e| e.into_inner());
        *s = Slot::Done(Some(vs.clone()));
    }
    t.cv.notify_all();
    mem_retain(k, vs);
}

/// Per-position declared symbol for every `UserExternalName` in
/// `params`, `None` for non-namespace-0 names. An empty vec marks a
/// verbatim variant (no symbol table — applies only under a
/// table-less consumer, where the raw indices are meaningful).
/// `None` means a tabled run couldn't resolve a name — the variant
/// is unshareable and must not be published: its indices belong to
/// this module's numbering alone.
fn uen_names(
    p: &Pipe,
    params: &FunctionParameters,
) -> Option<Vec<Option<String>>> {
    if p.uen_syms.is_empty() {
        return Some(Vec::new());
    }
    params
        .user_named_funcs()
        .values()
        .map(|n| {
            if n.namespace != 0 {
                Some(None)
            } else {
                p.uen_syms.get(&n.index).cloned().map(Some)
            }
        })
        .collect()
}

/// Resolve the producer's `UserExternalName` FuncId indices into this
/// module's numbering. `names` is the variant's per-position symbol
/// list; an empty list means verbatim (valid only when this pipe has
/// no table either). Every namespace-0 name must resolve here —
/// outputs reference a subset of the input's declared callees, so a
/// failure means the numbering differs and the variant is unusable.
/// Returns the `(ref, name)` pairs to install.
fn uen_remaps<'x>(
    p: &Pipe,
    params: &'x FunctionParameters,
    names: &[Option<String>],
) -> Option<Vec<(UserExternalNameRef, cranelift_codegen::ir::UserExternalName)>> {
    if names.is_empty() {
        // Verbatim variants carry no symbol table — a tabled consumer
        // can't trust raw indices. Unless the params name nothing at
        // all: an empty remap is trivially valid either way (this is
        // also what keeps leaf functions hittable across CGUs — a
        // tabled producer emits `names=[]` for them).
        return (p.uen_syms.is_empty() || params.user_named_funcs().is_empty()).then(Vec::new);
    }
    if names.len() != params.user_named_funcs().len() {
        return None;
    }
    params
        .user_named_funcs()
        .iter()
        .zip(names)
        .map(|((uref, n), sym)| {
            if n.namespace != 0 {
                return Some((uref, n.clone()));
            }
            let sym = sym.as_ref()?;
            let (&idx, _) = p
                .uen_syms
                .iter()
                .find(|(_, s)| s.as_str() == sym.as_str())?;
            Some((
                uref,
                UserExternalName {
                    namespace: 0,
                    index: idx,
                },
            ))
        })
        .collect()
}

/// `UserExternalName` for hashing: namespace-0 indices are
/// module-local FuncIds — substitute the declared symbol so identical
/// bodies in different CGUs key alike. Without a table the raw index
/// stands in; table-less contexts only see verbatim variants anyway.
#[derive(Hash)]
enum UKey<'x> {
    Sym(&'x str),
    Raw(u32, u32),
}

fn uen_key<'x>(n: &cranelift_codegen::ir::UserExternalName, p: &'x Pipe) -> UKey<'x> {
    if n.namespace == 0
        && let Some(sym) = p.uen_syms.get(&n.index)
    {
        UKey::Sym(sym)
    } else {
        UKey::Raw(n.namespace, n.index)
    }
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

/// `PLIRON_MCLOGFN=1`: hit/miss rows carry the function name and size —
/// measures which functions the cache actually dedups.
fn log_fn(what: &str, p: &Pipe) {
    if crate::config::env_is("PLIRON_MCLOGFN", "1") {
        pliron_cfg::trace(what, p.clctx.func.dfg.num_insts() as f64, p.fname);
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
        uen_key(name, p).hash(&mut h);
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

/// Canonical representative for each `Ptr<BasicBlock>` region label a
/// Pipe's fact maps can observe. Copies of one body in different CGUs
/// hold different arena indices for identical structures, so raw ptr
/// bits must never reach the input hash — `seq_hash`'s order-free
/// fold covers map order, not entry content. Every region referenced
/// by `block_region`/`scoped`/`operand_frozen` gets a sequential rep
/// in canonical first-seen order (layout blocks, then insts, then
/// values); their `region_parent` ancestors get
/// `(seed rep << 20 | depth)`, min over all seed walks — order-free,
/// so `ColMap`'s raw-ordinal iteration can't leak in either.
/// `region_parent` is only ever queried by `.get` chain from a
/// referenced region (licm/loadfwd `in_region`), so entries
/// unreachable from those regions are unobservable and must not be
/// keyed: copies carry region trees iropt shaped differently, and the
/// unreachable residue is exactly what differs.
fn region_reps(
    p: &Pipe,
) -> FxHashMap<pliron::context::Ptr<pliron::basic_block::BasicBlock>, u64> {
    let mut rep: FxHashMap<_, u64> = FxHashMap::default();
    let seed = |rep: &mut FxHashMap<_, u64>, r| {
        if !rep.contains_key(&r) {
            let n = rep.len() as u64;
            rep.insert(r, n);
        }
    };
    for b in p.clctx.func.stencil.layout.blocks() {
        if let Some(&r) = p.block_region.get(&b) {
            seed(&mut rep, r);
        }
    }
    let mut sc: Vec<_> = p.scoped.iter().collect();
    sc.sort_by_key(|(i, _)| i.index());
    for (_, &r) in sc {
        seed(&mut rep, r);
    }
    let mut of: Vec<_> = p.operand_frozen.iter().collect();
    of.sort_by_key(|(v, _)| v.index());
    for (_, es) in of {
        for (_, r) in es {
            seed(&mut rep, *r);
        }
    }
    let seeds: Vec<_> = rep.iter().map(|(&k, &v)| (k, v)).collect();
    for (r, base) in seeds {
        let mut cur = r;
        for d in 1u64..(1 << 20) {
            let Some(&par) = p.region_parent.get(cur) else {
                break;
            };
            if par == cur {
                break;
            }
            let cand = (base << 20) | d;
            if rep.get(&par).is_none_or(|&x| x > cand) {
                rep.insert(par, cand);
            }
            cur = par;
        }
    }
    rep
}

/// Everything a step can observe besides the function: the FnLower
/// fact maps plus target/config fields. Hashed once per function.
fn fn_inputs(p: &Pipe, an: &mut Analysis) -> [u8; 32] {
    if let Some(h) = an.fnh {
        return h;
    }
    let reps = region_reps(p);
    let rep_of = |r: &pliron::context::Ptr<pliron::basic_block::BasicBlock>| -> u64 {
        reps.get(r).copied().unwrap_or(1 << 63 | r.ord() as u64)
    };
    let mut h = Sha(sha2::Sha256::new());
    seq_hash(&mut h, p.nonnull.iter());
    seq_hash(&mut h, p.frozen.iter());
    seq_hash(&mut h, p.noalias.iter());
    seq_hash(&mut h, p.deref.iter());
    seq_hash(&mut h, p.nowrite.iter());
    seq_hash(&mut h, p.call_frozen.iter());
    seq_hash(&mut h, p.scoped.iter().map(|(i, r)| (i.index(), rep_of(r))));
    seq_hash(&mut h, p.invariant.iter());
    seq_hash(
        &mut h,
        p.operand_frozen.iter().map(|(v, es)| {
            (
                v.index(),
                es.iter().map(|(x, r)| (*x, rep_of(r))).collect::<Vec<_>>(),
            )
        }),
    );
    seq_hash(
        &mut h,
        p.block_region.iter().map(|(b, r)| (b.index(), rep_of(r))),
    );
    // Only the reachable ancestor graph is observable (see
    // `region_reps`) — a rep'd key's parent is rep'd by construction.
    seq_hash(
        &mut h,
        p.region_parent
            .iter()
            .filter(|(k, _)| reps.contains_key(k))
            .map(|(k, v)| (rep_of(&k), rep_of(v))),
    );
    seq_hash(&mut h, p.lifetimes.iter());
    seq_hash(&mut h, p.nocap.iter());
    seq_hash(&mut h, p.prof.iter());
    // Steps can observe the function's identity: `tailrec` matches
    // self-calls on `func_id`, `loopidiom` refuses libcall-named
    // `fname`s. The self-call's `UserExternalName` already hashes by
    // symbol (`uen_key`), so `func_id`'s module-local numbering only
    // splits otherwise-identical keys — `fname` is what must not
    // change hands: two stencils may be identical across functions and
    // a hit must never replay a decision made for a different self.
    p.fname.hash(&mut h);
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
    // `p.unopt` keys *which* steps run like the flag fields — a dead-
    // candidate fn must not replay a full-pipeline variant nor publish
    // one (uncached output would diverge either way).
    (p.simd, p.x64, p.cmp64, p.has_eh, p.dump, p.verify_full, p.unopt).hash(&mut h);
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
    Miss(Claim),
    Off,
}

/// Install a variant's `(stencil, params)` into `p`, remapping the
/// stored `FuncId` indices into this module's numbering. `false` when
/// it can't be expressed here — nothing has been applied then.
fn apply_named(
    p: &mut Pipe,
    func: (FunctionStencil, FunctionParameters),
    derived: &[(Value, Value)],
    names: &[Option<String>],
) -> bool {
    let Some(remaps) = uen_remaps(p, &func.1, names) else {
        return false;
    };
    apply_func(p, func, derived);
    for (uref, n) in remaps {
        p.clctx.func.params.reset_user_func_name(uref, n);
    }
    true
}

/// Replay the variant matching this run's step list and env reads.
/// Shared by the mem and disk tiers (the disk tier decodes first).
fn pipe_apply(p: &mut Pipe, variants: &[PipeVariant], ran: &[&str]) -> bool {
    let Some(v) = variants
        .iter()
        .find(|v| v.ran.iter().map(String::as_str).eq(ran.iter().copied()) && reads_ok(&v.reads))
    else {
        return false;
    };
    if !apply_named(p, v.func.clone(), &v.derived, &v.names) {
        return false;
    }
    for &(d, w) in &v.facts {
        crate::pipeline::carry_one(p, d, w);
    }
    v.stats.apply(p.stats);
    crate::config::bisect_skip(v.draws);
    true
}

/// Try to replay a whole cached pipeline. Returns a `Claim` on a miss
/// so `pipeline_store` can publish under the same key (and wake any
/// waiters); the input hash must be captured before any step mutates
/// the function.
pub fn pipeline_lookup(p: &mut Pipe, an: &mut Analysis) -> PipeLookup {
    if !enabled() && !mem_on() {
        return PipeLookup::Off;
    }
    struct G(Option<std::time::Instant>, &'static std::sync::atomic::AtomicU64);
    impl Drop for G {
        fn drop(&mut self) {
            if let Some(t) = self.0 {
                self.1.fetch_add(
                    t.elapsed().as_nanos() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
    }
    let _g = G(
        crate::config::env_is("PLIRON_MCPROF", "1").then(std::time::Instant::now),
        &crate::pipeline::MC_LOOKUP_T,
    );
    let k = pipe_key(p, an);
    an.pk = Some(k);
    let ran: Vec<&str> = crate::pipeline::selected(p, true)
        .iter()
        .map(|s| s.name)
        .collect();
    if mem_on() {
        enum Got {
            Ready(std::sync::Arc<Vec<PipeVariant>>),
            Wait(std::sync::Arc<Pending>),
            Mine(std::sync::Arc<Pending>),
        }
        let got = {
            let mut m = MEM.lock().unwrap_or_else(|e| e.into_inner());
            match m.get(&k) {
                Some(MemEnt::Ready(v)) => Got::Ready(v.clone()),
                Some(MemEnt::Pending(t)) => Got::Wait(t.clone()),
                None => {
                    let t = std::sync::Arc::new(Pending::new());
                    m.insert(k, MemEnt::Pending(t.clone()));
                    Got::Mine(t)
                }
            }
        };
        match got {
            Got::Ready(v) => {
                if pipe_apply(p, &v, &ran) {
                    log("mem-pipe-hit");
                    log_fn("mem-pipe-hit", p);
                    return PipeLookup::Hit;
                }
                // Decoded but no variant matched this run's step list
                // or reads — the run produces; its store merges in.
                log("pipe-miss");
                log_fn("pipe-miss-variant", p);
                return PipeLookup::Miss(Claim::bare(k));
            }
            Got::Wait(t) => {
                let done = {
                    let mut s = t.slot.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        match &*s {
                            Slot::Open => {
                                s = t.cv.wait(s).unwrap_or_else(|e| e.into_inner())
                            }
                            Slot::Done(r) => break r.clone(),
                        }
                    }
                };
                match done {
                    Some(v) => {
                        if pipe_apply(p, &v, &ran) {
                            log("mem-pipe-hit");
                            log_fn("mem-pipe-hit-wait", p);
                            return PipeLookup::Hit;
                        }
                        log("pipe-miss");
                        log_fn("pipe-miss-variant", p);
                        return PipeLookup::Miss(Claim::bare(k));
                    }
                    // Producer dropped without storing — take over as
                    // the producer for later lookups.
                    None => {
                        let t = std::sync::Arc::new(Pending::new());
                        if let Ok(mut m) = MEM.lock() {
                            m.insert(k, MemEnt::Pending(t.clone()));
                        }
                        log("pipe-miss");
                        log_fn("pipe-miss-reclaim", p);
                        return PipeLookup::Miss(Claim::holding(k, t));
                    }
                }
            }
            Got::Mine(t) => {
                // The disk tier may still hold it from another process.
                if enabled()
                    && let Some(path) = file_path("pipe", &k)
                    && let Ok(bytes) = std::fs::read(&path)
                    && let Ok(vs) = postcard::from_bytes::<Vec<PipeVariant>>(&bytes)
                    && pipe_apply(p, &vs, &ran)
                {
                    mem_fill(k, &t, vs);
                    log("pipe-hit");
                    return PipeLookup::Hit;
                }
                log("pipe-miss");
                log_fn("pipe-miss-new", p);
                return PipeLookup::Miss(Claim::holding(k, t));
            }
        }
    }
    // Disk tier only (`PLIRON_STEPCACHE` on, mem off).
    if enabled()
        && let Some(path) = file_path("pipe", &k)
        && let Ok(bytes) = std::fs::read(&path)
        && let Ok(vs) = postcard::from_bytes::<Vec<PipeVariant>>(&bytes)
        && pipe_apply(p, &vs, &ran)
    {
        log("pipe-hit");
        return PipeLookup::Hit;
    }
    log("pipe-miss");
    PipeLookup::Miss(Claim::bare(k))
}

/// Store a whole-pipeline variant after a real run. `before` is the
/// stats snapshot taken before the pipeline ran; `ran` is the list of
/// step names that ran. Fills the claim's pending slot (waking
/// waiters) and merges the variant into the key's blob on both tiers.
pub fn pipeline_store(
    claim: &mut Claim,
    p: &Pipe,
    before: &Stats,
    reads: std::collections::BTreeMap<String, Option<String>>,
    draws: u64,
    ran: impl Iterator<Item = &'static str>,
) {
    struct G(Option<std::time::Instant>);
    impl Drop for G {
        fn drop(&mut self) {
            if let Some(t) = self.0 {
                crate::pipeline::MC_STORE_T
                    .fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    let _g = G(crate::config::env_is("PLIRON_MCPROF", "1").then(std::time::Instant::now));
    let k = claim.key;
    let mut variant = PipeVariant {
        reads: reads.into_iter().collect(),
        ran: ran.map(str::to_string).collect(),
        func: (p.clctx.func.stencil.clone(), p.clctx.func.params.clone()),
        derived: p.derived.iter().map(|(a, b)| (*a, *b)).collect(),
        facts: p.carry_log.clone(),
        stats: StatDelta::delta(before, p.stats),
        draws,
        names: Vec::new(),
    };
    if mem_on()
        && let Some(names) = uen_names(p, &variant.func.1)
    {
        variant.names = names;
        let mut variants: Vec<PipeVariant> =
            mem_ready(&k).map(|v| v.as_ref().clone()).unwrap_or_default();
        match variants
            .iter_mut()
            .find(|v| v.reads == variant.reads && v.ran == variant.ran)
        {
            Some(v) => *v = variant.clone(),
            None => variants.push(variant.clone()),
        }
        claim.filled = true;
        match &claim.pend {
            Some(t) => mem_fill(k, t, variants),
            None => mem_retain(k, std::sync::Arc::new(variants)),
        }
    }
    if enabled()
        && let Some(path) = file_path("pipe", &k)
    {
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
    let (func, names, derived, facts, stats, out_hash, k) = {
        let ent = stepfile(an)?
            .iter()
            .find(|e| e.step == s.name && e.in_hash == cur)?;
        let v = ent.variants.iter().find(|v| reads_ok(&v.reads))?;
        (
            v.func.clone(),
            v.names.clone(),
            v.derived.clone(),
            v.facts.clone(),
            v.stats,
            v.out_hash,
            v.k,
        )
    };
    if let Some(func) = func {
        if !apply_named(p, func, &derived, &names) {
            return None;
        }
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
    // A stored func needs its user names resolvable; `None` from
    // `uen_names` means they aren't — this variant can't publish.
    let Some(names) = (match &func {
        Some((_, params)) => uen_names(p, params),
        None => Some(Vec::new()),
    }) else {
        return k;
    };
    let variant = Variant {
        reads: reads.into_iter().collect(),
        out_hash,
        func,
        derived: p.derived.iter().map(|(a, b)| (*a, *b)).collect(),
        facts: p.carry_log[cmark..].to_vec(),
        stats: StatDelta::delta(&before, p.stats),
        names,
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
    if crate::config::env_is("PLIRON_MCPROF", "1") {
        eprintln!(
            "mcprof lookup={}ms store={}ms record={}x",
            crate::pipeline::MC_LOOKUP_T.load(std::sync::atomic::Ordering::Relaxed) / 1_000_000,
            crate::pipeline::MC_STORE_T.load(std::sync::atomic::Ordering::Relaxed) / 1_000_000,
            pliron_cfg::RECORD_COUNT.load(std::sync::atomic::Ordering::Relaxed),
        );
        for (k, n) in pliron_cfg::record_keys_top().into_iter().take(12) {
            eprintln!("  {n:>8}x {k}");
        }
    }
    if !an.stepfile_dirty {
        return;
    }
    if let (Some(k), Some(file)) = (an.pk, an.stepfile.take())
        && let Some(path) = file_path("step", &k)
    {
        store(&path, &file);
    }
}
