//! `cmpmerge` (`PLIRON_CMPMERGE=0` off): wide-compare fast path for
//! guarded byte-compare cascades — the surviving half of LLVM's
//! `expandmemcmp` shape.
//!
//! Slice/array compare loops unrolled by `punroll`/`unroll` reach the
//! late pipeline as a chain of single-byte guarded compares:
//!
//! ```text
//!   blockC: chk = iadd idx, k ; icmp uge chk, lenA ; brif exitA(chk)
//!   blockD: icmp uge chk, lenB ; brif exitB(chk)
//!   blockE: a = load.i8 notrap pA+k ; b = load.i8 notrap pB+k
//!           brif (icmp.i8 eq a, b), next_byte, mismatch_k
//! ```
//!
//! — about 7 CLIF insts per byte where LLVM emits one `ldr`/`cmp`
//! pair per 8. This pass inserts, on the edge into the first compare
//! block, a fast path: one "high" bound check per (index, len) family
//! (`idx + K` unwrapped and below the bound proves every skipped
//! `idx + k < len` for `k <= K`), then the whole group's bytes as
//! wide `bxor` loads. Equal jumps to the all-equal continuation;
//! anything else falls back into the untouched serial chain, so no
//! exit shape needs to be understood.
//!
//! Soundness notes: the wide load executes only when every skipped
//! bound check provably passes, so it reads exactly the bytes the
//! serial path would load (all constituent loads must be `notrap`,
//! making the speculation safe regardless). Blocks skipped on the
//! fast path may contain only pure insts and `notrap` loads, every
//! non-compare `brif` must be a recognized `idx + k vs len` bound
//! check, and no value defined inside the skipped region may be used
//! outside it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockCall, Endianness, Function, Inst, InstBuilder, InstructionData, MemFlagsData,
    Opcode, Value, ValueDef, types,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::pipeline::{Analysis, CfgDtLa};

/// Max blocks explored between two compares of a run.
const MAX_HOP: usize = 24;
/// Max compares merged in one group (bytes).
const MAX_RUN: usize = 32;
/// Max groups per function.
const MAX_GROUPS: usize = 8;

#[derive(Clone, Copy)]
struct Side {
    base: Value,
    off: i64,
    flags: MemFlagsData,
}

/// A compare block: `brif (icmp.i8 eq|ne (load.i8 A)(load.i8 B))`.
#[derive(Clone, Copy)]
struct Cmp {
    block: Block,
    a: Side,
    b: Side,
    /// Equal-continue target.
    cont: Block,
}

/// `v` as a `load.i8`: (resolved base, imm offset, flags).
fn byte_load(func: &Function, v: Value) -> Option<Side> {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, 0) = func.dfg.value_def(v) else {
        return None;
    };
    let InstructionData::Load {
        opcode: Opcode::Load,
        arg,
        offset,
        flags: lf,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    if func.dfg.value_type(func.dfg.first_result(i)) != types::I8 {
        return None;
    }
    let flags = func.dfg.mem_flags[lf];
    if !flags.notrap() {
        return None;
    }
    Some(Side {
        base: func.dfg.resolve_aliases(arg),
        off: i64::from(i32::from(offset)),
        flags,
    })
}

/// Block's terminator as the byte compare: the two load sides and
/// which brif slot continues on equality.
fn as_cmp(func: &Function, b: Block) -> Option<Cmp> {
    let ti = func.layout.last_inst(b)?;
    let InstructionData::Brif {
        arg: cond, blocks, ..
    } = func.dfg.insts[ti]
    else {
        return None;
    };
    let cond = func.dfg.resolve_aliases(cond);
    let ValueDef::Result(ci, 0) = func.dfg.value_def(cond) else {
        return None;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: cc,
        args,
    } = func.dfg.insts[ci]
    else {
        return None;
    };
    let cont_slot = match cc {
        IntCC::Equal => 0,
        IntCC::NotEqual => 1,
        _ => return None,
    };
    if func.dfg.value_type(func.dfg.resolve_aliases(args[0])) != types::I8 {
        return None;
    }
    Some(Cmp {
        block: b,
        a: byte_load(func, args[0])?,
        b: byte_load(func, args[1])?,
        cont: blocks[cont_slot].block(&func.dfg.value_lists),
    })
}

