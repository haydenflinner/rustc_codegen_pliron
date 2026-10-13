//! Jump threading on the final Cranelift IR (`PLIRON_JUMPTHREAD`).
//!
//! After SROA, Rust's `Option`/`bool`-returning helpers become merges whose
//! incoming values are often constants (`None` = null, `true`/`false`), and the
//! merge block immediately tests them: `jump b(0)` … `b(v): brif v, …`.
//! Cranelift has no jump threading, so each such edge pays a materialised
//! constant, a jump and a test. Here a predecessor edge whose block arguments
//! decide the merge block's branch is retargeted straight to the taken
//! successor. Only merge blocks made of a few pure integer ops are threaded,
//! and only when no use of the merge block's values is reachable from the new
//! target without passing through the merge block, so dominance still holds.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, ExceptionTableItem, Function, Inst, InstBuilder, InstructionData,
    Opcode, ProgramPoint, Type, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_BODY: usize = 12;

fn mask(v: u64, bits: u32) -> u64 {
    if bits >= 64 {
        v
    } else {
        v & ((1u64 << bits) - 1)
    }
}

fn sext(v: u64, bits: u32) -> i64 {
    if bits >= 64 {
        v as i64
    } else {
        let s = 64 - bits;
        ((v << s) as i64) >> s
    }
}

fn bits(func: &Function, v: Value) -> Option<u32> {
    let ty = func.dfg.value_type(v);
    (ty.is_int() && ty.bits() <= 64).then(|| ty.bits())
}

/// Constant value of `v`: from `env`, or an `iconst` anywhere in the function.
pub fn known(func: &Function, env: &FxHashMap<Value, u64>, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    if let Some(&c) = env.get(&v) {
        return Some(c);
    }
    let ValueDef::Result(inst, 0) = func.dfg.value_def(v) else {
        return None;
    };
    match func.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(mask(imm.bits() as u64, bits(func, v)?)),
        _ => None,
    }
}

fn icmp(cc: IntCC, a: u64, b: u64, w: u32) -> bool {
    let (sa, sb) = (sext(a, w), sext(b, w));
    match cc {
        IntCC::Equal => a == b,
        IntCC::NotEqual => a != b,
        IntCC::SignedLessThan => sa < sb,
        IntCC::SignedGreaterThanOrEqual => sa >= sb,
        IntCC::SignedGreaterThan => sa > sb,
        IntCC::SignedLessThanOrEqual => sa <= sb,
        IntCC::UnsignedLessThan => a < b,
        IntCC::UnsignedGreaterThanOrEqual => a >= b,
        IntCC::UnsignedGreaterThan => a > b,
        IntCC::UnsignedLessThanOrEqual => a <= b,
    }
}

/// Evaluate one pure body instruction of a merge block.
pub fn eval(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    nz: &dyn Fn(Value) -> bool,
    rg: &dyn Fn(Value) -> Option<(u64, u64)>,
    inst: Inst,
) -> Option<u64> {
    static ARITH: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| crate::pass_enabled("PLIRON_EVAL_ARITH"));
    let res = func.dfg.first_result(inst);
    let w = bits(func, res)?;
    let r = match func.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => imm.bits() as u64,
        InstructionData::IntCompare { cond, args, .. } => {
            let aw = bits(func, args[0])?;
            let ranged = range_cmp(
                cond,
                known(func, env, args[0]),
                known(func, env, args[1]),
                rg(args[0]),
                rg(args[1]),
                aw,
            );
            match (known(func, env, args[0]), known(func, env, args[1])) {
                _ if ranged.is_some() => u64::from(ranged == Some(true)),
                (Some(a), Some(b)) => icmp(cond, a, b, aw) as u64,
                (Some(0), None) | (None, Some(0)) => {
                    let x = if known(func, env, args[0]).is_some() {
                        args[1]
                    } else {
                        args[0]
                    };
                    match (cond, nz(x)) {
                        (IntCC::Equal, true) => 0,
                        (IntCC::NotEqual, true) => 1,
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        InstructionData::Unary { opcode, arg } => {
            let aw = bits(func, arg)?;
            let a = known(func, env, arg)?;
            match opcode {
                Opcode::Uextend | Opcode::Ireduce => a,
                Opcode::Sextend => sext(a, aw) as u64,
                Opcode::Bnot => !a,
                Opcode::Ineg => a.wrapping_neg(),
                _ => return None,
            }
        }
        InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } => {
            let c = known(func, env, args[0])?;
            known(func, env, args[if c != 0 { 1 } else { 2 }])?
        }
        InstructionData::Binary { opcode, args } => {
            let (a, b) = (known(func, env, args[0])?, known(func, env, args[1])?);
            match opcode {
                Opcode::Band => a & b,
                Opcode::Bor => a | b,
                Opcode::Bxor => a ^ b,
                Opcode::Iadd => a.wrapping_add(b),
                Opcode::Isub => a.wrapping_sub(b),
                Opcode::Imul if *ARITH => a.wrapping_mul(b),
                Opcode::Udiv if *ARITH && b != 0 => a / b,
                Opcode::Urem if *ARITH && b != 0 => a % b,
                Opcode::Ishl if *ARITH => a << (b & u64::from(w - 1)),
                Opcode::Ushr if *ARITH => a >> (b & u64::from(w - 1)),
                Opcode::Sshr if *ARITH => (sext(a, w) >> (b & u64::from(w - 1))) as u64,
                Opcode::Rotl | Opcode::Rotr if *ARITH => {
                    let s = (b & u64::from(w - 1)) as u32;
                    let (l, r) = if opcode == Opcode::Rotl {
                        (s, w - s)
                    } else {
                        (w - s, s)
                    };
                    if s == 0 { a } else { (a << l) | (a >> r) }
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(mask(r, w))
}

type KBits = FxHashMap<Value, (u64, u64)>;

/// Known `(zero, one)` bit masks of `v` from constants, `env`, `kenv` and
/// bitwise ops / constant shifts / extends on them.
fn kbits(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    kenv: &KBits,
    v: Value,
    depth: u32,
) -> (u64, u64) {
    let v = func.dfg.resolve_aliases(v);
    let Some(w) = bits(func, v) else {
        return (0, 0);
    };
    let full = mask(u64::MAX, w);
    if let Some(c) = known(func, env, v) {
        return (!c & full, c);
    }
    if let Some(&k) = kenv.get(&v) {
        return k;
    }
    let Some(i) = func.dfg.value_def(v).inst() else {
        return (0, 0);
    };
    if depth >= 6 {
        return (0, 0);
    }
    let sub = |x: Value| kbits(func, env, kenv, x, depth + 1);
    let op = |opcode: Opcode, a: (u64, u64), b: (u64, u64)| -> (u64, u64) {
        let s = (b.0 | b.1 == full)
            .then_some(b.1)
            .filter(|&s| s < u64::from(w));
        match (opcode, s) {
            (Opcode::Band, _) => (a.0 | b.0, a.1 & b.1),
            (Opcode::Bor, _) => (a.0 & b.0, a.1 | b.1),
            (Opcode::Bxor, _) => ((a.0 & b.0) | (a.1 & b.1), (a.0 & b.1) | (a.1 & b.0)),
            (Opcode::Ishl, Some(s)) => ((a.0 << s) | ((1u64 << s) - 1), a.1 << s),
            (Opcode::Ushr, Some(s)) => ((a.0 >> s) | !(full >> s), a.1 >> s),
            _ => (0, 0),
        }
    };
    let r = match func.dfg.insts[i] {
        InstructionData::Binary { opcode, args } => op(opcode, sub(args[0]), sub(args[1])),
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => {
            let a = sub(arg);
            let aw = bits(func, arg).unwrap_or(w);
            (a.0 | (full & !mask(u64::MAX, aw)), a.1)
        }
        InstructionData::Unary {
            opcode: Opcode::Ireduce,
            arg,
        } => sub(arg),
        // Zero-extending loads: the upper bits are known zero.
        InstructionData::Load { opcode, .. } => match opcode {
            Opcode::Uload8 => (full & !0xFFu64, 0),
            Opcode::Uload16 => (full & !0xFFFFu64, 0),
            Opcode::Uload32 => (full & !0xFFFF_FFFFu64, 0),
            _ => (0, 0),
        },
        _ => (0, 0),
    };
    (r.0 & full, r.1 & full)
}

/// Constant result of body instruction `i` from known bits: fully known
/// values, and `x ==/!= k` where a known bit of `x` differs from `k`.
fn kfold(func: &Function, env: &FxHashMap<Value, u64>, kenv: &KBits, i: Inst) -> Option<u64> {
    if kenv.is_empty() {
        return None;
    }
    if let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] {
        if !matches!(cond, IntCC::Equal | IntCC::NotEqual) {
            return None;
        }
        let (x, k) = match (known(func, env, args[0]), known(func, env, args[1])) {
            (None, Some(k)) => (args[0], k),
            (Some(k), None) => (args[1], k),
            _ => return None,
        };
        let (z, o) = kbits(func, env, kenv, x, 0);
        return ((o & !k) | (z & k) != 0).then_some(u64::from(cond == IntCC::NotEqual));
    }
    let r = func.dfg.first_result(i);
    let full = mask(u64::MAX, bits(func, r)?);
    let (z, o) = kbits(func, env, kenv, r, 0);
    (z | o == full).then_some(o)
}

pub fn pure_op(func: &Function, inst: Inst) -> bool {
    use Opcode::*;
    matches!(
        func.dfg.insts[inst].opcode(),
        Iconst
            | Icmp
            | Uextend
            | Sextend
            | Ireduce
            | Bnot
            | Band
            | Bor
            | Bxor
            | Iadd
            | Isub
            | Select
            | Ishl
            | Ushr
            | StackAddr
    ) && func.dfg.inst_results(inst).len() == 1
}

/// For each block, the other blocks that use one of its values (params or results).
fn users(func: &Function) -> FxHashMap<Block, FxHashSet<Block>> {
    let mut out: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
    let def_block = |v: Value| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    };
    for b in func.layout.blocks() {
        for inst in func.layout.block_insts(b) {
            let mut note = |v: Value| {
                if let Some(d) = def_block(v)
                    && d != b
                {
                    out.entry(d).or_default().insert(b);
                }
            };
            for &v in func.dfg.inst_args(inst) {
                note(v);
            }
            for bc in func.dfg.insts[inst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        note(v);
                    }
                }
            }
        }
    }
    out
}

const MAX_WALK: usize = 4096;

/// Values known non-null: `!nonnull` loads, inbounds offsets of non-null
/// pointers, and block params whose every incoming argument is non-null
/// (greatest fixpoint).
pub struct NonNull<'a> {
    loads: &'a FxHashSet<Value>,
    derived: &'a FxHashMap<Value, Value>,
    params: FxHashSet<Value>,
}

impl<'a> NonNull<'a> {
    pub fn new(
        func: &Function,
        loads: &'a FxHashSet<Value>,
        derived: &'a FxHashMap<Value, Value>,
    ) -> Self {
        let mut nn = NonNull {
            loads,
            derived,
            params: FxHashSet::default(),
        };
        if loads.is_empty() {
            return nn;
        }
        let entry = func.layout.entry_block();
        for b in func.layout.blocks() {
            if Some(b) != entry {
                nn.params.extend(
                    func.dfg
                        .block_params(b)
                        .iter()
                        .copied()
                        .filter(|&p| func.dfg.value_type(p) == types::I64),
                );
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for b in func.layout.blocks() {
                let Some(t) = func.layout.last_inst(b) else {
                    continue;
                };
                for bc in func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    let d = bc.block(&func.dfg.value_lists);
                    let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                    for (i, &p) in func.dfg.block_params(d).iter().enumerate() {
                        if nn.params.contains(&p)
                            && !matches!(args.get(i), Some(&BlockArg::Value(v)) if nn.is(func, v))
                        {
                            nn.params.remove(&p);
                            changed = true;
                        }
                    }
                }
            }
        }
        nn
    }

