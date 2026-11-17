//! Differential checking of CLIF passes with `cranelift-interpreter`.
//!
//! [`check`] runs a function before and after a transformation on random
//! inputs inside a sandbox and compares what a caller could observe: the
//! return values, the sequence of calls made (callee + arguments), and every
//! byte written to memory the function can reach. A difference is a
//! miscompile candidate; the caller gets the input that shows it.
//!
//! The sandbox gives the function a flat memory made of disjoint regions:
//! one per pointer parameter, a few shared "heap" regions, and one per data
//! symbol. Region words are seeded with a mix of small integers, random bits
//! and pointers back into the regions, so loaded pointers can be followed.
//! Calls to other functions hit deterministic stubs that record the call and
//! return values derived from it; callees not known to be `nowrite` also
//! write a word through each pointer argument. `memcpy`/`memmove`/`memset`/
//! `memcmp` libcalls run for real.
//!
//! Anything the sandbox can't model (an unsupported opcode, a wild pointer,
//! running out of fuel, a trap) makes that run *inconclusive*, never a
//! failure: a mismatch is reported only when the original ran to completion.
//! A trap is treated as undefined behavior, which a transformation may
//! refine away.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Once;

use cranelift_codegen::data_value::DataValue;
use cranelift_codegen::ir::{
    self, Block, BlockArg, ExternalName, FuncRef, Function, GlobalValue, GlobalValueData, Inst,
    InstructionData, LibCall, MemFlagsData, Opcode, SigRef, StackSlot, Type, Value, types,
};
use cranelift_interpreter::address::{Address, AddressFunctionEntry, AddressRegion, AddressSize};
use cranelift_interpreter::frame::Frame;
use cranelift_interpreter::instruction::DfgInstructionContext;
use cranelift_interpreter::interpreter::LibCallHandler;
use cranelift_interpreter::state::{InterpreterFunctionRef, MemoryError, State};
use cranelift_interpreter::step::{ControlFlow, CraneliftTrap, step};
use crate::vmap::VMap;
use rustc_hash::{FxHashMap, FxHashSet};

/// Facts the optimizer was allowed to assume; inputs are generated to
/// respect them so a pass that relies on them isn't flagged.
#[derive(Default)]
pub struct Facts<'a> {
    pub nonnull: Option<&'a FxHashSet<Value>>,
    pub noalias: Option<&'a FxHashSet<Value>>,
    /// The region-scoped subset of `noalias` (clone args): excluded from
    /// function-wide isolation, kept so replays feed `Pipe::scoped_na`.
    pub scoped_na: Option<&'a FxHashSet<Value>>,
    /// Entry param → (dereferenceable bytes, writable).
    pub deref: Option<&'a FxHashMap<Value, (u64, bool)>>,
    /// Entry params pointing at memory that never changes.
    pub frozen: Option<&'a FxHashMap<Value, u64>>,
    /// Callees known to write no memory (`true`) or none before a normal return.
    pub nowrite: Option<&'a FxHashMap<FuncRef, bool>>,
    /// The function's own name as its callees spell it (`u0:<FuncId>`), so
    /// self-recursion is interpreted instead of stubbed. Defaults to the
    /// function's `name`.
    pub self_name: Option<String>,
    /// Derived pointer → its base, as the pipeline tracks them: an inbounds
    /// offset, non-null whenever its base is (`jumpthread::NonNull`). The
    /// original computing 0 from a non-zero base is undefined behavior
    /// (the run stops); also saved in repros so `clif-check` replays a
    /// step faithfully.
    pub derived: Option<&'a VMap<Value>>,
    /// Callees that are libc's `memcmp`/`bcmp` under a user symbol: run
    /// like the `Memcmp` libcall, not stubbed, so expanding one inline
    /// (`memcmpk`) compares like for like.
    pub memcmp: FxHashSet<FuncRef>,
    /// `(callee as calls spell it, argument index)` → the most bytes any
    /// call of it may reach through that pointer operand (`Pipe::nocap`'s
    /// extents, see [`stub_extents`]): slotprom keeps the fields beyond it
    /// in registers across the call, so the stub's fill stops there too.
    /// An operand not listed is unbounded.
    pub stub_extent: FxHashMap<(String, u32), u64>,
    /// Values of the original that stand for `undef` ([`Syms::undef`]).
    /// They are 0 in a run, like the bytes of a fresh frame, and like
    /// those they take the other fill when a mismatch runs the original
    /// again: a byte of the output that moves with them is one the
    /// source program never determined, and the rewritten function may
    /// show anything there. Without this a pass that stops copying
    /// `undef` (memcpyopt forwarding a temporary's writes: the unwritten
    /// part of the destination keeps its old bytes where the original
    /// copied the temporary's zeros) is reported.
    pub undef: Option<&'a FxHashSet<Value>>,
    /// The original's `lifetime.start` markers (`OwnedFacts::lifetimes`,
    /// starts only): where a slot's bytes become undefined again. Given
    /// by `xcorpus` for the reference function only. In a run of the
    /// original the slot takes the frame's fill there, so a byte read
    /// from the previous life of a slot (an `Option<Expr>` temporary of a
    /// loop whose `None` arm leaves the payload alone, copied whole) is
    /// one the rerun under the other fill changes, not a determined one.
    /// Not used for one build's steps: a stale marker there is what
    /// slotmerge must not trust, and the check must keep reporting it.
    pub lifetime_starts: Option<&'a [(Vec<StackSlot>, Option<Inst>, Block, bool)]>,
}

/// [`Facts::stub_extent`] for `func`: per direct callee and argument, the
/// largest extent over its call sites, where `extent(call, arg)` is the
/// bound known for that site (`None`: the callee may reach anything). An
/// operand unbounded at any one site stays unbounded.
pub fn stub_extents(
    func: &Function,
    extent: impl Fn(Inst, u32) -> Option<u64>,
) -> FxHashMap<(String, u32), u64> {
    let mut out: FxHashMap<(String, u32), u64> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let (InstructionData::Call { func_ref, .. } | InstructionData::TryCall { func_ref, .. }) =
                func.dfg.insts[i]
            else {
                continue;
            };
            let name = func.dfg.ext_funcs[func_ref].name.display(Some(&func.params)).to_string();
            for k in 0..func.dfg.inst_args(i).len() as u32 {
                let e = extent(i, k).unwrap_or(u64::MAX);
                let slot = out.entry((name.clone(), k)).or_insert(e);
                *slot = (*slot).max(e);
            }
        }
    }
    out.retain(|_, e| *e != u64::MAX);
    out
}

/// The bytes of an immutable data object, as a corpus records them
/// (`; diffcheck-data:`): what a load from the symbol yields. The IR
/// passes fold such loads (`constload`), so a function recorded with the
/// pass on holds the constant where the other build's loads the symbol.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DataImage {
    pub bytes: Vec<u8>,
    pub relocs: Vec<Reloc>,
}

/// A pointer inside a [`DataImage`]: `off` holds the address of `sym`
/// (a function when `func`) plus `addend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reloc {
    pub off: u32,
    pub func: bool,
    pub sym: String,
    pub addend: i64,
}

/// How one recorded function spells the symbols it names. A callee is
/// `u0:<FuncId>` and a data object `u1:<DataId>` in the text, and the ids
/// are a build's own: two builds of one program number them differently.
#[derive(Debug, Clone, Default)]
pub struct Syms {
    /// External name as displayed (`u0:12`, `u1:65`) -> linker symbol.
    pub names: FxHashMap<String, String>,
    /// Linker symbol -> the object's image, where the recorder knew it.
    pub data: FxHashMap<String, DataImage>,
    /// Values that stand for `undef` (`; diffcheck-undef:`): the lowering
    /// writes a 0 for a value the program does not determine (an `undef`
    /// constant, a promoted variable read before it is written), and the
    /// function as recorded cannot be told from one that means the 0.
    /// See [`Facts::undef`].
    pub undef: Vec<Value>,
    /// Callee symbol -> the bytes its `sret` parameter returns
    /// (`; diffcheck-sret:`): a stub writes that much through the return
    /// pointer, not to the end of whatever slot holds it.
    pub sret: FxHashMap<String, u64>,
}

/// A pair recorded from two builds of the same program ([`check_cross`]):
/// everything the checker keys by a callee's or a data object's name goes
/// by the symbol, so the two functions' stubs, function addresses and
/// data regions agree.
pub struct Cross<'a> {
    pub before: &'a Syms,
    pub after: &'a Syms,
    /// The function's own symbol (self-recursion runs for real).
    pub self_symbol: Option<&'a str>,
    /// Callees, by symbol, that `after`'s build knows to write no memory.
    /// A fact about the callee holds for both builds, and `after`'s
    /// pipeline may have used it.
    pub nowrite: &'a FxHashSet<String>,
    /// `(callee symbol, argument) -> extent` known to `after`'s build
    /// ([`Facts::stub_extent`]); the smaller of the two builds' bounds
    /// is used.
    pub extent: &'a FxHashMap<(String, u32), u64>,
}

/// What a stub did with one call's arguments. In a cross-build pair the
/// second function's stubs repeat it: a stub stands for one callee with
/// one behavior, and how many bytes it writes through a pointer must not
/// depend on which of the caller's objects the pointer names. An IR pass
/// changes that (`memcpyopt`: `call f(sret tmp); memcpy(dst, tmp)` becomes
/// `call f(sret dst)`, a parameter's memory where the original passed a
/// stack slot).
#[derive(Debug, Clone, Default)]
pub struct CallRec {
    name: String,
    args: Vec<ArgRec>,
}

#[derive(Debug, Clone, Copy)]
struct ArgRec {
    /// The argument pointed into the caller's frame: its value is the
    /// caller's to choose, so the other function may pass any pointer.
    frame: bool,
    /// Bytes the stub wrote through it.
    fill: u32,
    /// The window the stub read through it: its length, and which of its
    /// bytes had been written (bit `j`: byte `j`).
    seen: u32,
    mask: u64,
    /// The bytes it read there (unwritten ones 0).
    bytes: [u8; 64],
}

pub struct Config {
    /// Inputs tried per function pair.
    pub trials: u32,
    /// Interpreter steps per run before giving up.
    pub fuel: u64,
    pub seed: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            trials: 16,
            fuel: 200_000,
            seed: 0x5eed,
        }
    }
}

/// What one run of a function showed to the outside.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Returned {
        rets: Vec<DataValue>,
        calls: Vec<(String, Vec<DataValue>)>,
        /// Final value of every memory byte that differs from its seed, by address.
        writes: Vec<(u64, u8)>,
    },
    /// Hit a trap: undefined behavior as far as this check is concerned.
    Trapped(String),
    /// The sandbox couldn't model something; says nothing either way.
    Inconclusive(String),
}

#[derive(Default, Debug, Clone)]
pub struct Summary {
    /// Trials where the original returned normally and the result matched.
    pub compared: u32,
    pub inconclusive: u32,
    /// Why the first inconclusive trial couldn't be compared.
    pub why: Option<String>,
}

/// A trial whose outcomes differ.
#[derive(Debug)]
pub struct Mismatch {
    pub trial: u32,
    pub args: Vec<DataValue>,
    pub before: Outcome,
    pub after: Outcome,
    /// `before` run again under the other stack fill: a byte the two
    /// original runs agree on is one the source program determines.
    pub rerun: Option<Outcome>,
    /// Cross-build pair: the function's own return area ([`sret_area`]).
    pub sret: Option<(u64, u64)>,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "trial {} args {:?}", self.trial, self.args)?;
        writeln!(f, "  before: {}", describe(&self.before))?;
        write!(f, "  after:  {}", describe(&self.after))?;
        // The lists above stop at 64 bytes each; name the bytes that
        // decided it. `-` is a byte left at its seeded value.
        // Without a rerun that returned, no byte is known undetermined:
        // every differing byte is listed.
        if let (Outcome::Returned { writes: w0, .. }, Outcome::Returned { writes: wa, .. }) =
            (&self.before, &self.after)
        {
            let m = |w: &Vec<(u64, u8)>| w.iter().copied().collect::<BTreeMap<u64, u8>>();
            let (m0, ma) = (m(w0), m(wa));
            let m1 = match &self.rerun {
                Some(Outcome::Returned { writes: w1, .. }) => Some(m(w1)),
                _ => None,
            };
            let keys: std::collections::BTreeSet<u64> = m0
                .keys()
                .chain(m1.iter().flat_map(|m| m.keys()))
                .chain(ma.keys())
                .copied()
                .collect();
            let bad: Vec<u64> = keys
                .into_iter()
                .filter(|k| {
                    !unwritten_sret(self.sret, &m0, *k)
                        && m1.as_ref().is_none_or(|m1| m0.get(k) == m1.get(k))
                        && ma.get(k) != m0.get(k)
                })
                .collect();
            if !bad.is_empty() {
                let show = |v: Option<&u8>| v.map_or("-".to_string(), |b| format!("{b:x}"));
                let what = if m1.is_some() { "determined bytes" } else { "bytes" };
                write!(f, "\n  {} {what} differ (address: before, after):", bad.len())?;
                for k in bad.iter().take(32) {
                    write!(f, " {k:x}: {}, {};", show(m0.get(k)), show(ma.get(k)))?;
                }
            }
        }
        // A rerun that did not return pins nothing down and forgives
        // nothing: say so, or the report reads as if every byte counted.
        if let Some(r @ (Outcome::Trapped(_) | Outcome::Inconclusive(_))) = &self.rerun {
            write!(f, "\n  before, other stack fill: {}", describe(r))?;
        }
        Ok(())
    }
}

fn describe(o: &Outcome) -> String {
    match o {
        Outcome::Returned {
            rets,
            calls,
            writes,
        } => {
            let mut s = format!("returned {rets:?}");
            if !calls.is_empty() {
                let _ = write!(s, ", calls {calls:?}");
            }
            if !writes.is_empty() {
                let shown: Vec<_> = writes.iter().take(64).collect();
                let _ = write!(s, ", {} bytes written {shown:x?}", writes.len());
            }
            s
        }
        Outcome::Trapped(t) => format!("trapped: {t}"),
        Outcome::Inconclusive(r) => format!("inconclusive: {r}"),
    }
}

// r[impl verify.diffcheck.observe]
// r[impl verify.diffcheck.inconclusive]
/// Compare `before` and `after` on `cfg.trials` random inputs.
// r[impl clif.diffcheck.observe]
pub fn check(
    before: &Function,
    after: &Function,
    facts: &Facts,
    cfg: &Config,
) -> Result<Summary, Mismatch> {
    check_cross(before, after, facts, None, cfg)
}

