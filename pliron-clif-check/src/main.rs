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
//!                                          --jobs N, --limit N, --every N:
//!                                          each Nth file of the sorted list;
//!                                          PLIRON_VERIFY=full: the verifier
//!                                          after every step, a failure is a
//!                                          `panic` row naming the step;
//!                                          PLIRON_RA_CHECK=1: regalloc2's
//!                                          checker, a failure is a `racheck`
//!                                          row; --offset K: the --every
//!                                          sample from file K; --vcode FILE: every fn's
//!                                          vcode with depth labels, for
//!                                          harness/rastat.py)
//!   clif-check steps                       list step names
//!   clif-check xcorpus <dirA> <dirB> <out.json>
//!                                          the IR passes, by ablation: A is a
//!                                          corpus recorded with an IR pass off,
//!                                          B the same build with it on; every
//!                                          function in both is checked A against
//!                                          B (--pipeline: and A against the
//!                                          pipeline's output for B; --fn SUBSTR:
//!                                          matching symbols only; --jobs, --every,
//!                                          --limit as for report). See xcorpus.rs.
//!   clif-check xpair a.clif b.clif         one such pair (--pipeline, --print)
//!
//! Options (after the subcommand args): --trials N, --seed N, --fuel N, --print.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cranelift_codegen::Context;
use cranelift_codegen::entity::EntityRef as _;
use cranelift_codegen::ir::{Block, Function, Inst, StackSlot};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::Configurable;
use cranelift_module::FuncId;
use pliron_clif_opt::diffcheck::{self, Config, OwnedFacts};
use pliron_clif_opt::pipeline::{self, FnFlags, Pipe, Stats};

mod xcorpus;

