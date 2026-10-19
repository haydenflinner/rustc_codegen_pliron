//! Sparse small-domain set-membership trees → one guarded bitmask test.
//!
//! `char::is_whitespace`, `u8::is_ascii_*` and `match` arms over small
//! constants arrive from MIR as trees of `icmp`/`brif` blocks:
//!
//! ```text
//! blockR: c0 = icmp eq x, 32;   brif c0, hit(1), blockA
//! blockA: c1 = icmp ule 9, x;   brif c1, blockB, miss
//! blockB: c2 = icmp ule x, 13;  brif c2, hit(1), miss
//! ```
//!
//! When the tree's leaves partition into exactly two destinations and the
//! value set reaching the `hit` leaf fits a 64-bit window `[lo, lo+63]`, the
//! chain is a sparse membership test:
//!
//! ```text
//! blockR:
//!     d  = isub x, lo
//!     in = icmp ult d, 64           // guards the shift (ushr wraps mod 64)
//!     mg = select in, mask64, 0     // out-of-window ⇒ shift a zero mask
//!     sh = ushr mg, uextend(d)      // bit i set <=> lo+i accepted
//!     c  = icmp ne (band sh, 1), 0  // the tested bit lands at bit 0
//!     brif c, hit(args), miss(args)
//! ```
//!
//! ~7 straight-line insts and ONE conditional branch replace 3+ branchy
//! blocks; on AArch64 this lowers to `sub`+`cmp`+`csel`+`lsr`+`cbnz`, on x64
//! to `btq`+carry. Accepted sets stay exact: the guard rejects every `x`
//! outside the window (unsigned wrap of `d` covers `x < lo`), so the fold is
//! sign- and width-correct for any int type.
//!
//! A non-root block may only join the tree when it is a pure test block —
//! `icmp`/`iconst` insts feeding the `brif`, a single predecessor inside the
//! tree, and no result used outside it — so deleting it leaves nothing
//! dangling. Leaf edges must agree on their argument lists.
//!
//! `PLIRON_BITTAB=0` disables it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::types::{I8, I64, I128};
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// Internal test nodes allowed per tree (codegen cost bound).
const MAX_NODES: usize = 8;

/// Sorted disjoint inclusive interval set over the *unsigned* domain of the
/// tested value's type.
type ISet = Vec<(u64, u64)>;

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

fn full_set(bits: u32) -> ISet {
    vec![(0, mask(u64::MAX, bits))]
}

fn meet(a: &ISet, b: &ISet) -> ISet {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let lo = a[i].0.max(b[j].0);
        let hi = a[i].1.min(b[j].1);
        if lo <= hi {
            out.push((lo, hi));
        }
        if a[i].1 < b[j].1 { i += 1 } else { j += 1 }
    }
    out
}

fn union(a: &mut ISet, b: &ISet) {
    let mut m: ISet = a.iter().chain(b.iter()).copied().collect();
    m.sort();
    a.clear();
    for (lo, hi) in m {
        if let Some(last) = a.last_mut()
            && lo <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(hi);
            continue;
        }
        a.push((lo, hi));
    }
}

fn complement(s: &ISet, bits: u32) -> ISet {
    let max = mask(u64::MAX, bits);
    let mut out = Vec::new();
    let mut next = 0u64;
    for &(lo, hi) in s {
        if lo > next {
            out.push((next, lo - 1));
        }
        next = hi.saturating_add(1);
        if hi == max {
            return out;
        }
    }
    if next <= max {
        out.push((next, max));
    }
    out
}

/// Map a signed-domain interval `[a,b]` onto unsigned-domain pieces.
fn signed_to_unsigned(a: i64, b: i64, bits: u32) -> ISet {
    if a > b {
        return vec![];
    }
    let max = mask(u64::MAX, bits);
    let conv = |v: i64| -> u64 { mask(v as u64, bits) };
    if a >= 0 || b < 0 {
        vec![(conv(a), conv(b))]
    } else {
        // Wraps the sign boundary: [0, b], then the negative half. The
        // unsigned-domain pieces must stay sorted ascending — `meet`/
        // `complement` merge-walk the lists and silently drop intervals
        // that arrive out of order.
        vec![(0, conv(b)), (conv(a), max)]
    }
}