/// [`check`] for two functions that need not come from one build: with
/// `cross`, `before` and `after` are one source function as two builds
/// lowered it (one of them with an IR pass off), each naming its callees
/// and data by its own ids. `facts` are `before`'s.
pub fn check_cross(
    before: &Function,
    after: &Function,
    facts: &Facts,
    cross: Option<&Cross>,
    cfg: &Config,
) -> Result<Summary, Mismatch> {
    let mut sum = Summary::default();
    if before.signature != after.signature {
        sum.inconclusive = cfg.trials;
        sum.why = Some("signature changed".into());
        return Ok(sum);
    }
    let ptrs = pointer_params(before, facts);
    // Odd trials also treat every other i64 param as a pointer: params
    // that only flow into calls or stores aren't caught by the heuristic,
    // and a pointer value is still a valid (if large) integer.
    let mut all_ptrs = ptrs.clone();
    if let Some(entry) = before.layout.entry_block() {
        for &p in before.dfg.block_params(entry) {
            if before.dfg.value_type(p) == ptr_ty() {
                all_ptrs.entry(p).or_default();
            }
        }
    }
    let (xb, xa) = (cross.map(|c| c.before), cross.map(|c| c.after));
    let mut nowrite_names = nowrite_by_name(before, facts, xb);
    // The return area is the function's to fill (cross-build pairs).
    let sret = cross.and_then(|_| sret_area(before));
    let layout = slot_layout(&[before, after]);
    let pair_syms = match cross.map(|c| pair_symbols(before, after, c)) {
        Some(Err(sym)) => {
            sum.inconclusive = cfg.trials;
            sum.why = Some(format!("data symbol {sym} has a different image in each build"));
            return Ok(sum);
        }
        Some(Ok(t)) => Some(t),
        None => None,
    };
    if let Some(c) = cross {
        nowrite_names.extend(c.nowrite.iter().cloned());
    }
    for trial in 0..cfg.trials {
        let seed = mix(cfg.seed, trial as u64);
        let mut world = World::new(
            before,
            if trial % 2 == 1 { &all_ptrs } else { &ptrs },
            facts,
            seed,
            &layout,
            xb,
        );
        if let (Some(c), Some((names, images))) = (cross, &pair_syms) {
            for (key, n) in c.extent {
                let x = world.extent.entry(key.clone()).or_insert(*n);
                *x = (*x).min(*n);
            }
            // A callee returning through `sret` writes its value's size
            // there, not to the end of the slot (`<[(FieldKey, Expr)]>::to_vec`
            // in dsa: `Expr::clone` into the head of a 64-byte tuple
            // temporary whose other field A had written before the call;
            // a fill to the end of the slot clobbered it in A only).
            for (sym, n) in c.before.sret.iter().chain(&c.after.sret) {
                let x = world.extent.entry((sym.clone(), 0)).or_insert(*n);
                *x = (*x).min(*n);
            }
            world.add_symbols(names, images);
        }
        let self_name = match cross {
            // No symbol: a name no callee has.
            Some(c) => c.self_symbol.unwrap_or("\0").to_string(),
            None => facts
                .self_name
                .clone()
                .unwrap_or_else(|| before.name.to_string()),
        };
        let (b, btrace) = run(before, &world, &nowrite_names, &self_name, cfg.fuel, true, xb, None);
        let blog = &btrace.log;
        if cross.is_some() && let Some(m) = prefix_mismatch(&b, &btrace, trial, &world, &|w| {
            // The run that pins the arguments down, as below.
            let w1 = World { stack_fill: 0xa5, slot_pad: 0x25, ..w.clone() };
            run(before, &w1, &nowrite_names, &self_name, cfg.fuel, true, xb, None)
        }, &|| run(after, &world, &nowrite_names, &self_name, cfg.fuel, false, xa, Some(blog.as_slice()))) {
            return Err(m);
        }
        if !matches!(b, Outcome::Returned { .. }) {
            sum.inconclusive += 1;
            sum.why
                .get_or_insert_with(|| format!("before {}", describe(&b)));
            continue;
        }
        // A cross-build `after` repeats what `before`'s stubs did.
        let replay = cross.map(|_| blog.as_slice());
        let (a, atrace) = run(after, &world, &nowrite_names, &self_name, cfg.fuel, false, xa, replay);
        match &a {
            Outcome::Inconclusive(r) if !r.starts_with("out of fuel") => {
                sum.inconclusive += 1;
                sum.why
                    .get_or_insert_with(|| format!("after inconclusive: {r}"));
            }
            _ if same(&a, &b) => sum.compared += 1,
            _ => {
                // `b` may not pin this trial down: a pass can legally
                // reshuffle uninitialized bytes (slotmerge folds a slot
                // onto live storage, so its never-written reads yield
                // real bytes, not the zero fill). Re-run `before` under a
                // different stack fill; `a` must match on every byte the
                // original itself kept stable.
                // The same for stack addresses: a pass may move a slot
                // (slotmerge), and `end - buf` of a stubbed call's result
                // then differs by the move (serde_json `Number`'s
                // `Display`: ryu's returned length). The re-run also
                // shifts every slot, so bytes that depend on where the
                // frame's slots sit are not pinned either.
                let world1 = World {
                    stack_fill: 0xa5,
                    slot_pad: 0x25,
                    ..world.clone()
                };
                let (b1, _) = run(before, &world1, &nowrite_names, &self_name, cfg.fuel, true, xb, None);
                if underdetermined(&a, &atrace.calls, &b, &b1, sret) {
                    sum.inconclusive += 1;
                    sum.why.get_or_insert_with(|| match &b1 {
                        Outcome::Returned { .. } => {
                            "before's output depends on uninit stack bytes or slot addresses".into()
                        }
                        o => format!(
                            "before depends on uninit stack bytes or slot addresses (under the other stack fill: {})",
                            describe(o)
                        ),
                    });
                } else {
                    return Err(Mismatch {
                        trial,
                        args: world.args.clone(),
                        before: b,
                        after: a,
                        rerun: Some(b1),
                        sret,
                    });
                }
            }
        }
    }
    Ok(sum)
}

impl Default for ArgRec {
    fn default() -> Self {
        Self { frame: false, fill: 0, seen: 0, mask: 0, bytes: [0; 64] }
    }
}

/// What a run did besides its [`Outcome`].
#[derive(Default)]
struct Trace {
    /// What its stubs did, one entry per call.
    log: Vec<CallRec>,
    /// The calls made, when the run did not return (a returned run has
    /// them in its `Outcome`).
    calls: Vec<(String, Vec<DataValue>)>,
}

/// Did the sandbox, not the program, stop this run? An operation it
/// does not model (a copy whose length or address is a seeded word that
/// points nowhere) or the fuel: the calls made up to there are what the
/// program does. A trap, a panic of the interpreter (a `notrap` access
/// through a wild pointer) or a broken fact is undefined behavior or may
/// be, and says nothing about what came before.
fn sandbox_stop(o: &Outcome) -> bool {
    matches!(o, Outcome::Inconclusive(r)
        if r.starts_with("out of fuel") || r.ends_with("out of sandbox"))
}

/// Cross-build pairs: the calls are observable in order. When the
/// original stops for a reason of the sandbox's own ([`sandbox_stop`])
/// after `n` calls, the rewritten function must still make those `n`
/// calls with the same arguments, wherever it stops. Without this a
/// trial is compared only when the original returns, and an IR pass whose
/// wrong value first shows in a later loop iteration is seen only by a
/// trial whose random words carry it through every iteration
/// (`tests/mcrdwrap`: a `String` the second iteration starts from a copy
/// of another; no trial in 1,024 got there and returned). `rerun` runs
/// the original under the other fill, so arguments it does not determine
/// are not compared; `after` runs the rewritten function.
fn prefix_mismatch(
    b: &Outcome,
    bt: &Trace,
    trial: u32,
    world: &World,
    rerun: &dyn Fn(&World) -> (Outcome, Trace),
    after: &dyn Fn() -> (Outcome, Trace),
) -> Option<Mismatch> {
    if !sandbox_stop(b) || bt.calls.is_empty() {
        return None;
    }
    let (a, at) = after();
    let ac: &[(String, Vec<DataValue>)] = match &a {
        Outcome::Returned { calls, .. } => calls,
        o if sandbox_stop(o) => &at.calls,
        // A trap or a panic: the rewritten function is wrong, or it did
        // something the original left for later. Not this check's call.
        _ => return None,
    };
    let n = bt.calls.len().min(ac.len());
    let differ = |x: &(String, Vec<DataValue>), y: &(String, Vec<DataValue>)| {
        x.0 != y.0 || x.1.len() != y.1.len() || x.1.iter().map(norm).ne(y.1.iter().map(norm))
    };
    // A returned rewritten function that made fewer calls dropped some.
    let short = matches!(a, Outcome::Returned { .. }) && ac.len() < bt.calls.len();
    let first = (0..n).find(|&k| differ(&bt.calls[k], &ac[k]));
    if first.is_none() && !short {
        return None;
    }
    // Pinned down by the original under the other fill: the same calls
    // up to the difference, each argument byte the same in both runs.
    let (b1, b1t) = rerun(world);
    let c1: &[(String, Vec<DataValue>)] = match &b1 {
        Outcome::Returned { calls, .. } => calls,
        o if sandbox_stop(o) => &b1t.calls,
        _ => return None,
    };
    let upto = first.map_or(bt.calls.len(), |k| k + 1);
    if c1.len() < upto {
        return None;
    }
    for k in 0..upto {
        let (x0, x1) = (&bt.calls[k], &c1[k]);
        if x0.0 != x1.0 || x0.1.len() != x1.1.len() {
            return None;
        }
        if k + 1 < upto && x0.1.iter().map(norm).ne(x1.1.iter().map(norm)) {
            // An earlier call was handed an undetermined value: what the
            // stubs did from there on is a hash of it.
            return None;
        }
    }
    if let Some(k) = first {
        let (x0, x1, y) = (&bt.calls[k], &c1[k], &ac[k]);
        if x0.0 == y.0 && x0.1.len() == y.1.len() {
            // Only bytes both original runs agree on count.
            let det = x0.1.iter().zip(&x1.1).zip(&y.1).any(|((p, q), r)| {
                let (_, np) = norm(p);
                let (_, nq) = norm(q);
                let (_, nr) = norm(r);
                (0..p.ty().bytes() as usize).any(|j| np[j] == nq[j] && nr[j] != np[j])
            });
            if !det {
                return None;
            }
        }
    }
    // The report names the first call that differs and its differing
    // arguments (a stub's pointer arguments are followed by what it read
    // through them: a byte mask, then the bytes, a word at a time).
    let stop = |o: &Outcome| match o {
        Outcome::Inconclusive(r) => format!("stopped by the sandbox ({r})"),
        Outcome::Returned { .. } => "returned".to_string(),
        o => describe(o),
    };
    let (bs, as_) = match first {
        Some(k) => {
            let (x, y) = (&bt.calls[k], &ac[k]);
            let idx: Vec<usize> = (0..x.1.len().max(y.1.len()))
                .filter(|&j| x.1.get(j).map(norm) != y.1.get(j).map(norm))
                .collect();
            let pick = |c: &(String, Vec<DataValue>)| -> String {
                let vs: Vec<String> =
                    idx.iter().take(8).map(|&j| format!("{j}: {:?}", c.1.get(j))).collect();
                format!("call {k} `{}`, arguments {}", c.0, vs.join(", "))
            };
            (pick(x), pick(y))
        }
        None => (String::new(), String::new()),
    };
    let before = Outcome::Inconclusive(format!("{} after {} calls; {bs}", stop(b), bt.calls.len()));
    let after_o = Outcome::Inconclusive(format!("{} after {} calls; {as_}", stop(&a), ac.len()));
    Some(Mismatch { trial, args: world.args.clone(), before, after: after_o, rerun: None, sret: None })
}

/// By type and bytes: `DataValue`'s float `==` says NaN != NaN.
fn norm(v: &DataValue) -> (Type, [u8; 16]) {
    let mut b = [0u8; 16];
    match v {
        DataValue::F32(f) if f.is_nan() => b[..4].copy_from_slice(&0x7fc0_0000u32.to_le_bytes()),
        DataValue::F64(f) if f.is_nan() => {
            b[..8].copy_from_slice(&0x7ff8_0000_0000_0000u64.to_le_bytes())
        }
        v => v.write_to_slice_le(&mut b[..v.ty().bytes() as usize]),
    }
    (v.ty(), b)
}

/// Equality that treats any two NaNs as equal: passes may change payloads.
fn same(a: &Outcome, b: &Outcome) -> bool {
    let nv = |vs: &[DataValue]| vs.iter().map(norm).collect::<Vec<_>>();
    match (a, b) {
        (
            Outcome::Returned {
                rets: r1,
                calls: c1,
                writes: w1,
            },
            Outcome::Returned {
                rets: r2,
                calls: c2,
                writes: w2,
            },
        ) => {
            nv(r1) == nv(r2)
                && w1 == w2
                && c1.len() == c2.len()
                && c1
                    .iter()
                    .zip(c2)
                    .all(|(x, y)| x.0 == y.0 && nv(&x.1) == nv(&y.1))
        }
        _ => a == b,
    }
}

/// `a != b0` may still be a legal rewrite: passes can reshuffle bytes the
/// original never defined — slotmerge folds a slot onto live storage, so
/// reads of its never-written bytes yield real data instead of the
/// zero-fill convention. `b1` is `b0` re-run under a different stack
/// fill: every observable byte where `b0` and `b1` disagree was never
/// determined by the source program, and `a` may show anything there.
/// True when `a` differs from `b0` only on such bytes (or the original's
/// whole behavior is fill-dependent, which leaves nothing to check).
// r[impl clif.diffcheck.sound]
fn underdetermined(
    a: &Outcome,
    a_calls: &[(String, Vec<DataValue>)],
    b0: &Outcome,
    b1: &Outcome,
    sret: Option<(u64, u64)>,
) -> bool {
    let (
        Outcome::Returned {
            rets: r0,
            calls: c0,
            writes: w0,
        },
        Outcome::Returned {
            rets: r1,
            calls: c1,
            writes: w1,
        },
    ) = (b0, b1)
    else {
        // The other fill does not return where the first did: the
        // original's control flow, or the address of one of its accesses,
        // depends on bytes of the frame it never wrote. Such a byte has
        // no value the program determines (LLVM: a load of uninitialized
        // `alloca` memory is `undef`; Rust: reading it is undefined
        // behavior), so the run under the zero fill is one of many the
        // original allows and nothing in it is pinned.
        //
        // That holds for a rerun that is inconclusive as much as for one
        // that traps: the sandbox is deterministic and the two runs of
        // the original differ in the fill and the slots' place alone, so
        // whatever stopped the second (a `notrap` load through a pointer
        // made of fill bytes, which the interpreter reports as a panic;
        // fuel; an operation only the other path has) is a consequence of
        // those bytes. Until 2026-11-16 only a trap counted, and an
        // inconclusive rerun kept the report: two functions whose
        // original copies an enum's never-written payload to the caller's
        // memory (`FlattenCompat::next`: `self.frontiter = None`, a tag
        // store and a 364-byte copy; aarch64 `emit`: a 48-byte one) and
        // elsewhere tests a byte a stub left unfilled were reported for
        // the stale bytes slotmerge put under that payload.
        return !matches!(b1, Outcome::Returned { .. });
    };
    // The original's own shape moved with the fill: nothing to compare.
    if r0.len() != r1.len() || c0.len() != c1.len() {
        return true;
    }
    // The rewritten function did not return. When the original hands a
    // callee a value it never determined, a stub's results and writes from
    // there on are a hash of it, and so is whether a run traps later: if
    // the rewritten function made the same calls up to that one, its trap
    // pins nothing down. (`fuse_filaments_with`'s closure in dsa: an
    // `i64` copy of a struct field and its 4 bytes of padding goes to a
    // callee; slotmerge put another slot's bytes under the padding, the
    // stub answered differently and the rewritten run took a panic path.)
    // `a_calls`: the calls of a rewritten run that did not return, when
    // the caller has them.
    if !matches!(a, Outcome::Returned { .. }) {
        let same = |x: &(String, Vec<DataValue>), y: &(String, Vec<DataValue>)| {
            x.0 == y.0 && x.1.len() == y.1.len() && x.1.iter().map(norm).eq(y.1.iter().map(norm))
        };
        let Some(k) = c0.iter().zip(c1).position(|(x0, x1)| !same(x0, x1)) else {
            return false;
        };
        return a_calls.len() > k
            && a_calls[k].0 == c0[k].0
            && a_calls[..k].iter().zip(c0).all(|(y, x)| same(y, x));
    }
    let Outcome::Returned {
        rets: ra,
        calls: ca,
        writes: wa,
    } = a
    else {
        return false;
    };
    if ra.len() != r0.len() {
        return false;
    }
    // `v0`'s byte k is determined iff the two original runs agree on it;
    // `a` must reproduce every determined byte.
    let lanes_ok = |xs: &[DataValue], ys: &[DataValue], zs: &[DataValue]| {
        xs.iter()
            .zip(ys)
            .zip(zs)
            .all(|((xa, x0), x1)| {
                let (ta, na) = norm(xa);
                let (t0, n0) = norm(x0);
                let (t1, n1) = norm(x1);
                ta == t0
                    && t0 == t1
                    && (0..t0.bytes() as usize)
                        .all(|k| n0[k] != n1[k] || na[k] == n0[k])
            })
    };
    for ((x0, x1), ya) in c0.iter().zip(c1).zip(ca) {
        if x0.0 != x1.0 {
            return true; // the original's call sequence is fill-dependent
        }
        if x0.1.len() != x1.1.len() {
            return true; // and so is what this call was handed
        }
        if ya.0 != x0.0 || ya.1.len() != x0.1.len() || x0.1.len() != x1.1.len() {
            return false;
        }
        if !lanes_ok(&ya.1, &x0.1, &x1.1) {
            return false;
        }
        // The original handed this callee a value it never determined.
        // A stub's results and writes are a hash of its arguments, so
        // from here on the two original runs agree only by chance (a
        // one-bit `bool` result half the time): nothing later is pinned.
        if x0.1.iter().map(norm).ne(x1.1.iter().map(norm)) {
            return true;
        }
    }
    if ca.len() != c0.len() {
        return false;
    }
    if !lanes_ok(ra, r0, r1) {
        return false;
    }
    // Written bytes, by address. A write list holds the bytes that ended
    // up different from the seeded memory, so an absent address reads as
    // "the seed's byte" and is compared like any other value: an
    // undetermined byte may come out equal to the seed in one run and not
    // in another (slotmerge moved a padding byte of the returned struct
    // onto a slot whose byte matched the seed; the address sets differed
    // and that alone was reported). A byte is determined iff the two
    // original runs agree on it, absent or not.
    let m = |w: &Vec<(u64, u8)>| w.iter().copied().collect::<BTreeMap<u64, u8>>();
    let (m0, m1, ma) = (m(w0), m(w1), m(wa));
    m0.keys()
        .chain(m1.keys())
        .chain(ma.keys())
        .all(|k| unwritten_sret(sret, &m0, *k) || m0.get(k) != m1.get(k) || ma.get(k) == m0.get(k))
}