    pub fn is(&self, func: &Function, v: Value) -> bool {
        // `select c, a, b` is non-null iff both arms are; every term on
        // the worklist must justify itself (a `!nonnull` load, a param
        // the fixpoint kept, a symbol/stack address, or an `inbounds`
        // offset of a non-null base).
        let mut work = vec![func.dfg.resolve_aliases(v)];
        let mut seen: FxHashSet<Value> = FxHashSet::default();
        'terms: while let Some(mut v) = work.pop() {
            if !seen.insert(v) {
                continue;
            }
            for _ in 0..8 {
                if self.loads.contains(&v) || self.params.contains(&v) || addr_of_symbol(func, v) {
                    continue 'terms;
                }
                if let Some(i) = func.dfg.value_def(v).inst()
                    && let InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args,
                    } = func.dfg.insts[i]
                {
                    work.push(func.dfg.resolve_aliases(args[1]));
                    work.push(func.dfg.resolve_aliases(args[2]));
                    continue 'terms;
                }
                match self.derived.get(&v) {
                    // The map outlives the passes that fill it — a target
                    // deleted since is stored as a dangling alias, so chase
                    // only values that are still real; else unprovable.
                    Some(&b) if func.dfg.value_is_real(b) => {
                        v = b;
                        continue;
                    }
                    _ => return false,
                }
            }
            // Derived chain too deep to prove.
            return false;
        }
        true
    }
}

/// Addresses of symbols, thread-locals and stack slots are never null.
fn addr_of_symbol(func: &Function, v: Value) -> bool {
    func.dfg.value_def(v).inst().is_some_and(|i| {
        matches!(
            func.dfg.insts[i].opcode(),
            Opcode::TlsValue | Opcode::SymbolValue | Opcode::FuncAddr | Opcode::StackAddr
        ) && crate::pass_enabled("PLIRON_NN_ADDR")
    })
}

