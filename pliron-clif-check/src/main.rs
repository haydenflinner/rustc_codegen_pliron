//! `clif-check`: replay and hunt miscompiles in the CLIF pipeline without
//! rustc. Inputs are `.clif` files as the differential checker saves them
//! (`PLIRON_DIFFCHECK_DIR`), whose first line may carry the facts:
//! `; diffcheck-facts: nonnull=v0 noalias=... self=u0:N`.
//!
//!   clif-check step <name> before.clif     run one step, check it
//!   clif-check pipeline before.clif        run the default pipeline, check each step
//!   clif-check pair before.clif after.clif check two saved functions
//!   clif-check corpus <dir>                `pipeline` on every before.clif under dir
//!   clif-check bench <dir> [reps]          time `run_pipeline` over the corpus
//!   clif-check report <dir> <out.json>     pipeline over the corpus in parallel;
//!                                          per-fn size/time/verdict as JSON
//!   clif-check vcode <file> [--print]      one file: its allocated vcode
//!                                          (--print: the CLIF first)
//!   clif-check codegen <dir> <out.json>    pipeline + Cranelift (aarch64-apple,
//!                                          the backend's flags) over the corpus;
//!                                          per-fn instructions and stack accesses,
//!                                          also weighted 8^loop-depth (the
//!                                          register-allocation score)
//!                                          (--check: also diffcheck each fn,
//!                                          --jobs N, --limit N)
//!   clif-check steps                       list step names
//!
//! Options (after the subcommand args): --trials N, --seed N, --fuel N, --print.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cranelift_codegen::Context;
use cranelift_codegen::entity::EntityRef as _;
use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::Configurable;
use cranelift_module::FuncId;
use pliron_clif_opt::diffcheck::{self, Config, OwnedFacts};
use pliron_clif_opt::pipeline::{self, FnFlags, Pipe, Stats};