/// The sandbox addresses of `f`'s return area: the memory behind its
/// `sret` parameter.
fn sret_area(f: &Function) -> Option<(u64, u64)> {
    let k = f
        .signature
        .params
        .iter()
        .position(|p| p.purpose == ir::ArgumentPurpose::StructReturn)?;
    let base = param_base() + k as u64 * REGION_STRIDE;
    Some((base, base + REGION_STRIDE))
}

/// Is `k` a byte of the return area that the original left as it found
/// it? The return area holds nothing when the function is entered and
/// its caller reads the returned value only, so such a byte is not part
/// of what the function returns on this path (`None`'s payload) and the
/// other function may leave anything there. An IR pass does: memcpyopt
/// sends a callee's result straight to the return area where the
/// original called into a temporary and copied it on one arm only
/// (`parse_number`: `let (n, len) = parse_partial_number(s)?; if len ==
/// s.len() { Some(n) } else { None }`). `m0` holds the bytes the original
/// changed.
fn unwritten_sret(sret: Option<(u64, u64)>, m0: &BTreeMap<u64, u8>, k: u64) -> bool {
    sret.is_some_and(|(lo, hi)| (lo..hi).contains(&k)) && !m0.contains_key(&k)
}

fn mix(a: u64, b: u64) -> u64 {
    let mut z = a ^ b.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn hash_str(s: &str) -> u64 {
    s.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| mix(h, b as u64))
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix(self.0, 0)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// Sandbox address map. All addresses sit in the interpreter's `Stack`
// region (top two bits clear), which it passes through as plain offsets.
// With 64-bit pointers they must exceed `u32::MAX`: the interpreter
// decodes a smaller computed address as a 32-bit one, whose top bits
// name a different region.
//
// With 32-bit pointers (wasm32: `set_ptr32`) the same four areas sit
// below `1 << 30`, where a 32-bit address still decodes as a `Stack`
// offset, and pointer values are `I32`. Before this, every wasm32
// function that touched memory came back inconclusive
// (`heap_oob at stack_addr.i32`).
static PTR32: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Check functions whose pointers are `i32` (the wasm backend, and
/// `clif-check` under `PLIRON_CLIF_WASM=1`). Process-wide: one process
/// compiles for one target.
pub fn set_ptr32(on: bool) {
    PTR32.store(on, std::sync::atomic::Ordering::Relaxed);
}
fn p32() -> bool {
    PTR32.load(std::sync::atomic::Ordering::Relaxed)
}
fn param_base() -> u64 {
    if p32() { 0x0100_0000 } else { 0x10_0000_0000 }
}
fn heap_base() -> u64 {
    if p32() { 0x0200_0000 } else { 0x20_0000_0000 }
}
fn sym_base() -> u64 {
    if p32() { 0x0300_0000 } else { 0x30_0000_0000 }
}
fn stack_base() -> u64 {
    if p32() { 0x0400_0000 } else { 0x40_0000_0000 }
}
/// Bytes above `stack_base` that are frames.
fn stack_span() -> u64 {
    if p32() { 0x3000_0000 } else { 0x1000_0000 }
}
/// Regions an area holds before it runs into the next one (32-bit).
const AREA_REGIONS: u64 = 256;
fn ptr_ty() -> Type {
    if p32() { types::I32 } else { types::I64 }
}
/// The sandbox address `a` as a pointer value.
fn ptr(a: u64) -> DataValue {
    if p32() { DataValue::I32(a as u32 as i32) } else { DataValue::I64(a as i64) }
}
/// A value that can be a pointer, as an address.
fn as_ptr(v: &DataValue) -> Option<u64> {
    match v {
        DataValue::I64(p) if !p32() => Some(*p as u64),
        DataValue::I32(p) if p32() => Some(*p as u32 as u64),
        _ => None,
    }
}
const REGION_STRIDE: u64 = 0x1_0000;
/// Pointers point this far into their region, so small negative offsets work.
const PTR_SLACK: u64 = 64;
const HEAPS: u64 = 4;
const HEAP_SIZE: u64 = 512;

#[derive(Clone)]
struct Region {
    data: Vec<u8>,
    /// Seeded contents, to report writes as differences.
    init: Vec<u8>,
    /// Calls may write here (not frozen / read-only).
    stub_writable: bool,
    /// Frames of a cross-build pair: which bytes were written since the
    /// frame was made (1), so a stub can tell what its callee may read
    /// from what the caller never set. Empty: not tracked, every byte
    /// counts as written (memory that exists before the call).
    set: Vec<u8>,
}

/// The seeded sandbox one trial runs in; cloned per run.
#[derive(Clone)]
struct World {
    regions: BTreeMap<u64, Region>,
    args: Vec<DataValue>,
    seed: u64,
    /// Byte fresh stack frames are filled with. Normally 0; `check`
    /// re-runs `before` under a different fill on a mismatch to find
    /// output bytes the original never determined.
    stack_fill: u8,
    /// Unused pages ahead of the first stack slot in every frame. Normally
    /// 0; the re-run of `before` moves the slots with it.
    slot_pad: u64,
    /// [`slot_layout`] of the pair under check: every run of the trial
    /// puts slot `k` at the same place.
    slot_at: Vec<u64>,
    /// Bytes the slots of `slot_at` take.
    slots_len: u64,
    /// Values the optimizer may assume nonzero (`!nonnull` loads too): a
    /// load that yields 0 into one is undefined behavior, so the run stops.
    nonnull: FxHashSet<Value>,
    /// `Facts::derived`: inbounds offset → base. An offset that comes out
    /// 0 from a non-zero base is undefined behavior, so the run stops.
    derived: FxHashMap<Value, Value>,
    /// `Facts::memcmp` callees as call names spell them.
    memcmp: FxHashSet<String>,
    /// `Facts::undef`.
    undef: FxHashSet<Value>,
    /// `Facts::lifetime_starts`, by anchor: after an instruction, at a
    /// block's head.
    lt_inst: FxHashMap<Inst, Vec<StackSlot>>,
    lt_block: FxHashMap<Block, Vec<StackSlot>>,
    /// `Facts::stub_extent`.
    extent: FxHashMap<(String, u32), u64>,
    /// Data symbols placed ahead of the runs ([`World::add_symbols`]).
    syms: FxHashMap<String, u64>,
}

/// A displayed external name as the checker keys it: the linker symbol
/// in a cross-build pair, the text's own spelling otherwise.
fn key_of(d: String, x: Option<&Syms>) -> String {
    match x.and_then(|x| x.names.get(&d)) {
        Some(s) => s.clone(),
        None => d,
    }
}

fn ext_key(f: &Function, name: &ExternalName, x: Option<&Syms>) -> String {
    key_of(name.display(Some(&f.params)).to_string(), x)
}

/// The data symbols of a cross-build pair, sorted, and their images:
/// those either function names, and those their images point at. Both
/// functions run with every one of them at the same address, whichever of
/// the two names it (a pass folds the last load of a constant away, and
/// the symbol leaves that function). `Err`: a symbol the two builds
/// recorded different bytes for.
#[allow(clippy::type_complexity)]
fn pair_symbols<'c>(
    before: &Function,
    after: &Function,
    c: &'c Cross,
) -> Result<(Vec<String>, FxHashMap<String, &'c DataImage>), String> {
    let mut names: std::collections::BTreeSet<String> = Default::default();
    let mut images: FxHashMap<String, &DataImage> = FxHashMap::default();
    for (f, x) in [(before, c.before), (after, c.after)] {
        for gv in f.global_values.keys() {
            if let Some(n) = gv_name(f, gv, Some(x)) {
                names.insert(n);
            }
        }
        for (sym, img) in &x.data {
            match images.get(sym) {
                Some(old) if *old != img => return Err(sym.clone()),
                _ => {}
            }
            images.insert(sym.clone(), img);
        }
    }
    // Only what a function names, or what such an image points at, gets
    // a region: a header may describe more.
    let mut targets = Vec::new();
    for n in &names {
        if let Some(img) = images.get(n) {
            targets.extend(img.relocs.iter().filter(|r| !r.func).map(|r| r.sym.clone()));
        }
    }
    names.extend(targets);
    Ok((names.into_iter().collect(), images))
}

/// The value `func_addr` yields for the function keyed `key`.
fn func_addr_value(key: &str) -> Option<DataValue> {
    let size = if p32() { AddressSize::_32 } else { AddressSize::_64 };
    let a = Address::from_parts(
        size,
        AddressRegion::Function,
        AddressFunctionEntry::UserFunction as u64,
        hash_str(key) & 0xff_ffff,
    )
    .ok()?;
    DataValue::try_from(a).ok()
}

/// Give data symbol `name` a region and return the symbol's address. The
/// region is seeded from the name, so it is the same in every run and in
/// both functions of a cross-build pair; `image`'s bytes go over the
/// seed (its relocations are applied by the caller, once every symbol
/// has an address).
fn place_symbol(
    regions: &mut BTreeMap<u64, Region>,
    syms: &mut FxHashMap<String, u64>,
    seed: u64,
    name: &str,
    image: Option<&DataImage>,
) -> u64 {
    if let Some(&a) = syms.get(name) {
        return a;
    }
    if p32() && syms.len() as u64 >= AREA_REGIONS {
        // The 32-bit symbol area is full: an address with nothing
        // mapped, so a read through it ends the trial inconclusive
        // instead of landing in the stack area.
        let a = 0x3f00_0000 + syms.len() as u64 * 16;
        syms.insert(name.to_string(), a);
        return a;
    }
    let base = sym_base() + syms.len() as u64 * REGION_STRIDE;
    let mut rng = Rng(mix(seed, hash_str(name)));
    let targets: Vec<u64> = (0..HEAPS)
        .map(|h| heap_base() + h * REGION_STRIDE + PTR_SLACK)
        .collect();
    let len = image
        .map_or(0, |i| i.bytes.len() + 2 * PTR_SLACK as usize)
        .clamp(1024, (REGION_STRIDE - 16) as usize)
        .next_multiple_of(8);
    let mut data = vec![0u8; len];
    fill(&mut data, &targets, &mut rng);
    if let Some(img) = image {
        let o = PTR_SLACK as usize;
        let n = img.bytes.len().min(len - o);
        data[o..o + n].copy_from_slice(&img.bytes[..n]);
    }
    regions.insert(
        base,
        Region {
            init: data.clone(),
            data,
            stub_writable: false,
            set: Vec::new(),
        },
    );
    syms.insert(name.to_string(), base + PTR_SLACK);
    base + PTR_SLACK
}

impl World {
    /// Place `names` (sorted) with their `images`, and write the
    /// addresses the images' relocations name.
    fn add_symbols(&mut self, names: &[String], images: &FxHashMap<String, &DataImage>) {
        for n in names {
            place_symbol(&mut self.regions, &mut self.syms, self.seed, n, images.get(n).copied());
        }
        let w = if p32() { 4usize } else { 8 };
        for n in names {
            let (Some(img), Some(&at)) = (images.get(n), self.syms.get(n)) else { continue };
            for r in &img.relocs {
                let v = if r.func {
                    // A function's address plus an offset is not a
                    // callable value; leave the seeded bytes.
                    if r.addend != 0 {
                        continue;
                    }
                    match func_addr_value(&r.sym).as_ref().and_then(as_ptr) {
                        Some(v) => v,
                        None => continue,
                    }
                } else {
                    match self.syms.get(&r.sym) {
                        Some(&a) => a.wrapping_add(r.addend as u64),
                        None => continue,
                    }
                };
                let a = at + u64::from(r.off);
                let Some((&base, reg)) = self.regions.range_mut(..=a).next_back() else { continue };
                let o = (a - base) as usize;
                if o + w <= reg.data.len() {
                    reg.data[o..o + w].copy_from_slice(&v.to_le_bytes()[..w]);
                    reg.init[o..o + w].copy_from_slice(&v.to_le_bytes()[..w]);
                }
            }
        }
    }

    fn new(
        f: &Function,
        ptrs: &FxHashMap<Value, PtrInfo>,
        facts: &Facts,
        seed: u64,
        layout: &(Vec<u64>, u64),
        x: Option<&Syms>,
    ) -> Self {
        let mut rng = Rng(seed);
        let mut regions = BTreeMap::new();
        let mut targets: Vec<u64> = Vec::new();
        for h in 0..HEAPS {
            targets.push(heap_base() + h * REGION_STRIDE + PTR_SLACK);
        }
        let entry = f.layout.entry_block().expect("function has an entry block");
        let params = f.dfg.block_params(entry);
        for (i, p) in params.iter().enumerate() {
            if let Some(pi) = ptrs.get(p) {
                let size = pi.size.max(256).min(REGION_STRIDE - 2 * PTR_SLACK);
                let base = param_base() + i as u64 * REGION_STRIDE;
                regions.insert(
                    base,
                    Region {
                        data: vec![0; (size + 2 * PTR_SLACK) as usize],
                        init: Vec::new(),
                        stub_writable: !pi.frozen,
                        set: Vec::new(),
                    },
                );
                // A noalias param's memory is reachable only through the
                // param, so no seeded word may point into it.
                if !facts.noalias.is_some_and(|s| s.contains(p)) {
                    targets.push(base + PTR_SLACK);
                }
            }
        }
        for h in 0..HEAPS {
            regions.insert(
                heap_base() + h * REGION_STRIDE,
                Region {
                    data: vec![0; (HEAP_SIZE + 2 * PTR_SLACK) as usize],
                    init: Vec::new(),
                    stub_writable: true,
                    set: Vec::new(),
                },
            );
        }
        for r in regions.values_mut() {
            fill(&mut r.data, &targets, &mut rng);
            r.init = r.data.clone();
        }
        let mut args = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let ty = f.dfg.value_type(*p);
            let v = if let Some(pi) = ptrs.get(p) {
                let _ = pi;
                ptr(param_base() + i as u64 * REGION_STRIDE + PTR_SLACK)
            } else {
                let nonnull = facts.nonnull.is_some_and(|s| s.contains(p));
                let v = random_value(ty, &mut rng, nonnull);
                // Cross-build pairs: a byte parameter is 0 or 1, a value
                // every `bool` and `u8` may hold. A `bool` of -1 is not one
                // two functions have to treat alike (`vec![b; n]`: sroa off
                // tested `b == 0` for `alloc_zeroed`, on it tested `b & 1`).
                match v {
                    DataValue::I8(b) if x.is_some() => DataValue::I8(b & 1),
                    v => v,
                }
            };
            args.push(v);
        }
        Self {
            regions,
            args,
            seed,
            // `PLIRON_DIFFCHECK_FILL=<byte>`: fill fresh frames with it
            // from the start — a step that introduces an uninitialized
            // read is invisible under the zero fill when the right value
            // happens to be 0.
            stack_fill: crate::config::env_get("PLIRON_DIFFCHECK_FILL")
                .and_then(|v| u8::from_str_radix(v.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0),
            slot_pad: 0,
            slot_at: layout.0.clone(),
            slots_len: layout.1,
            nonnull: facts.nonnull.cloned().unwrap_or_default(),
            derived: facts
                .derived
                .map(|d| d.iter().map(|(v, &b)| (v, b)).collect())
                .unwrap_or_default(),
            memcmp: facts
                .memcmp
                .iter()
                .filter_map(|fr| f.dfg.ext_funcs.get(*fr))
                .map(|d| ext_key(f, &d.name, x))
                .collect(),
            undef: facts.undef.cloned().unwrap_or_default(),
            lt_inst: facts
                .lifetime_starts
                .into_iter()
                .flatten()
                .filter(|m| m.3)
                .filter_map(|(ss, a, _, _)| a.map(|a| (a, ss.clone())))
                .fold(FxHashMap::default(), |mut m: FxHashMap<Inst, Vec<StackSlot>>, (a, ss)| {
                    m.entry(a).or_default().extend(ss);
                    m
                }),
            lt_block: facts
                .lifetime_starts
                .into_iter()
                .flatten()
                .filter(|m| m.3 && m.1.is_none())
                .fold(FxHashMap::default(), |mut m: FxHashMap<Block, Vec<StackSlot>>, (ss, _, b, _)| {
                    m.entry(*b).or_default().extend(ss.iter().copied());
                    m
                }),
            syms: FxHashMap::default(),
            extent: {
                // A callee handed the address of a slot reaches that slot
                // and no further, so the slot's size bounds its operand
                // like a `nocap` extent does. The stub fills up to the end
                // of the slot the pointer lands in: without this bound,
                // slotmerge folding a 12-byte out-slot into the head of a
                // 128-byte one made the stub write 64 bytes over the
                // larger slot's live fields in the rewritten function only
                // (jiff's `Database::from_dir` and two more in the dsa
                // corpus). `f` is the original: both runs share its table.
                let slots = stub_extents(f, |i, k| slot_extent(f, f.dfg.inst_args(i)[k as usize]));
                let mut e: FxHashMap<(String, u32), u64> = FxHashMap::default();
                for ((name, k), n) in facts.stub_extent.iter().map(|(k, n)| (k.clone(), *n)).chain(slots) {
                    let bound = e.entry((key_of(name, x), k)).or_insert(n);
                    *bound = (*bound).min(n);
                }
                e
            },
        }
    }
}

/// A stack slot's place in a frame is a multiple of this, and its room.
const SLOT_PAGE: u64 = 0x1000;

/// Room for a slot of `size` bytes: whole pages, at least one.
fn slot_room(size: u32) -> u64 {
    u64::from(size).max(1).next_multiple_of(SLOT_PAGE)
}

/// Where each stack slot sits in a frame, by slot index, and the bytes
/// they take together. Distinct slots are distinct objects: an access
/// inside one never reaches another, so each gets room for its size.
/// Until 2026-11-16 every slot had one 4 KiB page whatever its size, and
/// a write past byte 4095 of a larger slot landed in the next slots
/// (`robust::incircleadapt`: the `memset` of a `[f64; 1152]` zeroed the
/// two arrays declared after it, in the original only once slotmerge had
/// moved one of them).
///
/// The place of slot `k` must not depend on which function of the pair
/// runs: a pass that shrinks a dead slot (slotmerge sets a merged slot's
/// size to 0) would otherwise shift every later slot's bytes, which the
/// function may read back without having written them (a callee fills
/// them through an escaped pointer) — a false mismatch. So the room of
/// slot `k` is that of its largest size in `fs`, the functions of the
/// pair; passes keep slot indices.
fn slot_layout(fs: &[&Function]) -> (Vec<u64>, u64) {
    let n = fs.iter().map(|f| f.sized_stack_slots.len()).max().unwrap_or(0);
    let mut at = Vec::with_capacity(n);
    let mut off = 0u64;
    for k in 0..n {
        let size = fs
            .iter()
            .filter_map(|f| f.sized_stack_slots.get(StackSlot::from_u32(k as u32)))
            .map(|d| d.size)
            .max()
            .unwrap_or(0);
        at.push(off);
        off += slot_room(size);
    }
    (at, off)
}

/// Bytes from `v` to the end of its stack slot, when `v` is a slot's
/// address plus constants.
fn slot_extent(f: &Function, v: Value) -> Option<u64> {
    let mut v = f.dfg.resolve_aliases(v);
    let mut off = 0i64;
    for _ in 0..16 {
        let i = f.dfg.value_def(v).inst()?;
        match f.dfg.insts[i] {
            InstructionData::StackAddr { stack_slot, offset, .. } => {
                let size = i64::from(f.sized_stack_slots[stack_slot].size);
                let at = off.checked_add(i64::from(i32::from(offset)))?;
                return (0..=size).contains(&at).then(|| (size - at) as u64);
            }
            InstructionData::Binary { opcode: Opcode::Iadd, args } => {
                let (a, b) = (f.dfg.resolve_aliases(args[0]), f.dfg.resolve_aliases(args[1]));
                let konst = |x: Value| match f.dfg.value_def(x).inst().map(|d| f.dfg.insts[d]) {
                    Some(InstructionData::UnaryImm { opcode: Opcode::Iconst, imm }) => Some(imm.bits()),
                    _ => None,
                };
                if let Some(c) = konst(b) {
                    off = off.checked_add(c)?;
                    v = a;
                } else if let Some(c) = konst(a) {
                    off = off.checked_add(c)?;
                    v = b;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

fn fill(data: &mut [u8], targets: &[u64], rng: &mut Rng) {
    // A pointer-sized cell at a time, so a loaded pointer is whole.
    for w in data.chunks_mut(if p32() { 4 } else { 8 }) {
        let v: u64 = match rng.below(10) {
            0..=4 => targets[rng.below(targets.len() as u64) as usize] + rng.below(8) * 8,
            // Zero gets its own share: null pointers and empty lengths.
            5 => 0,
            6..=8 => rng.below(64),
            _ => rng.next(),
        };
        let b = v.to_le_bytes();
        w.copy_from_slice(&b[..w.len()]);
    }
}

fn random_value(ty: Type, rng: &mut Rng, nonnull: bool) -> DataValue {
    if ty.is_vector() {
        let mut b = [0u8; 16];
        for x in b.iter_mut() {
            *x = rng.next() as u8;
        }
        return DataValue::read_from_slice_le(&b[..ty.bytes() as usize], ty);
    }
    if ty.is_float() {
        let x: f64 = match rng.below(8) {
            0 => 0.0,
            1 => 1.0,
            2 => -1.5,
            3 => f64::INFINITY,
            _ => (rng.next() as i64 % 1_000_000) as f64 / 64.0,
        };
        return match ty {
            types::F32 => DataValue::F32(ir::immediates::Ieee32::with_float(x as f32)),
            types::F64 => DataValue::F64(ir::immediates::Ieee64::with_float(x)),
            _ => DataValue::read_from_slice_le(&[0; 16][..ty.bytes() as usize], ty),
        };
    }
    let bits = ty.bits().min(128);
    let (min, max) = if bits >= 128 {
        (i128::MIN, i128::MAX)
    } else {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    };
    let n: i128 = match rng.below(10) {
        0 => 0,
        1 => 1,
        2 => -1,
        3 => rng.below(4) as i128 + 2,
        4 | 5 => rng.below(64) as i128,
        6 => max,
        7 => min,
        _ => rng.next() as i64 as i128,
    };
    let n = if nonnull && n == 0 { 1 } else { n };
    let mut b = n.to_le_bytes();
    // Truncate to the type: the low bytes, read back at that width.
    b[ty.bytes() as usize..].fill(0);
    DataValue::read_from_slice_le(&b[..ty.bytes() as usize], ty)
}

#[derive(Default, Clone, Copy)]
struct PtrInfo {
    size: u64,
    frozen: bool,
}

/// Entry params treated as pointers: those the facts describe, plus any
/// that (through adds and block params) reach a memory access address.
// r[impl clif.diffcheck.limits]
fn pointer_params(f: &Function, facts: &Facts) -> FxHashMap<Value, PtrInfo> {
    let mut out: FxHashMap<Value, PtrInfo> = FxHashMap::default();
    let Some(entry) = f.layout.entry_block() else {
        return out;
    };
    let params = f.dfg.block_params(entry).to_vec();
    for &p in &params {
        if f.dfg.value_type(p) != ptr_ty() {
            continue;
        }
        let mut pi = PtrInfo::default();
        let mut known = false;
        if let Some((n, w)) = facts.deref.and_then(|d| d.get(&p)) {
            pi.size = *n;
            pi.frozen |= !w;
            known = true;
        }
        if let Some(n) = facts.frozen.and_then(|d| d.get(&p)) {
            pi.size = pi.size.max(*n);
            pi.frozen = true;
            known = true;
        }
        // `nonnull` marks pointer params only; one that is just compared
        // and offset (a slice iterator's start) still must not wrap to 0.
        known |= facts.noalias.is_some_and(|s| s.contains(&p))
            || facts.nonnull.is_some_and(|s| s.contains(&p));
        if known || reaches_address(f, p) {
            out.insert(p, pi);
        }
    }
    out
}

fn reaches_address(f: &Function, root: Value) -> bool {
    let mut seen: FxHashSet<Value> = FxHashSet::default();
    let mut work = vec![root];
    seen.insert(root);
    // Uses of each value, built lazily once.
    let mut users: FxHashMap<Value, Vec<ir::Inst>> = FxHashMap::default();
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            for &a in f.dfg.inst_args(i) {
                users.entry(a).or_default().push(i);
            }
            for dest in
                f.dfg.insts[i].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
            {
                for a in dest.args(&f.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        users.entry(v).or_default().push(i);
                    }
                }
            }
        }
    }
    while let Some(v) = work.pop() {
        for &i in users.get(&v).map(Vec::as_slice).unwrap_or(&[]) {
            if address_operand(&f.dfg.insts[i]) == Some(v) {
                return true;
            }
            let mut push = |x: Value, work: &mut Vec<Value>| {
                if seen.insert(x) {
                    work.push(x);
                }
            };
            match f.dfg.insts[i].opcode() {
                Opcode::Iadd | Opcode::Isub => {
                    if let Some(&r) = f.dfg.inst_results(i).first() {
                        push(r, &mut work);
                    }
                }
                _ => {}
            }
            for dest in
                f.dfg.insts[i].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
            {
                let blk = dest.block(&f.dfg.value_lists);
                for (k, a) in dest.args(&f.dfg.value_lists).enumerate() {
                    if a == BlockArg::Value(v) {
                        push(f.dfg.block_params(blk)[k], &mut work);
                    }
                }
            }
        }
    }
    false
}

fn address_operand(d: &InstructionData) -> Option<Value> {
    match *d {
        InstructionData::Load { arg, .. } | InstructionData::LoadNoOffset { arg, .. } => Some(arg),
        InstructionData::Store { args, .. } | InstructionData::StoreNoOffset { args, .. } => {
            Some(args[1])
        }
        InstructionData::AtomicRmw { args, .. } => Some(args[0]),
        InstructionData::AtomicCas { args, .. } => Some(args[0]),
        _ => None,
    }
}

fn nowrite_by_name(f: &Function, facts: &Facts, x: Option<&Syms>) -> FxHashSet<String> {
    let mut out = FxHashSet::default();
    if let Some(m) = facts.nowrite {
        for (fr, _) in m {
            if let Some(d) = f.dfg.ext_funcs.get(*fr) {
                out.insert(ext_key(f, &d.name, x));
            }
        }
    }
    out
}

// --- the interpreter state -------------------------------------------------

struct Sandbox<'a> {
    func: &'a Function,
    /// Call frames of `func` (self-recursion runs for real) and where each
    /// frame's stack slots start.
    frames: Vec<(Frame<'a>, u64)>,
    frame_size: u64,
    /// `func`'s own name as callers spell it.
    self_name: String,
    regions: BTreeMap<u64, Region>,
    /// Byte offset of each stack slot within the frame, by slot index,
    /// ascending (`slot_layout`, after the `slot_pad` pages).
    slot_at: Vec<u64>,
    syms: FxHashMap<String, u64>,
    calls: Vec<(String, Vec<DataValue>)>,
    nowrite: &'a FxHashSet<String>,
    nonnull: &'a FxHashSet<Value>,
    /// Empty unless this run checks the facts (the original).
    derived: &'a FxHashMap<Value, Value>,
    memcmp: &'a FxHashSet<String>,
    extent: &'a FxHashMap<(String, u32), u64>,
    /// `Facts::undef`, when this run is the original under the other
    /// stack fill; empty otherwise.
    undef: &'a FxHashSet<Value>,
    /// `World::lt_inst`/`lt_block`, in runs of the original; empty otherwise.
    lt_inst: &'a FxHashMap<Inst, Vec<StackSlot>>,
    lt_block: &'a FxHashMap<Block, Vec<StackSlot>>,
    seed: u64,
    stack_fill: u8,
    pinned: DataValue,
    /// Last address a load or store fell outside the sandbox at.
    fault: Cell<Option<u64>>,
    /// `PLIRON_DIFFCHECK_WATCH=<hex address>`: every write that covers
    /// this byte is logged (`PLIRON_TRACE=pliron::diffcheck`) with the
    /// instruction that made it and, for a copy, the byte it came from —
    /// follow a differing byte of a report back to where the two runs
    /// part. The second field says which run this is.
    watch: Option<(u64, &'static str)>,
    /// `PLIRON_DIFFCHECK_PATH=1`: log every block entered, to diff the
    /// two runs' paths (a pass that keeps block numbers: slotmerge).
    path: Option<&'static str>,
    /// Cross-build pair: how this function spells symbols.
    xsyms: Option<&'a Syms>,
    /// Cross-build pair, second function: what the first one's stubs did.
    replay: Option<&'a [CallRec]>,
    /// What this run's stubs did, one entry per `calls` entry.
    log: Vec<CallRec>,
}

impl<'a> Sandbox<'a> {
    fn find(&self, addr: u64, len: u64) -> Option<(u64, &Region)> {
        let (&base, r) = self.regions.range(..=addr).next_back()?;
        let end = addr.checked_add(len)?;
        (end <= base + r.data.len() as u64).then_some((base, r))
    }

    fn bytes(&self, addr: u64, len: u64) -> Option<&[u8]> {
        let (base, r) = self.find(addr, len)?;
        let o = (addr - base) as usize;
        Some(&r.data[o..o + len as usize])
    }

    /// A `lifetime.start` of `sl` in the current frame: its bytes are
    /// undefined again, the frame's fill and unwritten.
    fn restart_slot(&mut self, sl: StackSlot) {
        let Some(&(_, base)) = self.frames.last() else { return };
        let (Some(&at), Some(d)) = (self.slot_at.get(sl.index()), self.func.sized_stack_slots.get(sl)) else {
            return;
        };
        let (addr, n) = (base + at, u64::from(d.size));
        let fill = self.stack_fill;
        if let Some(b) = self.bytes_mut(addr, n) {
            b.fill(fill);
        }
        self.set_written(addr, &vec![0; n as usize]);
    }

    fn bytes_mut(&mut self, addr: u64, len: u64) -> Option<&mut [u8]> {
        let base = self.find(addr, len)?.0;
        let r = self.regions.get_mut(&base)?;
        let o = (addr - base) as usize;
        if !r.set.is_empty() {
            r.set[o..o + len as usize].fill(1);
        }
        Some(&mut r.data[o..o + len as usize])
    }

    /// Which of the `len` bytes at `addr` have been written (1), for a
    /// region that tracks it; all of them otherwise.
    fn written(&self, addr: u64, len: u64) -> Option<Vec<u8>> {
        let (base, r) = self.find(addr, len)?;
        let o = (addr - base) as usize;
        Some(if r.set.is_empty() { vec![1; len as usize] } else { r.set[o..o + len as usize].to_vec() })
    }

    /// A copy carries its source's written-ness with it.
    fn set_written(&mut self, addr: u64, set: &[u8]) {
        let Some(base) = self.find(addr, set.len() as u64).map(|f| f.0) else { return };
        let r = self.regions.get_mut(&base).unwrap();
        if !r.set.is_empty() {
            let o = (addr - base) as usize;
            r.set[o..o + set.len()].copy_from_slice(set);
        }
    }

    /// Log a write of `len` bytes at `addr` when it covers the watched
    /// byte; `what(k)` describes it, `k` the watched byte's offset in it.
    fn note_write(&self, addr: u64, len: u64, what: impl FnOnce(u64) -> String) {
        let Some((w, run)) = self.watch else { return };
        if addr <= w && w - addr < len {
            let at = CUR_INST
                .with(Cell::get)
                .filter(|&i| self.func.dfg.inst_is_valid(i))
                .map(|i| format!("{i}: {}", self.func.dfg.display_inst(i)))
                .unwrap_or_default();
            tracing::info!(
                target: "pliron::diffcheck",
                "watch {w:#x} [{run}, fill {:#x}]: {} at {at}",
                self.stack_fill,
                what(w - addr)
            );
        }
    }

    fn symbol_addr(&mut self, name: &str) -> u64 {
        place_symbol(&mut self.regions, &mut self.syms, self.seed, name, None)
    }

    fn writes(&self) -> Vec<(u64, u8)> {
        let mut out = Vec::new();
        for (&base, r) in &self.regions {
            if base >= stack_base() {
                continue;
            }
            for (i, (a, b)) in r.data.iter().zip(&r.init).enumerate() {
                if a != b {
                    out.push((base + i as u64, *a));
                }
            }
        }
        out
    }
}

fn addr_of(a: &Address) -> Result<u64, MemoryError> {
    match a.region {
        AddressRegion::Stack => Ok(a.offset),
        _ => Err(MemoryError::InvalidAddress(DataValue::I64(a.offset as i64))),
    }
}

impl<'a> State<'a> for Sandbox<'a> {
    fn get_function(&self, _: FuncRef) -> Option<&'a Function> {
        None
    }
    fn get_current_function(&self) -> &'a Function {
        self.func
    }
    fn get_libcall_handler(&self) -> LibCallHandler {
        |_, _| Err(CraneliftTrap::UnreachableCodeReached)
    }
    fn push_frame(&mut self, function: &'a Function) {
        let base = stack_base() + self.frames.len() as u64 * self.frame_size;
        self.regions.insert(
            base,
            Region {
                // Uninit stack reads must yield a value slotprom-style
                // passes can legally rematerialize: 0 matches the
                // `iconst 0` emitted for never-stored slots. A nonzero
                // fill (World::stack_fill) re-runs the original to see
                // which observed bytes it actually determined.
                data: vec![self.stack_fill; self.frame_size as usize],
                init: Vec::new(),
                stub_writable: false,
                set: if self.xsyms.is_some() { vec![0; self.frame_size as usize] } else { Vec::new() },
            },
        );
        self.frames.push((Frame::new(function), base));
    }
    fn pop_frame(&mut self) {
        if let Some((_, base)) = self.frames.pop() {
            self.regions.remove(&base);
        }
    }
    fn current_frame_mut(&mut self) -> &mut Frame<'a> {
        &mut self.frames.last_mut().expect("frame").0
    }
    fn current_frame(&self) -> &Frame<'a> {
        &self.frames.last().expect("frame").0
    }
    fn stack_address(
        &self,
        size: AddressSize,
        slot: StackSlot,
        offset: u64,
    ) -> Result<Address, MemoryError> {
        let max = self.func.sized_stack_slots[slot].size as u64;
        // One past the end is a valid address (and the only one a
        // zero-size slot has); accesses through it are bounds-checked.
        if offset > max {
            return Err(MemoryError::InvalidOffset { offset, max });
        }
        let base = self.frames.last().expect("frame").1;
        Address::from_parts(
            size,
            AddressRegion::Stack,
            0,
            base + self.slot_at[slot.index()] + offset,
        )
    }
    fn checked_load(
        &self,
        address: Address,
        ty: Type,
        mem_flags: MemFlagsData,
    ) -> Result<DataValue, MemoryError> {
        let a = addr_of(&address)?;
        let n = ty.bytes() as u64;
        if self.find(a, n).is_none() {
            self.fault.set(Some(a));
        }
        let src = self.bytes(a, n).ok_or(MemoryError::OutOfBoundsLoad {
            addr: address,
            load_size: n as usize,
            mem_flags,
        })?;
        let v = DataValue::read_from_slice_le(src, ty);
        self.note_write(a, n, |_| format!("load of {v:?} from {a:#x}"));
        Ok(v)
    }
    fn checked_store(
        &mut self,
        address: Address,
        v: DataValue,
        mem_flags: MemFlagsData,
    ) -> Result<(), MemoryError> {
        let a = addr_of(&address)?;
        let n = v.ty().bytes() as u64;
        if self.find(a, n).is_none() {
            self.fault.set(Some(a));
        }
        self.note_write(a, n, |_| format!("store of {v:?} to {a:#x}"));
        let dst = self.bytes_mut(a, n).ok_or(MemoryError::OutOfBoundsStore {
            addr: address,
            store_size: n as usize,
            mem_flags,
        })?;
        v.write_to_slice_le(dst);
        Ok(())
    }
    fn function_address(
        &self,
        size: AddressSize,
        name: &ExternalName,
    ) -> Result<Address, MemoryError> {
        // Callees are stubs keyed by name; the address only needs to be
        // stable and recognizable when it comes back to `call_indirect`.
        let key = ext_key(self.func, name, self.xsyms);
        let idx = hash_str(&key) & 0xff_ffff;
        Address::from_parts(
            size,
            AddressRegion::Function,
            AddressFunctionEntry::UserFunction as u64,
            idx,
        )
    }
    fn get_function_from_address(&self, _: Address) -> Option<InterpreterFunctionRef<'a>> {
        None
    }
    fn resolve_global_value(&self, gv: GlobalValue) -> Result<DataValue, MemoryError> {
        // `symbol_addr` needs `&mut self`; symbols are pre-registered in
        // `run` so this lookup is read-only.
        resolve_gv(self, gv)
    }
    fn get_pinned_reg(&self) -> DataValue {
        self.pinned.clone()
    }
    fn set_pinned_reg(&mut self, v: DataValue) {
        self.pinned = v;
    }
}

