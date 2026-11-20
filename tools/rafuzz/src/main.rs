//! Seeded fuzz driver for the vendored regalloc2 (`harness/rafuzz.py`).
//!
//!   rafuzz [--start S] [--count N | --secs T] [--out DIR] [--replay SEED] [--print]
//!
//! Each seed expands (SplitMix64) into a byte string that regalloc2's own
//! generator (`fuzzing::func`, the options of `fuzzing::ion` plus the
//! PLIRON hooks) turns into a function. The function goes through `ion`,
//! then the symbolic checker (with the `edit_vregs` label check), and on
//! one seed in eight through a fresh context too, whose output must equal
//! the reused context's. The `PLIRON_RA2_*` knobs are read from the
//! environment once per process, as in the backend: the harness runs one
//! process per knob combination.
//!
//! Seed modes (`mode_of`): one seed in four allocates with 6..=16
//! registers per class instead of 32 (more splitting and spilling; an
//! allocation error there means the generator made an unallocatable
//! instruction and the seed is skipped), one in five leaves the PLIRON
//! hooks unanswered (upstream's exact generator), the rest answer them
//! (block frequencies, cold blocks, remat vregs, unmerged parameters).
//!
//! Output: one `FAIL seed=N ...` line per failure (the function and the
//! error go to `DIR/seed-N.txt`), then one `rafuzz:` summary line. Exit 1
//! when anything failed.

use arbitrary::Unstructured;
use regalloc2::fuzzing::func::{self, Func, Options};
use regalloc2::{checker::Checker, Ctx, Edit, MachineEnv, RegClass, RegallocOptions};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

const BASE: Options = Options {
    reused_inputs: true,
    fixed_regs: true,
    fixed_nonallocatable: true,
    clobbers: true,
    reftypes: true,
    callsite_ish_constraints: true,
    ..Options::DEFAULT
};

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

struct Case {
    mode: &'static str,
    opts: Options,
    preferred: usize,
    bytes: Vec<u8>,
    annotate: bool,
    validate: bool,
    fresh: bool,
}

fn case_of(seed: u64) -> Case {
    let mut st = seed ^ 0x5eed_0f0c_a2a2;
    let r = splitmix(&mut st);
    // Byte budget: 256 B .. 64 KiB, log-uniform (small functions are
    // quick to read when they fail, large ones stress splitting).
    let len = 256usize << (r % 9);
    let len = len + (splitmix(&mut st) as usize % len);
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        bytes.extend_from_slice(&splitmix(&mut st).to_le_bytes());
    }
    let mut opts = BASE;
    let (mode, preferred) = if seed % 4 == 0 {
        let total = 6 + (splitmix(&mut st) % 11) as usize;
        opts.alloc_regs = total;
        opts.num_uses_per_inst = 0..=6;
        opts.num_clobbers_per_inst = 0..=4;
        opts.num_callsite_ish_vregs_per_inst = 0..=8;
        opts.pliron_hints = true;
        ("tight", (total * 3 / 4).max(1))
    } else if seed % 5 == 0 {
        ("plain", 24)
    } else {
        opts.pliron_hints = true;
        ("hints", 24)
    };
    let f = splitmix(&mut st);
    Case { mode, opts, preferred, bytes, annotate: f & 1 != 0, validate: f & 2 != 0, fresh: f & 0x1c == 0 }
}

