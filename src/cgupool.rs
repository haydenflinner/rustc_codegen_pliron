//! Cross-CGU parallelism for the deferred pipeline.
//!
//! `compile_codegen_unit` runs on the main thread (it is TyCtxt-bound)
//! and stops after the mono-item `define`s: each CGU's pliron
//! `Context`+`State` — plain `Send` data — is queued here as a job.
//! `WriteBackendMethods::optimize`, running on rustc's jobserver-token
//! worker threads, pulls whatever job is runnable until *its own*
//! module's output exists, so workers never sit blocked on unscheduled
//! work — a parked continuation's prerequisites are always
//! already-submitted jobs, by construction of the ordinals.
//!
//! The only cross-CGU edge is the xfacts merge
//! (`State::facts_publish`/`facts_adopt`): a CGU's post-publish tail
//! parks until every lower-ordained participant has published. Ordinals
//! follow submission order, which is also the order serial codegen runs
//! `share_facts`, so each CGU adopts exactly the facts serial codegen
//! gives it — objects stay byte-identical to the serial path.
//!
//! Off switches: `PLIRON_CGUPAR=0`, `PLIRON_OPT_BISECT` (the bisect
//! counter must number pass applications deterministically) and
//! `--jobs-frontend` (pre-compiled CGUs scramble submission order) all
//! fall back to running `finish_module` inline on the main thread.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, LazyLock, Mutex};

use cranelift_codegen::isa::TargetIsa;
use cranelift_module::Linkage;
use pliron::builtin::op_interfaces::SingleBlockRegionInterface;
use pliron::builtin::ops::ModuleOp;
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::printable::Printable;
use pliron::r#type::{TypeHandle, TypedHandle};
use pliron_llvm::ops::FuncOp;
use pliron_llvm::types::{FuncType, PointerType, VoidType};
use rustc_codegen_ssa::back::write::SharedEmitter;
use rustc_data_structures::profiling::SelfProfilerRef;
use rustc_errors::DiagCtxt;
use rustc_middle::ty::TyCtxt;
use rustc_session::config::OptLevel;
use rustc_span::fatal_error::FatalErrorMarker;

use crate::context::{Exts, FuncInfo, State};
use crate::pt;
use crate::{abi, callgvn, constload, domcheck, guardelim, inline, instcombine, lutcorr, RustcProf};
use crate::{env_get, env_is, env_set, wasm};
use crate::{lower, memcpyopt, nnret, nocapture, nounwind, nowrite, pass_enabled, pgo, phisimp};
use crate::{retarg, spec, sroa, willreturn, xoridem};

/// The pliron half of a `CodegenCx`: everything `finish_module` needs
/// once the rustc-side work is done. `Send` across the job boundary.
pub(crate) struct Parts {
    pub ctx: Context,
    pub st: State,
    pub module: Ptr<Operation>,
}

/// `Session`-derived facts the deferred pipeline needs, captured once
/// per `codegen_crate` on the main thread (`Session` is not `Sync`).
pub(crate) struct CrateCfg {
    /// `opts.optimize != No` — the IR-opt pipeline runs at all.
    pub optimize: bool,
    /// This crate participates in the xfacts merge (`PLIRON_XFACTS`).
    pub xfacts: bool,
    pub wasm: bool,
    /// `panic_strategy() == Unwind` — lowering needs the EH paths.
    pub unwind: bool,
    /// COFF+unwind: emulated EH makes `nounwind` load-bearing in lower.
    pub coff_eh: bool,
    /// `--emit llvm-asm` or `PLIRON_DUMP`: keep the pre-opt IR text.
    pub emit_ir: bool,
    /// `PLIRON_HOT` names this crate: jumpthread runs in the CLIF passes.
    pub hot: bool,
    pub isa: Option<Arc<dyn TargetIsa>>,
    pub wasm_features: Vec<String>,
    pub sse42: bool,
    pub fmt: pgo::ObjFmt,
    pub aarch64: bool,
    /// `small`/`small_limit`/`ihot`: the inline cost knobs, precomputed.
    pub small: bool,
    pub small_limit: usize,
    pub ihot: usize,
}

impl CrateCfg {
    pub fn new(tcx: TyCtxt<'_>) -> Self {
        let sess = tcx.sess;
        let wasm = sess.target.arch == rustc_target::spec::Arch::Wasm32;
        let opt = sess.opts.optimize;
        let optimize = opt != OptLevel::No;
        let size_opt = matches!(opt, OptLevel::Size | OptLevel::SizeMin);
        let small = wasm || size_opt;
        // wasm only: callsites in loops get WASM_HOT x the flat limit —
        // std `hash_one` into hashbrown's probe loop. -Os/-Oz stay flat.
        let ihot = if wasm && !size_opt {
            env_get("PLIRON_INLINE_HOT")
                .and_then(|s| s.parse().ok())
                .unwrap_or(inline::WASM_HOT)
        } else {
            0
        };
        let unwind = sess.panic_strategy() == rustc_target::spec::PanicStrategy::Unwind;
        let binary_format = sess.target.options.binary_format;
        let fmt = match binary_format {
            rustc_target::spec::BinaryFormat::MachO => pgo::ObjFmt::MachO,
            rustc_target::spec::BinaryFormat::Coff => pgo::ObjFmt::Coff,
            rustc_target::spec::BinaryFormat::Wasm => pgo::ObjFmt::Wasm,
            _ => pgo::ObjFmt::Elf,
        };
        let emit_ir = sess
            .opts
            .output_types
            .contains_key(&rustc_session::config::OutputType::LlvmAssembly)
            || env_set("PLIRON_DUMP");
        let tailcalls = tcx.features().enabled(rustc_span::sym::explicit_tail_calls)
            || env_set("PLIRON_TAILCALL");
        if wasm && env_get("PLIRON_WASM_CLIF") == Some("0") {
            sess.dcx().warn(
                "PLIRON_WASM_CLIF=0 ignored: the dialect -> waffle path was \
                 removed; wasm always uses the CLIF pipeline",
            );
        }
        CrateCfg {
            optimize,
            xfacts: pass_enabled("PLIRON_XFACTS"),
            wasm,
            unwind,
            coff_eh: binary_format == rustc_target::spec::BinaryFormat::Coff && unwind,
            emit_ir,
            hot: env_get("PLIRON_HOT")
                .is_some_and(|c| c == tcx.crate_name(rustc_span::def_id::LOCAL_CRATE).as_str()),
            isa: if wasm {
                None
            } else {
                Some(crate::build_isa(sess, tailcalls))
            },
            wasm_features: if wasm {
                wasm::target_features(&sess.target, &sess.opts)
            } else {
                Vec::new()
            },
            sse42: sess
                .internal_target_features
                .contains(&rustc_span::Symbol::intern("sse4.2")),
            fmt,
            aarch64: sess.target.arch == rustc_target::spec::Arch::AArch64,
            small,
            small_limit: if wasm {
                inline::WASM_LIMIT
            } else {
                inline::SMALL_LIMIT
            },
            ihot,
        }
    }
}