fn gv_name(f: &Function, gv: GlobalValue, x: Option<&Syms>) -> Option<String> {
    match &f.global_values[gv] {
        GlobalValueData::Symbol { name, .. } => Some(ext_key(f, name, x)),
        _ => None,
    }
}

fn resolve_gv(s: &Sandbox, gv: GlobalValue) -> Result<DataValue, MemoryError> {
    let f = s.func;
    match &f.global_values[gv] {
        GlobalValueData::Symbol { offset, .. } => {
            let name = gv_name(f, gv, s.xsyms).unwrap();
            let base = *s
                .syms
                .get(&name)
                .ok_or(MemoryError::InvalidAddress(DataValue::I64(0)))?;
            Ok(ptr(base.wrapping_add(offset.bits() as u64)))
        }
        GlobalValueData::IAddImm { base, offset, .. } => {
            let b = resolve_gv(s, *base)?;
            let Some(a) = as_ptr(&b) else {
                return Err(MemoryError::InvalidAddress(b));
            };
            Ok(ptr(a.wrapping_add(offset.bits() as u64)))
        }
        GlobalValueData::Load {
            base,
            offset,
            global_type,
            ..
        } => {
            let Some(b) = as_ptr(&resolve_gv(s, *base)?) else {
                return Err(MemoryError::InvalidAddress(DataValue::I64(0)));
            };
            let a = b.wrapping_add(i64::from(*offset) as u64);
            let src = s
                .bytes(a, global_type.bytes() as u64)
                .ok_or(MemoryError::InvalidAddress(DataValue::I64(a as i64)))?;
            Ok(DataValue::read_from_slice_le(src, *global_type))
        }
        _ => Err(MemoryError::InvalidAddress(DataValue::I64(0))),
    }
}