/// Trace the `src → dst` edge at destination index `di` back through
/// single-predecessor `jump` chains to the `brif` whose arm selects it.
/// Returns `(guard_block, condition, taken_when_true)`.
fn edge_guard(
    func: &Function,
    cfg: &ControlFlowGraph,
    mut src: Block,
    mut di: usize,
) -> Option<(Block, Value, bool)> {
    for _ in 0..64 {
        let t = func.layout.last_inst(src)?;
        match func.dfg.insts[t] {
            InstructionData::Brif { arg: c, .. } => return Some((src, c, di == 0)),
            InstructionData::Jump { .. } => {
                // `src` falls through unconditionally; its single
                // predecessor's terminator holds the guard.
                let mut preds = cfg.pred_iter(src);
                let p = preds.next()?;
                if preds.next().is_some() {
                    return None;
                }
                let dests = func.dfg.insts[p.inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                // If `src` is reachable on more than one arm the edge
                // carries no arm information.
                let mut hit = dests
                    .iter()
                    .enumerate()
                    .filter(|(_, bc)| bc.block(&func.dfg.value_lists) == src)
                    .map(|(i, _)| i);
                di = hit.next()?;
                if hit.next().is_some() {
                    return None;
                }
                src = p.block;
            }
            _ => return None,
        }
    }
    None
}

/// `v` is provably `!= k` (`k` masked to `v`'s width): constants,
/// zero-extend loads, known bits (`band`/`bor`/shifts), selects whose
/// arms both qualify, or a block param in `cand` (the greatest-fixpoint
/// set `ne_params` computes — iterator decode pipelines route values
/// through cycles of params like `Chars`' forwarder merges, which a
/// plain recursion can't prove).
fn ne_const(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    cand: &FxHashSet<Value>,
    v: Value,
    k: u64,
    depth: u32,
) -> bool {
    if depth >= 8 {
        return false;
    }
    let v = func.dfg.resolve_aliases(v);
    let Some(w) = bits(func, v) else {
        return false;
    };
    let k = mask(k, w);
    if let Some(c) = known(func, env, v) {
        return c != k;
    }
    match func.dfg.value_def(v) {
        ValueDef::Param(..) => cand.contains(&v),
        ValueDef::Result(i, _) => match func.dfg.insts[i].opcode() {
            // Zero-extend loads clear every bit above the load width.
            Opcode::Uload8 => k >> 8 != 0,
            Opcode::Uload16 => k >> 16 != 0,
            Opcode::Uload32 => k >> 32 != 0,
            Opcode::Select => {
                let InstructionData::Ternary { args: sa, .. } = func.dfg.insts[i] else {
                    return false;
                };
                ne_const(func, env, cand, sa[1], k, depth + 1)
                    && ne_const(func, env, cand, sa[2], k, depth + 1)
            }
            _ => {
                let kenv = FxHashMap::default();
                let (z, o) = kbits(func, env, &kenv, v, 0);
                ((o & !k) | (z & k)) != 0
            }
        },
        _ => false,
    }
}

/// Greatest fixpoint: block params whose every value is provably `!= k`
/// (`k` masked per param width). Every param starts a candidate; one is
/// dropped when a non-self incoming arg can't be shown `!= k` (self edges
/// preserve the value, adding no candidates — cycles of provable params
/// survive the fixpoint).
fn ne_params(
    func: &Function,
    cfg: &ControlFlowGraph,
    env: &FxHashMap<Value, u64>,
    k: u64,
) -> FxHashSet<Value> {
    // Edges out of unreachable blocks contribute no runtime values; the
    // pass runs before dead-block sweeps, so count only reachable preds.
    let mut reach: FxHashSet<Block> = FxHashSet::default();
    let mut work = Vec::new();
    if let Some(e) = func.layout.entry_block() {
        reach.insert(e);
        work.push(e);
    }
    while let Some(b) = work.pop() {
        for s in cfg.succ_iter(b) {
            if reach.insert(s) {
                work.push(s);
            }
        }
    }
    let mut args_of: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    let mut cand: FxHashSet<Value> = FxHashSet::default();
    for b in func.layout.blocks() {
        if !reach.contains(&b) {
            continue;
        }
        for (pidx, &p) in func.dfg.block_params(b).iter().enumerate() {
            if bits(func, p).is_none() {
                continue;
            }
            let mut args = Vec::new();
            let mut ok = true;
            for pred in cfg.pred_iter(b) {
                if !reach.contains(&pred.block) {
                    continue;
                }
                let dests = func.dfg.insts[pred.inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                for bc in dests {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    match bc.args(&func.dfg.value_lists).nth(pidx) {
                        Some(BlockArg::Value(a)) => {
                            let a = func.dfg.resolve_aliases(a);
                            if a != p {
                                args.push(a);
                            }
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    break;
                }
            }
            // No non-self args (entry params, pure self-loops): nothing
            // proves `!= k`.
            if ok && !args.is_empty() {
                args_of.insert(p, args);
                cand.insert(p);
            }
        }
    }
    loop {
        let mut drop = Vec::new();
        for (&p, args) in &args_of {
            if cand.contains(&p) && args.iter().any(|&a| !ne_const(func, env, &cand, a, k, 0)) {
                drop.push(p);
            }
        }
        if drop.is_empty() {
            break;
        }
        for p in drop {
            cand.remove(&p);
        }
    }
    cand
}

/// `icmp eq/ne x, 0` with `x` known non-null → constant. Also the
/// `Option<NonNull>`-niche tests `icmp eq/ne (select c, 0, X), 0` (a
/// select is null exactly when `c` picks the zero arm) and the block-param
/// equivalent (a param is null exactly on zero-arg edges; when all edges
/// funnel through one `brif`, the test reduces to that condition) — the
/// shapes `slice::Iter::next()`/`Chars` leave after Option lowering.
/// The same select shape with a non-zero sentinel (`icmp eq/ne
/// (select c, X, K), K` with `X` provably `!= K`, e.g. `Option<char>`'s
/// `-1`) reduces to `!c`.
fn fold_null_tests(func: &mut Function, nn: &NonNull) -> usize {
    let none = FxHashMap::default();
    enum Fold {
        Const(i64),
        Alias(Value),
        Neg(Value),
        NN(Value),
    }
    // `brif`/`select` conditions are only guaranteed nonzero-vs-zero;
    // an `icmp` result is canonically 0/1, so aliasing `c` directly is
    // only safe when `c` itself is a canonical boolean.
    let booly = |func: &Function, v: Value| {
        func.dfg
            .value_def(v)
            .inst()
            .is_some_and(|i| matches!(func.dfg.insts[i].opcode(), Opcode::Icmp | Opcode::Fcmp))
    };
    let mut hits = Vec::new();
    let mut cfg_lazy: Option<ControlFlowGraph> = None;
    let mut dt_lazy: Option<DominatorTree> = None;
    // `ne_params` sets per sentinel constant, built on first use.
    let mut ne_sets: FxHashMap<u64, FxHashSet<Value>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for inst in func.layout.block_insts(b) {
            let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[inst] else {
                continue;
            };
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual)
                || func.dfg.value_type(args[0]).is_vector()
            {
                continue;
            }
            let ne = cond == IntCC::NotEqual;
            let (x, k) = match (known(func, &none, args[0]), known(func, &none, args[1])) {
                (Some(k), None) => (args[1], k),
                (None, Some(k)) => (args[0], k),
                _ => continue,
            };
            let Some(w) = bits(func, x) else {
                continue;
            };
            let k = mask(k, w);
            if k != 0 {
                if !crate::pass_enabled("PLIRON_SENTINEL") {
                    continue;
                }
                // `select c, X, K == K` iff `!c` when `X != K`;
                // `select c, K, X` mirrors. The sentinel test of a
                // niche-encoded enum (`Option<char>`'s `-1`).
                let x = func.dfg.resolve_aliases(x);
                let Some(d) = func.dfg.value_def(x).inst() else {
                    continue;
                };
                let InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args: sa,
                } = func.dfg.insts[d]
                else {
                    continue;
                };
                let cfg = cfg_lazy.get_or_insert_with(|| ControlFlowGraph::with_function(func));
                let cand = ne_sets
                    .entry(k)
                    .or_insert_with(|| ne_params(func, cfg, &none, k));
                let eq_is_c = if known(func, &none, sa[2]) == Some(k)
                    && ne_const(func, &none, cand, sa[1], k, 0)
                {
                    false
                } else if known(func, &none, sa[1]) == Some(k)
                    && ne_const(func, &none, cand, sa[2], k, 0)
                {
                    true
                } else {
                    continue;
                };
                let c = sa[0];
                if func.dfg.value_type(c) != func.dfg.value_type(func.dfg.first_result(inst)) {
                    continue;
                }
                // `x == K` ⇔ `eq_is_c ? c : !c`; `ne` inverts.
                let f = match (ne, eq_is_c) {
                    (false, true) | (true, false) if booly(func, c) => Fold::Alias(c),
                    (false, true) | (true, false) => Fold::NN(c),
                    _ => Fold::Neg(c),
                };
                hits.push((inst, f));
                continue;
            }
            if nn.is(func, x) {
                hits.push((inst, Fold::Const((cond == IntCC::NotEqual) as i64)));
                continue;
            }
            let x = func.dfg.resolve_aliases(x);
            // A block param is the other form Rust's niche layout takes
            // after SROA: `p` merges the `Some` pointer with the `None`
            // zero. `p == 0` iff a zero edge was taken; when every edge
            // funnels through the same `brif c`, the test is that
            // condition itself.
            if crate::pass_enabled("PLIRON_PARAMNULL")
                && let ValueDef::Param(d, pidx) = func.dfg.value_def(x)
            {
                let cfg = cfg_lazy.get_or_insert_with(|| ControlFlowGraph::with_function(func));
                let mut edges: Vec<(bool, bool)> = Vec::new();
                let mut guard: Option<(Block, Value)> = None;
                let mut ok = true;
                for pred in cfg.pred_iter(d) {
                    let dests = func.dfg.insts[pred.inst]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                    for (di, bc) in dests.iter().enumerate() {
                        if bc.block(&func.dfg.value_lists) != d {
                            continue;
                        }
                        let Some(BlockArg::Value(arg)) = bc.args(&func.dfg.value_lists).nth(pidx)
                        else {
                            ok = false;
                            break;
                        };
                        let Some((gb, c, arm)) = edge_guard(func, cfg, pred.block, di) else {
                            ok = false;
                            break;
                        };
                        match guard {
                            None => guard = Some((gb, c)),
                            Some((pb, pc)) if pb == gb && pc == c => {}
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                        let arg = func.dfg.resolve_aliases(arg);
                        match known(func, &none, arg) {
                            Some(0) => edges.push((arm, true)),
                            Some(_) => edges.push((arm, false)),
                            None if nn.is(func, arg) => edges.push((arm, false)),
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        break;
                    }
                }
                if !ok {
                    continue;
                }
                let (mut zarm, mut saw_x) = (None, false);
                for &(arm, is_z) in &edges {
                    if is_z {
                        match zarm {
                            None => zarm = Some(arm),
                            Some(z) if z == arm => {}
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    } else {
                        saw_x = true;
                    }
                }
                if !ok {
                    continue;
                }
                let Some(az) = zarm else {
                    // Every edge passes a non-null value — only meaningful
                    // with at least one edge (an entry param has none and
                    // would fold vacuously).
                    if !saw_x {
                        continue;
                    }
                    hits.push((inst, Fold::Const(ne as i64)));
                    continue;
                };
                if !saw_x || edges.iter().any(|&(a, z)| !z && a == az) {
                    continue;
                }
                let Some((_, c)) = guard else {
                    continue;
                };
                if func.dfg.value_type(c) != func.dfg.value_type(func.dfg.first_result(inst)) {
                    continue;
                }
                // `c` must dominate the test: every path to the merge
                // block passes the guard, so it does — except a guard
                // inside the merge block itself (self-edge), where the
                // compare could precede `c`'s def.
                let dt = dt_lazy.get_or_insert_with(|| DominatorTree::with_function(func, cfg));
                let pp = match func.dfg.value_def(c) {
                    ValueDef::Result(ci, _) => ProgramPoint::Inst(ci),
                    ValueDef::Param(cb, _) => ProgramPoint::Block(cb),
                    _ => continue,
                };
                if !dt.dominates(pp, inst, &func.layout) {
                    continue;
                }
                // `p == 0` ⇔ arm `az` taken ⇔ (az ? c : !c); `ne` inverts.
                let f = match (ne, az) {
                    (false, true) | (true, false) if booly(func, c) => Fold::Alias(c),
                    (false, true) | (true, false) => Fold::NN(c),
                    _ => Fold::Neg(c),
                };
                hits.push((inst, f));
                continue;
            }
            let Some(d) = func.dfg.value_def(x).inst() else {
                continue;
            };
            let InstructionData::Ternary {
                opcode: Opcode::Select,
                args: sa,
            } = func.dfg.insts[d]
            else {
                continue;
            };
            // `select c, 0, X == 0` iff `c` (X ≠ 0); `select c, X, 0` mirrors.
            let t0 = known(func, &none, sa[1]) == Some(0) && nn.is(func, sa[2]);
            let t1 = known(func, &none, sa[2]) == Some(0) && nn.is(func, sa[1]);
            let c = sa[0];
            let f = match (t0, t1, ne) {
                (true, _, false) | (false, true, true) if booly(func, c) => Fold::Alias(c),
                (true, _, false) | (false, true, true) => Fold::NN(c),
                (true, _, true) | (false, true, false) => Fold::Neg(c),
                _ => continue,
            };
            hits.push((inst, f));
        }
    }
    let n = hits.len();
    for (inst, f) in hits {
        match f {
            Fold::Const(k) => {
                let ty = func.dfg.value_type(func.dfg.first_result(inst));
                func.replace(inst).iconst(ty, k);
            }
            Fold::Alias(c) => {
                let res = func.dfg.first_result(inst);
                func.dfg.clear_results(inst);
                func.layout.remove_inst(inst);
                func.dfg.change_to_alias(res, c);
            }
            Fold::Neg(c) => {
                func.replace(inst).icmp_imm_s(IntCC::Equal, c, 0);
            }
            Fold::NN(c) => {
                func.replace(inst).icmp_imm_s(IntCC::NotEqual, c, 0);
            }
        }
    }
    n
}

enum Arg {
    V(Value),
    K(u64, Type),
    /// Clone of a merge-block body inst, args already rewritten to `Arg`s
    /// resolved in the pred — materialized before `pinst` at apply time.
    R(Inst, Vec<Arg>),
}

/// Resolve `v` (a merge-block value) to something usable on the threaded
/// edge: an incoming edge arg, a known constant, a cloneable pure body
/// inst (bounded depth), or the value itself if its def dominates `pinst`.
fn map_val(
    func: &Function,
    argv: &FxHashMap<Value, Value>,
    body_vals: &FxHashSet<Value>,
    env: &FxHashMap<Value, u64>,
    dom: &DominatorTree,
    pinst: Inst,
    v: Value,
    out: &mut Vec<Arg>,
    depth: usize,
) -> bool {
    if let Some(&pv) = argv.get(&v) {
        out.push(Arg::V(pv));
        return true;
    }
    if !body_vals.contains(&v) {
        // An external value is a legal edge arg only if its def
        // dominates the threaded branch — dominating `b` is not enough
        // (`pinst` may have arms that never reach `b`'s dominator).
        let pp = match func.dfg.value_def(v) {
            ValueDef::Result(di, _) => ProgramPoint::Inst(di),
            ValueDef::Param(db, _) => ProgramPoint::Block(db),
            _ => return false,
        };
        if !dom.dominates(pp, pinst, &func.layout) {
            return false;
        }
        out.push(Arg::V(v));
        return true;
    }
    // `env` tracks 64 bits; it can't describe a wider param (and `iconst`
    // tops out at i64).
    if let Some(&k) = env.get(&v) {
        if func.dfg.value_type(v).bits() <= 64 {
            out.push(Arg::K(k, func.dfg.value_type(v)));
            return true;
        }
        return false;
    }
    if depth == 0 {
        return false;
    }
    let Some(di) = func.dfg.value_def(v).inst() else {
        return false;
    };
    if !pure_op(func, di) || func.dfg.inst_results(di).len() != 1 {
        return false;
    }
    let args: Vec<Value> = func.dfg.inst_args(di).to_vec();
    let mut subs = Vec::with_capacity(args.len());
    for a in args {
        let a = func.dfg.resolve_aliases(a);
        let mut tmp = Vec::with_capacity(1);
        if !map_val(func, argv, body_vals, env, dom, pinst, a, &mut tmp, depth - 1) {
            return false;
        }
        subs.push(tmp.pop().unwrap());
    }
    out.push(Arg::R(di, subs));
    true
}

/// Emit an `Arg` before `pinst`, cloning remat insts in order.
fn emit_arg(func: &mut Function, pinst: Inst, a: &Arg) -> Value {
    match a {
        Arg::V(v) => *v,
        Arg::K(k, ty) => FuncCursor::new(func).at_inst(pinst).ins().iconst(*ty, *k as i64),
        Arg::R(di, subs) => {
            let vals: Vec<Value> = subs.iter().map(|s| emit_arg(func, pinst, s)).collect();
            let ni = func.dfg.clone_inst(*di);
            func.dfg.overwrite_inst_values(ni, vals.into_iter());
            func.layout.insert_inst(ni, pinst);
            func.dfg.first_result(ni)
        }
    }
}

/// A retargeted edge `pinst`'s destination `di` → `s`. With `repair`, the values
/// of the bypassed merge block that are used from `s` on become new params of
/// `s` (fed by the merge block's own branch `term`/`ti` and by the new edge).
struct Edit {
    pinst: Inst,
    di: usize,
    s: Block,
    args: Vec<Arg>,
    /// (merge term, arm, needed exports, region, other edges into `s`
    /// that must also feed the new params — (pred inst, dest idx)).
    repair: Option<(Inst, usize, Vec<Value>, Vec<Block>, Vec<(Inst, usize)>)>,
    /// Resolved args for each `repair` extra edge, filled in by the
    /// post-edit dominance validation (parallel to the `extra` list).
    eargs: Vec<Vec<Arg>>,
}

/// Blocks reachable from the entry without passing `defb`, in the graph
/// where each planned edit's `pinst -> s` edge already exists (the old
/// `pinst -> b` edges are kept too — an over-approximation that can only
/// reject, never accept wrongly).
fn reach_avoid(
    func: &Function,
    cfg: &ControlFlowGraph,
    new_succs: &FxHashMap<Block, Vec<Block>>,
    defb: Block,
) -> FxHashSet<Block> {
    let mut seen = FxHashSet::default();
    let Some(entry) = func.layout.entry_block() else {
        return seen;
    };
    if entry == defb {
        return seen;
    }
    seen.insert(entry);
    let mut work = vec![entry];
    while let Some(x) = work.pop() {
        let extra = new_succs.get(&x).into_iter().flatten().copied();
        for nx in cfg.succ_iter(x).chain(extra) {
            if nx != defb && seen.insert(nx) {
                work.push(nx);
            }
        }
    }
    seen
}

/// Constants a block's body computes without any incoming-edge context
/// (`iconst`s and pure folds of them). Those are the only merge values
/// safe to rematerialize on a repair edge when the original def stops
/// dominating it — a fresh `iconst` dominates every use by construction.
fn const_env(func: &Function, b: Block) -> FxHashMap<Value, u64> {
    let mut env = FxHashMap::default();
    let nz = |_: Value| false;
    let rg = |_: Value| None;
    for i in func.layout.block_insts(b) {
        if !pure_op(func, i) {
            continue;
        }
        if let Some(c) = eval(func, &env, &nz, &rg, i) {
            env.insert(func.dfg.first_result(i), c);
        }
    }
    env
}

/// Blocks reachable from `s` without entering `b`, or None past MAX_WALK.
fn region(cfg: &ControlFlowGraph, b: Block, s: Block) -> Option<Vec<Block>> {
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut order = Vec::new();
    let mut work = vec![s];
    while let Some(x) = work.pop() {
        if x == b || !seen.insert(x) {
            continue;
        }
        order.push(x);
        if order.len() > MAX_WALK {
            return None;
        }
        work.extend(cfg.succ_iter(x));
    }
    Some(order)
}

/// Thread one round; returns the number of edges retargeted.
pub(crate) fn round(func: &mut Function, nn: &NonNull, domcond: bool) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let users = users(func);
    let facts = if domcond {
        edge_facts(func, &cfg)
    } else {
        FxHashMap::default()
    };
    let entry = func.layout.entry_block();
    let kb_on = crate::pass_enabled("PLIRON_KBITS");
    let range2 = crate::pass_enabled("PLIRON_RANGE2");
    let dbg = std::env::var_os("PLIRON_JT_DEBUG").is_some();
    let def_block = |v: Value| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    };
    let mut edits: Vec<Edit> = Vec::new();
    let mut touched: FxHashSet<Inst> = FxHashSet::default();
    // Blocks taking part in any edit, and blocks no later edit may touch.
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut locked: FxHashSet<Block> = FxHashSet::default();
    for b in func.layout.blocks() {
        if Some(b) == entry || locked.contains(&b) {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        let Some((&term, body)) = insts.split_last() else {
            continue;
        };
        // `iconst`s cost nothing to evaluate through the env map; budget
        // the real ops only (long niche-merge chains — `Option<char>`
        // flag test + sentinel compare — otherwise exceed MAX_BODY).
        let real = if crate::pass_enabled("PLIRON_BODYK") {
            body.iter()
                .filter(|&&i| {
                    !matches!(
                        func.dfg.insts[i],
                        InstructionData::UnaryImm {
                            opcode: Opcode::Iconst,
                            ..
                        }
                    )
                })
                .count()
        } else {
            body.len()
        };
        if real > MAX_BODY || !body.iter().all(|&i| pure_op(func, i)) {
            continue;
        }
        let InstructionData::Brif {
            arg: cond,
            blocks: targets,
            ..
        } = func.dfg.insts[term]
        else {
            continue;
        };
        let params = func.dfg.block_params(b).to_vec();
        let body_vals: FxHashSet<Value> = body.iter().map(|&i| func.dfg.first_result(i)).collect();
        let b_users = users.get(&b);
        for pred in cfg.pred_iter(b) {
            let (pinst, pblock) = (pred.inst, pred.block);
            if pblock == b || touched.contains(&pinst) || locked.contains(&pblock) {
                continue;
            }
            if !matches!(func.dfg.insts[pinst].opcode(), Opcode::Jump | Opcode::Brif) {
                continue;
            }
            let dests = func.dfg.insts[pinst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            for (di, bc) in dests.iter().enumerate() {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                if args.len() != params.len() {
                    continue;
                }
                let mut env: FxHashMap<Value, u64> = FxHashMap::default();
                let mut argv: FxHashMap<Value, Value> = FxHashMap::default();
                let mut pn: FxHashSet<Value> = FxHashSet::default();
                let mut ok = true;
                for (&p, a) in params.iter().zip(&args) {
                    let BlockArg::Value(v) = *a else {
                        ok = false;
                        break;
                    };
                    let v = func.dfg.resolve_aliases(v);
                    argv.insert(p, v);
                    if let Some(c) = known(func, &env, v) {
                        env.insert(p, c);
                    } else if nn.is(func, v) {
                        pn.insert(p);
                    }
                }
                if !ok {
                    continue;
                }
                // Known bits of non-constant incoming args (e.g. a packed `Option` tag).
                let mut kenv: KBits = FxHashMap::default();
                if kb_on {
                    let none = FxHashMap::default();
                    for (&p, &v) in &argv {
                        if !env.contains_key(&p) {
                            let k = kbits(func, &env, &none, v, 0);
                            if k != (0, 0) {
                                kenv.insert(p, k);
                            }
                        }
                    }
                }
                let nz = |v: Value| {
                    let v = func.dfg.resolve_aliases(v);
                    nn.is(func, v) || pn.contains(&v)
                };
                // Ranges of the incoming args implied by branches dominating the pred.
                let mut prange: FxHashMap<Value, (u64, u64)> = FxHashMap::default();
                let fs = facts_at(&domtree, &facts, pblock);
                if !fs.is_empty() {
                    for (&p, &v) in &argv {
                        if !env.contains_key(&p)
                            && let Some(r) = urange(func, &fs, v, 0, range2)
                        {
                            prange.insert(p, r);
                        }
                    }
                }
                let rg = |v: Value| prange.get(&func.dfg.resolve_aliases(v)).copied();
                for &i in body {
                    if let Some(c) =
                        eval(func, &env, &nz, &rg, i).or_else(|| kfold(func, &env, &kenv, i))
                    {
                        env.insert(func.dfg.first_result(i), c);
                    }
                }
                let taken = match known(func, &env, cond) {
                    Some(c) => c != 0,
                    None if nz(cond) => true,
                    None if !kenv.is_empty() && kbits(func, &env, &kenv, cond, 0).1 != 0 => true,
                    None => continue,
                };
                let ti = if taken { 0 } else { 1 };
                let t = targets[ti];
                let s = t.block(&func.dfg.value_lists);
                if s == b || locked.contains(&s) {
                    if dbg {
                        eprintln!("jt skip {b}->{s}: self/locked");
                    }
                    continue;
                }
                let Some(reg) = region(&cfg, b, s) else {
                    if dbg {
                        eprintln!("jt skip {b}->{s}: region>MAX_WALK");
                    }
                    continue;
                };
                // B's values used from `s` on (resolved), in first-use order.
                let mut needed: Vec<Value> = Vec::new();
                if let Some(bu) = b_users
                    && reg.iter().any(|x| bu.contains(x))
                {
                    for &x in &reg {
                        if !bu.contains(&x) {
                            continue;
                        }
                        for inst in func.layout.block_insts(x) {
                            for v in func.dfg.inst_values(inst) {
                                let r = func.dfg.resolve_aliases(v);
                                if def_block(r) == Some(b) && !needed.contains(&r) {
                                    needed.push(r);
                                }
                            }
                        }
                    }
                }
                let repair = !needed.is_empty();
                // `s`'s other incoming edges (beyond the merge arm) must
                // also feed the new params: append the needed b-values
                // verbatim — legal when each def dominates that pred's
                // terminator (true for loop-carried exports like
                // tokenize's v1899, and always for the merge term's own
                // second arm when both arms land on `s`).
                let mut extra: Vec<(Inst, usize)> = Vec::new();
                if repair {
                    let mut okr = true;
                    for pr in cfg.pred_iter(s) {
                        let q = pr.inst;
                        let qdests = func.dfg.insts[q].branch_destination(
                            &func.dfg.jump_tables,
                            &func.dfg.exception_tables,
                        );
                        for (dj, qbc) in qdests.iter().enumerate() {
                            if qbc.block(&func.dfg.value_lists) != s || (q == term && dj == ti) {
                                continue;
                            }
                            for &w in &needed {
                                let pp = match func.dfg.value_def(w) {
                                    ValueDef::Result(di, _) => ProgramPoint::Inst(di),
                                    ValueDef::Param(db, _) => ProgramPoint::Block(db),
                                    _ => {
                                        okr = false;
                                        break;
                                    }
                                };
                                if !domtree.dominates(pp, q, &func.layout) {
                                    okr = false;
                                    break;
                                }
                            }
                            if !okr {
                                break;
                            }
                            extra.push((q, dj));
                        }
                        if !okr {
                            break;
                        }
                    }
                    if !okr {
                        if dbg {
                            eprintln!("jt skip {b}->{s} from {pblock}: extra edge can't supply exports");
                        }
                        continue;
                    }
                }
                if repair
                    && (seen.contains(&b)
                        || seen.contains(&s)
                        || seen.contains(&pblock)
                        || reg.iter().any(|x| seen.contains(x))
                        || !reg
                            .iter()
                            .filter(|x| b_users.is_some_and(|bu| bu.contains(x)))
                            .all(|&x| domtree.block_dominates(s, x)))
                {
                    if dbg {
                        eprintln!(
                            "jt skip {b}->{s} from {pblock}: repair seen={}/{}/{} regseen={} dom={}",
                            seen.contains(&b),
                            seen.contains(&s),
                            seen.contains(&pblock),
                            reg.iter().any(|x| seen.contains(x)),
                            reg.iter()
                                .filter(|x| b_users.is_some_and(|bu| bu.contains(x)))
                                .all(|&x| domtree.block_dominates(s, x))
                        );
                    }
                    continue;
                }
                let mut new = Vec::new();
                for a in t.args(&func.dfg.value_lists) {
                    let BlockArg::Value(v) = a else {
                        ok = false;
                        break;
                    };
                    ok &= map_val(
                        func,
                        &argv,
                        &body_vals,
                        &env,
                        &domtree,
                        pinst,
                        func.dfg.resolve_aliases(v),
                        &mut new,
                        4,
                    );
                }
                for &w in &needed {
                    ok &= map_val(
                        func, &argv, &body_vals, &env, &domtree, pinst, w, &mut new, 4,
                    );
                }
                if !ok {
                    continue;
                }
                touched.insert(pinst);
                seen.extend([b, s, pblock]);
                let repair = repair.then(|| {
                    touched.insert(term);
                    locked.extend([b, s, pblock]);
                    locked.extend(reg.iter().copied());
                    (term, ti, needed, reg, extra)
                });
                edits.push(Edit {
                    pinst,
                    di,
                    s,
                    args: new,
                    repair,
                    eargs: Vec::new(),
                });
                break;
            }
        }
    }
    // The batch's planned `pinst -> s` edges can open fresh paths that
    // bypass a def some appended edge arg relies on (an export fed
    // verbatim to `s`'s other preds dominated them only because every
    // route used to pass through `b` — think `s` looping back to its own
    // pred). Re-check every value placed on an edge against the
    // post-edit graph, rematerializing constants where possible and
    // dropping the edit otherwise.
    let mut new_succs: FxHashMap<Block, Vec<Block>> = FxHashMap::default();
    for e in &edits {
        if let Some(pb) = func.layout.inst_block(e.pinst) {
            new_succs.entry(pb).or_default().push(e.s);
        }
    }
    let mut memo: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
    let mut post_safe = |func: &Function, v: Value, u: Inst| -> bool {
        let pp = match func.dfg.value_def(v) {
            ValueDef::Result(di, _) => ProgramPoint::Inst(di),
            ValueDef::Param(db, _) => ProgramPoint::Block(db),
            _ => return false,
        };
        if !domtree.dominates(pp, u, &func.layout) {
            return false;
        }
        let Some(db) = def_block(v) else {
            return false;
        };
        let Some(ub) = func.layout.inst_block(u) else {
            return false;
        };
        if db == ub {
            // Dominance within the block was just checked: the def
            // sits before the terminator regardless of how the block
            // is reached.
            return true;
        }
        let r = memo
            .entry(db)
            .or_insert_with(|| reach_avoid(func, &cfg, &new_succs, db));
        !r.contains(&ub)
    };
    let mut benvs: FxHashMap<Block, FxHashMap<Value, u64>> = FxHashMap::default();
    let mut kept: Vec<Edit> = Vec::with_capacity(edits.len());
    'edits: for mut e in edits {
        // `V` leaves carried onto the retargeted edge itself.
        let mut bad = {
            let mut stack: Vec<&Arg> = e.args.iter().collect();
            let mut bad = false;
            while let Some(a) = stack.pop() {
                match a {
                    Arg::K(..) => {}
                    Arg::R(_, subs) => stack.extend(subs.iter()),
                    Arg::V(v) => {
                        let v = func.dfg.resolve_aliases(*v);
                        if !post_safe(func, v, e.pinst) {
                            bad = true;
                            break;
                        }
                    }
                }
            }
            bad
        };
        if !bad && let Some((term, _, needed, _, extra)) = &e.repair {
            let b = func.layout.inst_block(*term).unwrap();
            let benv = benvs.entry(b).or_insert_with(|| const_env(func, b));
            for &(q, _) in extra {
                let mut es = Vec::with_capacity(needed.len());
                for &w in needed {
                    let k = benv
                        .get(&w)
                        .filter(|_| func.dfg.value_type(w).bits() <= 64)
                        .copied();
                    if let Some(k) = k {
                        es.push(Arg::K(k, func.dfg.value_type(w)));
                    } else if post_safe(func, w, q) {
                        es.push(Arg::V(w));
                    } else {
                        bad = true;
                        break;
                    }
                }
                if bad {
                    break;
                }
                e.eargs.push(es);
            }
        }
        if bad {
            continue 'edits;
        }
        kept.push(e);
    }
    let n = kept.len();
    for e in kept {
        if let Some((term, ti, needed, reg, extra)) = e.repair {
            let mut to: FxHashMap<Value, Value> = FxHashMap::default();
            for &w in &needed {
                let ty = func.dfg.value_type(w);
                to.insert(w, func.dfg.append_block_param(e.s, ty));
            }
            for x in reg {
                let insts: Vec<Inst> = func.layout.block_insts(x).collect();
                for inst in insts {
                    let repl: FxHashMap<Value, Value> = func
                        .dfg
                        .inst_values(inst)
                        .filter_map(|v| to.get(&func.dfg.resolve_aliases(v)).map(|&n| (v, n)))
                        .collect();
                    if !repl.is_empty() {
                        func.dfg
                            .map_inst_values(inst, |v| repl.get(&v).copied().unwrap_or(v));
                    }
                }
            }
            // Every other edge into `s` feeds the new params — verbatim
            // b-values when they still dominate that edge post-edit,
            // freshly rematerialized constants otherwise (both resolved
            // during validation into `e.eargs`).
            for ((q, dj), es) in extra.iter().zip(&e.eargs) {
                let dest_ok = {
                    let dfg = &func.dfg;
                    dfg.insts[*q].branch_destination(
                        &dfg.jump_tables,
                        &dfg.exception_tables,
                    )[*dj]
                    .block(&dfg.value_lists)
                        == e.s
                };
                if !dest_ok {
                    continue;
                }
                let vals: Vec<Value> = es.iter().map(|a| emit_arg(func, *q, a)).collect();
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[*q].branch_destination_mut(
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                )[*dj];
                for v in vals {
                    bc.append_argument(v, &mut dfg.value_lists);
                }
            }
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[term]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[ti];
            for &w in &needed {
                bc.append_argument(w, &mut dfg.value_lists);
            }
        }
        let mut args = Vec::with_capacity(e.args.len());
        for a in &e.args {
            args.push(BlockArg::Value(emit_arg(func, e.pinst, a)));
        }
        let bc = BlockCall::new(e.s, args, &mut func.dfg.value_lists);
        let dfg = &mut func.dfg;
        dfg.insts[e.pinst]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.di] = bc;
    }
    n
}

/// Splice `jump`-only successors that have no other predecessor into their
/// predecessor, so a merge block and its test end up in one block.
pub fn merge_chains(func: &mut Function) {
    let cfg = ControlFlowGraph::with_function(func);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut gone: FxHashSet<Block> = FxHashSet::default();
    for x in blocks {
        if gone.contains(&x) {
            continue;
        }
        while let Some(term) = func.layout.last_inst(x) {
            let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
                break;
            };
            let y = destination.block(&func.dfg.value_lists);
            if y == x || Some(y) == entry || cfg.pred_iter(y).count() != 1 {
                break;
            }
            let args: Vec<Value> = destination
                .args(&func.dfg.value_lists)
                .filter_map(|a| match a {
                    BlockArg::Value(v) => Some(v),
                    _ => None,
                })
                .collect();
            if args.len() != func.dfg.num_block_params(y) {
                break;
            }
            // An arg that is one of y's own params (a dead y->x->y cycle or
            // a param permutation) would alias a param to itself or a
            // sibling; resolve first since existing aliases can mask it.
            let pset: FxHashSet<Value> = func.dfg.block_params(y).iter().copied().collect();
            if args
                .iter()
                .any(|&a| pset.contains(&func.dfg.resolve_aliases(a)))
            {
                break;
            }
            let params = func.dfg.detach_block_params(y);
            let params: Vec<Value> = params.as_slice(&func.dfg.value_lists).to_vec();
            for (p, a) in params.into_iter().zip(args) {
                func.dfg.change_to_alias(p, a);
            }
            func.layout.remove_inst(term);
            let insts: Vec<Inst> = func.layout.block_insts(y).collect();
            for i in insts {
                func.layout.remove_inst(i);
                func.layout.append_inst(i, x);
            }
            func.layout.remove_block(y);
            gone.insert(y);
        }
    }
}

/// A dispatch block `y`'s successors may use `y`'s params and results
/// directly — which is exactly what strands them when an edge tries to
/// thread past `y`. Rebind each sole-pred successor `s`: append a param
/// for every `y` value `s`'s dominated cone consumes, feed it on `y`'s
/// edge, and rename the uses. Once the exports travel as edge args an
/// edgespec threading can land on `s` — an enum-discriminant dispatch
/// whose use-cone is small (a couple of epilogue blocks) then folds.
pub fn rebind_exports(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    let mut edits = Vec::new();
    for y in func.layout.blocks().collect::<Vec<_>>() {
        let mut exports: FxHashSet<Value> = func.dfg.block_params(y).iter().copied().collect();
        for i in func.layout.block_insts(y) {
            for r in func.dfg.inst_results(i).iter() {
                exports.insert(*r);
            }
        }
        let Some(term) = func.layout.last_inst(y) else {
            continue;
        };
        let dests = func.dfg.insts[term]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .to_vec();
        let mut done: FxHashSet<Block> = FxHashSet::default();
        for bc in &dests {
            let s = bc.block(&func.dfg.value_lists);
            // Every edge into `s` must leave `y`'s terminator — `pred_iter`
            // counts instructions, so a brif with both arms on `s` still
            // qualifies (all its calls get args appended below).
            if s == y
                || !done.insert(s)
                || !cfg.pred_iter(s).all(|e| e.inst == term)
            {
                continue;
            }
            // `y` values consumed from `s`'s dominated cone — `s` owns the
            // only path there, so a param can carry each.
            let mut needed: Vec<Value> = Vec::new();
            let mut bail = false;
            for b in func.layout.blocks() {
                if b == y || (b != s && !dom.block_dominates(s, b)) {
                    continue;
                }
                for i in func.layout.block_insts(b) {
                    for v in func.dfg.inst_values(i) {
                        let v = func.dfg.resolve_aliases(v);
                        if exports.contains(&v) && !needed.contains(&v) {
                            if needed.len() >= 8 {
                                bail = true;
                                break;
                            }
                            needed.push(v);
                        }
                    }
                }
            }
            if bail || needed.is_empty() {
                continue;
            }
            edits.push((y, term, s, needed));
        }
    }
    let n = edits.len();
    for (y, term, s, needed) in edits {
        let mut repl: FxHashMap<Value, Value> = FxHashMap::default();
        for &w in &needed {
            let ty = func.dfg.value_type(w);
            let p = func.dfg.append_block_param(s, ty);
            repl.insert(w, p);
        }
        for b in func.layout.blocks().collect::<Vec<_>>() {
            if b == y || (b != s && !dom.block_dominates(s, b)) {
                continue;
            }
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            for i in insts {
                let to: FxHashMap<Value, Value> = func
                    .dfg
                    .inst_values(i)
                    .filter_map(|v| {
                        repl.get(&func.dfg.resolve_aliases(v))
                            .map(|&n| (v, n))
                    })
                    .collect();
                if !to.is_empty() {
                    func.dfg
                        .map_inst_values(i, |v| to.get(&v).copied().unwrap_or(v));
                }
            }
        }
        let dfg = &mut func.dfg;
        for bc in
            dfg.insts[term].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if bc.block(&dfg.value_lists) == s {
                for &w in &needed {
                    bc.append_argument(w, &mut dfg.value_lists);
                }
            }
        }
    }
    n
}