struct Opts {
    cfg: Config,
    print: bool,
    check: bool,
    jobs: usize,
    limit: usize,
}

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// r[impl clif.check]
fn main() -> ExitCode {
    // `PLIRON_TRACE=<env-filter>` (e.g. `pliron::jumpthread=info`) —
    // pass-internal `tracing` prof timers stay silent without a
    // subscriber.
    if let Ok(filter) = std::env::var("PLIRON_TRACE") {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_new(&filter)
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
            )
            .with_writer(std::io::stderr)
            .init();
        pliron_cfg::mark_tracing_on();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos = Vec::new();
    let mut opts = Opts {
        cfg: diffcheck::config_from_env(),
        print: false,
        check: false,
        jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
        limit: usize::MAX,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut num = || {
            it.next()
                .and_then(|v| v.parse::<u64>().ok())
                .expect("number after flag")
        };
        match a.as_str() {
            "--trials" => opts.cfg.trials = num() as u32,
            "--seed" => opts.cfg.seed = num(),
            "--fuel" => opts.cfg.fuel = num(),
            "--print" => opts.print = true,
            "--check" => opts.check = true,
            "--jobs" => opts.jobs = num().max(1) as usize,
            "--limit" => opts.limit = num() as usize,
            _ => pos.push(a.clone()),
        }
    }
    let isa = host_isa();
    let ok = match pos
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["steps"] => {
            let (f, facts) = (Function::new(), OwnedFacts::default());
            with_pipe(&*isa, f, &facts, |p| {
                for n in pipeline::selected_names(p) {
                    println!("{n}");
                }
            });
            true
        }
        ["step", name, file] => {
            let (f, facts) = load(Path::new(file));
            run_one(&*isa, name, f, &facts, &opts).0
        }
        ["pipeline", file] => pipeline_checked(&*isa, Path::new(file), &opts),
        ["pair", before, after] => {
            let (b, facts) = load(Path::new(before));
            let (a, _) = load(Path::new(after));
            report("pair", &b, &a, &facts, &opts)
        }
        ["corpus", dir] => {
            let mut files = Vec::new();
            collect(Path::new(dir), &mut files);
            // A PLIRON_CORPUS dump is flat `<name>.<n>.clif`, not
            // per-fn `before.clif` dirs — fall back to that layout.
            if files.is_empty() {
                collect_clif(Path::new(dir), &mut files);
            }
            files.sort();
            let mut bad = 0;
            for f in &files {
                println!("== {}", f.display());
                if !pipeline_checked(&*isa, f, &opts) {
                    bad += 1;
                }
            }
            println!("{} files, {bad} with mismatches", files.len());
            bad == 0
        }
        ["report", dir, out] => report_corpus(&isa, Path::new(dir), Path::new(out), &opts),
        ["codegen", dir, out] => codegen_corpus(Path::new(dir), Path::new(out), &opts),
        ["vcode", file] => {
            // One corpus file through the pipeline and Cranelift; print the
            // allocated vcode (what `codegen` scores).
            let isa = backend_isa();
            let (f, facts) = load(Path::new(file));
            let after = with_pipe(&*isa, f, &facts, |p| {
                pipeline::run_pipeline(p);
                p.clctx.func.clone()
            });
            if opts.print {
                println!("{}", after.display());
            }
            let mut ctx = Context::for_function(after);
            ctx.set_disasm(true);
            let c = ctx.compile(&*isa, &mut cranelift_codegen::control::ControlPlane::default()).expect("compile");
            let v = c.vcode.clone().unwrap_or_default();
            let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&ctx.func);
            let dt = cranelift_codegen::dominator_tree::DominatorTree::with_function(&ctx.func, &cfg);
            let mut la = cranelift_codegen::loop_analysis::LoopAnalysis::new();
            la.compute(&ctx.func, &cfg, &dt);
            let depth = |b: &str| {
                ctx.func.layout.blocks().find(|x| x.to_string() == b).map_or(0, |x| la.loop_level(x).level())
            };
            // Labels gain the CLIF loop depth: `block3: ; block7 depth=2`.
            for l in v.lines() {
                match l.split_once("; ") {
                    Some((h, c)) if h.trim_start().starts_with("block") => {
                        let d = c.split("->").map(|b| depth(b.trim())).max().unwrap_or(0);
                        println!("{h}; {c} depth={d}");
                    }
                    _ => println!("{l}"),
                }
            }
            true
        }
        ["bench", dir] => bench_corpus(&*isa, Path::new(dir), 1),
        ["bench", dir, reps] => bench_corpus(&*isa, Path::new(dir), reps.parse().expect("reps")),
        _ => {
            eprintln!(
                "{}",
                include_str!("main.rs")
                    .lines()
                    .take(12)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            return ExitCode::from(2);
        }
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn host_isa() -> std::sync::Arc<dyn TargetIsa> {
    let mut fb = cranelift_codegen::settings::builder();
    fb.set("is_pic", "true").unwrap();
    fb.set("enable_verifier", "true").unwrap();
    let flags = cranelift_codegen::settings::Flags::new(fb);
    cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
        .expect("host ISA")
        .finish(flags)
        .expect("ISA flags")
}

fn load(path: &Path) -> (Function, OwnedFacts) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut fs = cranelift_reader::parse_functions(&text)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(!fs.is_empty(), "{}: no function", path.display());
    (fs.remove(0), OwnedFacts::parse(&text))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.file_name().is_some_and(|n| n == "before.clif") {
            out.push(p);
        }
    }
}

/// Like `collect` but accepts any `*.clif` — `PLIRON_CORPUS` dumps use a
/// flat `<name>.<n>.clif` layout rather than per-fn `before.clif` dirs.
fn collect_clif(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_clif(&p, out);
        } else if p.extension().is_some_and(|x| x == "clif") {
            out.push(p);
        }
    }
}