// --- running ---------------------------------------------------------------

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
    /// Where the last quiet panic happened, for the inconclusive message.
    static PANIC_AT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// The instruction being interpreted, for the same message.
    static CUR_INST: Cell<Option<Inst>> = const { Cell::new(None) };
}

/// Interpreter `unimplemented!()`s are expected here; keep them off stderr.
fn install_quiet_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET.with(Cell::get) {
                prev(info)
            } else if let Some(l) = info.location() {
                PANIC_AT.with(|p| *p.borrow_mut() = format!(" at {}:{}", l.file(), l.line()));
            }
        }));
    });
}

/// `check_nonnull`: stop as undefined behavior when a load known nonnull
/// yields 0. Only for the original: an optimized function may hoist such a
/// load to where its value goes unused, which is fine (it is poison, not UB).
#[allow(clippy::too_many_arguments)]
fn run(
    f: &Function,
    world: &World,
    nowrite: &FxHashSet<String>,
    self_name: &str,
    fuel: u64,
    check_nonnull: bool,
    xsyms: Option<&Syms>,
    replay: Option<&[CallRec]>,
) -> (Outcome, Trace) {
    install_quiet_hook();
    QUIET.with(|q| q.set(true));
    let r = catch_unwind(AssertUnwindSafe(|| {
        run_inner(f, world, nowrite, self_name, fuel, check_nonnull, xsyms, replay)
    }));
    QUIET.with(|q| q.set(false));
    let o = match r {
        Ok(o) => return o,
        Err(e) => {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            let at = PANIC_AT.with(|p| std::mem::take(&mut *p.borrow_mut()));
            let inst = CUR_INST
                .with(Cell::get)
                .filter(|&i| f.dfg.inst_is_valid(i))
                .map(|i| format!(" ({i}: {})", f.dfg.display_inst(i)))
                .unwrap_or_default();
            Outcome::Inconclusive(format!("interpreter panic: {msg}{at}{inst}"))
        }
    };
    (o, Trace::default())
}

enum Stop {
    Trap(String),
    Unsupported(String),
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    f: &Function,
    world: &World,
    nowrite: &FxHashSet<String>,
    self_name: &str,
    fuel: u64,
    check_nonnull: bool,
    xsyms: Option<&Syms>,
    replay: Option<&[CallRec]>,
) -> (Outcome, Trace) {
    let no_facts = FxHashSet::default();
    let no_derived = FxHashMap::default();
    let (no_lt_inst, no_lt_block) = (FxHashMap::default(), FxHashMap::default());
    // Stack frame: the pair's layout (`slot_layout`), the same for
    // every run of the trial, behind the rerun's pad. A slot the layout
    // does not know (a function outside the pair) goes after the rest.
    let pad = world.slot_pad * SLOT_PAGE;
    let mut slot_at: Vec<u64> = world.slot_at.iter().map(|o| o + pad).collect();
    let mut off = pad + world.slots_len;
    for (_, d) in f.sized_stack_slots.iter().skip(slot_at.len()) {
        slot_at.push(off);
        off += slot_room(d.size);
    }
    let mut s = Sandbox {
        func: f,
        frames: Vec::new(),
        frame_size: off.next_multiple_of(16) + 16,
        self_name: self_name.to_string(),
        regions: world.regions.clone(),
        slot_at,
        syms: world.syms.clone(),
        calls: Vec::new(),
        nowrite,
        nonnull: if check_nonnull {
            &world.nonnull
        } else {
            &no_facts
        },
        derived: if check_nonnull {
            &world.derived
        } else {
            &no_derived
        },
        memcmp: &world.memcmp,
        extent: &world.extent,
        undef: if check_nonnull && world.stack_fill != 0 { &world.undef } else { &no_facts },
        lt_inst: if check_nonnull { &world.lt_inst } else { &no_lt_inst },
        lt_block: if check_nonnull { &world.lt_block } else { &no_lt_block },
        seed: world.seed,
        stack_fill: world.stack_fill,
        pinned: DataValue::I64(0),
        fault: Cell::new(None),
        watch: crate::config::env_get("PLIRON_DIFFCHECK_WATCH")
            .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .map(|w| (w, if check_nonnull { "before" } else { "after" })),
        path: crate::config::env_is("PLIRON_DIFFCHECK_PATH", "1")
            .then_some(if check_nonnull { "before" } else { "after" }),
        xsyms,
        replay,
        log: Vec::new(),
    };
    // Data symbols: same name, same address and contents in every run.
    // (A cross-build pair's are placed already: `World::add_symbols`.)
    let mut names: Vec<String> = f
        .global_values
        .keys()
        .filter_map(|gv| gv_name(f, gv, xsyms))
        .collect();
    names.sort();
    for n in names {
        s.symbol_addr(&n);
    }
    let mut left = fuel;
    let o = match exec(&mut s, world.args.clone(), &mut left) {
        Ok(rets) => Outcome::Returned {
            rets,
            writes: s.writes(),
            calls: std::mem::take(&mut s.calls),
        },
        Err(Stop::Trap(t)) => Outcome::Trapped(match s.fault.get() {
            Some(a) => format!("{t} (address {a:#x})"),
            None => t,
        }),
        Err(Stop::Unsupported(r)) => Outcome::Inconclusive(r),
    };
    (o, Trace { log: s.log, calls: s.calls })
}

/// Deepest self-recursion interpreted before giving up.
const MAX_DEPTH: usize = 48;

/// Runs one activation of `s.func` with `args`; self-calls recurse here.
fn exec(s: &mut Sandbox, args: Vec<DataValue>, left: &mut u64) -> Result<Vec<DataValue>, Stop> {
    let f = s.func;
    let entry = f.layout.entry_block().unwrap();
    s.push_frame(f);
    s.current_frame_mut()
        .set_all(f.dfg.block_params(entry), args);
    let r = exec_body(s, entry, left);
    s.pop_frame();
    r
}