/// Clone a small pure dispatch block into a `jump` predecessor — the pred's
/// edge then carries the dispatch's test and branch args itself (the
/// `select→icmp` discriminant in the pred folds against the clone to a
/// raw `brif`). Legal only while every def of `y` is used inside `y` —
/// `rebind_exports` creates exactly that shape by moving shared uses
/// onto successor params.
pub fn dispatch_dup(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    let entry = func.layout.entry_block();
    // Values with any use outside their def block: a `y` whose defs all
    // stay internal can be cloned without stranding downstream uses.
    let mut escaping: FxHashSet<Value> = FxHashSet::default();
    for x in func.layout.blocks() {
        for i in func.layout.block_insts(x) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                let db = match func.dfg.value_def(v) {
                    ValueDef::Param(b, _) => Some(b),
                    ValueDef::Result(di, _) => func.layout.inst_block(di),
                    _ => None,
                };
                if db.is_some_and(|b| b != x) {
                    escaping.insert(v);
                }
            }
        }
    }
    let mut edits = Vec::new();
    for p in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(p) else {
            continue;
        };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
            continue;
        };
        let y = destination.block(&func.dfg.value_lists);
        if y == p || Some(y) == entry {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(y).collect();
        let Some((&yterm, body)) = insts.split_last() else {
            continue;
        };
        if body.len() > 12 || !body.iter().all(|&i| pure_op(func, i)) {
            continue;
        }
        if !matches!(
            func.dfg.insts[yterm],
            InstructionData::Brif { .. } | InstructionData::Jump { .. }
        ) {
            continue;
        }
        let params = func.dfg.block_params(y).to_vec();
        let args: Vec<Value> = destination
            .args(&func.dfg.value_lists)
            .filter_map(|a| match a {
                BlockArg::Value(v) => Some(v),
                _ => None,
            })
            .collect();
        if args.len() != params.len() {
            continue;
        }
        // A `y` param re-bound to `y` itself can't be cloned.
        let pset: FxHashSet<Value> = params.iter().copied().collect();
        if args
            .iter()
            .any(|&a| pset.contains(&func.dfg.resolve_aliases(a)))
        {
            continue;
        }
        let map: FxHashMap<Value, Value> = params.iter().copied().zip(args).collect();
        let mut ok = true;
        // `y` insts and branch args must resolve to clones or to values
        // dominating `p`'s tail.
        for i in insts.iter().take(body.len()).chain([&yterm]) {
            for v in func.dfg.inst_values(*i) {
                let v = func.dfg.resolve_aliases(v);
                if map.contains_key(&v) {
                    continue;
                }
                let okd = match func.dfg.value_def(v) {
                    ValueDef::Param(b, _) => {
                        b == y
                            || (func.layout.is_block_inserted(b)
                                && dom.block_dominates(b, p))
                    }
                    ValueDef::Result(di, _) => {
                        func.layout.inst_block(di) == Some(y)
                            || dom.dominates(di, term, &func.layout)
                    }
                    _ => false,
                };
                if !okd {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        // Nothing `y` defines may be observed outside `y`.
        if ok {
            for i in insts.iter().take(body.len()) {
                for r in func.dfg.inst_results(*i).iter() {
                    if escaping.contains(&func.dfg.resolve_aliases(*r)) {
                        ok = false;
                    }
                }
            }
            if params.iter().any(|pv| escaping.contains(pv)) {
                ok = false;
            }
        }
        if ok {
            edits.push((p, term, y, map));
        }
    }
    let mut n = 0;
    for (_p, term, y, mut map) in edits {
        let insts: Vec<Inst> = func.layout.block_insts(y).collect();
        let Some((&yterm, body)) = insts.split_last() else {
            continue;
        };
        for &i in body {
            let ni = func.dfg.clone_inst(i);
            let vals: Vec<Value> = func
                .dfg
                .inst_values(ni)
                .map(|v| {
                    let v = func.dfg.resolve_aliases(v);
                    *map.get(&v).unwrap_or(&v)
                })
                .collect();
            func.dfg.overwrite_inst_values(ni, vals.into_iter());
            func.layout.insert_inst(ni, term);
            let nrs = func.dfg.inst_results(ni).to_vec();
            for (r, nr) in func.dfg.inst_results(i).to_vec().into_iter().zip(nrs) {
                map.insert(r, nr);
            }
        }
        let sub = |bc: BlockCall, map: &FxHashMap<Value, Value>, func: &Function| -> Vec<BlockArg> {
            bc.args(&func.dfg.value_lists)
                .map(|a| match a {
                    BlockArg::Value(v) => {
                        let v = func.dfg.resolve_aliases(v);
                        BlockArg::Value(*map.get(&v).unwrap_or(&v))
                    }
                    a => a,
                })
                .collect()
        };
        let mut cur = FuncCursor::new(func).at_inst(term);
        let made = match cur.func.dfg.insts[yterm] {
            InstructionData::Jump { destination, .. } => {
                let d = destination.block(&cur.func.dfg.value_lists);
                let args = sub(destination, &map, cur.func);
                Some(cur.ins().jump(d, &args))
            }
            InstructionData::Brif { arg, blocks, .. } => {
                let c = cur.func.dfg.resolve_aliases(arg);
                let c = *map.get(&c).unwrap_or(&c);
                let (tb, eb) = (
                    blocks[0].block(&cur.func.dfg.value_lists),
                    blocks[1].block(&cur.func.dfg.value_lists),
                );
                let ta = sub(blocks[0], &map, cur.func);
                let ea = sub(blocks[1], &map, cur.func);
                Some(cur.ins().brif(c, tb, &ta, eb, &ea))
            }
            _ => None,
        };
        if made.is_none() {
            continue;
        }
        func.layout.remove_inst(term);
        n += 1;
    }
    n
}

/// Retarget edges into blocks that only `jump` onward straight to the final
/// target, so a constant arg reaches the block that tests it. Each forwarded
/// arg must be a param of the bypassed block or defined where it dominates
/// the predecessor's branch. Returns the edges retargeted.
fn bypass_forwarders(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    // Block params used outside their own block: bypassing that block would
    // leave those uses undominated.
    let mut escaping: FxHashSet<Value> = FxHashSet::default();
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for &x in &blocks {
        for i in func.layout.block_insts(x) {
            let mut note = |v: Value| {
                let v = func.dfg.resolve_aliases(v);
                *uses.entry(v).or_default() += 1;
                if let ValueDef::Param(d, _) = func.dfg.value_def(v)
                    && d != x
                {
                    escaping.insert(v);
                }
            };
            func.dfg.inst_args(i).iter().for_each(|&v| note(v));
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        note(v);
                    }
                }
            }
        }
    }
    let mut n = 0;
    for b in blocks {
        if Some(b) == entry
            || func
                .dfg
                .block_params(b)
                .iter()
                .any(|p| escaping.contains(p))
        {
            continue;
        }
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        // Any other insts must be pure and used only by each other.
        let body: Vec<Inst> = func.layout.block_insts(b).filter(|&i| i != term).collect();
        let mut local: FxHashMap<Value, u32> = FxHashMap::default();
        for &i in &body {
            for &a in func.dfg.inst_args(i) {
                *local.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            }
        }
        if !body.iter().all(|&i| {
            pure_op(func, i) && {
                let r = func.dfg.first_result(i);
                uses.get(&r).copied().unwrap_or(0) == local.get(&r).copied().unwrap_or(0)
            }
        }) {
            continue;
        }
        let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
            continue;
        };
        let t = destination.block(&func.dfg.value_lists);
        if t == b {
            continue;
        }
        let mut targs = Vec::new();
        for a in destination.args(&func.dfg.value_lists) {
            match a {
                BlockArg::Value(v) => targs.push(func.dfg.resolve_aliases(v)),
                _ => targs.clear(),
            }
        }
        if targs.len() != func.dfg.num_block_params(t) {
            continue;
        }
        let params = func.dfg.block_params(b).to_vec();
        let preds: Vec<(Block, Inst)> = cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect();
        for (pb, pinst) in preds {
            if pb == b
                || !matches!(
                    func.dfg.insts[pinst].opcode(),
                    Opcode::Jump | Opcode::Brif | Opcode::BrTable
                )
            {
                continue;
            }
            let dests: Vec<BlockCall> = func.dfg.insts[pinst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .to_vec();
            for (di, bc) in dests.into_iter().enumerate() {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                let pargs: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                if pargs.len() != params.len() {
                    continue;
                }
                let mut new = Vec::with_capacity(targs.len());
                for &v in &targs {
                    if let Some(k) = params.iter().position(|&p| p == v) {
                        new.push(pargs[k]);
                        continue;
                    }
                    let ok = match func.dfg.value_def(v) {
                        ValueDef::Result(i, _) => {
                            i != pinst && domtree.dominates(i, pinst, &func.layout)
                        }
                        ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
                        _ => false,
                    };
                    if !ok {
                        break;
                    }
                    new.push(BlockArg::Value(v));
                }
                if new.len() != targs.len() {
                    continue;
                }
                let nbc = BlockCall::new(t, new, &mut func.dfg.value_lists);
                let dfg = &mut func.dfg;
                dfg.insts[pinst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
                    nbc;
                n += 1;
            }
        }
    }
    n
}