/// `lower::Host` on a worker: no `Session` — the profiler is the only
/// live rustc handle; user-facing fatals travel as [`HostFatal`] panics
/// and are relayed to the shared emitter by `run_until`.
struct WorkerHost {
    prof: Arc<RustcProf>,
    sse42: bool,
}

/// Panic payload for a clean (non-ICE) `Host::fatal` raised inside a job.
struct HostFatal(String);

impl lower::Host for WorkerHost {
    fn prof(&self) -> Arc<dyn lower::Prof> {
        self.prof.clone()
    }
    fn fatal(&self, msg: String) -> ! {
        std::panic::panic_any(HostFatal(msg))
    }
    fn sse42(&self) -> bool {
        self.sse42
    }
}

/// What a finished job hands back to its `PlironModule`.
pub(crate) struct Finished {
    pub obj: Vec<u8>,
    pub ir: String,
    pub asm: String,
}

/// xfacts behavior at the merge point in [`iropt_head`].
pub(crate) enum FactsMode {
    /// This compile does not merge facts at all.
    Skip,
    /// Publish + immediately adopt (serial ordering).
    Share,
    /// Publish only — the adopt half waits for the ordinal prefix.
    Publish(u32),
}

enum Step {
    /// iropt head through facts-publish; parks afterwards.
    A,
    /// adopt + iropt tail + emit; runs once the ordinal prefix published.
    B,
    /// Non-participant: the whole finish in one go, never parks.
    Whole,
}

struct Job {
    /// Dense id per submitted module — `done` results are keyed on it.
    key: u32,
    /// Participant ordinal (prefix sequencing); `u32::MAX` when `Whole`.
    ord: u32,
    name: String,
    cfg: Arc<CrateCfg>,
    parts: Parts,
    step: Step,
    ir: String,
    /// Stage-B wait set: participant ordinals this CGU must see
    /// published before adopting facts — the ords that own a symbol it
    /// declares without a body (see `Store::owners`). Under
    /// `PLIRON_XNEED=0` it is the full ordinal prefix, the old cover
    /// semantics. Empty for `Whole` jobs.
    needs: Vec<u32>,
    /// Two-phase facts (`hold`): stage B adopts from every participant.
    adopt_all: bool,
    /// Wall time spent in iropt stages A+B — the `*iropt*` timing row.
    /// Parked time between publish and adopt does not count.
    iropt: std::time::Duration,
}

enum RunOut {
    /// The job produced a module.
    Done { key: u32, out: Finished },
    /// Stage A completed: published under `job.ord`, stage B pending.
    Published { job: Job },
}

