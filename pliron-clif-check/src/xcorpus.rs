//! `clif-check xcorpus <dirA> <dirB> <out.json>` and `clif-check xpair
//! <a.clif> <b.clif>`: a differential check of the pliron-IR passes by
//! ablation across two recorded corpora.
//!
//! `<dirA>` is a `PLIRON_CORPUS` dump of a build with one IR pass (or a
//! set) switched off, `<dirB>` the dump of the same build with it on. A
//! corpus file is a function as lowered, after the IR passes and before
//! the CLIF pipeline, so the two files of one symbol differ by what the
//! pass did to that function, and the CLIF interpreter can compare them
//! (`diffcheck::check_cross`): A is the reference, B the function under
//! test. With `--pipeline` A is also compared with the CLIF pipeline's
//! output for B, which is where a fact the IR pass left stale shows (a
//! lifetime marker `slotmerge` trusts).
//!
//! Functions are paired by symbol (`self=` in the facts header; the `.N.`
//! of a file name is the order the build happened to lower in). A symbol
//! recorded several times (one copy per CGU) is paired when all but one
//! copy on each side have a byte-identical partner. A pair whose two
//! functions call different sets of callees is not the same function any
//! more (the inliner decided differently) and is skipped, as is one whose
//! signature differs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cranelift_codegen::ir::{ExternalName, Function, InstructionData};
use cranelift_codegen::isa::TargetIsa;
use pliron_clif_opt::diffcheck::{self, Cross, OwnedFacts, Syms};
use pliron_clif_opt::pipeline;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{Opts, collect_clif, parse_facts, with_pipe};

/// One side of a pair.
struct Side {
    func: Function,
    facts: OwnedFacts,
    syms: Syms,
}

fn load_side(text: &str) -> Option<Side> {
    let mut fs = cranelift_reader::parse_functions(text).ok()?;
    if fs.is_empty() {
        return None;
    }
    let func = fs.remove(0);
    let facts = parse_facts(text, &func);
    let syms = anon_by_content(facts.syms_of(&func));
    Some(Side { func, facts, syms })
}

/// The backend names a constant it makes up `__rcg_alloc.<n>`, `n` counting
/// in the order the build declared them: two builds of one program number
/// them differently, and one constant got two addresses in the pair
/// (`TryFromIntError`'s `Debug`: its vtable). Such a symbol whose image is
/// recorded goes by its content instead: bytes, and the relocations with
/// their targets renamed the same way (to a fixed depth).
fn anon_by_content(mut s: Syms) -> Syms {
    const ANON: &str = "__rcg_alloc.";
    let mut rename: FxHashMap<String, String> = FxHashMap::default();
    for _ in 0..3 {
        let mut next: FxHashMap<String, String> = FxHashMap::default();
        for (sym, img) in &s.data {
            if !sym.starts_with(ANON) {
                continue;
            }
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            let mut eat = |b: &[u8]| {
                for &x in b {
                    h = (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3);
                }
            };
            eat(&img.bytes);
            for r in &img.relocs {
                eat(&r.off.to_le_bytes());
                eat(rename.get(&r.sym).unwrap_or(&r.sym).as_bytes());
                eat(&r.addend.to_le_bytes());
            }
            next.insert(sym.clone(), format!("__rcg_alloc#{h:016x}"));
        }
        rename = next;
    }
    if rename.is_empty() {
        return s;
    }
    let re = |n: &String| rename.get(n).cloned().unwrap_or_else(|| n.clone());
    for v in s.names.values_mut() {
        *v = re(v);
    }
    s.data = std::mem::take(&mut s.data)
        .into_iter()
        .map(|(k, mut img)| {
            for r in &mut img.relocs {
                r.sym = re(&r.sym);
            }
            (re(&k), img)
        })
        .collect();
    s
}

/// The symbol a corpus file was recorded for: `self=` of its facts line,
/// which is the first line.
fn self_symbol(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::BufReader::new(std::fs::File::open(path).ok()?).read_line(&mut line).ok()?;
    let at = line.find(" self=")?;
    Some(line[at + 6..].split_whitespace().next()?.to_string())
}