/// The icmp a branch condition tests, through `band 1` and extends of its 0/1 result.
fn cond_icmp(func: &Function, v: Value) -> Option<Inst> {
    let env = FxHashMap::default();
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..6 {
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                ..
            } => return Some(i),
            InstructionData::Unary {
                opcode: Opcode::Uextend | Opcode::Ireduce,
                arg,
            } => v = func.dfg.resolve_aliases(arg),
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => {
                let (a, b) = (
                    func.dfg.resolve_aliases(args[0]),
                    func.dfg.resolve_aliases(args[1]),
                );
                if known(func, &env, b) == Some(1) {
                    v = a;
                } else if known(func, &env, a) == Some(1) {
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

type Cmp = (IntCC, Value, Value);

/// A scalar icmp as `(cc, x, y)`; `(x - y) ==/!= 0` becomes `x ==/!= y`.
fn norm_icmp(func: &Function, i: Inst) -> Option<Cmp> {
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let (mut a, mut b) = (
        func.dfg.resolve_aliases(args[0]),
        func.dfg.resolve_aliases(args[1]),
    );
    if func.dfg.value_type(a).is_vector() {
        return None;
    }
    if matches!(cond, IntCC::Equal | IntCC::NotEqual) {
        let env = FxHashMap::default();
        if known(func, &env, a) == Some(0) {
            std::mem::swap(&mut a, &mut b);
        }
        if known(func, &env, b) == Some(0)
            && let Some(d) = func.dfg.value_def(a).inst()
            && let InstructionData::Binary {
                opcode: Opcode::Isub,
                args: s,
            } = func.dfg.insts[d]
        {
            return Some((
                cond,
                func.dfg.resolve_aliases(s[0]),
                func.dfg.resolve_aliases(s[1]),
            ));
        }
    }
    Some((cond, a, b))
}

/// What a true fact `f` says about query `q`, if anything.
fn implied(f: Cmp, q: Cmp) -> Option<bool> {
    use IntCC::*;
    let (fc, fx, fy) = f;
    let qc = if (q.1, q.2) == (fx, fy) {
        q.0
    } else if (q.1, q.2) == (fy, fx) {
        q.0.swap_args()
    } else {
        return None;
    };
    if qc == fc {
        return Some(true);
    }
    if qc == fc.complement() {
        return Some(false);
    }
    match fc {
        Equal => Some(matches!(
            qc,
            UnsignedGreaterThanOrEqual
                | UnsignedLessThanOrEqual
                | SignedGreaterThanOrEqual
                | SignedLessThanOrEqual
        )),
        UnsignedLessThan | UnsignedGreaterThan | SignedLessThan | SignedGreaterThan => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Decide `a cc b` when one side is the constant `k` and the other lies in
/// the unsigned range `[lo, hi]` (`w`-bit compare).
fn range_cmp(
    cc: IntCC,
    a: Option<u64>,
    b: Option<u64>,
    ra: Option<(u64, u64)>,
    rb: Option<(u64, u64)>,
    w: u32,
) -> Option<bool> {
    use IntCC::*;
    let (cc, (lo, hi), k) = match (a, b) {
        (None, Some(k)) => (cc, ra?, k),
        (Some(k), None) => (cc.swap_args(), rb?, k),
        _ => return None,
    };
    let half = 1u64 << (w - 1);
    let cc = match cc {
        _ if hi >= half || k >= half => cc,
        SignedLessThan => UnsignedLessThan,
        SignedLessThanOrEqual => UnsignedLessThanOrEqual,
        SignedGreaterThan => UnsignedGreaterThan,
        SignedGreaterThanOrEqual => UnsignedGreaterThanOrEqual,
        c => c,
    };
    let dec = |t: bool, f: bool| {
        if t {
            Some(true)
        } else if f {
            Some(false)
        } else {
            None
        }
    };
    match cc {
        UnsignedLessThan => dec(hi < k, lo >= k),
        UnsignedLessThanOrEqual => dec(hi <= k, lo > k),
        UnsignedGreaterThan => dec(lo > k, hi <= k),
        UnsignedGreaterThanOrEqual => dec(lo >= k, hi < k),
        Equal => dec(lo == k && hi == k, k < lo || k > hi),
        NotEqual => dec(k < lo || k > hi, lo == k && hi == k),
        _ => None,
    }
}

/// Unsigned range of `v` from its width, `uextend`/`band` with a constant,
/// and the facts `fs` known where it is used.
fn full_range(func: &Function, v: Value) -> Option<(u64, u64)> {
    if func.dfg.value_type(v).is_vector() {
        return None;
    }
    let w = bits(func, v)?;
    Some((0, mask(u64::MAX, w)))
}

fn urange_or_full(
    func: &Function,
    fs: &[Cmp],
    v: Value,
    depth: u32,
    range2: bool,
) -> Option<(u64, u64)> {
    urange(func, fs, v, depth, range2).or_else(|| range2.then(|| full_range(func, v)).flatten())
}

fn urange(func: &Function, fs: &[Cmp], v: Value, depth: u32, range2: bool) -> Option<(u64, u64)> {
    let v = func.dfg.resolve_aliases(v);
    let w = bits(func, v)?;
    if func.dfg.value_type(v).is_vector() {
        return None;
    }
    let full = mask(u64::MAX, w);
    let (mut lo, mut hi) = (0u64, full);
    let env = FxHashMap::default();
    for &(cc, a, b) in fs {
        let (cc, k) = if a == v {
            (cc, known(func, &env, b))
        } else if b == v {
            (cc.swap_args(), known(func, &env, a))
        } else {
            continue;
        };
        let Some(k) = k else { continue };
        match cc {
            IntCC::UnsignedLessThan if k > 0 => hi = hi.min(k - 1),
            IntCC::UnsignedLessThanOrEqual => hi = hi.min(k),
            IntCC::UnsignedGreaterThan if k < full => lo = lo.max(k + 1),
            IntCC::UnsignedGreaterThanOrEqual => lo = lo.max(k),
            IntCC::Equal => {
                lo = lo.max(k);
                hi = hi.min(k);
            }
            _ => {}
        }
    }
    if depth < if range2 { 8 } else { 4 }
        && let Some(i) = func.dfg.value_def(v).inst()
    {
        match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } => {
                if let Some((l, h)) = urange_or_full(func, fs, arg, depth + 1, range2) {
                    lo = lo.max(l);
                    hi = hi.min(h);
                }
            }
            InstructionData::Binary {
                opcode: opcode @ (Opcode::Iadd | Opcode::Isub),
                args,
            } => {
                if range2
                    && opcode == Opcode::Isub
                    && let Some(k) = known(func, &env, args[0])
                    && let Some((l, h)) = urange_or_full(func, fs, args[1], depth + 1, range2)
                    && h <= k
                {
                    lo = lo.max(k - h);
                    hi = hi.min(k - l);
                }
                if let Some(k) = known(func, &env, args[1])
                    && let Some((l, h)) = urange_or_full(func, fs, args[0], depth + 1, range2)
                {
                    let (l2, h2) = if opcode == Opcode::Isub {
                        (l.wrapping_sub(k), h.wrapping_sub(k))
                    } else {
                        (l.wrapping_add(k), h.wrapping_add(k))
                    };
                    // Only when the whole range moves without wrapping.
                    let nowrap = if opcode == Opcode::Isub {
                        l >= k
                    } else {
                        h <= full - k
                    };
                    if nowrap {
                        lo = lo.max(l2);
                        hi = hi.min(h2);
                    }
                }
            }
            InstructionData::Unary {
                opcode: opcode @ (Opcode::Clz | Opcode::Ctz | Opcode::Popcnt),
                arg,
            } if range2 => {
                if let Some((l, h)) = urange_or_full(func, fs, arg, depth + 1, range2) {
                    let w = bits(func, arg)?;
                    let r = match opcode {
                        Opcode::Clz => {
                            let clzw = |x: u64| {
                                if x == 0 {
                                    w
                                } else {
                                    x.leading_zeros() - (64 - w)
                                }
                            };
                            (u64::from(clzw(h)), u64::from(clzw(l)))
                        }
                        Opcode::Ctz => (0, u64::from(if l > 0 { w - 1 } else { w })),
                        Opcode::Popcnt => (u64::from(l > 0), u64::from(w)),
                        _ => unreachable!(),
                    };
                    lo = lo.max(r.0);
                    hi = hi.min(r.1);
                }
            }
            InstructionData::Binary {
                opcode: Opcode::Ushr,
                args,
            } if range2 => {
                if let Some(s) = known(func, &env, args[1])
                    && s < u64::from(w)
                    && let Some((l, h)) = urange_or_full(func, fs, args[0], depth + 1, range2)
                {
                    lo = lo.max(l >> s);
                    hi = hi.min(h >> s);
                }
            }
            InstructionData::BinaryImm8 {
                opcode: Opcode::Ushr,
                arg,
                imm,
            } if range2 => {
                let s = u64::from(imm);
                if s < u64::from(w)
                    && let Some((l, h)) = urange_or_full(func, fs, arg, depth + 1, range2)
                {
                    lo = lo.max(l >> s);
                    hi = hi.min(h >> s);
                }
            }
            InstructionData::Unary {
                opcode: Opcode::Ireduce,
                arg,
            } => {
                if let Some((l, h)) = urange(func, fs, arg, depth + 1, range2)
                    && h <= full
                {
                    lo = lo.max(l);
                    hi = hi.min(h);
                }
            }
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => {
                if let Some(m) = known(func, &env, args[1]).or(known(func, &env, args[0])) {
                    hi = hi.min(m);
                }
            }
            _ => {}
        }
    }
    (lo <= hi && (lo, hi) != (0, full)).then_some((lo, hi))
}

/// The condition known on entry to each block whose only predecessor ends
/// in `brif c` (`c` or its complement).
fn edge_facts(func: &Function, cfg: &ControlFlowGraph) -> FxHashMap<Block, Cmp> {
    let mut fact: FxHashMap<Block, Cmp> = FxHashMap::default();
    for b in func.layout.blocks() {
        let mut preds = cfg.pred_iter(b);
        let (Some(p), None) = (preds.next(), preds.next()) else {
            continue;
        };
        if p.block == b {
            continue;
        }
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] else {
            continue;
        };
        let t = blocks[0].block(&func.dfg.value_lists);
        if t == blocks[1].block(&func.dfg.value_lists) {
            continue;
        }
        let Some((c, x, y)) = cond_icmp(func, arg).and_then(|i| norm_icmp(func, i)) else {
            continue;
        };
        fact.insert(b, (if b == t { c } else { c.complement() }, x, y));
    }
    fact
}

/// Index constants known on entry to blocks whose only predecessor is a
/// `br_table` listing them once (not as default): `idx == k`, and also
/// `x == k` when `idx = ireduce x` and `x` is a `uextend` no wider than `idx`.
fn table_facts(func: &Function, cfg: &ControlFlowGraph) -> FxHashMap<Block, Vec<(Value, u64)>> {
    let mut m: FxHashMap<Block, Vec<(Value, u64)>> = FxHashMap::default();
    let pool = &func.dfg.value_lists;
    for b in func.layout.blocks() {
        let mut preds = cfg.pred_iter(b);
        let (Some(p), None) = (preds.next(), preds.next()) else {
            continue;
        };
        if p.block == b {
            continue;
        }
        let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[p.inst] else {
            continue;
        };
        let jt = &func.dfg.jump_tables[table];
        if jt.default_block().block(pool) == b {
            continue;
        }
        let mut hits = jt
            .as_slice()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.block(pool) == b);
        let (Some((k, _)), None) = (hits.next(), hits.next()) else {
            continue;
        };
        let arg = func.dfg.resolve_aliases(arg);
        let mut v = vec![(arg, k as u64)];
        if let Some(i) = func.dfg.value_def(arg).inst()
            && let InstructionData::Unary {
                opcode: Opcode::Ireduce,
                arg: x,
            } = func.dfg.insts[i]
        {
            let x = func.dfg.resolve_aliases(x);
            if let Some(j) = func.dfg.value_def(x).inst()
                && let InstructionData::Unary {
                    opcode: Opcode::Uextend,
                    arg: n,
                } = func.dfg.insts[j]
                && bits(func, n)
                    .zip(bits(func, arg))
                    .is_some_and(|(n, a)| n <= a)
            {
                v.push((x, k as u64));
            }
        }
        m.insert(b, v);
    }
    m
}

/// Facts holding throughout `b`: those of `b` and its dominators.
fn facts_at(domtree: &DominatorTree, fact: &FxHashMap<Block, Cmp>, b: Block) -> Vec<Cmp> {
    let mut fs = Vec::new();
    if fact.is_empty() {
        return fs;
    }
    let mut cur = Some(b);
    for _ in 0..64 {
        let Some(c) = cur else { break };
        if let Some(&f) = fact.get(&c) {
            fs.push(f);
        }
        cur = domtree.idom(c);
    }
    fs
}

/// Fold icmps decided by a dominating branch: a block whose only predecessor
/// ends in `brif c` knows `c` (or its complement), as do the blocks it
/// dominates; equal operands or a constant against a known range decide it.
/// Returns the icmps folded.
pub fn fold_dominated_conds(func: &mut Function) -> usize {
    let folds = dominated_conds(func);
    for &(i, k) in &folds {
        let ty = func.dfg.value_type(func.dfg.first_result(i));
        func.replace(i).iconst(ty, i64::from(k));
    }
    folds.len()
}

/// The values of `fold_dominated_conds` and `fold_const_branches` without rewriting `func`.
pub fn known_values(func: &Function) -> FxHashMap<Value, u64> {
    let mut env: FxHashMap<Value, u64> = dominated_conds(func)
        .into_iter()
        .map(|(i, k)| (func.dfg.first_result(i), u64::from(k)))
        .collect();
    for _ in 0..2 {
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                if func.dfg.inst_results(i).len() == 1
                    && !env.contains_key(&func.dfg.first_result(i))
                    && let Some(c) = eval(func, &env, &|_| false, &|_| None, i)
                {
                    env.insert(func.dfg.first_result(i), c);
                }
            }
        }
    }
    env
}

