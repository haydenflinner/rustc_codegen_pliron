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
//!     c  = icmp ne sh, 0
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
use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

/// Internal test nodes allowed per tree (codegen cost bound).
const MAX_NODES: usize = 8;

/// Sorted disjoint inclusive interval set over the *unsigned* domain of the
/// tested value's type.
type ISet = Vec<(u64, u64)>;

fn mask(v: u64, bits: u32) -> u64 {
    if bits >= 64 { v } else { v & ((1u64 << bits) - 1) }
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
        // Wraps the sign boundary: negative half, then [0, b].
        vec![(conv(a), max), (0, conv(b))]
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
            if k == 0 { vec![] } else { vec![(0, k - 1)] }
        }
        IntCC::UnsignedLessThanOrEqual => vec![(0, k)],
        IntCC::UnsignedGreaterThanOrEqual => vec![(k, max)],
        IntCC::UnsignedGreaterThan => {
            if k == max { vec![] } else { vec![(k + 1, max)] }
        }
        IntCC::SignedLessThan => {
            if sk == smin { vec![] } else { signed_to_unsigned(smin, sk - 1, bits) }
        }
        IntCC::SignedLessThanOrEqual => signed_to_unsigned(smin, sk, bits),
        IntCC::SignedGreaterThanOrEqual => signed_to_unsigned(sk, smax, bits),
        IntCC::SignedGreaterThan => {
            if sk == smax { vec![] } else { signed_to_unsigned(sk + 1, smax, bits) }
        }
    }
}

fn iconst_bits(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
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
    let ci = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst()?;
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
                && destination
                    .args(&func.dfg.value_lists)
                    .next()
                    .is_none()
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
        ValueDef::Result(i, _) if dom.dominates(i, rterm, &func.layout) => {}
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
    // No result defined inside a dying block may be used outside the region
    // (leaf-edge args are re-emitted at the root, so their recorded use is
    // inside the region).
    for &c in &region {
        if c == root {
            continue;
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
                let dargs: Vec<BlockArg> =
                    destination.args(&func.dfg.value_lists).collect();
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
                    .is_some_and(|b| {
                        !dead.contains(&b) && dom.dominates(i, rterm, &func.layout)
                    });
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

pub fn run(func: &mut Function) -> usize {
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
    let mut plans = Vec::new();
    for b in func.layout.blocks() {
        if !dom.is_reachable(b) {
            continue;
        }
        if let Some(pl) = try_tree(func, &cfg, &dom, &ublocks, b) {
            plans.push(pl);
        }
    }
    let mut n = 0;
    for pl in plans {
        // An earlier fold may have removed this tree's blocks.
        if !func.layout.is_block_inserted(pl.root)
            || !func.layout.is_block_inserted(pl.hit.0)
            || !func.layout.is_block_inserted(pl.miss.0)
            || pl.dead.iter().any(|&b| !func.layout.is_block_inserted(b))
        {
            continue;
        }
        let Some(rterm) = func.layout.last_inst(pl.root) else {
            continue;
        };
        let deadset: FxHashSet<Block> = pl.dead.iter().copied().collect();
        let (Some(ha), Some(ma)) = (
            prep_args(func, &dom, pl.root, rterm, &deadset, &pl.hit.1),
            prep_args(func, &dom, pl.root, rterm, &deadset, &pl.miss.1),
        ) else {
            continue;
        };
        let xty = func.dfg.value_type(pl.x);
        func.layout.remove_inst(rterm);
        let mut cur = FuncCursor::new(func).at_bottom(pl.root);
        let emit_args = |cur: &mut FuncCursor, args: &[A]| -> Vec<BlockArg> {
            args.iter()
                .map(|a| match a {
                    A::V(v) => BlockArg::Value(*v),
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
        let g = cur.ins().iconst(xty, 64);
        let ig = cur.ins().icmp(IntCC::UnsignedLessThan, d, g);
        let dx = if xty == I64 {
            d
        } else {
            cur.ins().uextend(I64, d)
        };
        // `select in, mask, 0` keeps the guard and the test value in one
        // register: `lsr` after `csel` instead of `ushr`+`and`+`uextend`.
        let m = cur.ins().iconst(I64, pl.mask as i64);
        let z = cur.ins().iconst(I64, 0);
        let mg = cur.ins().select(ig, m, z);
        let sh = cur.ins().ushr(mg, dx);
        let c = cur.ins().icmp(IntCC::NotEqual, sh, z);
        cur.ins().brif(c, pl.hit.0, &ha, pl.miss.0, &ma);
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
