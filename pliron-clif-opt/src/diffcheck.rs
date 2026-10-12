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
    self, BlockArg, ExternalName, FuncRef, Function, GlobalValue, GlobalValueData, InstructionData,
    LibCall, MemFlagsData, Opcode, SigRef, StackSlot, Type, Value, types,
};
use cranelift_interpreter::address::{Address, AddressFunctionEntry, AddressRegion, AddressSize};
use cranelift_interpreter::frame::Frame;
use cranelift_interpreter::instruction::DfgInstructionContext;
use cranelift_interpreter::interpreter::LibCallHandler;
use cranelift_interpreter::state::{InterpreterFunctionRef, MemoryError, State};
use cranelift_interpreter::step::{ControlFlow, CraneliftTrap, step};
use rustc_hash::{FxHashMap, FxHashSet};

/// Facts the optimizer was allowed to assume; inputs are generated to
/// respect them so a pass that relies on them isn't flagged.
#[derive(Default)]
pub struct Facts<'a> {
    pub nonnull: Option<&'a FxHashSet<Value>>,
    pub noalias: Option<&'a FxHashSet<Value>>,
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
    /// Derived pointer → its base, as the pipeline tracks them. Not used by
    /// the check; saved in repros so `clif-check` replays a step faithfully.
    pub derived: Option<&'a FxHashMap<Value, Value>>,
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
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "trial {} args {:?}", self.trial, self.args)?;
        writeln!(f, "  before: {}", describe(&self.before))?;
        write!(f, "  after:  {}", describe(&self.after))
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
                let shown: Vec<_> = writes.iter().take(16).collect();
                let _ = write!(s, ", {} bytes written {shown:x?}", writes.len());
            }
            s
        }
        Outcome::Trapped(t) => format!("trapped: {t}"),
        Outcome::Inconclusive(r) => format!("inconclusive: {r}"),
    }
}

/// Compare `before` and `after` on `cfg.trials` random inputs.
pub fn check(
    before: &Function,
    after: &Function,
    facts: &Facts,
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
            if before.dfg.value_type(p) == types::I64 {
                all_ptrs.entry(p).or_default();
            }
        }
    }
    let nowrite_names = nowrite_by_name(before, facts);
    for trial in 0..cfg.trials {
        let seed = mix(cfg.seed, trial as u64);
        let world = World::new(
            before,
            if trial % 2 == 1 { &all_ptrs } else { &ptrs },
            facts,
            seed,
        );
        let self_name = facts
            .self_name
            .clone()
            .unwrap_or_else(|| before.name.to_string());
        let b = run(before, &world, &nowrite_names, &self_name, cfg.fuel, true);
        if !matches!(b, Outcome::Returned { .. }) {
            sum.inconclusive += 1;
            sum.why
                .get_or_insert_with(|| format!("before {}", describe(&b)));
            continue;
        }
        let a = run(after, &world, &nowrite_names, &self_name, cfg.fuel, false);
        match &a {
            Outcome::Inconclusive(r) if !r.starts_with("out of fuel") => {
                sum.inconclusive += 1;
                sum.why
                    .get_or_insert_with(|| format!("after inconclusive: {r}"));
            }
            _ if same(&a, &b) => sum.compared += 1,
            _ => {
                return Err(Mismatch {
                    trial,
                    args: world.args.clone(),
                    before: b,
                    after: a,
                });
            }
        }
    }
    Ok(sum)
}

/// Equality that treats any two NaNs as equal: passes may change payloads.
fn same(a: &Outcome, b: &Outcome) -> bool {
    // By type and bytes: `DataValue`'s float `==` says NaN != NaN.
    fn norm(v: &DataValue) -> (Type, [u8; 16]) {
        let mut b = [0u8; 16];
        match v {
            DataValue::F32(f) if f.is_nan() => {
                b[..4].copy_from_slice(&0x7fc0_0000u32.to_le_bytes())
            }
            DataValue::F64(f) if f.is_nan() => {
                b[..8].copy_from_slice(&0x7ff8_0000_0000_0000u64.to_le_bytes())
            }
            v => v.write_to_slice_le(&mut b[..v.ty().bytes() as usize]),
        }
        (v.ty(), b)
    }
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
// They must exceed `u32::MAX`: the interpreter decodes a smaller computed
// address as a 32-bit one, whose top bits name a different region.
const PARAM_BASE: u64 = 0x10_0000_0000;
const HEAP_BASE: u64 = 0x20_0000_0000;
const SYM_BASE: u64 = 0x30_0000_0000;
const STACK_BASE: u64 = 0x40_0000_0000;
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
}