pub fn known_value(func: &Function, env: &FxHashMap<Value, u64>, v: Value) -> Option<u64> {
    known(func, env, v)
}

fn dominated_conds(func: &Function) -> Vec<(Inst, bool)> {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let fact = edge_facts(func, &cfg);
    let tfact = table_facts(func, &cfg);
    let range2 = crate::pass_enabled("PLIRON_RANGE2");
    if !range2 && fact.is_empty() && tfact.is_empty() {
        return Vec::new();
    }
    let mut folds = Vec::new();
    for b in func.layout.blocks() {
        let fs = facts_at(&domtree, &fact, b);
        let mut env: FxHashMap<Value, u64> = FxHashMap::default();
        let mut cur = Some(b);
        for _ in 0..64 {
            let Some(c) = cur else { break };
            for &(v, k) in tfact.get(&c).into_iter().flatten() {
                env.insert(v, k);
            }
            cur = domtree.idom(c);
        }
        if !range2 && fs.is_empty() && env.is_empty() {
            continue;
        }
        // `x == y` facts let a query on `y` be decided through `x`.
        let eqs: Vec<(Value, Value)> = fs
            .iter()
            .filter(|f| f.0 == IntCC::Equal)
            .map(|f| (f.1, f.2))
            .collect();
        let alts = |v: Value| {
            let mut a = vec![v];
            for &(x, y) in &eqs {
                if x == v {
                    a.push(y);
                } else if y == v {
                    a.push(x);
                }
            }
            a
        };
        let decide = |q: Cmp| -> Option<bool> {
            if let Some(r) = fs.iter().find_map(|&f| implied(f, q)) {
                return Some(r);
            }
            let w = bits(func, q.1)?;
            let (ka, kb) = (known(func, &env, q.1), known(func, &env, q.2));
            if let (Some(a), Some(b)) = (ka, kb) {
                return Some(icmp(q.0, a, b, w));
            }
            range_cmp(
                q.0,
                ka,
                kb,
                urange(func, &fs, q.1, 0, range2),
                urange(func, &fs, q.2, 0, range2),
                w,
            )
        };
        for i in func.layout.block_insts(b) {
            let Some(q) = norm_icmp(func, i) else {
                continue;
            };
            let r = alts(q.1)
                .into_iter()
                .flat_map(|a| alts(q.2).into_iter().map(move |b| (q.0, a, b)))
                .find_map(decide);
            if let Some(k) = r {
                folds.push((i, k));
            }
        }
    }
    folds
}