/// A bound check on the chain: continue iff `x + k < len` (unsigned).
#[derive(Clone, Copy)]
struct Check {
    x: Value,
    len: Value,
    k: i64,
}

/// `v` as `X + k`: a single-term linear form with coefficient 1.
fn base_plus(func: &Function, v: Value) -> Option<(Value, i64)> {
    let l = crate::ivptr::lin(func, v, 0);
    if l.t.len() != 1 || l.t[0].1 != 1 {
        return None;
    }
    Some((l.t[0].0, l.k))
}

/// Classify a non-compare `brif c, T, F` whose continue side is the
/// edge to `cont` as a bound check that holds iff `x + k < len`.
fn as_check(func: &Function, brif: Inst, cont: Block) -> Option<Check> {
    let InstructionData::Brif {
        arg: cond, blocks, ..
    } = func.dfg.insts[brif]
    else {
        return None;
    };
    let cond = func.dfg.resolve_aliases(cond);
    let ValueDef::Result(ci, 0) = func.dfg.value_def(cond) else {
        return None;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: cc,
        args,
    } = func.dfg.insts[ci]
    else {
        return None;
    };
    let on_true = blocks[0].block(&func.dfg.value_lists) == cont;
    // Accepted (cc, side) pairs all have continue-condition implied
    // by `x < len`; `x` is fixed per pair, never ambiguous.
    let xi = match (cc, on_true) {
        (IntCC::UnsignedLessThan, true)
        | (IntCC::UnsignedLessThanOrEqual, true)
        | (IntCC::UnsignedGreaterThanOrEqual, false)
        | (IntCC::UnsignedGreaterThan, false) => 0,
        (IntCC::UnsignedGreaterThan, true) => 1,
        _ => return None,
    };
    let (x, k) = base_plus(func, args[xi])?;
    Some(Check {
        x,
        len: func.dfg.resolve_aliases(args[1 - xi]),
        k,
    })
}

/// An inst allowed inside a skipped block: pure or a `notrap` load.
fn skippable(func: &Function, i: Inst) -> bool {
    let op = func.dfg.insts[i].opcode();
    if op.is_terminator() || crate::jumpthread::pure_op(func, i) {
        return true;
    }
    matches!(
        &func.dfg.insts[i],
        InstructionData::Load { flags: lf, .. }
            if op.can_load() && func.dfg.mem_flags[*lf].notrap()
    )
}

/// Result of walking one hop toward the next compare. `path` and
/// `checks` (out params) get the route's interior blocks and bound
/// checks.
enum Hop {
    /// Found a compare block.
    Cmp(Block),
    /// No compare within the explored region — the run ends.
    Dead,
}

/// Linear walk from `start` through `jump`s and bound-check `brif`s
/// to the next compare block. Which side of a check continues is
/// decided by `as_check`'s condition, not reachability — where the
/// failing side leads is irrelevant since the fast path only runs
/// when every check provably passes.
fn hop(
    func: &Function,
    start: Block,
    seen: &mut FxHashSet<Block>,
    budget: &mut usize,
    path: &mut Vec<Block>,
    checks: &mut Vec<Check>,
) -> Hop {
    let mut b = start;
    loop {
        if *budget == 0 || !seen.insert(b) {
            return Hop::Dead;
        }
        *budget -= 1;
        let Some(ti) = func.layout.last_inst(b) else {
            return Hop::Dead;
        };
        match func.dfg.insts[ti] {
            InstructionData::Jump { .. } => {
                let t = func.dfg.insts[ti]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[0]
                    .block(&func.dfg.value_lists);
                path.push(b);
                b = t;
            }
            InstructionData::Brif { .. } => {
                if as_cmp(func, b).is_some() {
                    return Hop::Cmp(b);
                }
                let dests: Vec<Block> = func.dfg.insts[ti]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .map(|bc| bc.block(&func.dfg.value_lists))
                    .collect();
                if dests.len() != 2 {
                    return Hop::Dead;
                }
                // Exactly one direction can classify as a bound check
                // for a given condition.
                let mut next = None;
                for &d in &dests {
                    if let Some(chk) = as_check(func, ti, d) {
                        next = Some((chk, d));
                        break;
                    }
                }
                let Some((chk, d)) = next else {
                    return Hop::Dead;
                };
                checks.push(chk);
                path.push(b);
                b = d;
            }
            _ => return Hop::Dead,
        }
    }
}