#[derive(Default)]
struct Store {
    next_key: u32,
    next_ord: u32,
    ready: VecDeque<Job>,
    /// Stage-B jobs keyed by participant ordinal, waiting on `needs`.
    parked: BTreeMap<u32, Job>,
    /// Participant ordinals that have published.
    published: rustc_hash::FxHashSet<u32>,
    /// Symbol -> ords of CGUs that submitted a body for it. A stage-B's
    /// fact adoption only reads entries for its own bodiless decls, so
    /// it need only wait on those owners — the full prefix is a
    /// superset (`PLIRON_XNEED=0` restores it). Filled at `submit`,
    /// which is ord-ordered, so an adopting CGU always sees every
    /// owner below its own ordinal.
    owners: rustc_hash::FxHashMap<String, Vec<u32>>,
    done: rustc_hash::FxHashMap<u32, Result<Finished, Box<dyn std::any::Any + Send>>>,
    /// Two-phase facts (`PLIRON_XFACTS_ALL=1` under rustc's thin-local
    /// LTO): a published job waits here, keyed by its `done` key, instead
    /// of parking on lower ordinals. `optimize` returns with the module
    /// still pending; rustc runs `run_thin_lto` only after every module's
    /// `optimize`, so by `finish_held` every CGU has published and the
    /// adopt set is the same however the workers were scheduled. No
    /// worker ever waits on the barrier — rustc's coordinator owns it.
    hold: bool,
    held: rustc_hash::FxHashMap<u32, Job>,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(Default::default);
static CV: Condvar = Condvar::new();

thread_local! {
    /// Per-crate cfg captured by `begin` (main thread only).
    static CRATE_CFG: RefCell<Option<Arc<CrateCfg>>> = const { RefCell::new(None) };
}

/// Whether this compile defers CGU finishing to the worker pool. Anything
/// that must keep a deterministic *order of operations* across CGUs —
/// opt-bisect's global counter, or a parallel-frontend pre-compile that
/// scrambles submission order — takes the serial path instead.
pub(crate) fn enabled(tcx: TyCtxt<'_>) -> bool {
    pass_enabled("PLIRON_CGUPAR")
        && pliron_cfg::bisect_limit().is_none()
        && tcx.sess.opts.jobs.frontend.is_none()
}

/// Called once at the top of `codegen_crate`: captures the crate-level
/// cfg, clears the facts table and any leftovers from a previous compile.
pub(crate) fn begin(tcx: TyCtxt<'_>) {
    let cfg = Arc::new(CrateCfg::new(tcx));
    CRATE_CFG.with(|c| *c.borrow_mut() = Some(cfg));
    pliron_ir_opt::state::reset_facts();
    *STORE.lock().unwrap() = Store {
        hold: two_phase(tcx),
        ..Store::default()
    };
}

/// `PLIRON_XFACTS_ALL=1` and rustc chose thin-local LTO for this crate
/// (multi-CGU, optimizing): stage B moves behind the LTO barrier. Not
/// under `-Clinker-plugin-lto`: rustc then skips the thin step, and the
/// held modules reached `codegen` unfinished — cargo passes it to the
/// dependencies of an `lto = true` build (prql, nushell: every rlib got
/// an empty object).
pub(crate) fn two_phase(tcx: TyCtxt<'_>) -> bool {
    pliron_cfg::env_is("PLIRON_XFACTS_ALL", "1")
        && enabled(tcx)
        && tcx.sess.lto() == rustc_session::config::Lto::ThinLocal
        && !tcx.sess.opts.cg.linker_plugin_lto.enabled()
}

/// Whether this compile holds published jobs for `finish_held`.
pub(crate) fn holding() -> bool {
    STORE.lock().unwrap().hold
}

/// Hand a finished-define CGU to the pool; returns the `done` key the
/// owning module waits on in `optimize`.
pub(crate) fn submit(name: String, ctx: Context, st: State, module: Ptr<Operation>) -> u32 {
    let cfg = CRATE_CFG
        .with(|c| c.borrow().clone())
        .expect("cgupool::begin must run before submit");
    let participates = cfg.optimize && cfg.xfacts;
    let mut g = STORE.lock().unwrap();
    let key = g.next_key;
    g.next_key += 1;
    let (ord, step) = if participates {
        let ord = g.next_ord;
        g.next_ord += 1;
        (ord, Step::A)
    } else {
        (u32::MAX, Step::Whole)
    };
    pliron_cfg::mark(&format!("submit {name} ord={ord} key={key}"));
    if ord != u32::MAX && pass_enabled("PLIRON_XNEED") {
        // Same predicate `facts_publish` exports under: emitted bodies
        // only. Owners can only shrink before publish (dead_fns), so
        // this is a superset — a stale entry costs a waiter one publish
        // it did not strictly need, never a missed dependency.
        for (sym, f) in st.funcs.iter() {
            if f.linkage != Linkage::Import && pliron_ir_opt::has_body(&ctx, f.op) {
                g.owners.entry(sym.clone()).or_default().push(ord);
            }
        }
    }
    g.ready.push_back(Job {
        key,
        ord,
        name,
        cfg,
        parts: Parts { ctx, st, module },
        step,
        ir: String::new(),
        needs: Vec::new(),
        adopt_all: false,
        iropt: std::time::Duration::ZERO,
    });
    CV.notify_one();
    key
}

/// `optimize`'s side of the store: work jobs until module `key`'s output
/// exists, then hand it back. Pulling any runnable job — not just the
/// caller's own — is what makes the facts barrier safe: a token-holding
/// worker is never blocked while runnable work remains.
/// Pop one runnable job from `ready`, run it on this thread, and fold
/// the outcome back into the store (`done`/`published`/`parked`).
/// Returns `false` when nothing was runnable. Shared by `run_until`'s
/// wait loop and — through `lower`'s `CGU_STEAL` slot — the phase-(B)
/// drain, so a thread parked on either queue can lend its core to the
/// other.
fn step_once(prof: &SelfProfilerRef, emitter: &SharedEmitter) -> bool {
    let mut g = STORE.lock().unwrap();
    let Some(job) = g.ready.pop_front() else {
        return false;
    };
    let jkey = job.key;
    let jord = job.ord;
    drop(g);
    let r = catch_unwind(AssertUnwindSafe(|| run_job(job, prof)));
    g = STORE.lock().unwrap();
    match r {
        Ok(RunOut::Done { key, out }) => {
            g.done.insert(key, Ok(out));
        }
        Ok(RunOut::Published { job }) if g.hold => {
            g.published.insert(job.ord);
            g.held.insert(job.key, job);
        }
        Ok(RunOut::Published { mut job }) => {
            g.published.insert(job.ord);
            // Resolve the stage-B wait set now — `funcs` is final (the
            // job parks until stage-B adopts, nothing mutates it in
            // between). adopt only ever looks up this CGU's bodiless
            // decls, so waiting on the ords that OWN those symbols is
            // equivalent to the full prefix: every ord publishing a
            // queried sym is an owner, so merged `copies` counts and
            // `proven` checks for queried syms are unchanged; ords
            // contributing no queried sym merge nothing an adopt reads.
            job.needs = if pass_enabled("PLIRON_XNEED") {
                let mut v = Vec::new();
                for (sym, f) in job.parts.st.funcs.iter() {
                    if pliron_ir_opt::has_body(&job.parts.ctx, f.op) {
                        continue;
                    }
                    if let Some(ords) = g.owners.get(sym) {
                        v.extend(ords.iter().copied().filter(|&o| o < job.ord));
                    }
                }
                v.sort_unstable();
                v.dedup();
                v
            } else {
                (0..job.ord).collect()
            };
            if covered(&g.published, &job.needs) {
                g.ready.push_back(job);
            } else {
                g.parked.insert(job.ord, job);
            }
            release_parked(&mut g);
        }
        Err(p) => {
            let p = match p.downcast::<HostFatal>() {
                Ok(hf) => {
                    emit_err(emitter, hf.0);
                    Box::new(FatalErrorMarker) as Box<dyn std::any::Any + Send>
                }
                Err(p) => {
                    let msg = p
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| p.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic");
                    emit_err(emitter, format!("pliron worker panic: {msg}"));
                    p
                }
            };
            g.done.insert(jkey, Err(p));
            // A job that never reached publish would stall every
            // higher-ordinal parked job (and the coordinator, which
            // joins workers on abort). Count the ordinal as covered:
            // adopters merge only `by_ord` entries, so a CGU that
            // never published simply contributes no facts — the
            // conservative result, and the compile is dying anyway.
            if jord != u32::MAX {
                g.published.insert(jord);
                release_parked(&mut g);
            }
        }
    }
    CV.notify_all();
    true
}

/// Two-phase `optimize`: work jobs until module `key` has published
/// (`None`: held for `finish_held`) or finished outright (a
/// non-participant job).
pub(crate) fn run_until_published(
    key: u32,
    prof: &SelfProfilerRef,
    emitter: &SharedEmitter,
) -> Option<Finished> {
    let mut g = STORE.lock().unwrap();
    loop {
        if g.held.contains_key(&key) {
            return None;
        }
        if let Some(r) = g.done.remove(&key) {
            return Some(match r {
                Ok(f) => f,
                Err(p) => resume_unwind(p),
            });
        }
        drop(g);
        if step_once(prof, emitter) {
            g = STORE.lock().unwrap();
            continue;
        }
        g = STORE.lock().unwrap();
        if !g.done.contains_key(&key) && !g.held.contains_key(&key) && g.ready.is_empty() {
            g = CV.wait(g).unwrap();
        }
    }
}

/// Two-phase stage B: run held module `key` (adopting every CGU's facts)
/// and return its output. Called from `optimize_and_codegen_thin`.
/// `all = false` keeps prefix adoption — for a module rustc codegens
/// outside the LTO barrier (the allocator shim, optimized first).
pub(crate) fn finish_held(key: u32, all: bool, prof: &SelfProfilerRef, emitter: &SharedEmitter) -> Finished {
    {
        let mut g = STORE.lock().unwrap();
        let mut job = g.held.remove(&key).expect("finish_held: module not held");
        job.adopt_all = all;
        g.ready.push_front(job);
        CV.notify_all();
    }
    run_until(key, prof, emitter)
}

pub(crate) fn run_until(key: u32, prof: &SelfProfilerRef, emitter: &SharedEmitter) -> Finished {
    // The phase-(B) drain inside `finish_b` parks on its own result
    // channel when the fn pool is momentarily empty — install this
    // thread's `step_once` so it can run another CGU's work instead
    // (see lower::CGU_STEAL). Cleared before returning.
    let sprof = prof.clone();
    let sem = emitter.clone();
    lower::set_cgu_steal(Some(std::rc::Rc::new(move || {
        step_once(&sprof, &sem)
    })));
    let mut g = STORE.lock().unwrap();
    let out = loop {
        if let Some(r) = g.done.remove(&key) {
            // Clear before the `resume_unwind` path too — the TLS slot
            // outlives the loop break otherwise.
            lower::set_cgu_steal(None);
            break match r {
                Ok(f) => f,
                Err(p) => resume_unwind(p),
            };
        }
        // CGU work first, then the shared fn pool; park only when both
        // are empty. The wait's recheck covers pushes that landed
        // while the lock was down (their notify went to nobody).
        drop(g);
        if step_once(prof, emitter)
            || (pass_enabled("PLIRON_CGUSTEAL")
                && pliron_clif_lower::lower::steal_fnjob())
        {
            g = STORE.lock().unwrap();
            continue;
        }
        g = STORE.lock().unwrap();
        if !g.done.contains_key(&key) && g.ready.is_empty() {
            g = CV.wait(g).unwrap();
        }
    };
    out
}

/// Every ordinal in `needs` has published.
fn covered(published: &rustc_hash::FxHashSet<u32>, needs: &[u32]) -> bool {
    needs.iter().all(|o| published.contains(o))
}

/// Release parked stage-B jobs whose `needs` are now fully published.
fn release_parked(g: &mut Store) {
    let ready: Vec<u32> = g
        .parked
        .iter()
        .filter(|(_, j)| covered(&g.published, &j.needs))
        .map(|(&o, _)| o)
        .collect();
    for o in ready {
        let j = g.parked.remove(&o).unwrap();
        g.ready.push_back(j);
    }
}

fn emit_err(emitter: &SharedEmitter, msg: String) {
    DiagCtxt::new(Box::new(emitter.clone())).handle().err(msg);
}

fn run_job(mut job: Job, prof: &SelfProfilerRef) -> RunOut {
    let host = WorkerHost {
        prof: Arc::new(RustcProf(prof.clone())),
        sse42: job.cfg.sse42,
    };
    match job.step {
        Step::A => {
            pliron_cfg::mark(&format!("stageA+ {} ord={}", job.name, job.ord));
            job.ir = finish_pre(
                &mut job.parts.ctx,
                &mut job.parts.st,
                job.parts.module,
                &job.name,
                &job.cfg,
            );
            let _siro = tracing::info_span!(target: "pliron::phase", "iropt").entered();
            let _pgiro = prof.generic_activity("pliron_iropt");
            let t = std::time::Instant::now();
            iropt_head(
                &mut job.parts.ctx,
                &mut job.parts.st,
                prof,
                &job.name,
                &job.cfg,
                FactsMode::Publish(job.ord),
            );
            job.iropt += t.elapsed();
            job.step = Step::B;
            pliron_cfg::mark(&format!("publish {} ord={}", job.name, job.ord));
            RunOut::Published { job }
        }
        Step::B => {
            pliron_cfg::mark(&format!("stageB+ {} ord={}", job.name, job.ord));
            let _siro = tracing::info_span!(target: "pliron::phase", "iropt").entered();
            let _pgiro = prof.generic_activity("pliron_iropt");
            let t = std::time::Instant::now();
            iropt_tail(
                &mut job.parts.ctx,
                &mut job.parts.st,
                prof,
                &job.name,
                &job.cfg,
                Some((job.ord, job.adopt_all)),
            );
            job.iropt += t.elapsed();
            let out = finish_b(
                &job.parts.ctx,
                &mut job.parts.st,
                job.parts.module,
                &job.name,
                &job.cfg,
                &host,
                job.ir,
                job.iropt,
            );
            pliron_cfg::mark(&format!("doneB {} ord={}", job.name, job.ord));
            RunOut::Done { key: job.key, out }
        }
        Step::Whole => {
            job.ir = finish_pre(
                &mut job.parts.ctx,
                &mut job.parts.st,
                job.parts.module,
                &job.name,
                &job.cfg,
            );
            if job.cfg.optimize {
                let _siro = tracing::info_span!(target: "pliron::phase", "iropt").entered();
                let _pgiro = prof.generic_activity("pliron_iropt");
                let t = std::time::Instant::now();
                iropt_head(
                    &mut job.parts.ctx,
                    &mut job.parts.st,
                    prof,
                    &job.name,
                    &job.cfg,
                    FactsMode::Skip,
                );
                iropt_tail(
                    &mut job.parts.ctx,
                    &mut job.parts.st,
                    prof,
                    &job.name,
                    &job.cfg,
                    None,
                );
                job.iropt += t.elapsed();
            }
            let out = finish_b(
                &job.parts.ctx,
                &mut job.parts.st,
                job.parts.module,
                &job.name,
                &job.cfg,
                &host,
                job.ir,
                job.iropt,
            );
            RunOut::Done { key: job.key, out }
        }
    }
}

// ---- pliron-side helpers, ported from `CodegenCx` (which is tcx-bound). ---

fn type_func(ctx: &mut Context, args: &[TypeHandle], ret: TypeHandle) -> TypeHandle {
    FuncType::get(ctx, ret, args.to_vec(), false).into()
}

fn type_ptr(ctx: &mut Context) -> TypeHandle {
    PointerType::get(ctx, 0).into()
}

fn type_void(ctx: &mut Context) -> TypeHandle {
    VoidType::get(ctx).into()
}

fn ident(st: &mut State, sym: &str) -> Identifier {
    if let Some(i) = st.sym_to_ident.get(sym) {
        return i.clone();
    }
    st.counter += 1;
    let mut s = format!("s{}_", st.counter);
    s.extend(sym.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '_'
        }
    }));
    let id = Identifier::try_new(s.clone()).unwrap();
    st.sym_to_ident.insert(sym.to_string(), id.clone());
    st.ident_to_sym.insert(s, sym.to_string());
    id
}