/// A `Pipe` over `func` with the saved facts, empty pliron-side maps, and
/// every pass category on (as at -O with no `PLIRON_*` overrides).
fn with_pipe<R>(
    isa: &dyn TargetIsa,
    func: Function,
    facts: &OwnedFacts,
    k: impl FnOnce(&mut Pipe) -> R,
) -> R {
    let mut clctx = Context::for_function(func);
    let mut derived = facts.derived.clone();
    let mut stats = Stats::default();
    let flags = FnFlags {
        jumpthread: true,
        unreach: true,
        peep: true,
        tailmerge: true,
        taildup: true,
        tailrec: true,
        loadfwd: true,
        slot_dse: true,
        dse: true,
        indvars: true,
        licm: true,
        loopidiom: true,
        loopvec: true,
        bcheck: true,
        slp: true,
        keep_dbg_slots: false,
    };
    let func_id = facts
        .self_name
        .as_deref()
        .and_then(|n| n.strip_prefix("u0:"))
        .and_then(|n| n.parse().ok())
        .map(FuncId::from_u32)
        .unwrap_or(FuncId::from_u32(0));
    let x64 = isa.triple().architecture == target_lexicon::Architecture::X86_64;
    let (e1, e2, e3, e5, e6, mut e7, e8): (
        _,
        _,
        _,
        _,
        _,
        rustc_hash::FxHashSet<(cranelift_codegen::ir::Inst, u32)>,
        _,
    ) = Default::default();
    // A `; diffcheck-nocap:` header (PLIRON_CORPUS dumps) records the real
    // per-callee bounded arg positions; CLIFCHECK_NOCAP_ALL=1 marks every
    // call operand bounded for dumps without one.
    use cranelift_codegen::ir::instructions::CallInfo;
    if !facts.nocap.is_empty() {
        let set: rustc_hash::FxHashSet<(u32, u32)> = facts
            .nocap
            .iter()
            .map(|(r, a)| (r.index() as u32, *a))
            .collect();
        for b in clctx.func.layout.blocks() {
            for i in clctx.func.layout.block_insts(b) {
                let info = clctx.func.dfg.insts[i].analyze_call(
                    &clctx.func.dfg.value_lists,
                    &clctx.func.dfg.exception_tables,
                );
                if let CallInfo::Direct(fr, cargs) | CallInfo::DirectWithSig(fr, _, cargs) = info {
                    for k in 0..cargs.len() {
                        if set.contains(&(fr.index() as u32, k as u32)) {
                            e7.insert((i, k as u32));
                        }
                    }
                }
            }
        }
    } else if std::env::var("CLIFCHECK_NOCAP_ALL").is_ok() {
        use cranelift_codegen::ir::Opcode;
        for b in clctx.func.layout.blocks() {
            for i in clctx.func.layout.block_insts(b) {
                let op = clctx.func.dfg.insts[i].opcode();
                if op.is_call() {
                    let adj =
                        usize::from(matches!(op, Opcode::CallIndirect | Opcode::TryCallIndirect));
                    for k in adj..clctx.func.dfg.inst_args(i).len() {
                        e7.insert((i, (k - adj) as u32));
                    }
                }
            }
        }
    }
    let (mut nonnull, mut frozen, mut noalias, mut scoped_na, mut deref, mut e4, mut lifetimes) = (
        facts.nonnull.clone(),
        facts.frozen.clone(),
        facts.noalias.clone(),
        facts.scoped_na.clone(),
        facts.deref.clone(),
        Default::default(),
        facts.lifetimes(&clctx.func),
    );
    let mut fact_bits = cranelift_codegen::entity::SecondaryMap::new();
    let uen_syms = Default::default();
    let mut p = Pipe {
        fact_bits: &mut fact_bits,
        clctx: &mut clctx,
        isa,
        fname: "clif-check",
        func_id,
        uen_syms: &uen_syms,
        nonnull: &mut nonnull,
        derived: &mut derived,
        frozen: &mut frozen,
        noalias: &mut noalias,
        scoped_na: &mut scoped_na,
        deref: &mut deref,
        nowrite: &facts.nowrite,
        call_frozen: &e1,
        // Not in the facts header: every call may unwind (conservative).
        nounwind: &Default::default(),
        scoped: &e2,
        invariant: &e3,
        operand_frozen: &mut e4,
        block_region: &e5,
        prof: &e8,
        prof_hook: None,
        region_parent: &e6,
        inst_region: Default::default(),
        lifetimes: &mut lifetimes,
        nocap: &e7,
        edge_kills: Vec::new(),
        dbg_slots: &Default::default(),
        fn_names: &[],
        tcfg: isa.frontend_config(),
        simd: true,
        x64,
        cmp64: true,
        amode_scale: pipeline::amode_scale(isa),
        wasm: false,
        has_eh: false,
        flags: &flags,
        // `PLIRON_CLIF=<substr of the file's self name>` dumps steps.
        dump: std::env::var("PLIRON_CLIF").is_ok_and(|f| facts.self_name.as_deref().is_some_and(|n| n.contains(&f))),
        verify_full: false,
        since_reap: false,
        unopt: false,
        stats: &mut stats,
        carry_log: Vec::new(),
        cur_step: "",
    };
    p.seed_fact_bits();
    k(&mut p)
}