enum Outcome {
    /// The bytes ran out before a function was complete.
    NoFunc,
    /// Allocation error under a reduced register file: not a bug.
    Unallocatable,
    Ok { edits: usize, stack: usize },
    Fail { kind: &'static str, msg: String, func: Option<Func> },
}

thread_local! {
    static CTX: RefCell<Ctx> = RefCell::default();
    static PANIC: RefCell<String> = const { RefCell::new(String::new()) };
}

fn env_of(c: &Case) -> MachineEnv {
    if c.opts.alloc_regs < 32 {
        func::machine_env_sized(c.preferred, c.opts.alloc_regs)
    } else {
        func::machine_env()
    }
}

fn same_output(a: &regalloc2::Output, b: &regalloc2::Output) -> bool {
    a.num_spillslots == b.num_spillslots
        && a.edits.len() == b.edits.len()
        && a.edits.iter().zip(&b.edits).all(|((pa, Edit::Move { from: fa, to: ta }), (pb, Edit::Move { from: fb, to: tb }))| {
            pa == pb && fa == fb && ta == tb
        })
        && a.edit_vregs == b.edit_vregs
        && a.allocs == b.allocs
        && a.inst_alloc_offsets == b.inst_alloc_offsets
}

/// The generator gives up (a block parameter with no value of its class
/// in reach) on about two seeds in three; try a few windows of the bytes.
fn generate(c: &Case) -> Option<Func> {
    (0..8).find_map(|k| {
        let mut u = Unstructured::new(&c.bytes[(k * 97).min(c.bytes.len())..]);
        Func::arbitrary_with_options(&mut u, &c.opts).ok()
    })
}

/// A stack allocation names `spillslot_size(class)` units, aligned to that
/// size (named by its last unit when the function says so). Every one an
/// operand or edit uses must fit `num_spillslots`, and slots of different
/// start must not overlap: a v128 spill beside an 8-byte one once
/// overwrote it (greedy, `contains_rng`), which the value checker cannot see.
fn check_slot_extents(f: &Func, out: &regalloc2::Output) -> Result<(), String> {
    use regalloc2::Function;
    let mut used: Vec<(usize, usize, RegClass)> = Vec::new();
    let mut note = |a: regalloc2::Allocation, class: RegClass| {
        if let Some(sl) = a.as_stack() {
            let size = f.spillslot_size(class).max(1);
            let end = sl.index() + 1;
            let start = if f.multi_spillslot_named_by_last_slot() { end.saturating_sub(size) } else { sl.index() };
            used.push((start, size, class));
        }
    };
    for i in 0..f.num_insts() {
        let inst = regalloc2::Inst::new(i);
        for (op, &a) in f.inst_operands(inst).iter().zip(out.inst_allocs(inst)) {
            note(a, op.class());
        }
    }
    for (_, Edit::Move { from, to }) in &out.edits {
        match (from.as_reg(), to.as_reg()) {
            (Some(r), None) => note(*to, r.class()),
            (None, Some(r)) => note(*from, r.class()),
            _ => {}
        }
    }
    used.sort();
    used.dedup();
    for w in used.windows(2) {
        let (s0, z0, c0) = w[0];
        let (s1, _, c1) = w[1];
        if s0 + z0 > s1 && (s0 != s1 || c0 != c1) {
            return Err(format!("slot [{s0}, {}) of {c0:?} overlaps the one at {s1} of {c1:?}", s0 + z0));
        }
    }
    for &(s, z, c) in &used {
        if s % z != 0 {
            return Err(format!("{c:?} slot at {s} is not aligned to {z}"));
        }
        if s + z > out.num_spillslots {
            return Err(format!("{c:?} slot [{s}, {}) past num_spillslots {}", s + z, out.num_spillslots));
        }
    }
    Ok(())
}

fn run_case(c: &Case) -> Outcome {
    let Some(f) = generate(c) else {
        return Outcome::NoFunc;
    };
    let env = env_of(c);
    // PLIRON: honour `PLIRON_RA_ALGO=greedy|fast|ion` (harness/rafuzz.py).
    let ro = RegallocOptions {
        verbose_log: c.annotate,
        validate_ssa: c.validate,
        algorithm: regalloc2::ra_algo_from_env(),
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        CTX.with(|ctx| -> Result<(usize, usize), (&'static str, String)> {
            let mut ctx = ctx.borrow_mut();
            if let Err(e) = regalloc2::run_with_ctx(&f, &env, &ro, &mut ctx) {
                return Err(("alloc", format!("{e:?}")));
            }
            let out = &ctx.output;
            let mut ck = Checker::new(&f, &env);
            ck.prepare(out);
            if let Err(e) = ck.run() {
                return Err(("checker", format!("{e:?}")));
            }
            if let Err(e) = check_slot_extents(&f, out) {
                return Err(("slots", e));
            }
            if c.fresh {
                let mut fresh = Ctx::default();
                regalloc2::run_with_ctx(&f, &env, &ro, &mut fresh).map_err(|e| ("fresh-alloc", format!("{e:?}")))?;
                if !same_output(out, &fresh.output) {
                    return Err(("ctx-reuse", "a reused Ctx and a fresh one gave different outputs".into()));
                }
            }
            let stack = out
                .edits
                .iter()
                .filter(|(_, Edit::Move { from, to })| from.is_stack() || to.is_stack())
                .count();
            Ok((out.edits.len(), stack))
        })
    }));
    match r {
        Ok(Ok((edits, stack))) => Outcome::Ok { edits, stack },
        Ok(Err(("alloc", _))) if c.opts.alloc_regs < 32 => Outcome::Unallocatable,
        Ok(Err((kind, msg))) => {
            // A context left half-way is not reused.
            CTX.with(|ctx| *ctx.borrow_mut() = Ctx::default());
            Outcome::Fail { kind, msg, func: Some(f) }
        }
        Err(_) => {
            CTX.with(|ctx| *ctx.borrow_mut() = Ctx::default());
            let msg = PANIC.with(|p| std::mem::take(&mut *p.borrow_mut()));
            Outcome::Fail { kind: "panic", msg, func: Some(f) }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut start, mut count, mut secs) = (0u64, u64::MAX, None::<f64>);
    let mut out = std::path::PathBuf::from("target/rafuzz/fail");
    let (mut replay, mut print) = (None::<u64>, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut v = || it.next().unwrap_or_else(|| panic!("value after {a}")).clone();
        match a.as_str() {
            "--start" => start = v().parse().expect("--start N"),
            "--count" => count = v().parse().expect("--count N"),
            "--secs" => secs = Some(v().parse().expect("--secs T")),
            "--out" => out = v().into(),
            "--replay" => replay = Some(v().parse().expect("--replay SEED")),
            "--print" => print = true,
            _ => {
                eprintln!("usage: rafuzz [--start S] [--count N | --secs T] [--out DIR] [--replay SEED] [--print]");
                std::process::exit(2);
            }
        }
    }
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        PANIC.with(|p| *p.borrow_mut() = format!("{info}\n{bt}"));
    }));
    if let Some(s) = replay {
        (start, count, secs) = (s, 1, None);
    }
    if count == u64::MAX && secs.is_none() {
        count = 1000;
    }
    let deadline = secs.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let t0 = Instant::now();
    let (mut n, mut nofunc, mut unalloc, mut edits, mut stack, mut fails) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut modes = std::collections::BTreeMap::<&str, u64>::new();
    let mut seed = start;
    while seed - start < count && deadline.is_none_or(|d| Instant::now() < d) {
        let c = case_of(seed);
        if print && replay.is_some() {
            if let Some(f) = generate(&c) {
                println!("seed {seed} mode {} regs {}:\n{f:?}", c.mode, c.opts.alloc_regs);
            }
        }
        n += 1;
        match run_case(&c) {
            Outcome::NoFunc => nofunc += 1,
            Outcome::Unallocatable => unalloc += 1,
            Outcome::Ok { edits: e, stack: s } => {
                *modes.entry(c.mode).or_default() += 1;
                edits += e as u64;
                stack += s as u64;
            }
            Outcome::Fail { kind, msg, func } => {
                fails += 1;
                let first = msg.lines().next().unwrap_or("").chars().take(300).collect::<String>();
                println!("FAIL seed={seed} mode={} regs={} kind={kind}: {first}", c.mode, c.opts.alloc_regs);
                let _ = std::fs::create_dir_all(&out);
                let mut text = String::new();
                let knobs: Vec<String> = std::env::vars().filter(|(k, _)| k.starts_with("PLIRON_RA")).map(|(k, v)| format!("{k}={v}")).collect();
                let _ = writeln!(text, "seed {seed} mode {} regs {} kind {kind}\nknobs: {}\n", c.mode, c.opts.alloc_regs, knobs.join(" "));
                let _ = writeln!(text, "{msg}\n");
                if let Some(f) = func {
                    let _ = writeln!(text, "{f:?}");
                }
                let _ = std::fs::write(out.join(format!("seed-{seed}.txt")), text);
            }
        }
        seed += 1;
    }
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "rafuzz: seeds {start}..{seed} ({n} in {dt:.1}s, {:.0}/s): checked {:?}, no-func {nofunc}, unallocatable {unalloc}, edits {edits} (stack {stack}), potts {:?}, failures {fails}",
        n as f64 / dt.max(1e-9),
        modes,
        regalloc2::pliron_potts_counts()
    );
    std::process::exit(if fails > 0 { 1 } else { 0 });
}