/// Block params whose every incoming `jump`/`brif` arg is one value `v` (or
/// the param itself) become aliases of `v`. Threading's SSA repair creates
/// these, and Cranelift only removes them after our condition folding ran, so
/// the same value seen through two params would not match a dominating test.
pub fn remove_trivial_params(func: &mut Function) -> usize {
    #[derive(Clone, Copy, PartialEq)]
    enum In {
        Unset,
        One(Value),
        Many,
    }
    let entry = func.layout.entry_block();
    let mut inc: FxHashMap<Block, Vec<In>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let fixed = matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif);
        for bc in
            func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            let s = bc.block(&func.dfg.value_lists);
            let params = func.dfg.block_params(s);
            let st = inc
                .entry(s)
                .or_insert_with(|| vec![In::Unset; params.len()]);
            if !fixed {
                st.iter_mut().for_each(|x| *x = In::Many);
                continue;
            }
            for (i, a) in bc.args(&func.dfg.value_lists).enumerate().take(st.len()) {
                let new = match a {
                    BlockArg::Value(v) => {
                        let v = func.dfg.resolve_aliases(v);
                        if v == params[i] {
                            continue;
                        }
                        In::One(v)
                    }
                    _ => In::Many,
                };
                st[i] = match (st[i], new) {
                    (In::Unset, x) => x,
                    (In::One(a), In::One(b)) if a == b => In::One(a),
                    _ => In::Many,
                };
            }
        }
    }
    let def_block = |func: &Function, v: Value| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    };
    // `v` must dominate `b`: an edge passing the param itself (e.g. a loop
    // latch) does not constrain `v`'s position, so without this check the
    // alias can put `v` in scopes its def doesn't dominate.
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut rm: Vec<(Block, usize, Value, Value)> = Vec::new();
    for (&b, st) in &inc {
        if Some(b) == entry {
            continue;
        }
        let ps = func.dfg.block_params(b);
        for (i, s) in st.iter().enumerate() {
            if let In::One(v) = *s
                && let Some(d) = def_block(func, v)
                && d != b
                && dt.dominates(d, b, &func.layout)
            {
                rm.push((b, i, ps[i], v));
            }
        }
    }
    // No alias chains between params removed together (avoids alias loops).
    let gone: FxHashSet<Value> = rm.iter().map(|r| r.2).collect();
    rm.retain(|r| !gone.contains(&r.3));
    if rm.is_empty() {
        return 0;
    }
    let mut idx: FxHashMap<Block, Vec<usize>> = FxHashMap::default();
    for r in &rm {
        idx.entry(r.0).or_default().push(r.1);
    }
    for v in idx.values_mut() {
        v.sort_unstable_by(|a, b| b.cmp(a));
    }
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        if !matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif) {
            continue;
        }
        let dfg = &mut func.dfg;
        for bc in
            dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if let Some(is) = idx.get(&bc.block(&dfg.value_lists)) {
                for &i in is {
                    bc.remove(i, &mut dfg.value_lists);
                }
            }
        }
    }
    for &(_, _, p, v) in &rm {
        func.dfg.remove_block_param(p);
        func.dfg.change_to_alias(p, v);
    }
    rm.len()
}

fn mark_live_param(
    func: &Function,
    value: Value,
    live: &mut FxHashMap<Block, Vec<bool>>,
    work: &mut Vec<(Block, usize)>,
) {
    let value = func.dfg.resolve_aliases(value);
    let ValueDef::Param(block, index) = func.dfg.value_def(value) else {
        return;
    };
    let Some(is_live) = live
        .get_mut(&block)
        .and_then(|params| params.get_mut(index))
    else {
        return;
    };
    if !*is_live {
        *is_live = true;
        work.push((block, index));
    }
}