/// Runs step `name` on `func` and checks it. Returns (ok, function after).
fn run_one(
    isa: &dyn TargetIsa,
    name: &str,
    func: Function,
    facts: &OwnedFacts,
    o: &Opts,
) -> (bool, Function) {
    let before = func.clone();
    let after = with_pipe(isa, func, facts, |p| {
        // A saved `before.clif` comes from mid-pipeline, possibly with
        // edits the pipeline hadn't reaped yet: replay reap-gated steps
        // (`iconst128`'s `post_sweep`) as the pipeline would have, or a
        // diffcheck repro of them reports "unchanged".
        p.since_reap = true;
        if pipeline::run_named(p, name).is_none() {
            eprintln!("unknown step `{name}` (see `clif-check steps`)");
            std::process::exit(2);
        }
        if o.print {
            for (d, w) in &p.carry_log {
                eprintln!("carry@{name} {d} -> {w}");
            }
        }
        p.clctx.func.clone()
    });
    if after.display().to_string() == before.display().to_string() {
        println!("{name}: unchanged");
        return (true, after);
    }
    if o.print {
        println!("=== after {name} ===\n{}", after.display());
    }
    let ok = report(name, &before, &after, facts, o);
    (ok, after)
}

/// `bench <dir> [reps]` — wall-time the full `run_pipeline` over every
/// `before.clif` under `dir` (a `PLIRON_CORPUS` dump or a diffcheck
/// collection), printing the per-step timing table. Pure pass code: no
/// rustc, no sampler — the iteration loop for pipeline perf work.
fn bench_corpus(isa: &dyn TargetIsa, dir: &Path, reps: usize) -> bool {
    // The per-step table below reads `Stats::timing`; the pipeline skips
    // its per-step clock reads unless told the numbers are observable.
    pliron_clif_opt::config::mark_step_timing();
    let mut files = Vec::new();
    collect_clif(dir, &mut files);
    files.sort();
    let corpus: Vec<(Function, OwnedFacts)> = files.iter().map(|f| load(f)).collect();
    eprintln!("bench: {} functions, {reps} rep(s)", corpus.len());
    for r in 0..reps {
        let t0 = std::time::Instant::now();
        let mut stats = Stats::default();
        for (f, facts) in &corpus {
            with_pipe(isa, f.clone(), facts, |p| {
                pipeline::run_pipeline(p);
                stats.merge(&*p.stats);
            });
        }
        let el = t0.elapsed();
        println!("rep {r}: {el:.2?} over {} fns", corpus.len());
        if r + 1 == reps {
            println!(
                "  analysis builds: cfg={} dt={} la={} ({:.1?}+{:.1?}+{:.1?})",
                stats.cfg_builds,
                stats.dt_builds,
                stats.la_builds,
                std::time::Duration::from_nanos(stats.cfg_ns),
                std::time::Duration::from_nanos(stats.dt_ns),
                std::time::Duration::from_nanos(stats.la_ns),
            );
            let mut rows: Vec<_> = stats.timing.iter().collect();
            rows.sort_by_key(|(_, d)| std::cmp::Reverse(**d));
            for (n, d) in rows.iter().take(200) {
                println!("  {d:>9.1?}  {n}");
            }
        }
    }
    true
}

fn pipeline_checked(isa: &dyn TargetIsa, file: &Path, o: &Opts) -> bool {
    let (mut func, facts) = load(file);
    let names = with_pipe(isa, Function::new(), &facts, |p| {
        pipeline::selected_names(p)
    });
    let mut ok = true;
    for n in names {
        let r = run_one(isa, n, func, &facts, o);
        ok &= r.0;
        func = r.1;
        if !r.0 {
            break;
        }
    }
    ok
}

fn report(label: &str, before: &Function, after: &Function, facts: &OwnedFacts, o: &Opts) -> bool {
    if let Err(e) = cranelift_codegen::verify_function(
        after,
        &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
    ) {
        println!("{label}: VERIFIER ERROR\n{e}");
        if o.print {
            println!("=== after ===\n{}", after.display());
        }
        return false;
    }
    match diffcheck::check(before, after, &facts.facts(), &o.cfg) {
        Ok(s) => {
            println!(
                "{label}: ok compared={} inconclusive={} {}",
                s.compared,
                s.inconclusive,
                s.why.unwrap_or_default().lines().next().unwrap_or("")
            );
            true
        }
        Err(m) => {
            println!("{label}: MISMATCH\n{m}");
            if o.print {
                println!(
                    "---- before\n{}---- after\n{}",
                    before.display(),
                    after.display()
                );
            }
            false
        }
    }
}

