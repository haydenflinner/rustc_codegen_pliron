//! `clif-check`: replay and hunt miscompiles in the CLIF pipeline without
//! rustc. Inputs are `.clif` files as the differential checker saves them
//! (`PLIRON_DIFFCHECK_DIR`), whose first line may carry the facts:
//! `; diffcheck-facts: nonnull=v0 noalias=... self=u0:N`.
//!
//!   clif-check step <name> before.clif     run one step, check it
//!   clif-check pipeline before.clif        run the default pipeline, check each step
//!   clif-check pair before.clif after.clif check two saved functions
//!   clif-check corpus <dir>                `pipeline` on every before.clif under dir
//!   clif-check steps                       list step names
//!
//! Options (after the subcommand args): --trials N, --seed N, --fuel N, --print.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cranelift_codegen::Context;
use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::Configurable;
use cranelift_module::FuncId;
use pliron_clif_opt::diffcheck::{self, Config, OwnedFacts};
use pliron_clif_opt::pipeline::{self, FnFlags, Pipe, Stats};

struct Opts {
    cfg: Config,
    print: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos = Vec::new();
    let mut opts = Opts {
        cfg: diffcheck::config_from_env(),
        print: false,
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
    let (e1, e2, e3, e5, e6, e7, e8) = Default::default();
    let (mut nonnull, mut frozen, mut noalias, mut deref, mut e4, mut lifetimes) = (
        facts.nonnull.clone(),
        facts.frozen.clone(),
        facts.noalias.clone(),
        facts.deref.clone(),
        Default::default(),
        Vec::new(),
    );
    let mut p = Pipe {
        clctx: &mut clctx,
        isa,
        fname: "clif-check",
        func_id,
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
        region_parent: &e6,
        lifetimes: &mut lifetimes,
        nocap: &e7,
        tcfg: isa.frontend_config(),
        simd: true,
        x64,
        cmp64: true,
        has_eh: false,
        flags: &flags,
        dump: false,
        verify_full: false,
        stats: &mut stats,
        carry_log: Vec::new(),
    };
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
        p.clctx.func.clone()
    });
    if after.display().to_string() == before.display().to_string() {
        println!("{name}: unchanged");
        return (true, after);
    }
    let ok = report(name, &before, &after, facts, o);
    (ok, after)
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
