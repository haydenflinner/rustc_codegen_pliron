//! `.clif` filetests: each file under `filetests/` names a pass via
//! `; test <pass>` and asserts on the post-pass text:
//!
//!   ; test webmerge
//!   ; check: block1(v0: i64)
//!   ; check-not: block1(v0: i64, v1: i64)
//!   ; diffcheck
//!
//! Every file is `verify_function`-checked after the pass runs.
//! `diffcheck` additionally differential-interprets the before/after
//! functions — opt in only for tests whose inputs avoid UB (loads from
//! pointers, division, etc. can make the checker flag valid rewrites).

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Flags};
use pliron_clif_opt::clifpeep;
use pliron_clif_opt::diffcheck::{Config as DcConfig, Facts as DcFacts, check as diffcheck};
use pliron_clif_opt::{celim, commute, edgefwd, paramsink, rangefold};
use std::path::{Path, PathBuf};

type Pass = fn(&mut Function) -> usize;

fn pass(name: &str) -> Pass {
    match name {
        "bcheck" => |f| {
            let mut an = pliron_clif_opt::pipeline::Analysis::default();
            pliron_clif_opt::bcheck::run(f, &mut an, "filetest", true)
        },
        "brif_fold" => clifpeep::brif_fold,
        "celim" => celim::run,
        "coldargs" => clifpeep::coldargs,
        "coldedges" => clifpeep::coldedges,
        "coldrets" => clifpeep::coldrets,
        "commute" => commute::run,
        "const_sweep" => clifpeep::const_sweep,
        "cparm" => |f| clifpeep::cparm(f).0,
        "dgvn" => clifpeep::dgvn,
        "edgefwd" => edgefwd::run,
        "foldforwarders" => clifpeep::foldforwarders,
        "fusechains" => clifpeep::fusechains,
        "lcse" => clifpeep::lcse,
        "loadsel" => clifpeep::loadsel,
        "mask01" => clifpeep::mask01,
        "memcall" => clifpeep::memcall,
        "ovkill" => clifpeep::ovkill,
        "paramsink" => |f| paramsink::run(f).0,
        "rangefold" => rangefold::run,
        "rgmerge" => clifpeep::rgmerge,
        "sameargs" => |f| clifpeep::sameargs(f).0,
        "selargs" => clifpeep::selargs,
        "selfarg" => clifpeep::selfarg,
        "sgntest" => clifpeep::sgntest,
        "sublea" => clifpeep::sublea,
        "uloads32" => clifpeep::uloads32,
        "unswitch" => |f| {
            let mut an = pliron_clif_opt::pipeline::Analysis::default();
            pliron_clif_opt::bcheck::unswitch(f, &mut an, "filetest")
        },
        "vecslot" => clifpeep::vecslot,
        "webmerge" => |f| clifpeep::webmerge(f).0,
        "wide_minmax" => clifpeep::wide_minmax,
        other => panic!("unknown pass {other:?} — register it in filetests.rs"),
    }
}

fn clif_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(clif_files(&p));
            } else if p.extension().is_some_and(|x| x == "clif") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn filetests() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("filetests");
    let flags = Flags::new(settings::builder());
    let mut ran = 0;
    for path in clif_files(&dir) {
        let src = std::fs::read_to_string(&path).unwrap();
        let mut test: Option<&str> = None;
        let mut checks: Vec<&str> = vec![];
        let mut check_nots: Vec<&str> = vec![];
        let mut dcheck = false;
        for line in src.lines() {
            let line = line.trim();
            if let Some(t) = line.strip_prefix("; test ") {
                assert!(test.is_none(), "{path:?}: multiple `test` directives");
                test = Some(t.trim());
            } else if let Some(c) = line.strip_prefix("; check-not:") {
                check_nots.push(c.trim());
            } else if let Some(c) = line.strip_prefix("; check:") {
                checks.push(c.trim());
            } else if line == "; diffcheck" {
                dcheck = true;
            }
        }
        let t = test.unwrap_or_else(|| panic!("{path:?}: missing `; test <pass>`"));
        let p = pass(t);
        let mut funcs = cranelift_reader::parse_functions(&src)
            .unwrap_or_else(|e| panic!("{path:?}: parse failed: {e}"));
        for func in funcs.iter_mut() {
            let before = func.clone();
            let n = p(func);
            let _ = n; // `check`/`check-not` lines assert the shape
            if let Err(e) = cranelift_codegen::verify_function(func, &flags) {
                panic!("{path:?}: {t} produced invalid IR: {e}\n{}", func.display());
            }
            if dcheck && func.signature.call_conv == cranelift_codegen::isa::CallConv::Fast {
                diffcheck(&before, func, &DcFacts::default(), &DcConfig::default())
                    .unwrap_or_else(|e| panic!("{path:?}: {t} broke semantics: {e}"));
            }
            let text = func.display().to_string();
            for c in &checks {
                assert!(text.contains(c), "{path:?}: missing `{c}` in:\n{text}");
            }
            for c in &check_nots {
                assert!(!text.contains(c), "{path:?}: found `{c}` in:\n{text}");
            }
        }
        ran += 1;
    }
    assert!(ran > 0, "no .clif files under {}", dir.display());
    eprintln!("filetests: ran {ran} files");
}