/// One function's outcome in `report`.
struct FnRow {
    file: String,
    insts: (usize, usize),
    blocks: (usize, usize),
    us: u128,
    /// `ok`, `changed`... see `report_one`.
    status: &'static str,
    msg: String,
}

fn size_of(f: &Function) -> (usize, usize) {
    let mut n = 0;
    let mut b = 0;
    for blk in f.layout.blocks() {
        b += 1;
        n += f.layout.block_insts(blk).count();
    }
    (n, b)
}

// r[impl verify.corpus.replay]
/// Pipeline (and with `--check`, diffcheck) one corpus file. Status:
/// `ok` (checked equal or unchecked), `inconclusive` (checker couldn't
/// compare any trial), `mismatch`, `verifier`, `panic`, `parse`.
fn report_one(isa: &dyn TargetIsa, root: &Path, file: &Path, o: &Opts, steps: &std::sync::Mutex<Stats>) -> FnRow {
    let rel = file.strip_prefix(root).unwrap_or(file).display().to_string();
    let mut row = FnRow { file: rel, insts: (0, 0), blocks: (0, 0), us: 0, status: "ok", msg: String::new() };
    let loaded = std::fs::read_to_string(file).ok().and_then(|text| {
        let mut fs = cranelift_reader::parse_functions(&text).ok()?;
        (!fs.is_empty()).then(|| (fs.remove(0), OwnedFacts::parse(&text)))
    });
    let Some((func, facts)) = loaded else {
        row.status = "parse";
        return row;
    };
    let before = func.clone();
    let (ib, bb) = size_of(&before);
    let t0 = std::time::Instant::now();
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_pipe(isa, func, &facts, |p| {
            pipeline::run_pipeline(p);
            steps.lock().unwrap().merge(&*p.stats);
            p.clctx.func.clone()
        })
    }));
    row.us = t0.elapsed().as_micros();
    let after = match ran {
        Ok(a) => a,
        Err(e) => {
            row.status = "panic";
            row.msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            row.insts = (ib, 0);
            row.blocks = (bb, 0);
            return row;
        }
    };
    let (ia, ba) = size_of(&after);
    row.insts = (ib, ia);
    row.blocks = (bb, ba);
    if let Err(e) = cranelift_codegen::verify_function(
        &after,
        &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
    ) {
        row.status = "verifier";
        row.msg = e.to_string().lines().take(3).collect::<Vec<_>>().join(" | ");
        return row;
    }
    if o.check {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            diffcheck::check(&before, &after, &facts.facts(), &o.cfg).map_err(|m| m.to_string())
        }));
        match r {
            Ok(Ok(s)) if s.compared == 0 => {
                row.status = "inconclusive";
                row.msg = s.why.unwrap_or_default().lines().next().unwrap_or("").to_string();
            }
            Ok(Ok(_)) => {}
            Ok(Err(m)) => {
                row.status = "mismatch";
                row.msg = m.lines().take(4).collect::<Vec<_>>().join(" | ");
            }
            Err(_) => {
                row.status = "inconclusive";
                row.msg = "checker panicked".into();
            }
        }
    }
    row
}

