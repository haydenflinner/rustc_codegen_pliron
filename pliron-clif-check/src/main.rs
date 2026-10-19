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

fn main() -> ExitCode {
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
    let (mut nonnull, mut frozen, mut noalias, mut deref, mut e4, mut lifetimes) = (
        facts.nonnull.clone(),
        facts.frozen.clone(),
        facts.noalias.clone(),
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
        deref: &mut deref,
        nowrite: &facts.nowrite,
        call_frozen: &e1,
        scoped: &e2,
        invariant: &e3,
        operand_frozen: &mut e4,
        block_region: &e5,
        prof: &e8,
        prof_hook: None,
        region_parent: &e6,
        lifetimes: &mut lifetimes,
        nocap: &e7,
        fn_names: &[],
        tcfg: isa.frontend_config(),
        simd: true,
        x64,
        cmp64: true,
        amode_scale: pipeline::amode_scale(isa),
        has_eh: false,
        flags: &flags,
        dump: false,
        verify_full: false,
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
                "  analysis builds: cfg={} dt={} la={}",
                stats.cfg_builds, stats.dt_builds, stats.la_builds
            );
            let mut rows: Vec<_> = stats.timing.iter().collect();
            rows.sort_by_key(|(_, d)| std::cmp::Reverse(**d));
            for (n, d) in rows.iter().take(24) {
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