/// The symbols of the functions `f` calls directly, libcalls aside
/// (`memcpy` and friends run for real and an IR pass adds and removes
/// them), and how many indirect call sites it has (`indirect*<n>`): a
/// call through a pointer that one build loads from a constant table
/// and the other calls directly or inlines (`devirt`, which
/// `PLIRON_CONSTLOAD=0` turns off with `constload`) is an inliner
/// decision like any other. `Err`: a callee the recorder had no symbol
/// for.
fn callees(f: &Function, syms: &Syms) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    let mut indirect = 0usize;
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if matches!(
                f.dfg.insts[i].opcode(),
                cranelift_codegen::ir::Opcode::CallIndirect
                    | cranelift_codegen::ir::Opcode::TryCallIndirect
                    | cranelift_codegen::ir::Opcode::ReturnCallIndirect
            ) {
                indirect += 1;
            }
            let (InstructionData::Call { func_ref, .. } | InstructionData::TryCall { func_ref, .. }) =
                f.dfg.insts[i]
            else {
                continue;
            };
            let name = &f.dfg.ext_funcs[func_ref].name;
            if matches!(name, ExternalName::LibCall(_)) {
                continue;
            }
            let d = name.display(Some(&f.params)).to_string();
            match syms.names.get(&d) {
                Some(s) => {
                    out.insert(s.clone());
                }
                None => return Err(d),
            }
        }
    }
    if indirect > 0 {
        out.insert(format!("indirect*{indirect}"));
    }
    Ok(out)
}

/// What `b`'s build knows about the entry parameters holds for `a`'s as
/// well (they are one source function): the sandbox's inputs respect
/// both, since `b`'s pipeline may have used any of it.
fn merge_param_facts(fa: &mut OwnedFacts, a: &Function, fb: &OwnedFacts, b: &Function) {
    let (Some(ea), Some(eb)) = (a.layout.entry_block(), b.layout.entry_block()) else {
        return;
    };
    for (&pa, &pb) in a.dfg.block_params(ea).iter().zip(b.dfg.block_params(eb)) {
        if fb.nonnull.contains(&pb) {
            fa.nonnull.insert(pa);
        }
        if fb.noalias.contains(&pb) {
            fa.noalias.insert(pa);
        }
        if let Some(&(n, w)) = fb.deref.get(&pb) {
            let e = fa.deref.entry(pa).or_insert((n, w));
            *e = (e.0.max(n), e.1 && w);
        }
        if let Some(&n) = fb.frozen.get(&pb) {
            let e = fa.frozen.entry(pa).or_insert(n);
            *e = (*e).max(n);
        }
    }
}

/// One comparison's verdict.
#[derive(Default, Clone)]
struct Verdict {
    /// `ok`, `inconclusive`, `mismatch`, `verifier`, `panic`, or empty
    /// when the comparison did not run.
    status: &'static str,
    msg: String,
    compared: u32,
    inconclusive: u32,
}

fn verdict(r: std::thread::Result<Result<diffcheck::Summary, String>>) -> Verdict {
    match r {
        Ok(Ok(s)) => Verdict {
            status: if s.compared == 0 { "inconclusive" } else { "ok" },
            msg: s.why.unwrap_or_default().lines().next().unwrap_or("").to_string(),
            compared: s.compared,
            inconclusive: s.inconclusive,
        },
        Ok(Err(m)) => Verdict { status: "mismatch", msg: m, ..Default::default() },
        Err(_) => Verdict { status: "inconclusive", msg: "checker panicked".into(), ..Default::default() },
    }
}

/// A pair's outcome.
#[derive(Default)]
struct Row {
    symbol: String,
    a: String,
    b: String,
    /// `identical`, `skipped: ...`, `parse`, or the verdict of A against B.
    status: String,
    direct: Verdict,
    /// A against `pipeline(B)` (`--pipeline`).
    piped: Verdict,
    /// For a `--pipeline` mismatch: B against `pipeline(B)` on B's own
    /// terms, i.e. what `report --check` says of B.
    b_alone: &'static str,
}