/// `report <dir> <out.json>` — the live-loop workhorse: every corpus
/// function through `run_pipeline` on `--jobs` threads, written as JSON
/// for `harness/live/` (and the gate). Never aborts on a bad function:
/// panics, verifier errors and mismatches become rows.
fn report_corpus(isa: &std::sync::Arc<dyn TargetIsa>, dir: &Path, out: &Path, o: &Opts) -> bool {
    // `steps.timing` lands in the JSON's per-step table — the pipeline
    // skips its per-step clock reads unless told they're observable.
    pliron_clif_opt::config::mark_step_timing();
    let mut files = Vec::new();
    collect_clif(dir, &mut files);
    files.sort();
    files.truncate(o.limit);
    std::panic::set_hook(Box::new(|_| {}));
    let t0 = std::time::Instant::now();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let rows = std::sync::Mutex::new(Vec::with_capacity(files.len()));
    let steps = std::sync::Mutex::new(Stats::default());
    std::thread::scope(|s| {
        for _ in 0..o.jobs {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(f) = files.get(i) else { break };
                let r = report_one(&**isa, dir, f, o, &steps);
                rows.lock().unwrap().push(r);
            });
        }
    });
    let _ = std::panic::take_hook();
    let mut rows = rows.into_inner().unwrap();
    rows.sort_by(|a, b| a.file.cmp(&b.file));
    let wall = t0.elapsed();
    let steps = steps.into_inner().unwrap();
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for r in &rows {
        *counts.entry(r.status).or_default() += 1;
    }
    let mut step_ms: Vec<_> = steps.timing.iter().map(|(n, d)| (*n, d.as_secs_f64() * 1e3)).collect();
    step_ms.sort_by(|a, b| b.1.total_cmp(&a.1));
    let json = serde_json::json!({
        "generated": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        "corpus": dir.display().to_string(),
        "checked": o.check,
        "wall_ms": wall.as_secs_f64() * 1e3,
        "counts": counts,
        "steps": step_ms.iter().map(|(n, ms)| serde_json::json!({"step": n, "ms": ms})).collect::<Vec<_>>(),
        "functions": rows.iter().map(|r| serde_json::json!({
            "file": r.file, "status": r.status, "msg": r.msg, "us": r.us,
            "insts_before": r.insts.0, "insts_after": r.insts.1,
            "blocks_before": r.blocks.0, "blocks_after": r.blocks.1,
        })).collect::<Vec<_>>(),
    });
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(&json).unwrap()).expect("write report");
    std::fs::rename(&tmp, out).expect("rename report");
    let bad = rows.iter().filter(|r| matches!(r.status, "mismatch" | "verifier" | "panic")).count();
    println!(
        "report: {} fns in {wall:.2?} ({} jobs) — {counts:?} -> {}",
        rows.len(),
        o.jobs,
        out.display()
    );
    bad == 0
}

/// One function's machine-code shape in `codegen`.
#[derive(Default)]
struct CgRow {
    file: String,
    status: &'static str,
    insts: usize,
    stack: usize,
    /// Each instruction (stack access) counted 8^depth, depth capped at 3:
    /// a static stand-in for dynamic counts.
    winsts: u64,
    wstack: u64,
    /// Frame bytes: explicit stack slots, and regalloc spill slots.
    slots: u32,
    spills: u32,
    /// Loop-weighted stack accesses into spill slots (the rest of `wstack`
    /// is explicit stack slots: locals kept in memory).
    wspill: u64,
}

/// The backend's Cranelift flags for aarch64-apple-darwin (`src/lib.rs`).
fn backend_isa() -> std::sync::Arc<dyn TargetIsa> {
    let mut fb = cranelift_codegen::settings::builder();
    fb.set("is_pic", "true").unwrap();
    fb.set("opt_level", "speed").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    fb.set("enable_llvm_abi_extensions", "true").unwrap();
    fb.set("tls_model", "macho").unwrap();
    let flags = cranelift_codegen::settings::Flags::new(fb);
    cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .expect("aarch64 ISA")
        .finish(flags)
        .expect("ISA flags")
}

/// Score vcode text. Each `blockN: ; <clif block>` label (vendored
/// Cranelift names the CLIF block, or `a->b` for a critical edge, which
/// takes the deeper end) takes that block's loop depth from Cranelift's
/// loop analysis of the compiled function. Prologue/epilogue pushes and
/// pops (`[sp, #-16]!`, `[sp], #16`) are not stack accesses.
fn score_vcode(v: &str, row: &mut CgRow, spill_from: i64, depth_of: &dyn Fn(&str) -> u32) {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut depth: Vec<u32> = Vec::new();
    for l in v.lines() {
        let t = l.trim();
        let (head, clif) = t.split_once(';').map_or((t, ""), |(h, c)| (h.trim(), c.trim()));
        if head.starts_with("block") && head.ends_with(':') {
            blocks.push(Vec::new());
            depth.push(clif.split("->").map(|b| depth_of(b.trim())).max().unwrap_or(0));
        } else if !t.is_empty() && !blocks.is_empty() {
            blocks.last_mut().unwrap().push(t);
        }
    }
    for (i, insts) in blocks.iter().enumerate() {
        let w = 8u64.pow(depth[i].min(3));
        for t in insts {
            row.insts += 1;
            row.winsts += w;
            if t.contains("[sp") && !t.contains("]!") && !t.contains("], #") {
                row.stack += 1;
                row.wstack += w;
                let off = t.split("[sp").nth(1).and_then(|r| {
                    let r = r.trim_start_matches(", #").trim_start_matches(',').trim();
                    let num: String = r.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
                    if num.is_empty() {
                        Some(0)
                    } else if let Some(h) = num.strip_prefix("0x") {
                        i64::from_str_radix(h, 16).ok()
                    } else {
                        num.parse().ok()
                    }
                });
                if off.is_some_and(|o| o >= spill_from) {
                    row.wspill += w;
                }
            }
        }
    }
}