/// The merged wide load keeps flags minus `aligned` (a wider access
/// is not necessarily as aligned); endianness is pinned little.
fn merged_flags(f: MemFlagsData) -> MemFlagsData {
    let mut nf = MemFlagsData::new()
        .with_alias_region(f.alias_region())
        .with_trap_code(f.trap_code())
        .with_endianness(Endianness::Little);
    if f.notrap() {
        nf = nf.with_notrap();
    }
    if f.readonly() {
        nf = nf.with_readonly();
    }
    nf
}

/// Where `v` is defined, for the dominance check: its inst's block
/// or param block.
fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

/// Emit one group's fast path on edge `pred_inst`'s slot
/// `pred_slot -> b0`. `cmps` are the merged compare blocks (len `p`),
/// `interior`/`checks` the skipped blocks and bound checks.
fn emit(
    func: &mut Function,
    pred_inst: Inst,
    pred_slot: usize,
    b0: Block,
    eq: Block,
    cmps: &[Cmp],
    checks: &[Check],
) {
    let p = cmps.len();
    // One high bound check per (x, len) family.
    let mut fams: Vec<(Value, Value)> = Vec::new();
    for c in checks {
        if !fams.contains(&(c.x, c.len)) {
            fams.push((c.x, c.len));
        }
    }
    let xtys: Vec<cranelift_codegen::ir::Type> =
        fams.iter().map(|&(x, _)| func.dfg.value_type(x)).collect();
    let Some(pb) = func.layout.inst_block(pred_inst) else {
        return;
    };
    let wb = func.dfg.make_block();
    func.layout.insert_block_after(wb, pb);
    let ww = func.dfg.make_block();
    func.layout.insert_block_after(ww, wb);
    {
        let mut cur = FuncCursor::new(func).at_bottom(wb);
        let mut ok: Option<Value> = None;
        for (fi, &(x, len)) in fams.iter().enumerate() {
            let k = cur.ins().iconst(xtys[fi], p as i64 - 1);
            let chk = cur.ins().iadd(x, k);
            let c1 = cur.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, chk, x);
            let c2 = cur.ins().icmp(IntCC::UnsignedLessThan, chk, len);
            let o = cur.ins().band(c1, c2);
            ok = Some(match ok {
                None => o,
                Some(a) => cur.ins().band(a, o),
            });
        }
        let ok = ok.unwrap_or_else(|| cur.ins().iconst(types::I8, 1));
        cur.ins().brif(ok, ww, &[], b0, &[]);
    }
    {
        let mut cur = FuncCursor::new(func).at_bottom(ww);
        // Split the merged prefix into power-of-two wide pieces.
        let mut dtot: Option<Value> = None;
        let mut done = 0usize;
        while done < p {
            let w = [8usize, 4, 2]
                .into_iter()
                .find(|&w| done + w <= p)
                .unwrap_or(2);
            let ty = match w {
                2 => types::I16,
                4 => types::I32,
                _ => types::I64,
            };
            let a = cmps[done].a;
            let b = cmps[done].b;
            let wa = cur.ins().load(ty, merged_flags(a.flags), a.base, a.off as i32);
            let wbv = cur.ins().load(ty, merged_flags(b.flags), b.base, b.off as i32);
            let d = cur.ins().bxor(wa, wbv);
            let d = if ty.bits() < 64 {
                cur.ins().uextend(types::I64, d)
            } else {
                d
            };
            dtot = Some(match dtot {
                None => d,
                Some(x) => cur.ins().bor(x, d),
            });
            done += w;
        }
        let Some(d) = dtot else { return };
        let z = cur.ins().iconst(types::I64, 0);
        let ce = cur.ins().icmp(IntCC::Equal, d, z);
        // Equal: re-enter the serial chain at the last hop's entry,
        // which re-checks the provably-true bounds and runs the last
        // byte compare (it computes its own continue args). Any
        // difference falls back to the serial chain head.
        cur.ins().brif(ce, eq, &[], b0, &[]);
    }
    // Retarget the pred edge to the fast-path block.
    let dfg = &mut func.dfg;
    dfg.insts[pred_inst]
        .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[pred_slot] =
        BlockCall::new(wb, [], &mut dfg.value_lists);
}