/// The pair, checked. `text_*` are the two files.
/// Trials per pair unless `--trials`/`PLIRON_DIFFCHECK_TRIALS` says
/// otherwise: four times `report`'s. An IR pass's wrong value often shows
/// only in a loop's second iteration, which few seeds reach
/// (`tests/mcrdwrap` needs about 30).
const XTRIALS: u32 = 64;

fn check_pair(isa: &dyn TargetIsa, text_a: &str, text_b: &str, o: &Opts, row: &mut Row) -> Option<Function> {
    let cfg = diffcheck::Config {
        trials: if o.trials_given { o.cfg.trials } else { XTRIALS },
        fuel: o.cfg.fuel,
        seed: o.cfg.seed,
    };
    if text_a == text_b {
        row.status = "identical".into();
        return None;
    }
    let (Some(mut a), Some(b)) = (load_side(text_a), load_side(text_b)) else {
        row.status = "parse".into();
        return None;
    };
    if a.func.signature != b.func.signature {
        row.status = "skipped: signature differs".into();
        return None;
    }
    match (callees(&a.func, &a.syms), callees(&b.func, &b.syms)) {
        (Ok(ca), Ok(cb)) if ca == cb => {}
        (Ok(ca), Ok(cb)) => {
            row.status = "skipped: calls differ".into();
            row.direct.msg = ca.symmetric_difference(&cb).take(4).cloned().collect::<Vec<_>>().join(" ");
            return None;
        }
        (Err(d), _) | (_, Err(d)) => {
            row.status = "skipped: unnamed callee".into();
            row.direct.msg = d;
            return None;
        }
    }
    merge_param_facts(&mut a.facts, &a.func, &b.facts, &b.func);
    // B's callee facts, by symbol.
    let key = |d: &str| b.syms.names.get(d).cloned().unwrap_or_else(|| d.to_string());
    let nowrite: FxHashSet<String> = b
        .facts
        .nowrite
        .keys()
        .filter_map(|fr| b.func.dfg.ext_funcs.get(*fr))
        .map(|d| key(&d.name.display(Some(&b.func.params)).to_string()))
        .collect();
    let extent: FxHashMap<(String, u32), u64> =
        b.facts.stub_extent.iter().map(|((d, k), n)| ((key(d), *k), *n)).collect();
    let self_symbol = a.facts.self_symbol.clone().or_else(|| b.facts.self_symbol.clone());
    let cross = Cross {
        before: &a.syms,
        after: &b.syms,
        self_symbol: self_symbol.as_deref(),
        nowrite: &nowrite,
        extent: &extent,
    };
    let facts = a.facts.facts();
    let run = |after: &Function| {
        verdict(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            diffcheck::check_cross(&a.func, after, &facts, Some(&cross), &cfg).map_err(|m| m.to_string())
        })))
    };
    row.direct = run(&b.func);
    row.status = row.direct.status.into();
    if !o.pipeline {
        return None;
    }
    let piped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_pipe(isa, b.func.clone(), &b.facts, |p| {
            pipeline::run_pipeline(p);
            p.clctx.func.clone()
        })
    }));
    let piped = match piped {
        Ok(f) => f,
        Err(e) => {
            row.piped.status = "panic";
            row.piped.msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            return None;
        }
    };
    if let Err(e) = cranelift_codegen::verify_function(
        &piped,
        &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
    ) {
        row.piped.status = "verifier";
        row.piped.msg = e.to_string().lines().take(3).collect::<Vec<_>>().join(" | ");
        return Some(piped);
    }
    row.piped = run(&piped);
    if row.piped.status == "mismatch" {
        // Is it the pipeline on B, whatever A is? Then `report --check`
        // on B's corpus shows it too and names the step.
        let alone = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            diffcheck::check(&b.func, &piped, &b.facts.facts(), &cfg).map_err(|m| m.to_string())
        }));
        row.b_alone = verdict(alone).status;
    }
    Some(piped)
}