fn exec_body(s: &mut Sandbox, entry: ir::Block, left: &mut u64) -> Result<Vec<DataValue>, Stop> {
    let f = s.func;
    let mut inst = f.layout.first_inst(entry);
    while let Some(i) = inst {
        if !s.lt_inst.is_empty() || !s.lt_block.is_empty() {
            // The markers before `i`: after the instruction ahead of it in
            // its block, or at the block's head.
            let ss = match f.layout.prev_inst(i) {
                Some(p) => s.lt_inst.get(&p),
                None => f.layout.inst_block(i).and_then(|b| s.lt_block.get(&b)),
            };
            if let Some(ss) = ss {
                for &sl in ss.clone().iter() {
                    s.restart_slot(sl);
                }
            }
        }
        if *left == 0 {
            return Err(Stop::Unsupported("out of fuel".into()));
        }
        *left -= 1;
        CUR_INST.with(|c| c.set(Some(i)));
        let data = &f.dfg.insts[i];
        match data.opcode() {
            Opcode::Call
            | Opcode::CallIndirect
            | Opcode::ReturnCall
            | Opcode::ReturnCallIndirect
            | Opcode::TryCall
            | Opcode::TryCallIndirect => {
                let rets = do_call(s, i, left)?;
                match data.opcode() {
                    Opcode::ReturnCall | Opcode::ReturnCallIndirect => return Ok(rets),
                    Opcode::TryCall | Opcode::TryCallIndirect => {
                        // Stubs never unwind: take the normal-return edge.
                        let et = data.exception_table().unwrap();
                        let bc = f.dfg.exception_tables[et].normal_return();
                        let blk = bc.block(&f.dfg.value_lists);
                        let mut vals = Vec::new();
                        for a in bc.args(&f.dfg.value_lists) {
                            vals.push(match a {
                                BlockArg::Value(v) => s.current_frame().get(v).clone(),
                                BlockArg::TryCallRet(k) => rets[k as usize].clone(),
                                BlockArg::TryCallExn(_) => {
                                    return Err(Stop::Unsupported("try_call exn arg".into()));
                                }
                            });
                        }
                        s.current_frame_mut().set_all(f.dfg.block_params(blk), vals);
                        inst = f.layout.first_inst(blk);
                    }
                    _ => {
                        s.current_frame_mut().set_all(f.dfg.inst_results(i), rets);
                        inst = f.layout.next_inst(i);
                    }
                }
                continue;
            }
            Opcode::SymbolValue | Opcode::TlsValue => {
                let InstructionData::UnaryGlobalValue { global_value, .. } = *data else {
                    unreachable!()
                };
                let v = resolve_gv(s, global_value)
                    .map_err(|e| Stop::Unsupported(format!("global value: {e}")))?;
                s.current_frame_mut()
                    .set_all(f.dfg.inst_results(i), vec![v]);
                inst = f.layout.next_inst(i);
                continue;
            }
            _ => {}
        }
        let ctx = DfgInstructionContext::new(i, &f.dfg);
        let cf = step(s, ctx).map_err(|e| Stop::Unsupported(format!("{}: {e}", data.opcode())))?;
        match cf {
            ControlFlow::Assign(mut vals) => {
                // The sign and payload of a NaN that arithmetic produces
                // are not determined (Rust, wasm and IEEE 754 agree), and
                // passes change them legally: `commute` swaps the operands
                // of an `fadd` of two NaNs and another one's bits come
                // out. `norm` forgives that in returned values and call
                // arguments; a NaN stored to memory is bytes by then. So
                // every arithmetic NaN is the canonical one in both runs.
                // `fneg`, `fabs`, `fcopysign`, casts and copies are exact
                // on NaNs and stay so.
                if matches!(
                    data.opcode(),
                    Opcode::Fadd
                        | Opcode::Fsub
                        | Opcode::Fmul
                        | Opcode::Fdiv
                        | Opcode::Fma
                        | Opcode::Sqrt
                        | Opcode::Fmin
                        | Opcode::Fmax
                        | Opcode::Ceil
                        | Opcode::Floor
                        | Opcode::Trunc
                        | Opcode::Nearest
                        | Opcode::Fpromote
                        | Opcode::Fdemote
                ) {
                    for v in vals.iter_mut() {
                        match v {
                            DataValue::F32(f) if f.is_nan() => {
                                *v = DataValue::F32(ir::immediates::Ieee32::with_bits(0x7fc0_0000))
                            }
                            DataValue::F64(f) if f.is_nan() => {
                                *v = DataValue::F64(ir::immediates::Ieee64::with_bits(0x7ff8_0000_0000_0000))
                            }
                            _ => {}
                        }
                    }
                }
                // A value that stands for `undef` is the frame's fill
                // (`Facts::undef`; the set is empty unless this is the
                // original's run under the other fill).
                if !s.undef.is_empty() {
                    for (v, r) in vals.iter_mut().zip(f.dfg.inst_results(i)) {
                        if s.undef.contains(r) {
                            let ty = v.ty();
                            *v = DataValue::read_from_slice_le(&[s.stack_fill; 16][..ty.bytes() as usize], ty);
                        }
                    }
                }
                if data.opcode().can_load()
                    && let Some(&r) = f.dfg.inst_results(i).first()
                    && s.nonnull.contains(&r)
                    && matches!(vals.first(), Some(DataValue::I64(0) | DataValue::I32(0)))
                {
                    return Err(Stop::Trap(format!("nonnull load {r} yielded 0")));
                }
                // An inbounds offset of a non-null base is non-null
                // (`derived`); passes fold `offset == 0` on that. A stub
                // call returning 32 into `p - 32` breaks the premise, not
                // the pass (hashbrown's `next().map(|b| b.as_ptr())`
                // under `DebugMap::entries`).
                if vals.first().and_then(as_ptr) == Some(0)
                    && let Some(&r) = f.dfg.inst_results(i).first()
                    && let Some(&base) = s.derived.get(&r)
                    && let Some(base) = f.dfg.try_resolve_aliases(base)
                    && f.dfg.value_is_valid(base)
                    && matches!(
                        s.current_frame_mut().entries_mut().get(base.index()),
                        Some(Some(b)) if as_ptr(b).is_some_and(|b| b != 0)
                    )
                {
                    return Err(Stop::Trap(format!(
                        "inbounds offset {r} of non-null {base} yielded 0"
                    )));
                }
                s.current_frame_mut()
                    .set_all(f.dfg.inst_results(i), vals.to_vec());
                inst = f.layout.next_inst(i);
            }
            ControlFlow::Continue => inst = f.layout.next_inst(i),
            ControlFlow::ContinueAt(b, vals) => {
                if let Some(run) = s.path {
                    tracing::info!(target: "pliron::diffcheck", "path [{run}, fill {:#x}]: {b} {vals:?}", s.stack_fill);
                }
                s.current_frame_mut()
                    .set_all(f.dfg.block_params(b), vals.to_vec());
                inst = f.layout.first_inst(b);
            }
            ControlFlow::Return(vals) => return Ok(vals.to_vec()),
            ControlFlow::Trap(t) => {
                let blk = f.layout.inst_block(i).map(|b| format!(" in {b}")).unwrap_or_default();
                return Err(Stop::Trap(format!("{t} at {i}{blk}: {}", f.dfg.display_inst(i))));
            }
            ControlFlow::Call(..) | ControlFlow::ReturnCall(..) => {
                return Err(Stop::Unsupported("unexpected call".into()));
            }
        }
    }
    Err(Stop::Unsupported("fell off the end of a block".into()))
}

fn do_call(s: &mut Sandbox, i: ir::Inst, left: &mut u64) -> Result<Vec<DataValue>, Stop> {
    let f = s.func;
    let data = &f.dfg.insts[i];
    let all: Vec<DataValue> = f
        .dfg
        .inst_args(i)
        .iter()
        .map(|v| s.current_frame().get(*v).clone())
        .collect();
    let (name, sig, args): (String, SigRef, Vec<DataValue>) = match *data {
        InstructionData::Call { func_ref, .. } | InstructionData::TryCall { func_ref, .. } => {
            let ext = &f.dfg.ext_funcs[func_ref];
            if let ExternalName::LibCall(lc) = ext.name {
                let rets: Vec<Type> = f.dfg.signatures[ext.signature]
                    .returns
                    .iter()
                    .map(|r| r.value_type)
                    .collect();
                return libcall(s, lc, &all, &rets);
            }
            let name = ext_key(f, &ext.name, s.xsyms);
            if s.memcmp.contains(&name) {
                let rets: Vec<Type> = f.dfg.signatures[ext.signature].returns.iter().map(|r| r.value_type).collect();
                return libcall(s, LibCall::Memcmp, &all, &rets);
            }
            if name == s.self_name {
                if s.frames.len() >= MAX_DEPTH {
                    return Err(Stop::Unsupported("recursion too deep".into()));
                }
                return exec(s, all, left);
            }
            (name, ext.signature, all)
        }
        InstructionData::CallIndirect { .. } | InstructionData::TryCallIndirect { .. } => {
            let sig_ref = f.dfg.call_signature(i).unwrap();
            // Callee pointers come from seeded memory (vtables) or
            // `func_addr`; either way the same value in both runs, so the
            // value itself names the stub.
            let callee = all[0].clone();
            (format!("indirect:{callee:?}"), sig_ref, all[1..].to_vec())
        }
        _ => return Err(Stop::Unsupported(format!("call form {}", data.opcode()))),
    };
    stub(s, name, sig, args)
}

/// A deterministic stand-in for any callee: records the call, optionally
/// clobbers a word behind each pointer argument, returns hash-derived values.
fn stub(
    s: &mut Sandbox,
    name: String,
    sig: SigRef,
    args: Vec<DataValue>,
) -> Result<Vec<DataValue>, Stop> {
    let f = s.func;
    // Frame layout is the optimizer's to change (slot merging, SROA), so a
    // pointer into the frame is recorded and hashed without its offset.
    // The stub still writes through the real pointer: a callee writing an
    // out-param that aliases a merged slot must leave identical bytes
    // behind whichever layout it lands in — a merge that lets a folded
    // slot's stub fill clobber a live neighbor diverges and reports.
    let orig_args = args.clone();
    let in_frames = |p: u64| (stack_base()..stack_base() + stack_span()).contains(&p);
    // Cross-build pair, second function: the first one's stub at this
    // point of the call sequence, when it is the same call. (When it is
    // not, the two call lists differ and the trial reports that.)
    let rec: Option<&CallRec> = s
        .replay
        .and_then(|log| log.get(s.calls.len()))
        .filter(|r| r.name == name && r.args.len() == args.len());
    // Where the first function passed the address of a slot of its own,
    // the argument's value was that function's to choose: whatever
    // pointer this one passes there is the same argument. What the callee
    // writes through it still lands where this function's pointer points.
    let args: Vec<DataValue> = args
        .into_iter()
        .enumerate()
        .map(|(k, a)| match as_ptr(&a) {
            Some(p) if in_frames(p) || rec.is_some_and(|r| r.args[k].frame) => ptr(stack_base()),
            _ => a,
        })
        .collect();
    // Bytes from a frame address to the end of its stack slot. Each
    // frame holds the slots at `slot_at` (outer frames count too — a
    // callee can out-param into a caller's slot).
    let room_at = |s: &Sandbox, p: u64| -> Option<u64> {
        let d = p - stack_base();
        let fidx = d / s.frame_size;
        if fidx as usize >= s.frames.len() {
            return None;
        }
        // The slot whose room holds the address: the last one that
        // starts at or before it.
        let rel = d % s.frame_size;
        let slot = s.slot_at.partition_point(|&o| o <= rel).checked_sub(1)?;
        let in_off = rel - s.slot_at[slot];
        let sz = s.func.sized_stack_slots.get(StackSlot::from_u32(slot as u32))?.size as u64;
        (in_off < sz).then(|| sz - in_off)
    };
    let mut arec: Vec<ArgRec> = orig_args
        .iter()
        .map(|a| ArgRec { frame: as_ptr(a).is_some_and(in_frames), ..ArgRec::default() })
        .collect();
    // Cross-build pair: what the callee can read through each pointer is
    // part of the call. A callee reads the memory it is handed, and an IR
    // pass moves the writes that fill it (memcpyopt sends a temporary's
    // writers to the copy's destination; with the copy gone, a later
    // reader of the temporary reads the destination, which a loop may
    // have changed since: two `String`s owning one block, `tests/mcrdwrap`).
    // The first function's stub takes the bytes written so far in the
    // window its pointer opens (to the end of the slot or region, the
    // operand's extent, 64 bytes); the second one's takes the same bytes
    // of the same window behind its own pointer. Bytes of a frame that
    // were never written are not the callee's to read and are left out:
    // an out-slot shows nothing.
    let mut args = args;
    if s.xsyms.is_some() {
        for (k, a) in orig_args.iter().enumerate() {
            let Some(p) = as_ptr(a) else { continue };
            // The callee writes an `sret` area and may not read it: what
            // it held before is dead, and the pipeline need not keep it
            // (`tokenize::setup`: the slot of a `format!` result, reused).
            if f.dfg.signatures[sig].params.get(k).is_some_and(|p| p.purpose == ir::ArgumentPurpose::StructReturn) {
                continue;
            }
            // The window this function's pointer opens: to the end of its
            // slot or region, the operand's extent, 64 bytes.
            let ext = s.extent.get(&(name.clone(), k as u32)).copied().unwrap_or(u64::MAX);
            let room = if in_frames(p) {
                room_at(s, p)
            } else {
                s.find(p, 1).map(|(base, reg)| base + reg.data.len() as u64 - p)
            };
            let own = room.unwrap_or(0).min(64).min(ext);
            // The second function reads the first one's window, as far as
            // its own reaches; beyond that the first one's bytes stand.
            // The callee reads one object, and a window ends at the end of
            // the slot that holds it: where one build keeps more in that
            // slot (sroa off: the whole aggregate around the field passed),
            // the bytes past the object are not the callee's to read.
            let (len, mask, theirs) = match rec {
                Some(r) => (u64::from(r.args[k].seen), Some(r.args[k].mask), Some(r.args[k].bytes)),
                None => (own, None, None),
            };
            if len == 0 {
                continue;
            }
            let mine = len.min(own);
            let mut bytes = s.bytes(p, mine).map(<[u8]>::to_vec).unwrap_or_else(|| vec![0; mine as usize]);
            if let Some(t) = theirs {
                bytes.extend_from_slice(&t[mine as usize..len as usize]);
            }
            let mask = mask.unwrap_or_else(|| {
                let set = s.written(p, len).unwrap_or_else(|| vec![0; len as usize]);
                set.iter().enumerate().fold(0u64, |m, (j, &w)| m | (u64::from(w != 0) << j))
            });
            if mask == 0 {
                continue;
            }
            arec[k].seen = len as u32;
            arec[k].mask = mask;
            let mut seen = [0u8; 64];
            for (j, b) in bytes.iter().enumerate() {
                if mask >> j & 1 != 0 {
                    seen[j] = *b;
                }
            }
            // A frame address in the window (an argument array of
            // `&local`s) is the function's to choose, like one passed
            // directly: the same canonical value.
            let w = if p32() { 4usize } else { 8 };
            let mut j = (p as usize).next_multiple_of(w) - p as usize;
            while j + w <= len as usize {
                let mut cell = [0u8; 8];
                cell[..w].copy_from_slice(&seen[j..j + w]);
                if (mask >> j) & ((1u64 << w) - 1) == (1u64 << w) - 1
                    && in_frames(u64::from_le_bytes(cell))
                {
                    seen[j..j + w].copy_from_slice(&stack_base().to_le_bytes()[..w]);
                }
                j += w;
            }
            arec[k].bytes = seen;
            // Which bytes, then the bytes, a word at a time.
            args.push(DataValue::I64(mask as i64));
            for w in seen[..(len as usize).next_multiple_of(8)].chunks(8) {
                args.push(DataValue::I64(i64::from_le_bytes(w.try_into().unwrap())));
            }
        }
    }
    let mut h = mix(s.seed, hash_str(&name));
    h = mix(h, s.calls.len() as u64);
    for a in &args {
        let mut b = [0u8; 16];
        a.write_to_slice_le(&mut b[..a.ty().bytes() as usize]);
        h = mix(h, u64::from_le_bytes(b[..8].try_into().unwrap()));
        h = mix(h, u64::from_le_bytes(b[8..].try_into().unwrap()));
    }
    // The two fills: up to 64 bytes for a slot of the caller's frame, one
    // word for other memory.
    // In a cross-build pair both are words a function can go on with
    // (small integers, zero, pointers into the heap regions, as seeded
    // memory holds them): the IR passes move code around calls, and a
    // run that dies at the first use of what a callee returned through
    // memory (a `Vec` header that is 24 random bytes) sees none of it.
    let xmode = s.xsyms.is_some();
    let words = |k: usize, n: usize| -> Vec<u8> {
        let targets: Vec<u64> = (0..HEAPS).map(|h| heap_base() + h * REGION_STRIDE + PTR_SLACK).collect();
        let mut v = vec![0u8; n.next_multiple_of(8)];
        fill(&mut v, &targets, &mut Rng(mix(h, k as u64)));
        v.truncate(n);
        v
    };
    let frame_fill = |k: usize, n: usize| -> Vec<u8> {
        if xmode {
            return words(k, n);
        }
        (0..n).map(|j| (mix(h, k as u64) >> ((j % 8) * 8)) as u8 ^ j as u8).collect()
    };
    let word_fill = |k: usize| -> Vec<u8> {
        if xmode { words(k, 8) } else { mix(h, k as u64).to_le_bytes().to_vec() }
    };
    if let Some(rec) = rec {
        // The same bytes the first function's stub wrote, as far as the
        // object this pointer names has room for them.
        for (k, a) in orig_args.iter().enumerate() {
            let (Some(p), r) = (as_ptr(a), rec.args[k]) else { continue };
            if r.fill == 0 {
                continue;
            }
            let room = if in_frames(p) {
                room_at(s, p)
            } else {
                s.find(p, 1)
                    .filter(|(base, reg)| reg.stub_writable && *base < stack_base())
                    .map(|(base, reg)| base + reg.data.len() as u64 - p)
            };
            let Some(room) = room else { continue };
            // The same bytes, wherever this pointer points: a callee that
            // fills its `sret` area fills all of it, in the caller's frame
            // or (memcpyopt forwarded `call f(sret tmp); memcpy(dst, tmp)`)
            // in the heap. Both fills are the same words in cross mode.
            let v = frame_fill(k, (r.fill as u64).min(room) as usize);
            let n = v.len() as u64;
            s.note_write(p, n, |_| format!("stub fill of {n} bytes at {p:#x} (argument {k}, as in the other build)"));
            if let Some(b) = s.bytes_mut(p, n) {
                b.copy_from_slice(&v);
                arec[k].fill = n as u32;
            }
        }
    } else if !s.nowrite.contains(&name) {
        for (k, a) in orig_args.iter().enumerate() {
            if let Some(p) = as_ptr(a) {
                // The callee reaches at most this many bytes through the
                // operand (`&mut RawVecInner` inside a `String` slot: 16
                // of its 24; the `len` beyond is the caller's to keep in
                // a register).
                let ext = s.extent.get(&(name.clone(), k as u32)).copied().unwrap_or(u64::MAX);
                if in_frames(p) {
                    // Frame pointer: bound the fill to the containing
                    // slot's extent so it can't spill into a neighbor.
                    let Some(room) = room_at(s, p) else { continue };
                    let n = room.min(64).min(ext) as usize;
                    let v = frame_fill(k, n);
                    s.note_write(p, n as u64, |_| format!("stub fill of {n} bytes at {p:#x} (argument {k})"));
                    if let Some(b) = s.bytes_mut(p, n as u64) {
                        b.copy_from_slice(&v[..n]);
                        arec[k].fill = n as u32;
                    }
                    continue;
                }
                let writable = s
                    .find(p, 8)
                    .is_some_and(|(base, r)| r.stub_writable && base < stack_base());
                if writable && ext >= 8 {
                    let v = word_fill(k);
                    s.note_write(p, 8, |_| format!("stub fill of 8 bytes at {p:#x} (argument {k})"));
                    s.bytes_mut(p, 8).unwrap().copy_from_slice(&v);
                    arec[k].fill = 8;
                }
            }
        }
    }
    // `PLIRON_DIFFCHECK_PATH=1` logs the calls with the blocks.
    if let Some(run) = s.path {
        tracing::info!(target: "pliron::diffcheck", "call [{run}, fill {:#x}] {}: {name} {args:x?} (as passed: {orig_args:x?})", s.stack_fill, s.calls.len());
    }
    s.log.push(CallRec { name: name.clone(), args: arec });
    s.calls.push((name, args));
    let mut rets = Vec::new();
    for (k, r) in f.dfg.signatures[sig].returns.iter().enumerate() {
        let mut rng = Rng(mix(h, 1000 + k as u64));
        let v = if r.value_type == ptr_ty() && rng.below(2) == 0 {
            ptr(heap_base() + rng.below(HEAPS) * REGION_STRIDE + PTR_SLACK + rng.below(8) * 8)
        } else if xmode && r.value_type == types::I8 {
            // A byte result is mostly a `bool` or the tag of a
            // `Result<(), E>`: 0 or 1, mostly 0. `random_value` never
            // makes it 0, so every `write!(..)?` failed and a loop around
            // one never came round a second time (`tests/mcrdwrap`); and a
            // `bool` of 46 is one no function has to treat alike (sroa off
            // returned it as is, on it came back as `46 & 1`: rustc
            // demangle's `in_binder`).
            DataValue::I8(i8::from(rng.below(4) == 0))
        } else {
            random_value(r.value_type, &mut rng, true)
        };
        rets.push(v);
    }
    Ok(rets)
}