/// The unsigned-domain set satisfying `x CC k` at `bits` width.
fn sat(cc: IntCC, k: u64, bits: u32) -> ISet {
    let max = mask(u64::MAX, bits);
    let k = mask(k, bits);
    let sk = sext(k, bits);
    let (smin, smax) = if bits >= 64 {
        (i64::MIN, i64::MAX)
    } else {
        (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
    };
    match cc {
        IntCC::Equal => vec![(k, k)],
        IntCC::NotEqual => complement(&vec![(k, k)], bits),
        IntCC::UnsignedLessThan => {
            if k == 0 {
                vec![]
            } else {
                vec![(0, k - 1)]
            }
        }
        IntCC::UnsignedLessThanOrEqual => vec![(0, k)],
        IntCC::UnsignedGreaterThanOrEqual => vec![(k, max)],
        IntCC::UnsignedGreaterThan => {
            if k == max {
                vec![]
            } else {
                vec![(k + 1, max)]
            }
        }
        IntCC::SignedLessThan => {
            if sk == smin {
                vec![]
            } else {
                signed_to_unsigned(smin, sk - 1, bits)
            }
        }
        IntCC::SignedLessThanOrEqual => signed_to_unsigned(smin, sk, bits),
        IntCC::SignedGreaterThanOrEqual => signed_to_unsigned(sk, smax, bits),
        IntCC::SignedGreaterThan => {
            if sk == smax {
                vec![]
            } else {
                signed_to_unsigned(sk + 1, smax, bits)
            }
        }
    }
}

fn iconst_bits(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.resolved_value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

/// The `(icmp inst, cc, x, k)` a block's `brif` tests, with the constant
/// normalized to the right-hand side. `None` unless the terminator is `brif`
/// on an in-block `icmp`.
fn test_of(func: &Function, b: Block) -> Option<(Inst, IntCC, Value, u64)> {
    let t = func.layout.last_inst(b)?;
    let InstructionData::Brif { arg: c, .. } = func.dfg.insts[t] else {
        return None;
    };
    let ci = func.dfg.resolved_value_def(c).inst()?;
    if func.layout.inst_block(ci) != Some(b) {
        return None;
    }
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args: [a, b2],
    } = func.dfg.insts[ci]
    else {
        return None;
    };
    let (a, b2) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b2));
    if let Some(k) = iconst_bits(func, b2) {
        Some((ci, cond, a, k as u64))
    } else {
        iconst_bits(func, a).map(|k| (ci, cond.swap_args(), b2, k as u64))
    }
}

/// `b` may join the fold region: every inst is `iconst`/`icmp`/`brif`/`jump`;
/// a `brif` must test an in-block `icmp`, and a `jump` (pure glue) must have
/// an empty arg list so sets propagate unchanged.
fn cand_ok(func: &Function, b: Block) -> bool {
    for i in func.layout.block_insts(b) {
        if !matches!(
            func.dfg.insts[i].opcode(),
            Opcode::Iconst | Opcode::Icmp | Opcode::Brif | Opcode::Jump
        ) {
            return false;
        }
    }
    let Some(t) = func.layout.last_inst(b) else {
        return false;
    };
    match func.dfg.insts[t] {
        InstructionData::Jump { destination, .. } => {
            func.dfg
                .block_params(destination.block(&func.dfg.value_lists))
                .is_empty()
                && destination.args(&func.dfg.value_lists).next().is_none()
        }
        InstructionData::Brif { .. } => test_of(func, b).is_some(),
        _ => false,
    }
}

struct Plan {
    root: Block,
    x: Value,
    lo: u64,
    mask: u64,
    hit: (Block, Vec<BlockArg>),
    miss: (Block, Vec<BlockArg>),
    dead: Vec<Block>,
}