/// `xpair <a.clif> <b.clif>`: one pair, in full.
pub fn xpair(isa: &dyn TargetIsa, a: &Path, b: &Path, o: &Opts) -> bool {
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let (ta, tb) = (read(a), read(b));
    let mut row = Row::default();
    let piped = check_pair(isa, &ta, &tb, o, &mut row);
    println!("A against B: {} {}", row.status, row.direct.msg);
    if row.direct.status == "ok" || row.direct.status == "inconclusive" {
        println!("  compared={} inconclusive={}", row.direct.compared, row.direct.inconclusive);
    }
    if !row.piped.status.is_empty() {
        println!("A against pipeline(B): {} {}", row.piped.status, row.piped.msg);
        if row.piped.status == "ok" || row.piped.status == "inconclusive" {
            println!("  compared={} inconclusive={}", row.piped.compared, row.piped.inconclusive);
        }
        if !row.b_alone.is_empty() {
            println!("B against pipeline(B), B's own facts: {}", row.b_alone);
        }
    }
    if o.print
        && let (Some(sa), Some(sb)) = (load_side(&ta), load_side(&tb))
    {
        println!("---- A\n{}---- B\n{}", sa.func.display(), sb.func.display());
        if let Some(p) = piped {
            println!("---- pipeline(B)\n{}", p.display());
        }
    }
    !bad(&row)
}

fn bad(r: &Row) -> bool {
    r.status == "mismatch" || matches!(r.piped.status, "mismatch" | "verifier" | "panic")
}

/// The files of a corpus by symbol.
fn index(dir: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_clif(dir, &mut files);
    files.sort();
    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for f in files {
        if let Some(s) = self_symbol(&f) {
            out.entry(s).or_default().push(f);
        }
    }
    out
}