fn append_to_module(ctx: &mut Context, module: Ptr<Operation>, op: Ptr<Operation>) {
    let m = Operation::get_op::<ModuleOp>(module, ctx).unwrap();
    m.append_operation(ctx, op, 0);
}

fn declare_fn_sym(
    ctx: &mut Context,
    st: &mut State,
    module: Ptr<Operation>,
    sym: &str,
    fn_ty: TypeHandle,
    linkage: Linkage,
    exts: Exts,
) -> Ptr<Operation> {
    if let Some(f) = st.funcs.get_mut(sym) {
        if linkage != Linkage::Import {
            f.linkage = linkage;
            f.exts = exts;
            f.ty = fn_ty;
        }
        return f.op;
    }
    let id = ident(st, sym);
    let op = {
        let fty = TypedHandle::<FuncType>::from_handle(fn_ty, ctx).unwrap();
        let op = FuncOp::new(ctx, id, fty).get_operation();
        append_to_module(ctx, module, op);
        op
    };
    st.funcs.insert(
        sym.to_string(),
        FuncInfo {
            op,
            ty: fn_ty,
            linkage,
            exts,
            no_inline: false,
            always_inline: false,
            inline_hint: false,
            cold: false,
            nounwind: false,
            odr: false,
            sret_ty: None,
            frozen: Vec::new(),
            noalias: Vec::new(),
            nonnull: Vec::new(),
            nparams: 0,
            tail_cc: false,
        },
    );
    op
}