fn codegen_one(isa: &dyn TargetIsa, root: &Path, file: &Path) -> CgRow {
    let mut row = CgRow { file: file.strip_prefix(root).unwrap_or(file).display().to_string(), status: "ok", ..Default::default() };
    let loaded = std::fs::read_to_string(file).ok().and_then(|text| {
        let mut fs = cranelift_reader::parse_functions(&text).ok()?;
        (!fs.is_empty()).then(|| (fs.remove(0), OwnedFacts::parse(&text)))
    });
    let Some((func, facts)) = loaded else {
        row.status = "parse";
        return row;
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let after = with_pipe(isa, func, &facts, |p| {
            pipeline::run_pipeline(p);
            p.clctx.func.clone()
        });
        let mut ctx = Context::for_function(after);
        ctx.set_disasm(true);
        let v = ctx.compile(isa, &mut cranelift_codegen::control::ControlPlane::default())
            .ok()
            .and_then(|c| c.vcode.clone())?;
        // Loop depth per CLIF block of the function as compiled.
        let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&ctx.func);
        let dt = cranelift_codegen::dominator_tree::DominatorTree::with_function(&ctx.func, &cfg);
        let mut la = cranelift_codegen::loop_analysis::LoopAnalysis::new();
        la.compute(&ctx.func, &cfg, &dt);
        let depths: std::collections::HashMap<String, u32> = ctx
            .func
            .layout
            .blocks()
            .map(|b| (b.to_string(), la.loop_level(b).level() as u32))
            .collect();
        Some((v, depths))
    }));
    match r {
        Ok(Some((v, depths))) => {
            let [slots, spills, _, out, _] = cranelift_codegen::pliron_last_frame();
            (row.slots, row.spills) = (slots, spills);
            score_vcode(&v, &mut row, i64::from(out) + i64::from(slots), &|b| depths.get(b).copied().unwrap_or(0));
        }
        Ok(None) => row.status = "compile",
        Err(_) => row.status = "panic",
    }
    row
}

/// `codegen <dir> <out.json>`: see the usage text.
fn codegen_corpus(dir: &Path, out: &Path, o: &Opts) -> bool {
    let isa = backend_isa();
    let mut files = Vec::new();
    collect_clif(dir, &mut files);
    files.sort();
    files.truncate(o.limit);
    std::panic::set_hook(Box::new(|_| {}));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let rows = std::sync::Mutex::new(Vec::with_capacity(files.len()));
    std::thread::scope(|s| {
        for _ in 0..o.jobs {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(f) = files.get(i) else { break };
                let r = codegen_one(&*isa, dir, f);
                rows.lock().unwrap().push(r);
            });
        }
    });
    let _ = std::panic::take_hook();
    let mut rows = rows.into_inner().unwrap();
    rows.sort_by(|a, b| a.file.cmp(&b.file));
    let sum = |f: fn(&CgRow) -> u64| rows.iter().map(f).sum::<u64>();
    let json = serde_json::json!({
        "corpus": dir.display().to_string(),
        "insts": sum(|r| r.insts as u64), "stack": sum(|r| r.stack as u64),
        "winsts": sum(|r| r.winsts), "wstack": sum(|r| r.wstack), "wspill": sum(|r| r.wspill),
        "functions": rows.iter().map(|r| serde_json::json!({
            "file": r.file, "status": r.status, "insts": r.insts, "stack": r.stack,
            "winsts": r.winsts, "wstack": r.wstack, "slots": r.slots, "spills": r.spills, "wspill": r.wspill,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(out, serde_json::to_string(&json).unwrap()).expect("write codegen report");
    let bad = rows.iter().filter(|r| r.status != "ok").count();
    println!(
        "codegen: {} fns ({bad} failed): insts {} stack {} | weighted insts {} stack {} -> {}",
        rows.len(),
        json["insts"], json["stack"], json["winsts"], json["wstack"],
        out.display()
    );
    bad == 0
}