fn try_tree(
    func: &Function,
    cfg: &ControlFlowGraph,
    dom: &DominatorTree,
    ublocks: &FxHashMap<Value, FxHashSet<Block>>,
    root: Block,
) -> Option<Plan> {
    let (_, _, x0, _) = test_of(func, root)?;
    let x = func.dfg.resolve_aliases(x0);
    let ty = func.dfg.value_type(x);
    if !ty.is_int() || ty.bits() > 64 {
        return None;
    }
    let bits = ty.bits();
    // `x` must be defined at or before the root tail.
    let rterm = func.layout.last_inst(root)?;
    match func.dfg.value_def(x) {
        ValueDef::Param(b, _) if b == root || dom.block_dominates(b, root) => {}
        ValueDef::Result(i, _) if dom.try_dominates(i, rterm, &func.layout) => {}
        _ => return None,
    }

    // Phase A: collect candidate members — glue/test blocks reachable from
    // the root — then grow the fold region as a strict tree: a candidate
    // joins only when it has exactly one predecessor, inside the region.
    // A join node like `block669` (two in-tree preds) stays a leaf, which is
    // exactly what makes the ASCII-whitespace shape a 2-edge fold.
    let mut cand: FxHashSet<Block> = FxHashSet::default();
    let mut work = vec![root];
    let mut seen: FxHashSet<Block> = [root].into_iter().collect();
    while let Some(b) = work.pop() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let mut arms: smallvec::SmallVec<[Block; 4]> = Default::default();
        match func.dfg.insts[t] {
            InstructionData::Brif { blocks, .. } => {
                for bc in blocks {
                    arms.push(bc.block(&func.dfg.value_lists));
                }
            }
            InstructionData::Jump { destination, .. } => {
                arms.push(destination.block(&func.dfg.value_lists));
            }
            _ => continue,
        }
        for db in arms {
            if seen.insert(db) && db != root && cand_ok(func, db) {
                cand.insert(db);
                work.push(db);
            }
        }
        if cand.len() >= MAX_NODES {
            return None;
        }
    }
    let mut region: FxHashSet<Block> = [root].into_iter().collect();
    loop {
        let mut added = false;
        for &c in &cand {
            if region.contains(&c) {
                continue;
            }
            let mut preds = cfg.pred_iter(c);
            if let (Some(p), None) = (preds.next(), preds.next())
                && region.contains(&p.block)
            {
                region.insert(c);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    // Profitability: fewer than two folded blocks means the ~8-inst mask
    // sequence replaces less code than it adds.
    if region.len() < 3 {
        return None;
    }
    // No value defined inside a dying block may be used outside the region
    // — results *and* block params (a param's uses can live in any block
    // the dying block's edges fed, not just inside the tree).
    // Leaf-edge args are re-emitted at the root, so their recorded use is
    // inside the region.
    for &c in &region {
        if c == root {
            continue;
        }
        for p in func.dfg.block_params(c) {
            if let Some(us) = ublocks.get(&func.dfg.resolve_aliases(*p))
                && !us.is_subset(&region)
            {
                return None;
            }
        }
        for i in func.layout.block_insts(c) {
            for r in func.dfg.inst_results(i) {
                if let Some(us) = ublocks.get(&func.dfg.resolve_aliases(*r))
                    && !us.is_subset(&region)
                {
                    return None;
                }
            }
        }
    }

    // Phase B: propagate the accepted-value interval set along every region
    // edge; each leaf edge is keyed by (block, args) — the same leaf block
    // reached with different args (e.g. a 0/1 result param) is a distinct
    // outcome the folded `brif` can still express.
    let mut edges: FxHashMap<(Block, Vec<BlockArg>), ISet> = FxHashMap::default();
    let mut stack: Vec<(Block, ISet)> = vec![(root, full_set(bits))];
    let mut fuel = 4 * MAX_NODES + 8;
    while let Some((b, iset)) = stack.pop() {
        if iset.is_empty() {
            continue;
        }
        if fuel == 0 {
            return None;
        }
        fuel -= 1;
        let t = func.layout.last_inst(b)?;
        let mut out: smallvec::SmallVec<[(Block, Vec<BlockArg>, ISet); 4]> = Default::default();
        match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => {
                let db = destination.block(&func.dfg.value_lists);
                let dargs: Vec<BlockArg> = destination.args(&func.dfg.value_lists).collect();
                out.push((db, dargs, iset));
            }
            InstructionData::Brif { blocks, .. } => {
                let Some((_, cc, bx, k)) = test_of(func, b) else {
                    return None;
                };
                if func.dfg.resolve_aliases(bx) != x {
                    return None;
                }
                let s = sat(cc, k, bits);
                let c = complement(&s, bits);
                for (i, bc) in blocks.iter().enumerate() {
                    let eset = meet(&iset, if i == 0 { &s } else { &c });
                    if eset.is_empty() {
                        continue;
                    }
                    out.push((
                        bc.block(&func.dfg.value_lists),
                        bc.args(&func.dfg.value_lists).collect(),
                        eset,
                    ));
                }
            }
            _ => return None,
        }
        for (db, dargs, eset) in out {
            if region.contains(&db) {
                stack.push((db, eset));
            } else {
                union(edges.entry((db, dargs)).or_default(), &eset);
            }
        }
    }
    if edges.len() != 2 {
        return None;
    }
    // Pick the leaf edge whose accepted set fits a 64-bit window.
    let mut pick = None;
    for ((lb, la), s) in &edges {
        if region.contains(lb) {
            return None;
        }
        let (Some(&(mn, _)), Some(&(_, mx))) = (s.first(), s.last()) else {
            continue;
        };
        let lo = if mx <= 63 { 0 } else { mn };
        if mx - lo > 63 {
            continue;
        }
        let mut m = 0u64;
        for &(a, b) in s {
            for i in a..=b {
                m |= 1u64 << (i - lo);
            }
        }
        if m == 0 {
            continue;
        }
        let other = edges.keys().find(|(ob, _)| ob != lb)?.clone();
        pick = Some((lb, la.clone(), lo, m, other));
        break;
    }
    let (hb, hargs, lo, m, other) = pick?;
    Some(Plan {
        root,
        x,
        lo,
        mask: m,
        hit: (*hb, hargs),
        miss: other,
        dead: region.iter().copied().filter(|&b| b != root).collect(),
    })
}

/// An arg usable at the root tail: a dominating value, or an `iconst` to
/// rematerialize (possibly dying with a removed interior block).
enum A {
    V(Value),
    K(cranelift_codegen::ir::Type, i64),
}

fn prep_args(
    func: &Function,
    dom: &DominatorTree,
    root: Block,
    rterm: Inst,
    dead: &FxHashSet<Block>,
    args: &[BlockArg],
) -> Option<Vec<A>> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        let BlockArg::Value(v) = a else { return None };
        let v = func.dfg.resolve_aliases(*v);
        match func.dfg.value_def(v) {
            ValueDef::Param(b, _) if b == root || dom.block_dominates(b, root) => {
                out.push(A::V(v));
            }
            ValueDef::Result(i, _) => {
                let ty = func.dfg.value_type(v);
                // Still in the layout, not in a dying block, and dominating
                // the root tail: keep it. Otherwise only a rematerializable
                // `iconst` is allowed.
                let live_dom = func
                    .layout
                    .inst_block(i)
                    .is_some_and(|b| !dead.contains(&b) && dom.try_dominates(i, rterm, &func.layout));
                if live_dom {
                    out.push(A::V(v));
                } else {
                    match func.dfg.insts[i] {
                        InstructionData::UnaryImm {
                            opcode: Opcode::Iconst,
                            imm,
                        } => out.push(A::K(ty, imm.bits())),
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Range-refolding: `bor` trees of `icmp eq x, k` leaves sharing one
/// resolved `x` collapse consecutive constants into `icmp ult (x-lo), len`
/// range checks — `x ∈ {0,1,2,3,4}` becomes one `ult` instead of five
/// `eq`s and four `bor`s. Works on scalars and vectors: a vectorized
/// SwitchInt arrives as `vconst`-vs-`vconst` eq chains that otherwise
/// cost 5+ compares per vector of bytes (e.g. `is_ascii_whitespace`).
/// Runs of length ≥2 fold; singleton `k`s keep their original `icmp`.
fn eqrange(func: &mut Function) -> usize {
    /// Lane-typed constant bits: `iconst` scalar, or a `vconst` that
    /// splats one lane value (all lanes byte-identical).
    fn const_of(func: &Function, v: Value) -> Option<u64> {
        let di = func.dfg.resolved_value_def(v).inst()?;
        match func.dfg.insts[di] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits() as u64),
            InstructionData::UnaryConst {
                opcode: Opcode::Vconst,
                constant_handle,
            } => {
                let ty = func.dfg.value_type(func.dfg.first_result(di));
                let lb = (ty.lane_bits() / 8) as usize;
                if lb == 0 || lb > 8 {
                    return None;
                }
                let d = func.dfg.constants.get(constant_handle).as_slice();
                if d.is_empty() || d.len() % lb != 0 || !d.chunks(lb).all(|c| c == &d[..lb]) {
                    return None;
                }
                let mut k = 0u64;
                for (j, &b) in d[..lb].iter().enumerate() {
                    k |= (b as u64) << (8 * j);
                }
                Some(k)
            }
            _ => None,
        }
    }
    /// `ty`-typed constant `k`: `iconst` scalar or splat `vconst`.
    fn kconst(cur: &mut FuncCursor, ty: cranelift_codegen::ir::Type, k: u64) -> Value {
        if ty.is_vector() {
            let lb = (ty.lane_bits() / 8) as usize;
            let mut bytes = Vec::with_capacity(lb * ty.lane_count() as usize);
            for _ in 0..ty.lane_count() {
                bytes.extend_from_slice(&k.to_le_bytes()[..lb]);
            }
            let h = cur.func.dfg.constants.insert(bytes.into());
            cur.ins().vconst(ty, h)
        } else {
            cur.ins().iconst(ty, k as i64)
        }
    }
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let interior: FxHashSet<Value> = func
            .layout
            .block_insts(b)
            .flat_map(|i| {
                if let InstructionData::Binary {
                    opcode: Opcode::Bor,
                    args,
                } = func.dfg.insts[i]
                {
                    args.to_vec()
                } else {
                    Vec::new()
                }
            })
            .collect();
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let InstructionData::Binary {
                opcode: Opcode::Bor,
                args,
            } = func.dfg.insts[i]
            else {
                continue;
            };
            if func.dfg.inst_results(i).len() != 1 {
                continue;
            }
            let out = func.dfg.first_result(i);
            if interior.contains(&func.dfg.resolve_aliases(out)) {
                continue;
            }
            let ty = func.dfg.value_type(out);
            if !ty.lane_type().is_int() {
                continue;
            }
            // Gather the tree: interior `bor`s, leaves `icmp eq x, k` all
            // on the same resolved `x`.
            let mut x = None;
            let mut ks: Vec<u64> = Vec::new();
            let mut stack = vec![args[0], args[1]];
            let mut ok = true;
            while let Some(v) = stack.pop() {
                let v = func.dfg.resolve_aliases(v);
                let Some(di) = func.dfg.value_def(v).inst() else {
                    ok = false;
                    break;
                };
                match func.dfg.insts[di] {
                    InstructionData::Binary {
                        opcode: Opcode::Bor,
                        args,
                    } => {
                        stack.push(args[0]);
                        stack.push(args[1]);
                    }
                    InstructionData::IntCompare {
                        cond: IntCC::Equal,
                        args,
                        ..
                    } => {
                        let (a, c) = (
                            func.dfg.resolve_aliases(args[0]),
                            func.dfg.resolve_aliases(args[1]),
                        );
                        let (xv, k) = match (const_of(func, a), const_of(func, c)) {
                            (None, Some(k)) => (a, k),
                            (Some(k), None) => (c, k),
                            _ => {
                                ok = false;
                                break;
                            }
                        };
                        match x {
                            None => x = Some(xv),
                            Some(e) if e == xv => {}
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                        ks.push(mask(k, func.dfg.value_type(xv).lane_bits()));
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || ks.len() < 3 {
                continue;
            }
            ks.sort_unstable();
            ks.dedup();
            let x = x.unwrap();
            let xty = func.dfg.value_type(x);
            // `iconst`/`vconst` can't express a lane wider than 64 bits.
            if xty.lane_bits() > 64 {
                continue;
            }
            // Maximal consecutive runs; a run folds when it is cheaper than
            // the eqs it replaces. Scalar `ult` is sub+cmp — beats len≥2.
            // Vector `ult` lowers to a psubb/pminub/pcmpeqb/pcmpeqd/pxor
            // sequence (~5 insts + bor share), so it only pays at len≥4.
            let min_len = if xty.is_vector() { 4 } else { 2 };
            let mut runs: Vec<(u64, u64)> = Vec::new();
            for &k in &ks {
                match runs.last_mut() {
                    Some(r) if k == r.1 + 1 => r.1 = k,
                    _ => runs.push((k, k)),
                }
            }
            let folded: usize = runs
                .iter()
                .map(|(lo, hi)| (hi - lo + 1) as usize)
                .filter(|&l| l >= min_len)
                .sum();
            if folded < 2 {
                continue;
            }
            // Rebuild: range checks for runs, original `icmp eq` results
            // for singletons (k -> leaf value, only singles looked up).
            let single: FxHashMap<u64, Value> = {
                let mut m = FxHashMap::default();
                if runs.iter().any(|&(lo, hi)| hi - lo + 1 < min_len as u64) {
                    let mut st = vec![args[0], args[1]];
                    while let Some(v) = st.pop() {
                        let v = func.dfg.resolve_aliases(v);
                        let Some(di) = func.dfg.value_def(v).inst() else {
                            continue;
                        };
                        match func.dfg.insts[di] {
                            InstructionData::Binary {
                                opcode: Opcode::Bor,
                                args,
                            } => {
                                st.push(args[0]);
                                st.push(args[1]);
                            }
                            InstructionData::IntCompare {
                                cond: IntCC::Equal,
                                args,
                                ..
                            } => {
                                let (a, c) = (
                                    func.dfg.resolve_aliases(args[0]),
                                    func.dfg.resolve_aliases(args[1]),
                                );
                                let k = const_of(func, c).or_else(|| const_of(func, a));
                                if let Some(k) = k {
                                    m.insert(mask(k, xty.lane_bits()), v);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                m
            };
            let mut cur = FuncCursor::new(func).at_inst(i);
            let mut parts: Vec<Value> = Vec::new();
            for (lo, hi) in runs {
                let len = hi - lo + 1;
                // `len` must fit the lane type unsigned — a full-domain run
                // (len = 2^bits) would truncate the bound to 0.
                let len_fits = xty.lane_bits() == 64 || len < (1u64 << xty.lane_bits());
                if len >= min_len as u64 && len_fits {
                    let d = if lo > 0 {
                        let k = kconst(&mut cur, xty, lo);
                        cur.ins().isub(x, k)
                    } else {
                        x
                    };
                    let k = kconst(&mut cur, xty, len);
                    parts.push(cur.ins().icmp(IntCC::UnsignedLessThan, d, k));
                } else {
                    // Below-threshold run: keep each original eq leaf.
                    for k in lo..=hi {
                        if let Some(&v) = single.get(&k) {
                            parts.push(v);
                        } else {
                            let k = kconst(&mut cur, xty, k);
                            parts.push(cur.ins().icmp(IntCC::Equal, x, k));
                        }
                    }
                }
            }
            let mut c = parts[0];
            for &p in &parts[1..] {
                c = cur.ins().bor(c, p);
            }
            func.dfg.clear_results(i);
            func.dfg.change_to_alias(out, c);
            func.layout.remove_inst(i);
            n += 1;
        }
    }
    n
}

/// Flat form: a `bor` tree over `icmp eq x, k` leaves inside one block —
/// what the branchy tree becomes once ifconv or the vectorizer's scan
/// expansion collapses it into straight-line data flow. With ≥4 distinct
/// constants inside one 64-bit window the whole chain is the same guarded
/// `ushr` test, emitted in place of the root; the dead leaves are left
/// for regalloc to drop (they may have other uses).
fn flat(func: &mut Function) -> usize {
    fn const_of(func: &Function, v: Value) -> Option<i64> {
        let di = func.dfg.resolved_value_def(v).inst()?;
        if let InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } = func.dfg.insts[di]
        {
            Some(imm.bits())
        } else {
            None
        }
    }
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        // Values consumed by a `bor` in this block: a candidate whose own
        // result is one is an interior tree node, not the root — folding
        // it first would strand the parent's mixed tree.
        let interior: FxHashSet<Value> = func
            .layout
            .block_insts(b)
            .flat_map(|i| {
                if let InstructionData::Binary {
                    opcode: Opcode::Bor,
                    args,
                } = func.dfg.insts[i]
                {
                    args.to_vec()
                } else {
                    Vec::new()
                }
            })
            .collect();
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let InstructionData::Binary {
                opcode: Opcode::Bor,
                args,
            } = func.dfg.insts[i]
            else {
                continue;
            };
            if func.dfg.inst_results(i).len() != 1 {
                continue;
            }
            let out = func.dfg.first_result(i);
            if interior.contains(&func.dfg.resolve_aliases(out)) {
                continue;
            }
            if func.dfg.value_type(out) != I8 || func.dfg.resolve_aliases(out) != out {
                continue;
            }
            // Walk the tree: interior nodes are `bor`, leaves must all be
            // `icmp eq` on the same resolved operand against an iconst.
            let mut x = None;
            let mut ks: Vec<u64> = Vec::new();
            let mut stack = vec![args[0], args[1]];
            let mut ok = true;
            while let Some(v) = stack.pop() {
                let v = func.dfg.resolve_aliases(v);
                let Some(di) = func.dfg.value_def(v).inst() else {
                    ok = false;
                    break;
                };
                match func.dfg.insts[di] {
                    InstructionData::Binary {
                        opcode: Opcode::Bor,
                        args,
                    } => {
                        stack.push(args[0]);
                        stack.push(args[1]);
                    }
                    InstructionData::IntCompare {
                        cond: IntCC::Equal,
                        args,
                        ..
                    } => {
                        let (a, c) = (
                            func.dfg.resolve_aliases(args[0]),
                            func.dfg.resolve_aliases(args[1]),
                        );
                        let (xv, k) = match (const_of(func, a), const_of(func, c)) {
                            (None, Some(k)) => (a, k),
                            (Some(k), None) => (c, k),
                            _ => {
                                ok = false;
                                break;
                            }
                        };
                        let bits = func.dfg.value_type(xv).bits();
                        let kv = mask(k as u64, bits);
                        if bits > 64 || kv >= 64 {
                            ok = false;
                            break;
                        }
                        match x {
                            None => x = Some(xv),
                            Some(e) if e == xv => {}
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                        ks.push(kv);
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || ks.len() < 4 {
                continue;
            }
            let (lo, hi) = (
                ks.iter().copied().min().unwrap(),
                ks.iter().copied().max().unwrap(),
            );
            if hi - lo >= 64 {
                continue;
            }
            let bits = ks.iter().fold(0u64, |m, k| m | (1 << (k - lo)));
            let x = x.unwrap();
            let xty = func.dfg.value_type(x);
            let mut cur = FuncCursor::new(func).at_inst(i);
            let d = if lo > 0 {
                let c = cur.ins().iconst(xty, lo as i64);
                cur.ins().isub(x, c)
            } else {
                x
            };
            let g = cur.ins().iconst(xty, 64);
            let ig = cur.ins().icmp(IntCC::UnsignedLessThan, d, g);
            let dx = if xty == I64 {
                d
            } else {
                cur.ins().uextend(I64, d)
            };
            let m = cur.ins().iconst(I64, bits as i64);
            let z = cur.ins().iconst(I64, 0);
            let mg = cur.ins().select(ig, m, z);
            let sh = cur.ins().ushr(mg, dx);
            let bit = cur.ins().band_imm_u(sh, 1);
            let c = cur.ins().icmp(IntCC::NotEqual, bit, z);
            func.dfg.clear_results(i);
            func.dfg.change_to_alias(out, c);
            func.layout.remove_inst(i);
            n += 1;
        }
    }
    n
}

pub fn run(func: &mut Function) -> usize {
    // Fold one tree at a time and re-analyze after each: plans computed
    // against a stale dominator tree could otherwise keep a tested value
    // or leaf arg whose defining block an earlier fold just removed,
    // leaving dangling operands that crash `value_def` downstream.
    // `flat` first: a scalar chain dense enough for a masked `ushr`/`bt`
    // test is strictly cheaper than the range checks `eqrange` would break
    // it into, and once folded there is no `bor` tree left to range-fold.
    // `eqrange` then takes the leftovers — vector chains and sparse scalar
    // sets outside a 64-bit window.
    let mut n = flat(func);
    if crate::pass_enabled("PLIRON_EQRANGE") {
        n += eqrange(func);
    }
    loop {
        let cfg = ControlFlowGraph::with_function(func);
        let dom = DominatorTree::with_function(func, &cfg);
        // value -> blocks using it
        let mut ublocks: FxHashMap<Value, FxHashSet<Block>> = FxHashMap::default();
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    ublocks
                        .entry(func.dfg.resolve_aliases(v))
                        .or_default()
                        .insert(b);
                }
            }
        }
        let mut plan = None;
        for b in func.layout.blocks() {
            if !dom.is_reachable(b) {
                continue;
            }
            if let Some(pl) = try_tree(func, &cfg, &dom, &ublocks, b) {
                plan = Some(pl);
                break;
            }
        }
        let Some(pl) = plan else {
            break;
        };
        if !func.layout.is_block_inserted(pl.root)
            || !func.layout.is_block_inserted(pl.hit.0)
            || !func.layout.is_block_inserted(pl.miss.0)
            || pl.dead.iter().any(|&b| !func.layout.is_block_inserted(b))
        {
            break;
        }
        let Some(rterm) = func.layout.last_inst(pl.root) else {
            break;
        };
        let deadset: FxHashSet<Block> = pl.dead.iter().copied().collect();
        let (Some(ha), Some(ma)) = (
            prep_args(func, &dom, pl.root, rterm, &deadset, &pl.hit.1),
            prep_args(func, &dom, pl.root, rterm, &deadset, &pl.miss.1),
        ) else {
            break;
        };
        let xty = func.dfg.value_type(pl.x);
        // Two branches, one per condition: `d <= msb(mask)` rejects every
        // index that cannot hit a set bit (and every `d >= 64`, which would
        // wrap the shift), so out-of-window `x` pays a single compare —
        // `cmp;ja` then `bt;jae` on x64, `cmp;b.hi`+`tbz` on AArch64.
        let msb = 63 - pl.mask.leading_zeros() as i64;
        let tb = func.dfg.make_block();
        func.layout.remove_inst(rterm);
        func.layout.insert_block_after(tb, pl.root);
        let mut cur = FuncCursor::new(func).at_bottom(pl.root);
        let emit_args = |cur: &mut FuncCursor, args: &[A]| -> Vec<BlockArg> {
            args.iter()
                .map(|a| match a {
                    A::V(v) => BlockArg::Value(*v),
                    A::K(ty, k) if *ty == I128 => {
                        // `iconst` has no i128 form — rebuild the
                        // constant from two i64 halves.
                        let lo = cur.ins().iconst(I64, *k);
                        let hi = cur.ins().iconst(I64, k >> 63);
                        BlockArg::Value(cur.ins().iconcat(lo, hi))
                    }
                    A::K(ty, k) => BlockArg::Value(cur.ins().iconst(*ty, *k)),
                })
                .collect()
        };
        let ha = emit_args(&mut cur, &ha);
        let ma = emit_args(&mut cur, &ma);
        let d = if pl.lo > 0 {
            let c = cur.ins().iconst(xty, pl.lo as i64);
            cur.ins().isub(pl.x, c)
        } else {
            pl.x
        };
        let g = cur.ins().iconst(xty, msb);
        let ig = cur.ins().icmp(IntCC::UnsignedLessThanOrEqual, d, g);
        cur.ins().brif(ig, tb, &[], pl.miss.0, &ma);
        let mut cur = FuncCursor::new(func).at_bottom(tb);
        let dx = if xty == I64 {
            d
        } else {
            cur.ins().uextend(I64, d)
        };
        let m = cur.ins().iconst(I64, pl.mask as i64);
        let z = cur.ins().iconst(I64, 0);
        let sh = cur.ins().ushr(m, dx);
        // Bit `d` of the mask lands at bit 0 of `sh`: test it, not the
        // whole shifted value — `sh != 0` would accept every `x` at or
        // below the mask's highest set bit, i.e. the interval's holes.
        let bit = cur.ins().band_imm_u(sh, 1);
        // Branch on the 0/1 `band` result directly: aarch64 lowers
        // `brif (band x, pow2)` to `tbnz`, but only matches the
        // `icmp eq`-against-zero form (`tbz`) — the `ne` here would
        // lower to cmp+csel+branch instead.
        if crate::pass_enabled("PLIRON_BITBRIF") {
            cur.ins().brif(bit, pl.hit.0, &ha, pl.miss.0, &ma);
        } else {
            let c = cur.ins().icmp(IntCC::NotEqual, bit, z);
            cur.ins().brif(c, pl.hit.0, &ha, pl.miss.0, &ma);
        }
        for &b in &pl.dead {
            while let Some(i) = func.layout.first_inst(b) {
                func.layout.remove_inst(i);
            }
            func.layout.remove_block(b);
        }
        n += 1;
    }
    if n > 0 {
        crate::edgespec::sweep_dead(func);
        crate::edgespec::sweep_blocks(func);
    }
    n
}