/// `PLIRON_DUMPFN=<substr>`: dump the pliron IR of matching functions
/// after select IR-opt phases (`pliron::dump` tracing target) — the
/// per-function counterpart to `PLIRON_DUMP_OPT`'s whole-module dumps.
fn dumpfn(ctx: &Context, st: &State, phase: &str) {
    let Some(want) = env_get("PLIRON_DUMPFN") else {
        return;
    };
    for (name, f) in &st.funcs {
        if !name.contains(&want) || !pliron_ir_opt::has_body(ctx, f.op) {
            continue;
        }
        tracing::info!(target: "pliron::dump", "==== {phase} {name} ====\n{}", f.op.deref(ctx).disp(ctx));
    }
}

fn print_ir(ctx: &Context, module: Ptr<Operation>) -> String {
    module.deref(ctx).disp(ctx).to_string()
}

// ---- the deferred pipeline, shared with the serial `finish_module`. ----

/// Everything before IR-opt proper: the `--emit llvm-asm`/`PLIRON_DUMP`
/// print, the O0 always-inline round, PGO instrument/load.
pub(crate) fn finish_pre(
    ctx: &mut Context,
    st: &mut State,
    module: Ptr<Operation>,
    name: &str,
    cfg: &CrateCfg,
) -> String {
    let ir = if cfg.emit_ir {
        print_ir(ctx, module)
    } else {
        String::new()
    };
    if env_set("PLIRON_DUMP") && pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::dump", "==== {name} ====\n{ir}")
    }
    if cfg.optimize {
        pliron_ir_opt::dbg::drop_spills(ctx, st);
    }
    if !cfg.optimize && pass_enabled("PLIRON_ALWAYS_INLINE") {
        inline::run(
            ctx,
            st,
            true,
            inline::SMALL_LIMIT,
            0,
            None,
            true,
            None,
            &mut None,
        );
        domcheck::run(ctx, st, "always-inline");
        inline::dead_fns(ctx, st);
    }
    // PGO: `PLIRON_PGO=<dir>` instruments every body (source-level
    // attribution -- the inliner clones counters, keeping counts on the
    // callee), `PLIRON_PGO_USE=<file>` loads a profile before the
    // pipeline. Both run after the O0 always-inline round so
    // fingerprints are computed at the same CFG stage in gen and use
    // builds.
    if env_set("PLIRON_PGO") || env_set("PLIRON_PGO_USE") {
        let fmt = cfg.fmt;
        let mut asm = None;
        if env_set("PLIRON_PGO") {
            let (hdr, ctor, _) = pgo::instrument(ctx, st, name, fmt);
            asm = pgo::ctor_asm(&hdr, &ctor, cfg.aarch64, fmt);
        }
        if let Some(f) = env_get("PLIRON_PGO_USE") {
            pgo::load(ctx, st, f);
        }
        if let Some(a) = asm {
            // The ctor's `jmp __pliron_prof_register` resolves through the
            // splice's symbol map like any other callee -- declare the
            // import so Mach-O gets its `_` mangle (and every format a
            // real undefined symbol rather than an asm-only name).
            let ptr = type_ptr(ctx);
            let void = type_void(ctx);
            let fty = type_func(ctx, &[ptr], void);
            declare_fn_sym(
                ctx,
                st,
                module,
                "__pliron_prof_register",
                fty,
                Linkage::Import,
                Exts::default(),
            );
            st.asm.push_str(&a);
        }
    }
    ir
}