/// Build the compare run starting at `b0`: consecutive `+1` offsets
/// on both sides. Returns (compares, per-hop interior blocks,
/// per-hop checks); the interior/check vectors align with hops, i.e.
/// `hops[i]` is what compare `i + 1` skips over.
fn grow(
    func: &Function,
    cmap: &FxHashMap<Block, Cmp>,
    b0: Block,
    seen_run: &mut FxHashSet<Block>,
) -> (Vec<Cmp>, Vec<(Vec<Block>, Vec<Check>)>) {
    let c0 = cmap[&b0];
    let (ab, bb) = (c0.a.base, c0.b.base);
    let mut run = vec![c0];
    let mut hops = Vec::new();
    seen_run.insert(b0);
    while run.len() < MAX_RUN {
        let mut seen: FxHashSet<Block> = seen_run.clone();
        let mut budget = MAX_HOP;
        let mut path = Vec::new();
        let mut checks = Vec::new();
        let Hop::Cmp(cb) =
            hop(func, run.last().unwrap().cont, &mut seen, &mut budget, &mut path, &mut checks)
        else {
            break;
        };
        let Some(&nx) = cmap.get(&cb) else {
            break;
        };
        if seen_run.contains(&cb) {
            break;
        }
        let prev = run.last().unwrap();
        if nx.a.base != ab
            || nx.b.base != bb
            || nx.a.off != prev.a.off + 1
            || nx.b.off != prev.b.off + 1
        {
            break;
        }
        seen_run.insert(cb);
        run.push(nx);
        hops.push((path, checks));
    }
    (run, hops)
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let mut cmap: FxHashMap<Block, Cmp> = FxHashMap::default();
    for b in func.layout.blocks() {
        if let Some(c) = as_cmp(func, b) {
            cmap.insert(b, c);
        }
    }
    if cmap.is_empty() {
        return 0;
    }
    let mut n = 0;
    let mut grouped: FxHashSet<Block> = FxHashSet::default();
    let starts: Vec<Block> = cmap.keys().copied().collect();
    for b0 in starts {
        if grouped.contains(&b0) || n >= MAX_GROUPS || !pliron_cfg::bisect("cmpmerge") {
            continue;
        }
        let mut seen_run = grouped.clone();
        let (run, hops) = grow(func, &cmap, b0, &mut seen_run);
        // Merged byte count: drop a trailing single byte.
        let p = run.len() - (run.len() % 2);
        if p < 2 {
            tracing::trace!("cmpmerge: run too short ({}) from {b0:?}", run.len());
            continue;
        }
        let run = &run[..p];
        let hops = &hops[..p - 1];
        // Every skipped check's k must lie inside the merged prefix:
        // `x + kmax < len` and `x + kmax >= x` imply `x + k < len` for
        // all `0 <= k <= kmax`; a negative k could wrap below x and
        // fail a check the fast path would skip.
        let kmax = p as i64 - 1;
        if hops
            .iter()
            .flat_map(|h| h.1.iter())
            .any(|c| c.k > kmax || c.k < 0)
        {
            tracing::trace!("cmpmerge: check beyond merged prefix from {b0:?}");
            continue;
        }
        // The wide-equal path re-enters the serial chain at the last
        // hop's first interior block (or the last compare itself when
        // the hop has no interior): it re-runs the final bounds checks
        // — which provably pass — plus the last byte compare, which
        // computes its own continue args. Every value the tail segment
        // uses from outside itself must be available at the insertion
        // edge (checked via `need` below).
        let tail: Vec<Block> = hops[p - 2]
            .0
            .iter()
            .copied()
            .chain(std::iter::once(run[p - 1].block))
            .collect();
        let tset: FxHashSet<Block> = tail.iter().copied().collect();
        if tset.iter().any(|&b| func.dfg.num_block_params(b) != 0) {
            continue;
        }
        let mut lneed: Vec<Value> = Vec::new();
        for &b in &tail {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    match def_block(func, v) {
                        Some(d) if tset.contains(&d) => {}
                        _ => {
                            if !lneed.contains(&v) {
                                lneed.push(v);
                            }
                        }
                    }
                }
            }
        }
        // Skipped region S: b0's block, compare blocks 1..p and each
        // hop's interior. Only pure/notrap insts; no def escapes.
        let mut sset: FxHashSet<Block> = FxHashSet::default();
        sset.insert(b0);
        for c in run.iter().skip(1) {
            sset.insert(c.block);
        }
        let mut checks: Vec<Check> = Vec::new();
        for h in hops {
            for &pb in &h.0 {
                sset.insert(pb);
            }
            checks.extend(h.1.iter().copied());
        }
        let mut sdefs: Vec<Value> = Vec::new();
        let mut clean = true;
        for &sb in &sset {
            for i in func.layout.block_insts(sb) {
                if !skippable(func, i) {
                    tracing::trace!("cmpmerge: nonskippable {i} in {sb} ({b0:?})");
                    clean = false;
                    break;
                }
                sdefs.extend(func.dfg.inst_results(i).iter().copied());
            }
            if !clean {
                break;
            }
        }
        if !clean || func.dfg.num_block_params(b0) != 0 {
            continue;
        }
        let uses = crate::uses::Uses::new(func);
        if let Some(&v) = sdefs
            .iter()
            .find(|&&v| uses.sites(v).iter().any(|&(b, _)| !sset.contains(&b)))
        {
            tracing::trace!("cmpmerge: {v} escapes skipped region ({b0:?})");
            continue;
        }
        // Find a pred edge into b0 whose block every needed value
        // dominates (or defines).
        let mut need: Vec<Value> = vec![run[0].a.base, run[0].b.base];
        for c in &checks {
            need.push(c.x);
            need.push(c.len);
        }
        need.extend(lneed);
        let pick = {
            let CfgDtLa { cfg, dt, .. } = an.cfg_dt_la(func);
            cfg.pred_iter(b0).find_map(|pr| {
                let pb = func.layout.inst_block(pr.inst)?;
                if sset.contains(&pb) {
                    return None;
                }
                let slot = func.dfg.insts[pr.inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .position(|bc| bc.block(&func.dfg.value_lists) == b0)?;
                need.iter().all(|&v| {
                    def_block(func, v)
                        .is_some_and(|d| dt.dominates(d, pb, &func.layout))
                })
                .then_some((pr.inst, slot))
            })
        };
        if let Some((inst, slot)) = pick {
            emit(func, inst, slot, b0, tail[0], run, &checks);
            n += 1;
            grouped.extend(run.iter().map(|c| c.block));
            // The CFG changed; rebuild analyses lazily for the next
            // group and for steps after this one.
            an.invalidate(func);
            continue;
        }
        tracing::trace!("cmpmerge: no usable pred edge ({b0:?})");
    }
    n
}