/// `xcorpus <dirA> <dirB> <out.json>`: every pair, on `--jobs` threads.
pub fn xcorpus(isa: &std::sync::Arc<dyn TargetIsa>, dir_a: &Path, dir_b: &Path, out: &Path, o: &Opts) -> bool {
    let (ia, ib) = (index(dir_a), index(dir_b));
    // The work list: (symbol, file in A, file in B), or a row that is
    // decided already.
    let mut work: Vec<(String, Option<PathBuf>, Option<PathBuf>, &'static str)> = Vec::new();
    let symbols: BTreeSet<&String> = ia.keys().chain(ib.keys()).collect();
    let rel = |root: &Path, p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    for s in symbols {
        if o.only.as_ref().is_some_and(|f| !s.contains(f.as_str())) {
            continue;
        }
        match (ia.get(s), ib.get(s)) {
            (Some(_), None) => work.push((s.clone(), None, None, "only in A")),
            (None, Some(_)) => work.push((s.clone(), None, None, "only in B")),
            (Some(fa), Some(fb)) if fa.len() == 1 && fb.len() == 1 => {
                work.push((s.clone(), Some(fa[0].clone()), Some(fb[0].clone()), ""))
            }
            (Some(fa), Some(fb)) => {
                // One copy per CGU. Copies with a byte-identical partner
                // on the other side are that partner's pair; a single
                // copy left on each side is a pair too.
                let read = |fs: &[PathBuf]| -> Vec<(PathBuf, String)> {
                    fs.iter().map(|f| (f.clone(), std::fs::read_to_string(f).unwrap_or_default())).collect()
                };
                let (mut ta, mut tb) = (read(fa), read(fb));
                let mut same = 0;
                ta.retain(|(_, t)| match tb.iter().position(|(_, u)| u == t) {
                    Some(k) => {
                        tb.swap_remove(k);
                        same += 1;
                        false
                    }
                    None => true,
                });
                for _ in 0..same {
                    work.push((s.clone(), None, None, "identical"));
                }
                if ta.len() == 1 && tb.len() == 1 {
                    work.push((s.clone(), Some(ta.remove(0).0), Some(tb.remove(0).0), ""));
                } else {
                    for _ in 0..ta.len().max(tb.len()) {
                        work.push((s.clone(), None, None, "ambiguous"));
                    }
                }
            }
            (None, None) => unreachable!(),
        }
    }
    if o.every > 1 {
        work = work.into_iter().step_by(o.every).collect();
    }
    work.truncate(o.limit);
    std::panic::set_hook(Box::new(|_| {}));
    let t0 = std::time::Instant::now();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let rows = std::sync::Mutex::new(Vec::with_capacity(work.len()));
    std::thread::scope(|sc| {
        for _ in 0..o.jobs {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some((sym, fa, fb, fixed)) = work.get(i) else { break };
                let mut row = Row { symbol: sym.clone(), status: fixed.to_string(), ..Default::default() };
                if let (Some(fa), Some(fb)) = (fa, fb) {
                    row.a = rel(dir_a, fa);
                    row.b = rel(dir_b, fb);
                    match (std::fs::read_to_string(fa), std::fs::read_to_string(fb)) {
                        (Ok(ta), Ok(tb)) => {
                            check_pair(&**isa, &ta, &tb, o, &mut row);
                        }
                        _ => row.status = "parse".into(),
                    }
                }
                rows.lock().unwrap().push(row);
            });
        }
    });
    let _ = std::panic::take_hook();
    let mut rows = rows.into_inner().unwrap();
    rows.sort_by(|x, y| (&x.symbol, &x.a).cmp(&(&y.symbol, &y.a)));
    let wall = t0.elapsed();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut pcounts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &rows {
        *counts.entry(r.status.clone()).or_default() += 1;
        if !r.piped.status.is_empty() {
            *pcounts.entry(r.piped.status).or_default() += 1;
        }
    }
    let v = |v: &Verdict| {
        serde_json::json!({
            "status": v.status, "msg": v.msg, "compared": v.compared, "inconclusive": v.inconclusive,
        })
    };
    let json = serde_json::json!({
        "generated": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        "a": dir_a.display().to_string(),
        "b": dir_b.display().to_string(),
        "pipeline": o.pipeline,
        "wall_ms": wall.as_secs_f64() * 1e3,
        "counts": counts,
        "pipeline_counts": pcounts,
        "functions": rows.iter().map(|r| serde_json::json!({
            "symbol": r.symbol, "a": r.a, "b": r.b, "status": r.status,
            "direct": v(&r.direct), "piped": v(&r.piped), "b_alone": r.b_alone,
        })).collect::<Vec<_>>(),
    });
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(&json).unwrap()).expect("write report");
    std::fs::rename(&tmp, out).expect("rename report");
    println!("xcorpus: {} pairs in {wall:.2?} ({} jobs) -> {}", rows.len(), o.jobs, out.display());
    println!("  A against B:           {counts:?}");
    if o.pipeline {
        println!("  A against pipeline(B): {pcounts:?}");
    }
    let wasm = if pliron_cfg::env_is("PLIRON_CLIF_WASM", "1") { "PLIRON_CLIF_WASM=1 " } else { "" };
    for r in rows.iter().filter(|r| bad(r)) {
        println!("\n== {}", r.symbol);
        if r.status == "mismatch" {
            println!("A against B: MISMATCH\n{}", r.direct.msg);
        }
        if matches!(r.piped.status, "mismatch" | "verifier" | "panic") {
            println!("A against pipeline(B): {}\n{}", r.piped.status.to_uppercase(), r.piped.msg);
            if !r.b_alone.is_empty() {
                println!("B against pipeline(B), B's own facts: {}", r.b_alone);
            }
        }
        println!(
            "replay: {wasm}clif-check xpair {} {}{}",
            dir_a.join(&r.a).display(),
            dir_b.join(&r.b).display(),
            if o.pipeline { " --pipeline" } else { "" }
        );
    }
    !rows.iter().any(bad)
}