/// IR-opt through the xfacts point. `facts` picks the merge mode; a
/// `Publish` job then parks until its ordinal prefix has published.
pub(crate) fn iropt_head(
    ctx: &mut Context,
    st: &mut State,
    prof: &SelfProfilerRef,
    cgu: &str,
    cfg: &CrateCfg,
    facts: FactsMode,
) {
    pliron_ir_opt::opt_skip_init(ctx, st);
    // Copies nothing calls yet are emit-only fodder — mark them before
    // the IPO scans so `bodied`/`opt_body` leave them alone entirely.
    if st.earlydead {
        pt!(prof, cgu, "earlydead", inline::mark_dead_unopt(ctx, st));
    }
    let small = cfg.small;
    let small_limit = cfg.small_limit;
    let hot = cfg.ihot;
    if pass_enabled("PLIRON_FROZEN") {
        pt!(
            prof,
            cgu,
            "frozen",
            st.frozen = crate::context::frozen_values(ctx, st).into_iter().collect()
        );
    }
    if pass_enabled("PLIRON_NOALIAS_FWD") {
        pt!(
            prof,
            cgu,
            "noalias",
            st.noalias = crate::context::noalias_values(ctx, st)
                .into_iter()
                .map(|(v, d, w)| (v, (d, w)))
                .collect()
        );
    }
    if pass_enabled("PLIRON_NNARGS") {
        pt!(
            prof,
            cgu,
            "nnargs",
            st.nnargs = crate::context::nonnull_values(ctx, st)
                .into_iter()
                .collect()
        );
    }
    let mut adj = None;
    let d1 = pt!(
        prof,
        cgu,
        "inline",
        inline::run(
            ctx,
            st,
            small,
            small_limit,
            hot,
            None,
            false,
            None,
            &mut adj
        )
    );
    // Second round: callsites that just moved into a hot caller (e.g.
    // `get`'s `hash_one` landing inside `main`'s probe loop) never got
    // scored in their new context -- re-run so the loop bonus applies.
    // Incremental: rescore only the callers round 1 inlined into, plus
    // callers of callees that lost a callsite (a now-single callee can
    // newly inline free there).
    if pass_enabled("PLIRON_INLINE_ITER") && !d1.callers.is_empty() {
        let mut touched = d1.callers;
        if let Some(a) = &adj {
            for c in &d1.callees {
                if let Some(v) = a.callers_of.get(c) {
                    touched.extend(v.iter().cloned());
                }
            }
        }
        pt!(
            prof,
            cgu,
            "inline2",
            inline::run(
                ctx,
                st,
                small,
                small_limit,
                hot,
                None,
                false,
                Some(&touched),
                &mut adj,
            )
        );
    }
    pt!(
        prof,
        cgu,
        "domcheck:inline",
        domcheck::run(ctx, st, "inline")
    );
    // Inlining kills most copy callsites — mark the dead ones now so
    // spec/sroa/constload/devirt never walk their emit-only bodies.
    // (devirt-inline below can still inline into a live caller; a copy
    // marked here stays callerless for good: aiming a call at it needs
    // a `Sym` const, which being untaken rules out.)
    if st.earlydead {
        pt!(prof, cgu, "earlydead2", inline::mark_dead_unopt(ctx, st));
    }
    if pass_enabled("PLIRON_SPEC") {
        pt!(prof, cgu, "spec", spec::run(ctx, st));
        pt!(prof, cgu, "domcheck:spec", domcheck::run(ctx, st, "spec"));
    }
    // `grew` tracks call-graph growth AFTER the round-1 fact analyses
    // (inline rounds 1+2 already sit inside every round-1 window): only
    // devirt's indirect->direct call rewrites and devirt-inline splices
    // can change a monotone fact fixpoint's output; sroa escape copies
    // are checked separately at the gates that stores/captures affect.
    let mut grew = false;
    // Emulated EH (wasm, COFF) unwinds through ordinary calls the body
    // scan can't see -- `llvm.wasm.throw`-class entries are declared
    // `!can_unwind` externs and `pliron.eh.rethrow` is a CallIntrinsicOp —
    // so nounwind.rs models them explicitly (THROWS + the rethrow check).
    // On COFF the inference also lets lowering skip the flag check after
    // calls that can't throw.
    if pass_enabled("PLIRON_NOUNWIND") || cfg.coff_eh {
        pt!(prof, cgu, "nounwind", nounwind::run(ctx, st));
    }
    if pass_enabled("PLIRON_SRET2REG") {
        pt!(prof, cgu, "sret2reg", abi::run(ctx, st));
        pt!(prof, cgu, "domcheck:abi", domcheck::run(ctx, st, "abi"));
    }
    if env_is("PLIRON_DEADARG", "1") {
        pt!(prof, cgu, "deadarg", abi::dead_args(ctx, st));
    }
    if pass_enabled("PLIRON_INSTCOMBINE") {
        pt!(prof, cgu, "instcombine", instcombine::run(ctx, st));
        pt!(
            prof,
            cgu,
            "domcheck:ic",
            domcheck::run(ctx, st, "instcombine")
        );
    }
    if pass_enabled("PLIRON_MEMCPYOPT") {
        pt!(prof, cgu, "memcpyopt", memcpyopt::run(ctx, st));
        pt!(
            prof,
            cgu,
            "domcheck:mc",
            domcheck::run(ctx, st, "memcpyopt")
        );
    }
    if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_SROA_FWD") {
        pt!(prof, cgu, "sroa-fwd", sroa::forward(ctx, st));
        pt!(prof, cgu, "domcheck:sf", domcheck::run(ctx, st, "sroa-fwd"));
    }
    if pass_enabled("PLIRON_PHISIMP") {
        pt!(prof, cgu, "phisimp", phisimp::run(ctx, st));
        pt!(prof, cgu, "domcheck:ps", domcheck::run(ctx, st, "phisimp"));
        dumpfn(ctx, st, "phisimp");
    }
    // Run ahead of sroa: its `copyable_arg` check consults `st.nowrite`
    // and `st.nocapture` to decide which call args can take a fresh copy.
    if pass_enabled("PLIRON_NOWRITE") {
        pt!(prof, cgu, "nowrite", nowrite::run(ctx, st));
    }
    if pass_enabled("PLIRON_NOCAP") {
        pt!(prof, cgu, "nocap", nocapture::run(ctx, st));
    }
    if pass_enabled("PLIRON_SROA") {
        pt!(prof, cgu, "sroa", sroa::run(ctx, st));
        pt!(prof, cgu, "domcheck:sroa", domcheck::run(ctx, st, "sroa"));
        dumpfn(ctx, st, "sroa");
    }
    if pass_enabled("PLIRON_CONSTLOAD") {
        pt!(prof, cgu, "constload", constload::run(ctx, st));
        pt!(
            prof,
            cgu,
            "domcheck:cl",
            domcheck::run(ctx, st, "constload")
        );
        // Inlining + SROA make vtable pointers constant: call those slots
        // directly, inline them and clean up again (`PLIRON_DEVIRT=0` disables).
        if pass_enabled("PLIRON_DEVIRT") {
            let (sites, owners) = pt!(prof, cgu, "devirt", inline::devirt(ctx, st));
            pt!(prof, cgu, "domcheck:dv", domcheck::run(ctx, st, "devirt"));
            grew |= !sites.is_empty();
            if !sites.is_empty() && pass_enabled("PLIRON_DEVIRT_INLINE") {
                // Passes since the inline rounds may have moved calls —
                // rebuild the adjacency fresh.
                adj = None;
                let d3 = pt!(
                    prof,
                    cgu,
                    "devirt-inline",
                    inline::run(
                        ctx,
                        st,
                        small,
                        small_limit,
                        hot,
                        Some(&sites),
                        false,
                        Some(&owners),
                        &mut adj,
                    )
                );
                grew |= !d3.callers.is_empty();
                pt!(
                    prof,
                    cgu,
                    "domcheck:di",
                    domcheck::run(ctx, st, "devirt-inline")
                );
                if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_SROA_FWD") {
                    pt!(prof, cgu, "sroa-fwd2", sroa::forward(ctx, st));
                }
                if pass_enabled("PLIRON_PHISIMP") && pass_enabled("PLIRON_DEVIRT_PHI") {
                    pt!(prof, cgu, "phisimp2", phisimp::run(ctx, st));
                }
                if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_DEVIRT_SROA") {
                    pt!(prof, cgu, "sroa2", sroa::run(ctx, st));
                }
                if pass_enabled("PLIRON_DEVIRT_CL") {
                    pt!(prof, cgu, "constload2", constload::run(ctx, st));
                }
            }
        }
    }
    // The call graph is final after devirt-inline: every later pass only
    // deletes calls. Mark copies inlining left callerless now so the
    // whole tail (`nocap2`..`callgvn`, `dead_fns`' recounts) skips them.
    if st.earlydead {
        pt!(prof, cgu, "earlydead3", inline::mark_dead_unopt(ctx, st));
    }
    // Re-run capture inference now that devirt/constload have made
    // callee sets final: indirect calls through fn-ptr slots resolve to
    // `arg` edges, and params those callees don't keep go non-capturing.
    // The earlier run stays for sroa's `copyable_arg` check.
    // Ascent rules are monotone: only added bodies (inline), rewritten
    // call targets (devirt) or escaping copies can derive new captures;
    // without them the recompute is byte-identical to nocap1's.
    if pass_enabled("PLIRON_NOCAP") && (grew || !st.escape_copies.is_empty()) {
        pt!(prof, cgu, "nocap2", nocapture::run(ctx, st));
    }
    // IPO return-value attributes, after inlining/devirt so remaining
    // direct calls are the final set. `nnret` is consumed at lowering
    // (nonnull call results feed jumpthread); `retarg` folds call
    // results to the returned argument in place.
    if pass_enabled("PLIRON_NNRET") {
        pt!(prof, cgu, "nnret", nnret::run(ctx, st));
    }
    if pass_enabled("PLIRON_RETARG") {
        pt!(prof, cgu, "retarg", retarg::run(ctx, st));
        pt!(prof, cgu, "domcheck:ra", domcheck::run(ctx, st, "retarg"));
    }
    // Provably-dead panic paths, on the canonical post-sroa IR (the
    // bounds check's slot indirections are gone by now): a callee whose
    // only unwind path is a dead bounds-check panic becomes nounwind
    // and its writes-only-on-nonreturn nowrite=Some(false) upgrades to
    // Some(true), which dead-callsite elim consumes. nounwind/nowrite
    // re-run so the summaries see the cleaned CFG.
    if pass_enabled("PLIRON_GUARDELIM") {
        pt!(prof, cgu, "guardelim", guardelim::run(ctx, st));
    }
    // nounwind2/nowrite2 exist to re-derive the summaries after
    // guardelim cleans panic CFGs and retarg rewrites calls; those are
    // deletions only (monotone rules => stale facts stay conservative),
    // so the only growth sources worth a re-run are the devirt window
    // (`grew`) plus sroa's escape copies adding stores/captures.
    if (pass_enabled("PLIRON_NOUNWIND") || cfg.coff_eh) && grew {
        pt!(prof, cgu, "nounwind2", nounwind::run(ctx, st));
    }
    if pass_enabled("PLIRON_NOWRITE") && (grew || !st.escape_copies.is_empty()) {
        pt!(prof, cgu, "nowrite2", nowrite::run(ctx, st));
    }
    // `willreturn` on the final post-inline/devirt call set: callgvn
    // needs it to erase still-used dedups and to speculate hoists.
    if pass_enabled("PLIRON_WILLRETURN") {
        pt!(prof, cgu, "willreturn", willreturn::run(ctx, st));
    }
    // Cross-CGU contract facts: publish this CGU's emitted bodies so
    // a sibling CGU's calls to the same symbol see nocapture/nowrite/
    // nounwind/nnret/willreturn, and adopt facts for symbols this CGU
    // only declares — a foreign call binds an emitted copy the merged
    // fact was computed on. Runs ahead of callgvn, which reads
    // nowrite + willreturn, and lowering, which reads all five.
    if cfg.xfacts {
        match facts {
            FactsMode::Share => {
                pt!(prof, cgu, "xfacts", st.share_facts(ctx));
            }
            FactsMode::Publish(ord) => {
                pt!(prof, cgu, "xfacts", st.facts_publish(ctx, ord));
            }
            FactsMode::Skip => {}
        }
    }
}