struct Opts {
    cfg: Config,
    print: bool,
    check: bool,
    jobs: usize,
    limit: usize,
    /// `report`/`codegen`: every Nth file (a deterministic sample of a big corpus).
    every: usize,
    /// `codegen --offset K`: the sample starts at the Kth file (rotating
    /// samples, harness/rafuzz.py).
    offset: usize,
    /// `xcorpus`/`xpair`: also compare A with the pipeline's output for B.
    pipeline: bool,
    /// `xcorpus`: only symbols containing this.
    only: Option<String>,
    /// `--trials` or `PLIRON_DIFFCHECK_TRIALS` was given.
    trials_given: bool,
    /// `codegen --vcode FILE`: write each function's vcode, labels with
    /// loop depths, as `== <file>` sections.
    vcode_out: Option<PathBuf>,
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
    // `PLIRON_CLIF_WASM=1`: i32 pointers in the checker's sandbox. Set
    // here and not only where a `Pipe` is built: `pair` builds none, and
    // checked a wasm pair with 64-bit addresses (every trial inconclusive,
    // `heap_oob` at the first `stack_addr.i32`).
    diffcheck::set_ptr32(pliron_cfg::env_is("PLIRON_CLIF_WASM", "1"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos = Vec::new();
    let mut opts = Opts {
        cfg: diffcheck::config_from_env(),
        print: false,
        check: false,
        jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
        limit: usize::MAX,
        every: 1,
        offset: 0,
        pipeline: false,
        only: None,
        trials_given: std::env::var_os("PLIRON_DIFFCHECK_TRIALS").is_some(),
        vcode_out: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut num = || {
            it.next()
                .and_then(|v| v.parse::<u64>().ok())
                .expect("number after flag")
        };
        match a.as_str() {
            "--trials" => {
                opts.cfg.trials = num() as u32;
                opts.trials_given = true;
            }
            "--seed" => opts.cfg.seed = num(),
            "--fuel" => opts.cfg.fuel = num(),
            "--print" => opts.print = true,
            "--check" => opts.check = true,
            "--jobs" => opts.jobs = num().max(1) as usize,
            "--limit" => opts.limit = num() as usize,
            "--every" => opts.every = num().max(1) as usize,
            "--offset" => opts.offset = num() as usize,
            "--pipeline" => opts.pipeline = true,
            "--fn" => opts.only = Some(it.next().expect("substring after --fn").clone()),
            "--vcode" => opts.vcode_out = Some(PathBuf::from(it.next().expect("file after --vcode"))),
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
            run_one(&*isa, name, f, &facts, &opts, &mut None).0
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
        ["xcorpus", a, b, out] => xcorpus::xcorpus(&isa, Path::new(a), Path::new(b), Path::new(out), &opts),
        ["xpair", a, b] => xcorpus::xpair(&*isa, Path::new(a), Path::new(b), &opts),
        ["vcode", file] => {
            // One corpus file through the pipeline and Cranelift; print the
            // allocated vcode (what `codegen` scores).
            let isa = backend_isa();
            let (f, facts) = load(Path::new(file));
            // `PLIRON_RA_EXPLAIN=1`: regalloc2's `ra2x:` decision log.
            if std::env::var_os("PLIRON_RA_EXPLAIN").is_some() {
                cranelift_codegen::ra_set_explain(true);
            }
            let mut after = with_pipe(&*isa, f, &facts, |p| {
                pipeline::run_pipeline(p);
                p.clctx.func.clone()
            });
            // `PLIRON_IVSR=2`: the backend's compile-both choice.
            if pliron_cfg::env_is("PLIRON_IVSR", "2") {
                let margin = pliron_cfg::env_get("PLIRON_IVSR_MARGIN").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                if let Some((k, a, b, take)) = pliron_clif_opt::ivsr::pick(&mut after, &*isa, margin) {
                    eprintln!("ivsr-pick: rewrites {k} cost {a:?} -> {b:?} take {take}");
                }
            }
            if opts.print {
                println!("{}", after.display());
            }
            let mut ctx = Context::for_function(after);
            ctx.set_disasm(true);
            // `PLIRON_RA_EXPLAIN` (any value): regalloc2's `ra2x:` decision log.
            let explain = std::env::var_os("PLIRON_RA_EXPLAIN").is_some();
            if explain {
                cranelift_codegen::ra_set_explain(true);
            }
            let c = ctx.compile(&*isa, &mut cranelift_codegen::control::ControlPlane::default()).expect("compile");
            if explain {
                cranelift_codegen::ra_set_explain(false);
            }
            {
                let [slots, spills, clobber, out, fixed] = cranelift_codegen::pliron_last_frame();
                eprintln!("frame-info: slots={slots} spills={spills} clobber={clobber} outgoing={out} fixed={fixed}");
            }
            let v = c.vcode.clone().unwrap_or_default();
            if std::env::var_os("CLIF_CHECK_POST_EGRAPH").is_some() {
                // The function as Cranelift lowered it (after its egraph).
                println!("{}", ctx.func.display());
            }
            if let Err(e) = cranelift_codegen::verify_function(&ctx.func, &*isa) {
                eprintln!("post-egraph verifier errors:\n{e}");
            }
            let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&ctx.func);
            let dt = cranelift_codegen::dominator_tree::DominatorTree::with_function(&ctx.func, &cfg);
            let mut la = cranelift_codegen::loop_analysis::LoopAnalysis::new();
            la.compute(&ctx.func, &cfg, &dt);
            let depth = |b: &str| {
                ctx.func.layout.blocks().find(|x| x.to_string() == b).map_or(0, |x| la.loop_level(x).level())
            };
            print!("{}", depth_labels(&v, &|b| depth(b) as u32));
            true
        }
        ["bench", dir] => bench_corpus(&*isa, Path::new(dir), 1),
        ["bench", dir, reps] => bench_corpus(&*isa, Path::new(dir), reps.parse().expect("reps")),
        _ => {
            eprintln!(
                "{}",
                include_str!("main.rs")
                    .lines()
                    .take(34)
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
    let func = fs.remove(0);
    let facts = parse_facts(&text, &func);
    (func, facts)
}

/// A file's fact headers, with `self=` bound to the function's own id.
fn parse_facts(text: &str, func: &Function) -> OwnedFacts {
    let mut facts = OwnedFacts::parse(text);
    facts.bind_self(func);
    facts
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

/// `Pipe::lifetimes`: the lifetime markers as the pipeline carries them.
type Lts = Vec<(Vec<StackSlot>, Option<Inst>, Block, bool)>;

/// A `Pipe` over `func` with the saved facts, empty pliron-side maps, and
/// every pass category on (as at -O with no `PLIRON_*` overrides).
fn with_pipe<R>(
    isa: &dyn TargetIsa,
    func: Function,
    facts: &OwnedFacts,
    k: impl FnOnce(&mut Pipe) -> R,
) -> R {
    with_pipe_lts(isa, func, facts, None, k)
}

/// [`with_pipe`] with the lifetime markers given (`lts`) instead of read
/// from the file's header. The header places a marker by the ordinal of
/// its anchor instruction in the function *as recorded*; for a function
/// some steps have already rewritten, those ordinals name other
/// instructions.
fn with_pipe_lts<R>(
    isa: &dyn TargetIsa,
    func: Function,
    facts: &OwnedFacts,
    lts: Option<Lts>,
    k: impl FnOnce(&mut Pipe) -> R,
) -> R {
    let mut clctx = Context::for_function(func);
    let mut derived = facts.derived.clone();
    let mut stats = Stats::default();
    // `PLIRON_<PASS>=0` switches, read as the backend reads them.
    let flags = FnFlags::from_env();
    let func_id = facts
        .self_name
        .as_deref()
        .and_then(|n| n.strip_prefix("u0:"))
        .and_then(|n| n.parse().ok())
        .map(FuncId::from_u32)
        // No self-call in the text: an id no callee can have.
        .unwrap_or(FuncId::from_u32(u32::MAX - 1));
    let (e1, e2, e3, mut e5, mut e6, mut e7, e8): (
        _,
        _,
        _,
        rustc_hash::FxHashMap<_, _>,
        pliron::cols::ColMap<_, _>,
        rustc_hash::FxHashMap<(cranelift_codegen::ir::Inst, u32), u64>,
        _,
    ) = Default::default();
    // Region-scoped facts (`; diffcheck-bregion/rparent/rnn:`): regions
    // are pliron blocks in the pipeline, so stand fresh ones in for the
    // numbered regions (they are only compared).
    let mut pctx = pliron::context::Context::new();
    let nreg = facts
        .block_region
        .iter()
        .map(|&(_, r)| r)
        .chain(facts.region_parent.iter().flat_map(|&(r, q)| [r, q]))
        .chain(facts.region_nn.iter().flat_map(|(_, rs)| rs.iter().copied()))
        .max()
        .map_or(0, |m| m + 1);
    let regs: Vec<_> =
        (0..nreg).map(|_| pliron::basic_block::BasicBlock::new(&mut pctx, None, vec![])).collect();
    let bords: Vec<_> = clctx.func.layout.blocks().collect();
    for &(b, r) in &facts.block_region {
        if let Some(&b) = bords.get(b as usize) {
            e5.insert(b, regs[r as usize]);
        }
    }
    for &(r, q) in &facts.region_parent {
        e6.insert(regs[r as usize], regs[q as usize]);
    }
    let region_nn: rustc_hash::FxHashMap<_, Vec<_>> = facts
        .region_nn
        .iter()
        .map(|(v, rs)| (*v, rs.iter().map(|&r| regs[r as usize]).collect()))
        .collect();
    // A `; diffcheck-nocap:` header (PLIRON_CORPUS dumps) records the real
    // per-callee bounded arg positions; CLIFCHECK_NOCAP_ALL=1 marks every
    // call operand bounded for dumps without one.
    use cranelift_codegen::ir::instructions::CallInfo;
    if !facts.nocap.is_empty() {
        // The callee's several sites can carry different extents — keep
        // the widest recorded one per position.
        let mut extents: rustc_hash::FxHashMap<(u32, u32), u64> = Default::default();
        for &(r, a, ext) in &facts.nocap {
            let e = extents.entry((r.index() as u32, a)).or_insert(ext);
            *e = (*e).max(ext);
        }
        for b in clctx.func.layout.blocks() {
            for i in clctx.func.layout.block_insts(b) {
                let info = clctx.func.dfg.insts[i].analyze_call(
                    &clctx.func.dfg.value_lists,
                    &clctx.func.dfg.exception_tables,
                );
                if let CallInfo::Direct(fr, cargs) | CallInfo::DirectWithSig(fr, _, cargs) = info {
                    for k in 0..cargs.len() {
                        if let Some(&ext) = extents.get(&(fr.index() as u32, k as u32)) {
                            e7.insert((i, k as u32), ext);
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
                        e7.insert((i, (k - adj) as u32), u64::MAX);
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
        lts.unwrap_or_else(|| facts.lifetimes(&clctx.func)),
    );
    // `; diffcheck-names:` (FuncRef -> symbol) back into the module's
    // symbol table by user-name index, so name-keyed steps (`memcmpk`)
    // see their callees on replay.
    let mut fn_names: Vec<String> = Vec::new();
    for (fr, name) in &facts.names {
        if let Some(ef) = clctx.func.dfg.ext_funcs.get(*fr)
            && let cranelift_codegen::ir::ExternalName::User(u) = ef.name
        {
            let un = &clctx.func.params.user_named_funcs()[u];
            if un.namespace == 0 {
                let k = un.index as usize;
                if fn_names.len() <= k {
                    fn_names.resize(k + 1, String::new());
                }
                fn_names[k] = name.clone();
            }
        }
    }
    let mut fact_bits = cranelift_codegen::entity::SecondaryMap::new();
    let uen_syms = Default::default();
    let wasm_mode = pliron_cfg::env_is("PLIRON_CLIF_WASM", "1");
    pliron_clif_opt::diffcheck::set_ptr32(wasm_mode);
    let mut p = Pipe {
        const_tables: &pipeline::EMPTY_TABLES,
        region_nn: &region_nn,
        fact_bits: &mut fact_bits,
        clctx: &mut clctx,
        isa,
        fname: "clif-check",
        func_id,
        uen_syms: &uen_syms,
        nonnull: &mut nonnull,
        vfacts: &facts.vfacts,
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
        renames: Vec::new(),
                site_facts: Default::default(),
        nocap: &e7,
        edge_kills: Vec::new(),
        dbg_slots: &Default::default(),
        fn_names: &fn_names,
        target: if wasm_mode { pliron_clif_opt::target::Target::wasm32() } else { pliron_clif_opt::target::of_isa(isa) },
        corpus_syms: None,
        tcfg: isa.frontend_config(),
        // The wasm backend runs the pipeline with `simd` (no simd128 by
        // default), `x64` and `cmp64` off.
        simd: !wasm_mode,
        cmp64: !wasm_mode,
        // `PLIRON_CLIF_WASM=1`: replay as the wasm backend runs the
        // pipeline (`Pipe::wasm`, stepped-pointer indvars). The
        // interpreter diff stays inconclusive for i32-pointer code, but
        // the wasm-only transforms run and `PLIRON_VERIFY=full` checks
        // every step: three of the four compiler panics the dsa build
        // hit on 2026-11-16 were in paths a native replay never takes.
        has_eh: false,
        flags: &flags,
        // `PLIRON_CLIF=<substr of the file's self name>` dumps steps.
        dump: std::env::var("PLIRON_CLIF").is_ok_and(|f| facts.self_symbol.as_deref().or(facts.self_name.as_deref()).is_some_and(|n| n.contains(&f))),
        // `PLIRON_VERIFY=full`: the verifier after every step, as in the
        // backend (a failing step panics; `report` records it as `panic`).
        verify_full: pliron_cfg::env_is("PLIRON_VERIFY", "full"),
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
/// `lts`: the lifetime markers a previous step of the same function left
/// (`None`: the file's), replaced by the ones this step leaves.
fn run_one(
    isa: &dyn TargetIsa,
    name: &str,
    func: Function,
    facts: &OwnedFacts,
    o: &Opts,
    lts: &mut Option<Lts>,
) -> (bool, Function) {
    let before = func.clone();
    let carried = lts.take();
    let after = with_pipe_lts(isa, func, facts, carried, |p| {
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
        *lts = Some(p.lifetimes.clone());
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
            // Cost with no effect: time each step spent on functions it
            // left unchanged, with how often it changed one. A step high
            // in this table with a low hit rate wants a tighter shape
            // gate; a `...2`/late repeat of a step that almost never
            // fires is a round to cut.
            let mut rep: Vec<_> = stats.repeat.iter().collect();
            rep.sort_by_key(|(_, e)| std::cmp::Reverse(e.2));
            let rep_sum: std::time::Duration = rep.iter().map(|(_, e)| e.2).sum();
            println!("  repeats with nothing changed since the family's last no-effect run: {rep_sum:.1?}");
            println!("  {:>9}  {:>7}  {:>7}  step", "time", "repeats", "changed");
            for (n, (runs, fired, d)) in rep.iter().take(25) {
                println!("  {d:>9.1?}  {runs:>7}  {fired:>7}  {n}");
            }
            let total: std::time::Duration = stats.timing.values().sum();
            let mut idle: Vec<_> = stats.effect.iter().collect();
            idle.sort_by_key(|(_, e)| std::cmp::Reverse(e.2));
            let idle_sum: std::time::Duration = idle.iter().map(|(_, e)| e.2).sum();
            println!(
                "  no-effect time: {idle_sum:.1?} of {total:.1?} ({:.0}%)",
                100.0 * idle_sum.as_secs_f64() / total.as_secs_f64().max(1e-9)
            );
            println!("  {:>9}  {:>6}  {:>7}  {:>6}  step", "idle", "of all", "ran", "fired");
            for (n, (ran, fired, d)) in idle.iter().take(40) {
                println!(
                    "  {d:>9.1?}  {:>5.1}%  {ran:>7}  {:>5.1}%  {n}",
                    100.0 * d.as_secs_f64() / total.as_secs_f64().max(1e-9),
                    100.0 * *fired as f64 / (*ran).max(1) as f64
                );
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
    // The markers go from step to step as in the pipeline. Each step
    // used to read them from the header again, against a function the
    // earlier steps had rewritten: slotmerge then merged by markers at
    // the wrong instructions, and a function the whole pipeline breaks
    // passed every step here (two of the dsa corpus, 2026-11-16, where
    // this now names slotmerge as `PLIRON_DIFFCHECK=step` does).
    let mut lts = None;
    for n in names {
        let r = run_one(isa, n, func, &facts, o, &mut lts);
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
        (!fs.is_empty()).then(|| {
            let func = fs.remove(0);
            let facts = parse_facts(&text, &func);
            (func, facts)
        })
    });
    let Some((mut func, facts)) = loaded else {
        row.status = "parse";
        return row;
    };
    // Name the function after its corpus file so per-function traces
    // (`PLIRON_TRACE=pliron::<step>`) can be grepped.
    let stem = std::path::Path::new(&row.file).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // Only what the reader accepts in a `%name`: a `.` in it (the corpus
    // file's `.<n>` suffix) made a saved `before.clif` unparseable.
    let stem: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    func.name = cranelift_codegen::ir::UserFuncName::testcase(stem.as_bytes());
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
                row.msg = m.lines().take(5).collect::<Vec<_>>().join(" | ");
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
    if o.every > 1 {
        files = files.into_iter().step_by(o.every).collect();
    }
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
    /// A failed row: the panic or compile error.
    msg: String,
    /// `--vcode`: the allocated vcode with depth labels.
    vtext: String,
    /// Register-to-register copies (`mov`/`fmov` between registers), plain
    /// and loop-weighted: what coalescing and move resolution leave.
    movs: usize,
    wmov: u64,
    deadmov: usize,
    wdeadmov: u64,
    /// `PLIRON_RA2_MOVESTATS=1`: the allocator's moves by kind, weighted
    /// by block frequency (`regalloc2::MV_NAMES`; `16 + k` stack side).
    mv: [u64; 32],
    /// Standalone narrow extensions: `uxtb`/`uxth`/`sxtb`/`sxth` and
    /// `and w, w, #0xff`/`#0xffff`.
    ext: u64,
}

/// Labels gain the CLIF loop depth: `block3: ; block7 depth=2` (a split
/// critical edge `a->b` takes the deeper end).
fn depth_labels(v: &str, depth: &dyn Fn(&str) -> u32) -> String {
    let mut out = String::with_capacity(v.len() + v.len() / 8);
    for l in v.lines() {
        match l.split_once("; ") {
            Some((h, c)) if h.trim_start().starts_with("block") => {
                let d = c.split("->").map(|b| depth(b.trim())).max().unwrap_or(0);
                out.push_str(&format!("{h}; {c} depth={d}\n"));
            }
            _ => {
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    out
}

/// The text of a caught panic.
fn panic_text(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "non-string panic".into())
}

/// A register-to-register copy in aarch64 vcode text.
fn is_reg_copy(t: &str) -> bool {
    let (mn, ops) = t.split_once(' ').unwrap_or((t, ""));
    if mn != "mov" && mn != "fmov" {
        return false;
    }
    let ops: Vec<&str> = ops.split(',').map(str::trim).collect();
    let reg = |o: &str| {
        let o = o.split('.').next().unwrap_or(o);
        let (c, n) = o.split_at(1.min(o.len()));
        matches!(c, "x" | "w" | "d" | "s" | "q" | "v" | "h") && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
            || o == "fp" || o == "lr"
    };
    ops.len() == 2 && reg(ops[0]) && reg(ops[1])
}

/// The backend's Cranelift flags for aarch64-apple-darwin (`src/lib.rs`).
fn backend_isa() -> std::sync::Arc<dyn TargetIsa> {
    let mut fb = cranelift_codegen::settings::builder();
    fb.set("is_pic", "true").unwrap();
    fb.set("opt_level", "speed").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    fb.set("enable_llvm_abi_extensions", "true").unwrap();
    // `CLIF_CHECK_TRIPLE=aarch64-unknown-linux-musl`: another target's ISA
    // and ABI (Linux frees x18), as the held-out builds use.
    let triple = std::env::var("CLIF_CHECK_TRIPLE").unwrap_or_else(|_| "aarch64-apple-darwin".into());
    fb.set("tls_model", if triple.contains("apple") { "macho" } else { "elf_gd" }).unwrap();
    // `PLIRON_RA_ALGO=single_pass`: Cranelift's other allocator, to compare.
    // Only the settings flag's own values go there; `greedy`/`mcts`/`fast` are
    // regalloc2 algorithms that `machinst/compile.rs` reads from the env itself.
    if let Ok(a) = std::env::var("PLIRON_RA_ALGO") {
        if matches!(a.as_str(), "backtracking" | "single_pass") {
            fb.set("regalloc_algorithm", &a).unwrap();
        }
    }
    // `PLIRON_RA_CHECK=1`: regalloc2's symbolic checker on every function,
    // as the backend sets it (`src/lib.rs`).
    if pliron_cfg::env_is("PLIRON_RA_CHECK", "1") {
        fb.set("regalloc_checker", "true").unwrap();
    }
    let flags = cranelift_codegen::settings::Flags::new(fb);
    cranelift_codegen::isa::lookup_by_name(&triple)
        .expect("ISA for CLIF_CHECK_TRIPLE")
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
        // Copies overwritten by a later copy in the same run of copies
        // before anything read them (dead moves).
        let mut pending: Vec<String> = Vec::new();
        for t in insts {
            row.insts += 1;
            row.winsts += w;
            let mn = t.split_whitespace().next().unwrap_or("");
            if matches!(mn, "uxtb" | "uxth" | "sxtb" | "sxth")
                || (mn == "and" && (t.ends_with("#255") || t.ends_with("#65535") || t.ends_with("#0xff") || t.ends_with("#0xffff")))
            {
                row.ext += 1;
            }
            if is_reg_copy(t) {
                row.movs += 1;
                row.wmov += w;
                let ops: Vec<String> = t.split_once(' ').map_or("", |x| x.1).split(',')
                    .map(|o| o.trim().split('.').next().unwrap_or("").replace('w', "x")).collect();
                pending.retain(|r| *r != ops[1]);
                if pending.contains(&ops[0]) {
                    row.deadmov += 1;
                    row.wdeadmov += w;
                }
                pending.retain(|r| *r != ops[0]);
                pending.push(ops[0].clone());
            } else {
                pending.clear();
            }
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

fn codegen_one(isa: &dyn TargetIsa, root: &Path, file: &Path, keep_vcode: bool) -> CgRow {
    let mut row = CgRow { file: file.strip_prefix(root).unwrap_or(file).display().to_string(), status: "ok", ..Default::default() };
    let loaded = std::fs::read_to_string(file).ok().and_then(|text| {
        let mut fs = cranelift_reader::parse_functions(&text).ok()?;
        (!fs.is_empty()).then(|| {
            let func = fs.remove(0);
            let facts = parse_facts(&text, &func);
            (func, facts)
        })
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
        let v = match ctx.compile(isa, &mut cranelift_codegen::control::ControlPlane::default()) {
            Ok(c) => c.vcode.clone().unwrap_or_default(),
            Err(e) => return Err(format!("{:?}", e.inner)),
        };
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
        Ok((v, depths))
    }));
    match r {
        Ok(Ok((v, depths))) => {
            let [slots, spills, _, out, _] = cranelift_codegen::pliron_last_frame();
            (row.slots, row.spills) = (slots, spills);
            let depth = |b: &str| depths.get(b).copied().unwrap_or(0);
            score_vcode(&v, &mut row, i64::from(out) + i64::from(slots), &depth);
            row.mv = regalloc2::movestats_take_local().map(|w| w / 1000);
            if keep_vcode {
                row.vtext = format!("; frame slots={slots} spills={spills} out={out}\n{}", depth_labels(&v, &depth));
            }
        }
        Ok(Err(msg)) => (row.status, row.msg) = ("compile", msg),
        Err(e) => {
            row.msg = panic_text(&*e);
            // The checker's `expect` (machinst/compile.rs) and the
            // post-allocation one (`PLIRON_RELOADPRE_CHECK`).
            row.status = if row.msg.contains("register allocation checker") || row.msg.contains("checker errors") {
                "racheck"
            } else {
                "panic"
            };
        }
    }
    row
}

/// `codegen <dir> <out.json>`: see the usage text.
fn codegen_corpus(dir: &Path, out: &Path, o: &Opts) -> bool {
    let isa = backend_isa();
    let mut files = Vec::new();
    collect_clif(dir, &mut files);
    files.sort();
    if o.every > 1 {
        files = files.into_iter().skip(o.offset % o.every).step_by(o.every).collect();
    }
    files.truncate(o.limit);
    std::panic::set_hook(Box::new(|_| {}));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let rows = std::sync::Mutex::new(Vec::with_capacity(files.len()));
    std::thread::scope(|s| {
        for _ in 0..o.jobs {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(f) = files.get(i) else { break };
                let r = codegen_one(&*isa, dir, f, o.vcode_out.is_some());
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
        "winsts": sum(|r| r.winsts), "wstack": sum(|r| r.wstack), "wspill": sum(|r| r.wspill), "ext": sum(|r| r.ext),
        "movs": sum(|r| r.movs as u64), "wmov": sum(|r| r.wmov),
        "deadmov": sum(|r| r.deadmov as u64), "wdeadmov": sum(|r| r.wdeadmov),
        "functions": rows.iter().map(|r| {
            let mut j = serde_json::json!({
                "file": r.file, "status": r.status, "insts": r.insts, "stack": r.stack,
                "winsts": r.winsts, "wstack": r.wstack, "slots": r.slots, "spills": r.spills, "wspill": r.wspill,
                "movs": r.movs, "wmov": r.wmov, "mv": r.mv, "deadmov": r.deadmov, "ext": r.ext,
            });
            if !r.msg.is_empty() {
                j["msg"] = r.msg.chars().take(4000).collect::<String>().into();
            }
            j
        }).collect::<Vec<_>>(),
    });
    std::fs::write(out, serde_json::to_string(&json).unwrap()).expect("write codegen report");
    if let Some(vp) = &o.vcode_out {
        use std::io::Write as _;
        let mut w = std::io::BufWriter::new(std::fs::File::create(vp).expect("create --vcode file"));
        for r in rows.iter().filter(|r| r.status == "ok") {
            writeln!(w, "== {}", r.file).and_then(|_| w.write_all(r.vtext.as_bytes())).expect("write --vcode");
        }
    }
    let bad = rows.iter().filter(|r| r.status != "ok").count();
    // `PLIRON_RA2_MOVESTATS=1`: the allocator's moves by cause.
    if std::env::var("PLIRON_RA2_MOVESTATS").as_deref() == Ok("1") {
        let st = regalloc2::movestats_snapshot();
        println!("ra2 moves by kind: reg-reg count / weighted | with a stack side count / weighted");
        for (k, name) in regalloc2::MV_NAMES.iter().enumerate() {
            let (r, m) = (st[k], st[16 + k]);
            if r[0] + m[0] > 0 {
                println!("  {name:12} {:>8} {:>12.0} | {:>8} {:>12.0}", r[0], r[1] as f64 / 1000.0, m[0], m[1] as f64 / 1000.0);
            }
        }
    }
    println!(
        "codegen: {} fns ({bad} failed): insts {} stack {} movs {} | weighted insts {} stack {} spill {} movs {} (dead {} / {}) | ext {} -> {}",
        rows.len(),
        json["insts"], json["stack"], json["movs"], json["winsts"], json["wstack"], json["wspill"], json["wmov"], json["deadmov"], json["wdeadmov"], json["ext"],
        out.display()
    );
    bad == 0
}