fn libcall(
    s: &mut Sandbox,
    lc: LibCall,
    a: &[DataValue],
    rets: &[Type],
) -> Result<Vec<DataValue>, Stop> {
    let int = |v: &DataValue| -> u64 {
        match v {
            DataValue::I64(x) => *x as u64,
            DataValue::I32(x) => *x as u32 as u64,
            DataValue::I8(x) => *x as u8 as u64,
            _ => 0,
        }
    };
    let oob = || Stop::Unsupported(format!("{lc} out of sandbox"));
    match lc {
        LibCall::Memcpy | LibCall::Memmove => {
            let (d, src, n) = (int(&a[0]), int(&a[1]), int(&a[2]));
            if n > 0 {
                let tmp = s.bytes(src, n).ok_or_else(oob)?.to_vec();
                let set = s.written(src, n);
                s.note_write(d, n, |k| {
                    format!("copy of {n} bytes to {d:#x}: byte {:#x} from {:#x}", tmp[k as usize], src + k)
                });
                s.bytes_mut(d, n).ok_or_else(oob)?.copy_from_slice(&tmp);
                if let Some(set) = set {
                    s.set_written(d, &set);
                }
            }
            Ok(rets.iter().map(|_| ptr(d)).collect())
        }
        LibCall::Memset => {
            let (d, c, n) = (int(&a[0]), int(&a[1]), int(&a[2]));
            if n > 0 {
                s.note_write(d, n, |_| format!("memset of {n} bytes at {d:#x} to {:#x}", c as u8));
                s.bytes_mut(d, n).ok_or_else(oob)?.fill(c as u8);
            }
            Ok(rets.iter().map(|_| ptr(d)).collect())
        }
        LibCall::Memcmp => {
            let (x, y, n) = (int(&a[0]), int(&a[1]), int(&a[2]));
            let r = if n == 0 {
                0
            } else {
                let (p, q) = (
                    s.bytes(x, n).ok_or_else(oob)?,
                    s.bytes(y, n).ok_or_else(oob)?,
                );
                match p.cmp(q) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                }
            };
            Ok(rets
                .iter()
                .map(|t| {
                    DataValue::read_from_slice_le(
                        &(r as i64).to_le_bytes()[..t.bytes() as usize],
                        *t,
                    )
                })
                .collect())
        }
        _ => Err(Stop::Unsupported(format!("libcall {lc}"))),
    }
}

// --- pipeline integration and repro files ----------------------------------

/// How `PLIRON_DIFFCHECK` asks the pipeline to check functions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// `=1`: check the pipeline's input against its output; on a mismatch,
    /// replay step by step to find the step that introduced it.
    Whole,
    /// `=step`: check every step that changes the function.
    PerStep,
}

// r[impl verify.diffcheck.modes]
/// The mode for function `fname`, honoring `PLIRON_DIFFCHECK_FN=<substr>`.
// r[impl clif.diffcheck.modes]
pub fn mode_for(fname: &str) -> Option<Mode> {
    let mode = match crate::config::env_get("PLIRON_DIFFCHECK")? {
        "0" => return None,
        "step" => Mode::PerStep,
        _ => Mode::Whole,
    };
    if let Some(filt) = crate::config::env_get("PLIRON_DIFFCHECK_FN")
        && !fname.contains(filt)
    {
        return None;
    }
    Some(mode)
}

/// `PLIRON_DIFFCHECK_TRIALS`, `PLIRON_DIFFCHECK_FUEL`, `PLIRON_DIFFCHECK_SEED`.
pub fn config_from_env() -> Config {
    let num = |k: &str| crate::config::env_get(k).and_then(|v| v.parse::<u64>().ok());
    let d = Config::default();
    Config {
        trials: num("PLIRON_DIFFCHECK_TRIALS").map_or(d.trials, |n| n as u32),
        fuel: num("PLIRON_DIFFCHECK_FUEL").unwrap_or(d.fuel),
        seed: num("PLIRON_DIFFCHECK_SEED").unwrap_or(d.seed),
    }
}

/// Check one step's input against its output; on a mismatch write a repro
/// and return `true`.
pub fn check_and_report(
    fname: &str,
    step: &str,
    before: &Function,
    after: &Function,
    facts: &Facts,
) -> bool {
    match check(before, after, facts, &config_from_env()) {
        Ok(_) => false,
        Err(m) => {
            report(fname, step, before, after, facts, &m);
            true
        }
    }
}

pub(crate) fn safe_name(fname: &str) -> String {
    fname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect()
}

// r[impl verify.diffcheck.repro]
/// Write `before.clif` / `after.clif` / `report.txt` under
/// `PLIRON_DIFFCHECK_DIR` (default `pliron-diffcheck/`) and log one line.
/// `PLIRON_DIFFCHECK_PANIC=1` turns the report into a panic.
pub fn report(
    fname: &str,
    step: &str,
    before: &Function,
    after: &Function,
    facts: &Facts,
    m: &Mismatch,
) {
    let root = crate::config::env_get("PLIRON_DIFFCHECK_DIR").unwrap_or("pliron-diffcheck");
    let safe = safe_name(fname);
    let dir = std::path::Path::new(root).join(format!(
        "{safe}.{}",
        step.trim_matches(|c| c == '<' || c == '>')
            .replace(' ', "_")
    ));
    let header = facts_header(before, facts);
    let wrote = std::fs::create_dir_all(&dir).and_then(|_| {
        std::fs::write(
            dir.join("before.clif"),
            format!("{header}{}", clif_text(before)),
        )?;
        std::fs::write(
            dir.join("after.clif"),
            format!("{header}{}", clif_text(after)),
        )?;
        let repro = if step.starts_with('<') {
            "clif-check pipeline before.clif".to_string()
        } else {
            format!("clif-check step {step} before.clif")
        };
        std::fs::write(
            dir.join("report.txt"),
            format!("function: {fname}\nstep: {step}\n{m}\n\nreproduce: {repro}\n"),
        )
    });
    let where_ = match wrote {
        Ok(()) => dir.display().to_string(),
        Err(e) => format!("(could not write repro: {e})"),
    };
    let msg =
        format!("diffcheck: `{step}` changed the behavior of `{fname}`\n{m}\nrepro: {where_}");
    if crate::config::env_is("PLIRON_DIFFCHECK_PANIC", "1") {
        panic!("{msg}");
    }
    eprintln!("{msg}");
}

/// `f` as parseable text. `Function::display` prints a global value's
/// symbol as `userextnameN`, which the reader can't map back to its
/// `u<ns>:<idx>` name; spell those out.
pub fn clif_text(f: &Function) -> String {
    let text = f.display().to_string();
    let names = f.params.user_named_funcs();
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(i) = rest.find("userextname") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + "userextname".len()..];
        let n = tail.bytes().take_while(u8::is_ascii_digit).count();
        match tail[..n]
            .parse::<usize>()
            .ok()
            .and_then(|k| names.iter().nth(k))
        {
            Some((_, name)) => {
                let _ = write!(out, "u{}:{}", name.namespace, name.index);
            }
            None => out.push_str(&rest[i..i + "userextname".len() + n]),
        }
        rest = &tail[n..];
    }
    out.push_str(rest);
    out
}

/// Facts in a form that survives a round trip through a `.clif` file.
#[derive(Default, Debug)]
// r[impl clif.check.fidelity]
pub struct OwnedFacts {
    pub nonnull: FxHashSet<Value>,
    pub noalias: FxHashSet<Value>,
    pub scoped_na: FxHashSet<Value>,
    pub deref: FxHashMap<Value, (u64, bool)>,
    pub frozen: FxHashMap<Value, u64>,
    pub nowrite: FxHashMap<FuncRef, bool>,
    pub self_name: Option<String>,
    /// The symbol a corpus dump's `self=` held, once [`Self::bind_self`]
    /// has turned `self_name` into the `u0:<FuncId>` spelling.
    pub self_symbol: Option<String>,
    /// [`Facts::stub_extent`], filled by [`Self::bind_self`] from `nocap`.
    pub stub_extent: FxHashMap<(String, u32), u64>,
    pub derived: VMap<Value>,
    /// `(FuncRef, arg-index, extent)` triples a `; diffcheck-nocap:`
    /// header records; applied to every direct callsite of that callee
    /// on replay (`u64::MAX` = unknown = whole slot).
    pub nocap: Vec<(FuncRef, u32, u64)>,
    /// `FuncRef -> callee symbol` from a `; diffcheck-names:` header.
    pub names: Vec<(FuncRef, String)>,
    /// `; diffcheck-lifetimes:` markers as `(slots, anchor inst
    /// layout-ordinal, block index, is-start)`. Slots are stored as
    /// `StackSlot` indices — the operand `Value` is resolved to slots at
    /// dump time, before the pipeline can fold it away. Resolve inst/block
    /// ordinals against the parsed `Function` with [`Self::lifetimes`].
    pub lifetimes: Vec<(Vec<u32>, Option<u32>, u32, bool)>,
    /// Region-scoped facts (`jumpthread::RegionNN`) from the
    /// `; diffcheck-bregion/rparent/rnn:` headers, regions numbered:
    /// (block layout ordinal, region), (region, parent), and value ->
    /// regions it is non-null in.
    pub block_region: Vec<(u32, u32)>,
    pub region_parent: Vec<(u32, u32)>,
    pub region_nn: Vec<(Value, Vec<u32>)>,
    /// `; diffcheck-undef:` ([`Facts::undef`]).
    pub undef: FxHashSet<Value>,
    /// `; diffcheck-syms:` and `; diffcheck-data:`: every external name of
    /// the function by symbol, and the images of the constant data among
    /// them ([`Self::syms_of`] adds what the `names` header knows).
    pub syms: Syms,
}

impl Syms {
    /// May `s` stand in a `; diffcheck-syms:`/`; diffcheck-data:` item?
    fn plain(s: &str) -> bool {
        !s.is_empty() && !s.contains([',', '=', '/', '+', ' ', '\n'])
    }