/// The post-facts tail: adopt (when `adopt_ord`), dead-call elim +
/// readonly dedup + loop-call hoist, the xor idiom fold, and the
/// lowering-side flag scan + dead-fn sweep.
pub(crate) fn iropt_tail(
    ctx: &mut Context,
    st: &mut State,
    prof: &SelfProfilerRef,
    cgu: &str,
    _cfg: &CrateCfg,
    adopt_ord: Option<(u32, bool)>,
) {
    match adopt_ord {
        Some((ord, true)) => pt!(prof, cgu, "xfacts-adopt", st.facts_adopt_all(ctx, ord)),
        Some((ord, false)) => pt!(prof, cgu, "xfacts-adopt", st.facts_adopt(ctx, ord)),
        None => {}
    }
    // Dead-call elim + readonly call dedup + loop-call hoist, on the
    // final post-inline call set.
    if pass_enabled("PLIRON_CALLGVN") {
        pt!(prof, cgu, "callgvn", callgvn::run(ctx, st));
        pt!(prof, cgu, "domcheck:cg", domcheck::run(ctx, st, "callgvn"));
    }
    // `acc ^= inv` loop idiom: a hoisted call's result feeding only
    // an even-count xor recurrence leaves the loop a no-op -- erase
    // it, then re-run callgvn so the now-unused call dies too.
    if pass_enabled("PLIRON_XORIDEM") {
        // callgvn2 exists only to sweep the calls a xoridem fold
        // deadened; with zero folds the re-run is byte-identical.
        let xn = pt!(prof, cgu, "xoridem", xoridem::run(ctx, st));
        if xn > 0 && pass_enabled("PLIRON_CALLGVN") {
            pt!(prof, cgu, "callgvn2", callgvn::run(ctx, st));
            pt!(
                prof,
                cgu,
                "domcheck:cg2",
                domcheck::run(ctx, st, "callgvn2")
            );
        }
    }
    // switch -> x*s + b + corr[x] (the corsix lookup-table-correction
    // scheme): almost-linear switch results get a narrow correction-table
    // load instead of a br_table jump.
    if env_is("PLIRON_LUTCORR", "1") {
        pt!(prof, cgu, "lutcorr", lutcorr::run(ctx, st));
        pt!(prof, cgu, "domcheck:lc", domcheck::run(ctx, st, "lutcorr"));
    }
    pliron_clif_lower::scan::pass_flags(st);
    if pass_enabled("PLIRON_DEADFN") {
        inline::dead_fns(ctx, st);
    }
}

/// Post-iropt: the `*iropt*` timing line, the `PLIRON_DUMP_OPT` print,
/// then CLIF lowering (native object or wasm module).
pub(crate) fn finish_b(
    ctx: &Context,
    st: &mut State,
    module: Ptr<Operation>,
    name: &str,
    cfg: &CrateCfg,
    host: &dyn lower::Host,
    ir: String,
    iropt: std::time::Duration,
) -> Finished {
    pliron_cfg::trace("*iropt*", iropt.as_secs_f64() * 1e3, name);
    if env_set("PLIRON_DUMP_OPT") && pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::dump", "==== {name} (optimized) ====\n{}", print_ir(ctx, module))
    }
    let obj = if cfg.wasm {
        wasm::lower_to_wasm_clif(ctx, st, host, name, &cfg.wasm_features, cfg.unwind)
    } else {
        lower::lower_to_object(
            cfg.unwind,
            cfg.hot,
            ctx,
            st,
            cfg.isa.clone().expect("native isa"),
            name,
            host,
        )
    };
    let asm = std::mem::take(&mut st.asm);
    Finished { obj, ir, asm }
}