pub fn remove_dead_params(func: &mut Function) -> usize {
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut live: FxHashMap<Block, Vec<bool>> = blocks
        .iter()
        .map(|&block| (block, vec![false; func.dfg.num_block_params(block)]))
        .collect();
    let mut protected: FxHashSet<Block> = func.layout.entry_block().into_iter().collect();
    let mut preds: FxHashMap<Block, Vec<Vec<BlockArg>>> = FxHashMap::default();
    let mut insts = Vec::new();

    for &block in &blocks {
        for inst in func.layout.block_insts(block) {
            insts.push(inst);
            let data = &func.dfg.insts[inst];
            for call in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                let target = call.block(&func.dfg.value_lists);
                let args: Vec<BlockArg> = call.args(&func.dfg.value_lists).collect();
                if args.iter().any(|arg| !matches!(arg, BlockArg::Value(_))) {
                    protected.insert(target);
                }
                preds.entry(target).or_default().push(args);
            }
            if let Some(table) = data.exception_table() {
                for target in func.dfg.exception_tables[table].all_branches() {
                    protected.insert(target.block(&func.dfg.value_lists));
                }
            }
        }
    }

    let mut work = Vec::new();
    for &block in &blocks {
        if protected.contains(&block) {
            for (index, is_live) in live.get_mut(&block).unwrap().iter_mut().enumerate() {
                if !*is_live {
                    *is_live = true;
                    work.push((block, index));
                }
            }
        }
    }
    for inst in &insts {
        for &value in func.dfg.inst_args(*inst) {
            mark_live_param(func, value, &mut live, &mut work);
        }
        if let Some(table) = func.dfg.insts[*inst].exception_table() {
            for item in func.dfg.exception_tables[table].items() {
                if let ExceptionTableItem::Context(value) = item {
                    mark_live_param(func, value, &mut live, &mut work);
                }
            }
        }
    }

    while let Some((target, index)) = work.pop() {
        let Some(incoming) = preds.get(&target) else {
            continue;
        };
        for args in incoming {
            if let Some(BlockArg::Value(value)) = args.get(index).copied() {
                mark_live_param(func, value, &mut live, &mut work);
            }
        }
    }

    let mut dead: FxHashMap<Block, Vec<usize>> = FxHashMap::default();
    for &block in &blocks {
        if protected.contains(&block) {
            continue;
        }
        for (index, &is_live) in live.get(&block).unwrap().iter().enumerate() {
            if !is_live {
                dead.entry(block).or_default().push(index);
            }
        }
    }
    if dead.is_empty() {
        return 0;
    }
    for indices in dead.values_mut() {
        indices.sort_unstable_by(|a, b| b.cmp(a));
    }

    let terms: Vec<Inst> = blocks
        .iter()
        .filter_map(|&block| func.layout.last_inst(block))
        .collect();
    for term in terms {
        let dfg = &mut func.dfg;
        for call in
            dfg.insts[term].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if let Some(indices) = dead.get(&call.block(&dfg.value_lists)) {
                for &index in indices {
                    call.remove(index, &mut dfg.value_lists);
                }
            }
        }
    }

    let removed = dead.values().map(Vec::len).sum();
    for (&block, indices) in &dead {
        for &index in indices {
            let param = func.dfg.block_params(block)[index];
            func.dfg.remove_block_param(param);
        }
    }
    removed
}

fn remove_unused_notrap_loads(func: &mut Function) {
    remove_dead_insts(func, false);
}

/// Drop blocks not reachable from the entry, unless a reachable instruction
/// uses a value they define (then nothing is removed). Returns blocks removed.
pub fn remove_unreachable_blocks(func: &mut Function) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut work = vec![entry];
    while let Some(b) = work.pop() {
        if !seen.insert(b) {
            continue;
        }
        if let Some(term) = func.layout.last_inst(b) {
            let data = &func.dfg.insts[term];
            for call in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                work.push(call.block(&func.dfg.value_lists));
            }
        }
    }
    let dead: Vec<Block> = func.layout.blocks().filter(|b| !seen.contains(b)).collect();
    if dead.is_empty() {
        return 0;
    }
    let defined_in_dead =
        |func: &Function, v: Value| match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
            ValueDef::Result(i, _) => func
                .layout
                .inst_block(i)
                .is_some_and(|b| !seen.contains(&b)),
            ValueDef::Param(b, _) => !seen.contains(&b),
            _ => false,
        };
    for &b in &seen {
        for i in func.layout.block_insts(b) {
            if func.dfg.inst_values(i).any(|v| defined_in_dead(func, v)) {
                return 0;
            }
        }
    }
    for &b in &dead {
        while let Some(i) = func.layout.last_inst(b) {
            func.layout.remove_inst(i);
        }
        func.layout.remove_block(b);
    }
    dead.len()
}

/// `brif c, B(args), B(args)` -> `jump B(args)`. Returns branches folded.
pub fn fold_same_target_brifs(func: &mut Function) -> usize {
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { blocks: [x, y], .. } = func.dfg.insts[t] else {
            continue;
        };
        let pool = &func.dfg.value_lists;
        if x.block(pool) == y.block(pool) && x.args(pool).eq(y.args(pool)) {
            func.dfg.insts[t] = InstructionData::Jump {
                opcode: Opcode::Jump,
                destination: x,
            };
            n += 1;
        }
    }
    n
}

fn nonzero_iconst(func: &Function, v: Value) -> bool {
    let Some(i) = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst() else {
        return false;
    };
    matches!(
        func.dfg.insts[i],
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } if imm.bits() != 0
    )
}

/// Can `inst` be deleted when none of its results are used? Always true for
/// `notrap` loads; with `pure`, also for side-effect-free, non-trapping ops
/// (plus `udiv`/`urem` by a nonzero constant).
pub fn removable(func: &Function, inst: Inst, pure: bool) -> bool {
    let data = &func.dfg.insts[inst];
    if let InstructionData::Load { flags, .. } = *data {
        return func.dfg.mem_flags[flags].notrap();
    }
    let op = data.opcode();
    pure && !(op.is_call()
        || op.is_branch()
        || op.is_terminator()
        || op.can_load()
        || op.can_store()
        || op.other_side_effects())
        && (!op.can_trap()
            || matches!(
                *data,
                InstructionData::Binary { opcode: Opcode::Udiv | Opcode::Urem, args }
                    if nonzero_iconst(func, args[1])
            ))
}

/// Delete instructions whose results are all unused (see `removable`),
/// repeating until nothing changes. Returns the number deleted.
pub fn remove_dead_insts(func: &mut Function, pure: bool) -> usize {
    let mut n = 0;
    loop {
        let mut used: FxHashSet<Value> = FxHashSet::default();
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                for value in func.dfg.inst_values(inst) {
                    used.insert(func.dfg.resolve_aliases(value));
                }
            }
        }

        let dead: Vec<Inst> = func
            .layout
            .blocks()
            .flat_map(|block| func.layout.block_insts(block))
            .filter(|&inst| {
                let results = func.dfg.inst_results(inst);
                !results.is_empty()
                    && results
                        .iter()
                        .all(|&r| !used.contains(&func.dfg.resolve_aliases(r)))
                    && removable(func, inst, pure)
            })
            .collect();
        if dead.is_empty() {
            return n;
        }
        n += dead.len();
        for inst in dead {
            func.layout.remove_inst(inst);
            func.dfg.clear_results(inst);
        }
    }
}

/// Thread until nothing changes (bounded); returns the edges retargeted.
pub fn run(
    func: &mut Function,
    loads: &FxHashSet<Value>,
    derived: &mut FxHashMap<Value, Value>,
) -> (usize, usize, usize) {
    merge_chains(func);
    let domcond = crate::pass_enabled("PLIRON_DOMCOND");
    let trivp = crate::pass_enabled("PLIRON_TRIVPARAM");
    let deadp = crate::pass_enabled("PLIRON_DEADPARAM");
    let ifconv = crate::pass_enabled("PLIRON_IFCONV");
    let mut total = 0;
    let mut dead_params = 0;
    let mut dominated_folds = 0;
    let chk = crate::env_set("PLIRON_JT_CHECK");
    let check = |func: &Function, tag: &str| {
        if chk
            && let Err(e) = cranelift_codegen::verify_function(
                func,
                &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
            )
        {
            panic!("jt step {tag}: {e}\n{}", func.display());
        }
    };
    for _ in 0..8 {
        if trivp {
            remove_trivial_params(func);
            check(func, "trivp");
        }
        if deadp {
            let removed = remove_dead_params(func);
            dead_params += removed;
            if removed != 0 {
                remove_unused_notrap_loads(func);
            }
            check(func, "deadp");
        }
        let dc = if domcond {
            fold_dominated_conds(func)
        } else {
            0
        };
        dominated_folds += dc;
        check(func, "domcond");
        // Select-diamond if-conversion feeds back: folded forwarders expose
        // new diamonds and bypass_forwarders collapses the residue. The dead
        // side blocks keep phantom CFG edges (skewing dominance checks), so
        // sweep them once unreachable.
        let ic = if ifconv {
            crate::ifconv::run(func, derived)
        } else {
            0
        };
        check(func, "ifconv");
        if ic > 0 {
            remove_unreachable_blocks(func);
        }
        check(func, "unreach");
        let n = dc + ic + bypass_forwarders(func);
        check(func, "bypass");
        // Rebuild after `ifconv` so cloned `inbounds` offsets are seen.
        let nn = NonNull::new(func, loads, derived);
        let n = n + round(func, &nn, domcond);
        check(func, "round");
        total += n;
        if n == 0 {
            break;
        }
    }
    let nn = NonNull::new(func, loads, derived);
    (
        total + fold_null_tests(func, &nn),
        dead_params,
        dominated_folds,
    )
}

#[cfg(test)]
mod tests {
    use super::remove_dead_params;
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::{BlockArg, Function, InstBuilder, types};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    #[test]
    fn removes_cycle_only_param_and_keeps_exit_condition_param() {
        let mut func = Function::new();
        let mut builder_ctx = FunctionBuilderContext::new();
        let (loop_block, exit_block) = {
            let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let entry = builder.create_block();
            let loop_block = builder.create_block();
            let exit_block = builder.create_block();
            builder.append_block_param(loop_block, types::I64);
            builder.append_block_param(loop_block, types::I64);
            builder.append_block_param(exit_block, types::I64);

            builder.switch_to_block(entry);
            let zero = builder.ins().iconst(types::I64, 0);
            let entry_args = [BlockArg::Value(zero), BlockArg::Value(zero)];
            builder.ins().jump(loop_block, &entry_args);

            builder.switch_to_block(loop_block);
            let cycle = builder.block_params(loop_block)[0];
            let counter = builder.block_params(loop_block)[1];
            let next = builder.ins().iadd_imm_s(counter, 1);
            let condition = builder
                .ins()
                .icmp_imm_u(IntCC::UnsignedLessThan, counter, 4);
            let loop_args = [BlockArg::Value(cycle), BlockArg::Value(next)];
            let exit_args = [BlockArg::Value(counter)];
            builder
                .ins()
                .brif(condition, loop_block, &loop_args, exit_block, &exit_args);

            builder.switch_to_block(exit_block);
            let result = builder.block_params(exit_block)[0];
            builder.ins().return_(&[result]);
            builder.seal_all_blocks();
            (loop_block, exit_block)
        };

        assert_eq!(remove_dead_params(&mut func), 1);
        assert_eq!(func.dfg.num_block_params(loop_block), 1);
        assert_eq!(func.dfg.num_block_params(exit_block), 1);
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                for call in func.dfg.insts[inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    if call.block(&func.dfg.value_lists) == loop_block {
                        assert_eq!(call.len(&func.dfg.value_lists), 1);
                    }
                }
            }
        }
    }
}

/// Turns `brif`/`br_table` on a compile-time constant into a `jump`. Later passes (load
/// forwarding) expose constants after threading ran, and Cranelift never folds branches,
/// so the dead successor's block arguments would otherwise stay live.
pub fn fold_const_branches(func: &mut Function) -> usize {
    let mut env: FxHashMap<Value, u64> = FxHashMap::default();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for _ in 0..2 {
        for &b in &blocks {
            for i in func.layout.block_insts(b) {
                if func.dfg.inst_results(i).len() == 1
                    && let Some(c) = eval(func, &env, &|_| false, &|_| None, i)
                {
                    env.insert(func.dfg.first_result(i), c);
                }
            }
        }
    }
    let mut n = 0;
    for &b in &blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let dest = match func.dfg.insts[t] {
            InstructionData::Brif { arg, blocks, .. } => {
                let arg = func.dfg.resolve_aliases(arg);
                known(func, &env, arg).map(|c| blocks[usize::from(c == 0)])
            }
            InstructionData::BranchTable { arg, table, .. } => {
                let arg = func.dfg.resolve_aliases(arg);
                known(func, &env, arg).map(|c| {
                    let jt = &func.dfg.jump_tables[table];
                    usize::try_from(c)
                        .ok()
                        .and_then(|c| jt.as_slice().get(c).copied())
                        .unwrap_or(jt.default_block())
                })
            }
            _ => None,
        };
        if let Some(d) = dest {
            func.dfg.insts[t] = InstructionData::Jump {
                opcode: Opcode::Jump,
                destination: d,
            };
            n += 1;
        }
    }
    n
}