    /// The two header lines a corpus dump records these as (empty when
    /// there is nothing to say):
    /// `; diffcheck-syms: u0:12=<symbol>,u1:65=<symbol>` and
    /// `; diffcheck-data: <symbol>=<hex bytes>[/<offset>+<f|d>+<symbol>+<addend>]..,..`.
    pub fn header(&self) -> String {
        let mut names: Vec<String> = self
            .names
            .iter()
            .filter(|(_, s)| Self::plain(s))
            .map(|(d, s)| format!("{d}={s}"))
            .collect();
        names.sort();
        let mut data: Vec<String> = self
            .data
            .iter()
            .filter(|(s, img)| Self::plain(s) && img.relocs.iter().all(|r| Self::plain(&r.sym)))
            .map(|(s, img)| {
                let mut t = format!("{s}=");
                for b in &img.bytes {
                    let _ = write!(t, "{b:02x}");
                }
                for r in &img.relocs {
                    let _ = write!(t, "/{}+{}+{}+{}", r.off, if r.func { 'f' } else { 'd' }, r.sym, r.addend);
                }
                t
            })
            .collect();
        data.sort();
        let mut out = String::new();
        if !self.undef.is_empty() {
            let mut vs: Vec<u32> = self.undef.iter().map(|v| v.as_u32()).collect();
            vs.sort_unstable();
            vs.dedup();
            let vs: Vec<String> = vs.iter().map(|v| format!("v{v}")).collect();
            let _ = writeln!(out, "; diffcheck-undef: {}", vs.join(","));
        }
        if !self.sret.is_empty() {
            let mut v: Vec<String> = self
                .sret
                .iter()
                .filter(|(s, _)| Self::plain(s))
                .map(|(s, n)| format!("{s}={n}"))
                .collect();
            v.sort();
            let _ = writeln!(out, "; diffcheck-sret: {}", v.join(","));
        }
        if !names.is_empty() {
            let _ = writeln!(out, "; diffcheck-syms: {}", names.join(","));
        }
        if !data.is_empty() {
            let _ = writeln!(out, "; diffcheck-data: {}", data.join(","));
        }
        out
    }

    fn parse_line(&mut self, line: &str) {
        if let Some(list) = line.strip_prefix("; diffcheck-sret:") {
            for item in list.split(',') {
                if let Some((s, n)) = item.trim().split_once('=')
                    && let Ok(n) = n.parse()
                {
                    self.sret.insert(s.to_string(), n);
                }
            }
            return;
        }
        if let Some(list) = line.strip_prefix("; diffcheck-syms:") {
            for item in list.split(',') {
                if let Some((d, s)) = item.trim().split_once('=') {
                    self.names.insert(d.to_string(), s.to_string());
                }
            }
        } else if let Some(list) = line.strip_prefix("; diffcheck-data:") {
            for item in list.split(',') {
                let Some((sym, rest)) = item.trim().split_once('=') else { continue };
                let mut parts = rest.split('/');
                let hex = parts.next().unwrap_or("");
                let bytes: Option<Vec<u8>> = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok())
                    .collect();
                let relocs: Option<Vec<Reloc>> = parts
                    .map(|r| {
                        let f: Vec<&str> = r.split('+').collect();
                        (f.len() == 4).then_some(())?;
                        Some(Reloc {
                            off: f[0].parse().ok()?,
                            func: f[1] == "f",
                            sym: f[2].to_string(),
                            addend: f[3].parse().ok()?,
                        })
                    })
                    .collect();
                if let (Some(bytes), Some(relocs), true) = (bytes, relocs, hex.len() % 2 == 0) {
                    self.data.insert(sym.to_string(), DataImage { bytes, relocs });
                }
            }
        }
    }
}

impl OwnedFacts {
    /// How `func` spells its symbols: the `syms` header, and for a corpus
    /// recorded before it existed the callees of the `names` header.
    pub fn syms_of(&self, func: &Function) -> Syms {
        let mut out = self.syms.clone();
        for (fr, sym) in &self.names {
            if let Some(d) = func.dfg.ext_funcs.get(*fr) {
                out.names
                    .entry(d.name.display(Some(&func.params)).to_string())
                    .or_insert_with(|| sym.clone());
            }
        }
        out
    }

    /// A corpus dump records `self=<symbol>`, a diffcheck repro
    /// `self=u0:<FuncId>`; callees are spelled `u0:<FuncId>` in both. Turn
    /// the symbol into that spelling through the `names` header, so a
    /// replay knows which calls are self-calls: `tailrec` keys on the
    /// `FuncId` and the interpreter runs self-recursion for real. A
    /// function that never calls itself has no id in the text: `self_name`
    /// becomes `None` (the caller must then not invent an id — defaulting
    /// to 0 made `tailrec` loop on calls to whichever callee was `u0:0`).
    pub fn bind_self(&mut self, func: &Function) {
        // The `nocap` header is per callee (every direct call of it).
        let mut by_callee: FxHashMap<(FuncRef, u32), u64> = FxHashMap::default();
        for &(fr, a, e) in &self.nocap {
            let x = by_callee.entry((fr, a)).or_insert(e);
            *x = (*x).max(e);
        }
        self.stub_extent = stub_extents(func, |i, k| match func.dfg.insts[i] {
            InstructionData::Call { func_ref, .. } | InstructionData::TryCall { func_ref, .. } => {
                by_callee.get(&(func_ref, k)).copied()
            }
            _ => None,
        });
        let Some(sym) = self.self_name.take_if(|n| !n.starts_with("u0:")) else {
            return;
        };
        self.self_name = self.names.iter().find(|(_, n)| *n == sym).and_then(|(fr, _)| {
            let ExternalName::User(u) = func.dfg.ext_funcs.get(*fr)?.name else {
                return None;
            };
            let un = &func.params.user_named_funcs()[u];
            (un.namespace == 0).then(|| format!("u0:{}", un.index))
        });
        self.self_symbol = Some(sym);
    }

    pub fn facts(&self) -> Facts<'_> {
        Facts {
            nonnull: Some(&self.nonnull),
            noalias: Some(&self.noalias),
            scoped_na: Some(&self.scoped_na),
            deref: Some(&self.deref),
            frozen: Some(&self.frozen),
            nowrite: Some(&self.nowrite),
            self_name: self.self_name.clone(),
            derived: Some(&self.derived),
            memcmp: self
                .names
                .iter()
                .filter(|(_, n)| n == "memcmp" || n == "bcmp")
                .map(|(r, _)| *r)
                .collect(),
            stub_extent: self.stub_extent.clone(),
            undef: Some(&self.undef),
            lifetime_starts: None,
        }
    }

    /// Rebuild `Pipe::lifetimes` from the parsed ordinals: positions are
    /// layout ordinals (the reader numbers entities in text order), and a
    /// missing anchor or stale block degrades to the block fallback like a
    /// deleted-anchor marker does.
    pub fn lifetimes(&self, f: &Function) -> Vec<(Vec<StackSlot>, Option<Inst>, Block, bool)> {
        let mut iords: Vec<Inst> = Vec::new();
        let mut bords: Vec<Block> = Vec::new();
        for b in f.layout.blocks() {
            bords.push(b);
            iords.extend(f.layout.block_insts(b));
        }
        self.lifetimes
            .iter()
            .filter_map(|(ss, a, b, s)| {
                let a = a.and_then(|o| iords.get(o as usize).copied());
                let b = bords.get(*b as usize).copied()?;
                let ss = ss
                    .iter()
                    .map(|&s| StackSlot::from_u32(s))
                    .filter(|s| s.index() < f.sized_stack_slots.len())
                    .collect();
                Some((ss, a, b, *s))
            })
            .collect()
    }

    /// Parse the `; diffcheck-facts:` line `facts_header` writes (plus the
    /// optional `; diffcheck-nocap:`/`; diffcheck-names:` companions a
    /// `PLIRON_CORPUS` dump adds); absent means no facts.
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        let fref = |s: &str| -> Option<FuncRef> {
            s.strip_prefix("fn")?.parse().ok().map(FuncRef::from_u32)
        };
        let num = |s: &str, c: char| -> Option<u32> { s.trim().strip_prefix(c)?.parse().ok() };
        for line in text.lines() {
            out.syms.parse_line(line);
            if let Some(list) = line.strip_prefix("; diffcheck-undef:") {
                out.undef.extend(
                    list.split(',')
                        .filter_map(|v| v.trim().strip_prefix('v')?.parse().ok())
                        .map(Value::from_u32),
                );
            }
            if let Some(list) = line.strip_prefix("; diffcheck-nocap:") {
                for item in list.split(',').filter(|x| !x.is_empty()) {
                    // `fn{funcref}:{arg}` or `fn{funcref}:{arg}:{extent}`;
                    // a missing extent is the conservative `u64::MAX`.
                    let mut it = item.trim().split(':');
                    if let (Some(f), Some(a)) = (it.next(), it.next())
                        && let (Some(r), Ok(a)) = (fref(f), a.parse())
                    {
                        let ext = it
                            .next()
                            .and_then(|e| e.parse().ok())
                            .unwrap_or(u64::MAX);
                        out.nocap.push((r, a, ext));
                    }
                }
            } else if let Some(list) = line.strip_prefix("; diffcheck-names:") {
                for item in list.split(',').filter(|x| !x.is_empty()) {
                    if let Some((f, n)) = item.split_once(':')
                        && let Some(r) = fref(f)
                    {
                        out.names.push((r, n.to_string()));
                    }
                }
            } else if let Some(list) = line.strip_prefix("; diffcheck-bregion:") {
                for item in list.split(',').filter(|x| !x.trim().is_empty()) {
                    if let Some((b, r)) = item.trim().split_once('=')
                        && let (Some(b), Some(r)) = (num(b, 'b'), num(r, 'r'))
                    {
                        out.block_region.push((b, r));
                    }
                }
            } else if let Some(list) = line.strip_prefix("; diffcheck-rparent:") {
                for item in list.split(',').filter(|x| !x.trim().is_empty()) {
                    if let Some((r, q)) = item.trim().split_once('<')
                        && let (Some(r), Some(q)) = (num(r, 'r'), num(q, 'r'))
                    {
                        out.region_parent.push((r, q));
                    }
                }
            } else if let Some(list) = line.strip_prefix("; diffcheck-rnn:") {
                for item in list.split(',').filter(|x| !x.trim().is_empty()) {
                    if let Some((v, rs)) = item.trim().split_once(':')
                        && let Some(v) = num(v, 'v')
                    {
                        let rs = rs.split('+').filter_map(|r| num(r, 'r')).collect();
                        out.region_nn.push((Value::from_u32(v), rs));
                    }
                }
            } else if let Some(list) = line.strip_prefix("; diffcheck-lifetimes:") {
                for item in list.split(',').filter(|x| !x.is_empty()) {
                    let p: Vec<&str> = item.trim().split(':').collect();
                    if p.len() == 4
                        && let (Some(b), Ok(s)) = (
                            p[2].strip_prefix('b').and_then(|n| n.parse::<u32>().ok()),
                            p[3].parse::<u8>(),
                        )
                    {
                        let ss: Vec<u32> = p[0]
                            .split('+')
                            .filter_map(|t| t.strip_prefix('s').and_then(|n| n.parse().ok()))
                            .collect();
                        let a = p[1].strip_prefix('i').and_then(|n| n.parse::<u32>().ok());
                        out.lifetimes.push((ss, a, b, s != 0));
                    }
                }
            }
        }
        let Some(line) = text
            .lines()
            .find_map(|l| l.strip_prefix("; diffcheck-facts:"))
        else {
            return out;
        };
        let val =
            |s: &str| -> Option<Value> { s.strip_prefix('v')?.parse().ok().map(Value::from_u32) };
        let fref = |s: &str| -> Option<FuncRef> {
            s.strip_prefix("fn")?.parse().ok().map(FuncRef::from_u32)
        };
        for field in line.split_whitespace() {
            let Some((k, list)) = field.split_once('=') else {
                continue;
            };
            if k == "self" {
                out.self_name = Some(list.to_string());
                continue;
            }
            for item in list.split(',').filter(|x| !x.is_empty()) {
                let parts: Vec<&str> = item.split(':').collect();
                match k {
                    "nonnull" => out.nonnull.extend(val(parts[0])),
                    "noalias" => out.noalias.extend(val(parts[0])),
                    "scopedna" => out.scoped_na.extend(val(parts[0])),
                    "deref" if parts.len() == 3 => {
                        if let (Some(v), Ok(n)) = (val(parts[0]), parts[1].parse()) {
                            out.deref.insert(v, (n, parts[2] == "w"));
                        }
                    }
                    "frozen" if parts.len() == 2 => {
                        if let (Some(v), Ok(n)) = (val(parts[0]), parts[1].parse()) {
                            out.frozen.insert(v, n);
                        }
                    }
                    "derived" if parts.len() == 2 => {
                        if let (Some(v), Some(b)) = (val(parts[0]), val(parts[1])) {
                            out.derived.insert(v, b);
                        }
                    }
                    "nowrite" if parts.len() == 2 => {
                        if let Some(r) = fref(parts[0]) {
                            out.nowrite.insert(r, parts[1] == "1");
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

use cranelift_codegen::entity::EntityRef as _;

/// One comment line recording the facts a repro needs: nonnull values,
/// derived pointers, and what concerns `f`'s entry params and callees.
pub fn facts_header(f: &Function, facts: &Facts) -> String {
    let params: FxHashSet<Value> = f
        .layout
        .entry_block()
        .map(|b| f.dfg.block_params(b).iter().copied().collect())
        .unwrap_or_default();
    let mut s = String::from("; diffcheck-facts:");
    let sorted = |vals: Vec<String>| {
        let mut v = vals;
        v.sort();
        v.join(",")
    };
    let set = |o: Option<&FxHashSet<Value>>| -> Vec<String> {
        o.map(|s| {
            s.iter()
                .filter(|v| params.contains(v))
                .map(|v| v.to_string())
                .collect()
        })
        .unwrap_or_default()
    };
    // Every nonnull value, not just params: loads carry it too.
    let nonnull = sorted(
        facts
            .nonnull
            .map(|s| s.iter().map(|v| v.to_string()).collect())
            .unwrap_or_default(),
    );
    let derived = sorted(
        facts
            .derived
            .map(|m| m.iter().map(|(v, b)| format!("{v}:{b}")).collect())
            .unwrap_or_default(),
    );
    let noalias = sorted(set(facts.noalias));
    // Entry params, plus the scoped-`noalias` candidates of
    // `PLIRON_SCOPED_NOALIAS`: an inlined callee's written-through
    // `noalias` param is a clone-entry block param that is scoped only
    // (`loadfwd::scoped_seed`). Without those a replay has no candidate
    // and says nothing about `scopedfwd`. The `readonly` half (scoped and
    // `noalias`) stays out, as do its `noalias`/`deref` entries: on a
    // replay it would be a candidate, which it is not in the compile.
    let scopedna = sorted(
        facts
            .scoped_na
            .map(|s| {
                s.iter()
                    .filter(|&&v| {
                        params.contains(&v)
                            || (!facts.noalias.is_some_and(|n| n.contains(&v))
                                && matches!(f.dfg.try_value_def(v), Some(ir::ValueDef::Param(b, _)) if f.layout.is_block_inserted(b)))
                    })
                    .map(|v| v.to_string())
                    .collect()
            })
            .unwrap_or_default(),
    );
    let deref = sorted(
        facts
            .deref
            .map(|m| {
                m.iter()
                    .filter(|(v, _)| params.contains(v))
                    .map(|(v, (n, w))| format!("{v}:{n}:{}", if *w { "w" } else { "r" }))
                    .collect()
            })
            .unwrap_or_default(),
    );
    let frozen = sorted(
        facts
            .frozen
            .map(|m| {
                m.iter()
                    .filter(|(v, _)| params.contains(v))
                    .map(|(v, n)| format!("{v}:{n}"))
                    .collect()
            })
            .unwrap_or_default(),
    );
    let nowrite = sorted(
        facts
            .nowrite
            .map(|m| {
                m.iter()
                    .filter(|(r, _)| f.dfg.ext_funcs.is_valid(**r))
                    .map(|(r, b)| format!("fn{}:{}", r.index(), u8::from(*b)))
                    .collect()
            })
            .unwrap_or_default(),
    );
    let _ = write!(
        s,
        " nonnull={nonnull} noalias={noalias} scopedna={scopedna} deref={deref} frozen={frozen} nowrite={nowrite} derived={derived}"
    );
    if let Some(n) = &facts.self_name {
        let _ = write!(s, " self={n}");
    }
    s.push('\n');
    s
}