/// The seeded sandbox one trial runs in; cloned per run.
#[derive(Clone)]
struct World {
    regions: BTreeMap<u64, Region>,
    args: Vec<DataValue>,
    seed: u64,
    /// Values the optimizer may assume nonzero (`!nonnull` loads too): a
    /// load that yields 0 into one is undefined behavior, so the run stops.
    nonnull: FxHashSet<Value>,
}

impl World {
    fn new(f: &Function, ptrs: &FxHashMap<Value, PtrInfo>, facts: &Facts, seed: u64) -> Self {
        let mut rng = Rng(seed);
        let mut regions = BTreeMap::new();
        let mut targets: Vec<u64> = Vec::new();
        for h in 0..HEAPS {
            targets.push(HEAP_BASE + h * REGION_STRIDE + PTR_SLACK);
        }
        let entry = f.layout.entry_block().expect("function has an entry block");
        let params = f.dfg.block_params(entry);
        for (i, p) in params.iter().enumerate() {
            if let Some(pi) = ptrs.get(p) {
                let size = pi.size.max(256).min(REGION_STRIDE - 2 * PTR_SLACK);
                let base = PARAM_BASE + i as u64 * REGION_STRIDE;
                regions.insert(
                    base,
                    Region {
                        data: vec![0; (size + 2 * PTR_SLACK) as usize],
                        init: Vec::new(),
                        stub_writable: !pi.frozen,
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
                HEAP_BASE + h * REGION_STRIDE,
                Region {
                    data: vec![0; (HEAP_SIZE + 2 * PTR_SLACK) as usize],
                    init: Vec::new(),
                    stub_writable: true,
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
                DataValue::I64((PARAM_BASE + i as u64 * REGION_STRIDE + PTR_SLACK) as i64)
            } else {
                let nonnull = facts.nonnull.is_some_and(|s| s.contains(p));
                random_value(ty, &mut rng, nonnull)
            };
            args.push(v);
        }
        Self {
            regions,
            args,
            seed,
            nonnull: facts.nonnull.cloned().unwrap_or_default(),
        }
    }
}

fn fill(data: &mut [u8], targets: &[u64], rng: &mut Rng) {
    for w in data.chunks_mut(8) {
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
fn pointer_params(f: &Function, facts: &Facts) -> FxHashMap<Value, PtrInfo> {
    let mut out: FxHashMap<Value, PtrInfo> = FxHashMap::default();
    let Some(entry) = f.layout.entry_block() else {
        return out;
    };
    let params = f.dfg.block_params(entry).to_vec();
    for &p in &params {
        if f.dfg.value_type(p) != types::I64 {
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

fn nowrite_by_name(f: &Function, facts: &Facts) -> FxHashSet<String> {
    let mut out = FxHashSet::default();
    if let Some(m) = facts.nowrite {
        for (fr, _) in m {
            if let Some(d) = f.dfg.ext_funcs.get(*fr) {
                out.insert(d.name.display(Some(&f.params)).to_string());
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
    /// Byte offset of each stack slot within the frame.
    slot_off: FxHashMap<StackSlot, u64>,
    syms: FxHashMap<String, u64>,
    calls: Vec<(String, Vec<DataValue>)>,
    nowrite: &'a FxHashSet<String>,
    nonnull: &'a FxHashSet<Value>,
    seed: u64,
    pinned: DataValue,
    /// Last address a load or store fell outside the sandbox at.
    fault: Cell<Option<u64>>,
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

    fn bytes_mut(&mut self, addr: u64, len: u64) -> Option<&mut [u8]> {
        let base = self.find(addr, len)?.0;
        let r = self.regions.get_mut(&base)?;
        let o = (addr - base) as usize;
        Some(&mut r.data[o..o + len as usize])
    }

    fn symbol_addr(&mut self, name: &str) -> u64 {
        if let Some(&a) = self.syms.get(name) {
            return a;
        }
        let base = SYM_BASE + self.syms.len() as u64 * REGION_STRIDE;
        let mut rng = Rng(mix(self.seed, hash_str(name)));
        let targets: Vec<u64> = (0..HEAPS)
            .map(|h| HEAP_BASE + h * REGION_STRIDE + PTR_SLACK)
            .collect();
        let mut data = vec![0u8; 1024];
        fill(&mut data, &targets, &mut rng);
        self.regions.insert(
            base,
            Region {
                init: data.clone(),
                data,
                stub_writable: false,
            },
        );
        self.syms.insert(name.to_string(), base + PTR_SLACK);
        base + PTR_SLACK
    }

    fn writes(&self) -> Vec<(u64, u8)> {
        let mut out = Vec::new();
        for (&base, r) in &self.regions {
            if base >= STACK_BASE {
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
        let base = STACK_BASE + self.frames.len() as u64 * self.frame_size;
        self.regions.insert(
            base,
            Region {
                data: vec![0xa5; self.frame_size as usize],
                init: Vec::new(),
                stub_writable: false,
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
            base + self.slot_off[&slot] + offset,
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
        Ok(DataValue::read_from_slice_le(src, ty))
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
        let key = name.display(Some(&self.func.params)).to_string();
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

fn gv_name(f: &Function, gv: GlobalValue) -> Option<String> {
    match &f.global_values[gv] {
        GlobalValueData::Symbol { name, .. } => Some(name.display(Some(&f.params)).to_string()),
        _ => None,
    }
}

fn resolve_gv(s: &Sandbox, gv: GlobalValue) -> Result<DataValue, MemoryError> {
    let f = s.func;
    match &f.global_values[gv] {
        GlobalValueData::Symbol { offset, .. } => {
            let name = gv_name(f, gv).unwrap();
            let base = *s
                .syms
                .get(&name)
                .ok_or(MemoryError::InvalidAddress(DataValue::I64(0)))?;
            Ok(DataValue::I64(
                base.wrapping_add(offset.bits() as u64) as i64
            ))
        }
        GlobalValueData::IAddImm { base, offset, .. } => {
            let b = resolve_gv(s, *base)?;
            let DataValue::I64(b) = b else {
                return Err(MemoryError::InvalidAddress(b));
            };
            Ok(DataValue::I64(b.wrapping_add(offset.bits())))
        }
        GlobalValueData::Load {
            base,
            offset,
            global_type,
            ..
        } => {
            let DataValue::I64(b) = resolve_gv(s, *base)? else {
                return Err(MemoryError::InvalidAddress(DataValue::I64(0)));
            };
            let a = (b as u64).wrapping_add(i64::from(*offset) as u64);
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
fn run(
    f: &Function,
    world: &World,
    nowrite: &FxHashSet<String>,
    self_name: &str,
    fuel: u64,
    check_nonnull: bool,
) -> Outcome {
    install_quiet_hook();
    QUIET.with(|q| q.set(true));
    let r = catch_unwind(AssertUnwindSafe(|| {
        run_inner(f, world, nowrite, self_name, fuel, check_nonnull)
    }));
    QUIET.with(|q| q.set(false));
    match r {
        Ok(o) => o,
        Err(e) => {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            let at = PANIC_AT.with(|p| std::mem::take(&mut *p.borrow_mut()));
            Outcome::Inconclusive(format!("interpreter panic: {msg}{at}"))
        }
    }
}

enum Stop {
    Trap(String),
    Unsupported(String),
}

fn run_inner(
    f: &Function,
    world: &World,
    nowrite: &FxHashSet<String>,
    self_name: &str,
    fuel: u64,
    check_nonnull: bool,
) -> Outcome {
    let no_facts = FxHashSet::default();
    // Stack frame: slots laid out in order, aligned, seeded with a pattern.
    let mut slot_off = FxHashMap::default();
    let mut off = 0u64;
    for (slot, d) in f.sized_stack_slots.iter() {
        let align = 1u64 << d.align_shift;
        off = off.next_multiple_of(align.max(1));
        slot_off.insert(slot, off);
        off += d.size as u64;
    }
    let mut s = Sandbox {
        func: f,
        frames: Vec::new(),
        frame_size: off.next_multiple_of(16) + 16,
        self_name: self_name.to_string(),
        regions: world.regions.clone(),
        slot_off,
        syms: FxHashMap::default(),
        calls: Vec::new(),
        nowrite,
        nonnull: if check_nonnull {
            &world.nonnull
        } else {
            &no_facts
        },
        seed: world.seed,
        pinned: DataValue::I64(0),
        fault: Cell::new(None),
    };
    // Data symbols: same name, same address and contents in every run.
    let mut names: Vec<String> = f
        .global_values
        .keys()
        .filter_map(|gv| gv_name(f, gv))
        .collect();
    names.sort();
    for n in names {
        s.symbol_addr(&n);
    }
    let mut left = fuel;
    match exec(&mut s, world.args.clone(), &mut left) {
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
    }
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
        if *left == 0 {
            return Err(Stop::Unsupported("out of fuel".into()));
        }
        *left -= 1;
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
            ControlFlow::Assign(vals) => {
                if data.opcode().can_load()
                    && let Some(&r) = f.dfg.inst_results(i).first()
                    && s.nonnull.contains(&r)
                    && matches!(vals.first(), Some(DataValue::I64(0) | DataValue::I32(0)))
                {
                    return Err(Stop::Trap(format!("nonnull load {r} yielded 0")));
                }
                s.current_frame_mut()
                    .set_all(f.dfg.inst_results(i), vals.to_vec());
                inst = f.layout.next_inst(i);
            }
            ControlFlow::Continue => inst = f.layout.next_inst(i),
            ControlFlow::ContinueAt(b, vals) => {
                s.current_frame_mut()
                    .set_all(f.dfg.block_params(b), vals.to_vec());
                inst = f.layout.first_inst(b);
            }
            ControlFlow::Return(vals) => return Ok(vals.to_vec()),
            ControlFlow::Trap(t) => {
                return Err(Stop::Trap(format!("{t} at {i}: {}", f.dfg.display_inst(i))));
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
            let name = ext.name.display(Some(&f.params)).to_string();
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
    let args: Vec<DataValue> = args
        .into_iter()
        .map(|a| match a {
            DataValue::I64(p) if (STACK_BASE..STACK_BASE + 0x1000_0000).contains(&(p as u64)) => {
                DataValue::I64(STACK_BASE as i64)
            }
            a => a,
        })
        .collect();
    let mut h = mix(s.seed, hash_str(&name));
    h = mix(h, s.calls.len() as u64);
    for a in &args {
        let mut b = [0u8; 16];
        a.write_to_slice_le(&mut b[..a.ty().bytes() as usize]);
        h = mix(h, u64::from_le_bytes(b[..8].try_into().unwrap()));
        h = mix(h, u64::from_le_bytes(b[8..].try_into().unwrap()));
    }
    if !s.nowrite.contains(&name) {
        for (k, a) in args.iter().enumerate() {
            if let DataValue::I64(p) = a {
                let p = *p as u64;
                let writable = s
                    .find(p, 8)
                    .is_some_and(|(base, r)| r.stub_writable && base < STACK_BASE);
                if writable {
                    let v = mix(h, k as u64).to_le_bytes();
                    s.bytes_mut(p, 8).unwrap().copy_from_slice(&v);
                }
            }
        }
    }
    s.calls.push((name, args));
    let mut rets = Vec::new();
    for (k, r) in f.dfg.signatures[sig].returns.iter().enumerate() {
        let mut rng = Rng(mix(h, 1000 + k as u64));
        let v = if r.value_type == types::I64 && rng.below(2) == 0 {
            DataValue::I64(
                (HEAP_BASE + rng.below(HEAPS) * REGION_STRIDE + PTR_SLACK + rng.below(8) * 8)
                    as i64,
            )
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
                s.bytes_mut(d, n).ok_or_else(oob)?.copy_from_slice(&tmp);
            }
            Ok(rets.iter().map(|_| DataValue::I64(d as i64)).collect())
        }
        LibCall::Memset => {
            let (d, c, n) = (int(&a[0]), int(&a[1]), int(&a[2]));
            if n > 0 {
                s.bytes_mut(d, n).ok_or_else(oob)?.fill(c as u8);
            }
            Ok(rets.iter().map(|_| DataValue::I64(d as i64)).collect())
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

/// The mode for function `fname`, honoring `PLIRON_DIFFCHECK_FN=<substr>`.
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
    let safe: String = fname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
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
pub struct OwnedFacts {
    pub nonnull: FxHashSet<Value>,
    pub noalias: FxHashSet<Value>,
    pub deref: FxHashMap<Value, (u64, bool)>,
    pub frozen: FxHashMap<Value, u64>,
    pub nowrite: FxHashMap<FuncRef, bool>,
    pub self_name: Option<String>,
    pub derived: FxHashMap<Value, Value>,
}

impl OwnedFacts {
    pub fn facts(&self) -> Facts<'_> {
        Facts {
            nonnull: Some(&self.nonnull),
            noalias: Some(&self.noalias),
            deref: Some(&self.deref),
            frozen: Some(&self.frozen),
            nowrite: Some(&self.nowrite),
            self_name: self.self_name.clone(),
            derived: Some(&self.derived),
        }
    }

    /// Parse the `; diffcheck-facts:` line `facts_header` writes; absent
    /// means no facts.
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
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
        " nonnull={nonnull} noalias={noalias} deref={deref} frozen={frozen} nowrite={nowrite} derived={derived}"
    );
    if let Some(n) = &facts.self_name {
        let _ = write!(s, " self={n}");
    }
    s.push('\n');
    s
}
