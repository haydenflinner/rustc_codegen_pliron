//! A compare on a join's parameter that every incoming edge decides
//! (`PLIRON_JOINCMP=1`, opt-in).
//!
//! hashbrown's `RawTableInner::new_uninitialized` allocates, then rebuilds
//! its bucket count from the length the allocator returned when that is not
//! the length it asked for. The allocation's `Result<NonNull<[u8]>, _>`
//! reaches a join as parameters:
//!
//! ```text
//! block104:  v219 = icmp eq size, 0
//!            brif v219, block147(8, 0), block142    ; zero size: dangling
//! block142:  v221 = call alloc(size, 8)
//!            brif (v221 == 0), fail, block147(v221, size)
//! block147(ptr, len):
//!            v262 = icmp ne len, size
//!            brif v262, recompute, done
//! ```
//!
//! Each edge decides `v262`: `block142` passes `size` itself, and the
//! zero-size edge passes `0` where `size == 0`. Jump threading reads only
//! edge arguments, `edgeknown` and `brthread` only conditions they already
//! hold as values, so the division by the bucket size behind `recompute`
//! stayed (lol_html's `SelectorHandlersLocator` set: 143 instructions per
//! grow from empty, LLVM's 75).
//!
//! Rule: an `icmp` with an operand that is a parameter of block `J`, and
//! whose other operand is a constant, another parameter of `J` or a value
//! defined above `J`. On every edge into `J` the parameters are replaced by
//! the edge's arguments and the compare is decided by: equal operands,
//! constants, or a condition known on the edge (the edge's own `brif` side
//! and the facts of the predecessor's dominator chain, as in `brthread`).
//! A known `a == b` or `a != b` decides compares of `a` and `b`; a known
//! `x == k` substitutes `k` for `x`. When every reachable edge gives the
//! same answer the compare becomes that constant, and branches on it fold.
//!
//! Sound: the compare is a pure function of its operands; a parameter of
//! `J` has, at any point `J` dominates, the value of the last edge taken
//! into `J`, and the other operand is fixed above `J`. Facts hold on the
//! edge for the reasons `brthread` gives.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef};
use rustc_hash::FxHashMap;

/// Facts read per lookup (dominator depth walked).
const MAX_WALK: usize = 64;
/// Edges evaluated per join.
const MAX_PREDS: usize = 16;

#[derive(Clone, Copy)]
struct Fact {
    cond: Value,
    val: bool,
    next: u32,
}
const NIL: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    K(u64),
    V(Value),
}

fn mask(bits: u32, x: u64) -> u64 {
    if bits >= 64 { x } else { x & ((1u64 << bits) - 1) }
}

fn sext(bits: u32, x: u64) -> i64 {
    if bits >= 64 { x as i64 } else { ((x << (64 - bits)) as i64) >> (64 - bits) }
}

fn op_of(func: &Function, v: Value, bits: u32) -> Op {
    let v = func.dfg.resolve_aliases(v);
    if let ValueDef::Result(i, _) = func.dfg.value_def(v)
        && let InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } = func.dfg.insts[i]
    {
        return Op::K(mask(bits, imm.bits() as u64));
    }
    Op::V(v)
}

/// `cc` on two equal operands.
fn on_equal(cc: IntCC) -> bool {
    matches!(
        cc,
        IntCC::Equal
            | IntCC::UnsignedGreaterThanOrEqual
            | IntCC::UnsignedLessThanOrEqual
            | IntCC::SignedGreaterThanOrEqual
            | IntCC::SignedLessThanOrEqual
    )
}

fn eval(cc: IntCC, a: u64, b: u64, bits: u32) -> bool {
    let (sa, sb) = (sext(bits, a), sext(bits, b));
    match cc {
        IntCC::Equal => a == b,
        IntCC::NotEqual => a != b,
        IntCC::UnsignedLessThan => a < b,
        IntCC::UnsignedLessThanOrEqual => a <= b,
        IntCC::UnsignedGreaterThan => a > b,
        IntCC::UnsignedGreaterThanOrEqual => a >= b,
        IntCC::SignedLessThan => sa < sb,
        IntCC::SignedLessThanOrEqual => sa <= sb,
        IntCC::SignedGreaterThan => sa > sb,
        IntCC::SignedGreaterThanOrEqual => sa >= sb,
    }
}

/// Decided without facts.
fn direct(cc: IntCC, a: Op, b: Op, bits: u32) -> Option<bool> {
    if a == b {
        return Some(on_equal(cc));
    }
    match (a, b) {
        (Op::K(x), Op::K(y)) => Some(eval(cc, x, y, bits)),
        _ => None,
    }
}

/// A scalar integer compare: `(cc, lhs, rhs, bits)`.
fn compare(func: &Function, i: Inst) -> Option<(IntCC, Op, Op, u32)> {
    match func.dfg.insts[i] {
        InstructionData::IntCompare { opcode: Opcode::Icmp, cond, args } => {
            let ty = func.dfg.value_type(args[0]);
            if !ty.is_int() || ty.bits() > 64 {
                return None;
            }
            let bits = ty.bits();
            Some((cond, op_of(func, args[0], bits), op_of(func, args[1], bits), bits))
        }
        _ => None,
    }
}

/// Decide `cc a b` from the edge's facts (`own` first, then the chain).
fn decide(func: &Function, facts: &[Fact], own: Option<(Value, bool)>, at: u32, cc: IntCC, a: Op, b: Op, bits: u32) -> Option<bool> {
    if let Some(r) = direct(cc, a, b, bits) {
        return Some(r);
    }
    let mut at = at;
    let mut first = own;
    let mut n = 0;
    loop {
        let (c, val) = if let Some(f) = first.take() {
            f
        } else {
            if at == NIL || n >= MAX_WALK {
                return None;
            }
            let f = facts[at as usize];
            at = f.next;
            n += 1;
            (f.cond, f.val)
        };
        let Some(ci) = func.dfg.value_def(c).inst() else { continue };
        let Some((cc2, x, y, bits2)) = compare(func, ci) else { continue };
        if bits2 != bits {
            continue;
        }
        // The fact is the query (or its mirror / complement).
        if (x, y) == (a, b) {
            if cc2 == cc {
                return Some(val);
            }
            if cc2 == cc.complement() {
                return Some(!val);
            }
        }
        if (x, y) == (b, a) {
            if cc2 == cc.swap_args() {
                return Some(val);
            }
            if cc2 == cc.swap_args().complement() {
                return Some(!val);
            }
        }
        let eq = match (cc2, val) {
            (IntCC::Equal, true) | (IntCC::NotEqual, false) => true,
            (IntCC::Equal, false) | (IntCC::NotEqual, true) => false,
            _ => continue,
        };
        let same = (x, y) == (a, b) || (x, y) == (b, a);
        if same {
            if eq {
                return Some(on_equal(cc));
            }
            match cc {
                IntCC::Equal => return Some(false),
                IntCC::NotEqual => return Some(true),
                _ => continue,
            }
        }
        if eq {
            // `x == k`: substitute.
            let sub = |o: Op| -> Op {
                match (x, y) {
                    (Op::V(v), Op::K(k)) | (Op::K(k), Op::V(v)) if o == Op::V(v) => Op::K(k),
                    _ => o,
                }
            };
            let (a2, b2) = (sub(a), sub(b));
            if (a2, b2) != (a, b)
                && let Some(r) = direct(cc, a2, b2, bits)
            {
                return Some(r);
            }
        }
    }
}

pub fn run(func: &mut Function, cfg: &ControlFlowGraph, dt: &DominatorTree) -> usize {
    // Candidates: compares on a block parameter.
    let mut cands: Vec<(Inst, Block)> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let Some((_, a, o, _)) = compare(func, i) else { continue };
            let pblock = |op: Op| match op {
                Op::V(v) => match func.dfg.value_def(v) {
                    ValueDef::Param(j, _) => Some(j),
                    _ => None,
                },
                _ => None,
            };
            let Some(j) = pblock(a).or_else(|| pblock(o)) else { continue };
            if !dt.is_reachable(j) {
                continue;
            }
            // The other operand is fixed at `j`'s entry.
            let fixed = |op: Op| match op {
                Op::K(_) => true,
                Op::V(v) => match func.dfg.value_def(v) {
                    ValueDef::Param(pj, _) => pj == j || (pj != j && dt.block_dominates(pj, j)),
                    ValueDef::Result(di, _) => func.layout.inst_block(di).is_some_and(|db| db != j && dt.block_dominates(db, j)),
                    _ => false,
                },
            };
            if !fixed(a) || !fixed(o) {
                continue;
            }
            cands.push((i, j));
        }
    }
    if cands.is_empty() {
        return 0;
    }
    // Facts per block (see `brthread`).
    let mut facts: Vec<Fact> = Vec::new();
    let mut head: FxHashMap<Block, u32> = FxHashMap::default();
    for &b in dt.cfg_rpo() {
        let mut h = dt.idom(b).and_then(|i| head.get(&i).copied()).unwrap_or(NIL);
        let mut preds = cfg.pred_iter(b);
        if let (Some(p), None) = (preds.next(), preds.next())
            && let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst]
        {
            let (t, f) = (blocks[0].block(&func.dfg.value_lists), blocks[1].block(&func.dfg.value_lists));
            if t != f {
                facts.push(Fact { cond: func.dfg.resolve_aliases(arg), val: t == b, next: h });
                h = (facts.len() - 1) as u32;
            }
        }
        head.insert(b, h);
    }
    let mut n = 0;
    for (i, j) in cands {
        let Some((cc, a, o, bits)) = compare(func, i) else { continue };
        let params: Vec<Value> = func.dfg.block_params(j).to_vec();
        let mut verdict: Option<bool> = None;
        let mut ok = true;
        let mut edges = 0;
        'preds: for p in cfg.pred_iter(j) {
            if !dt.is_reachable(p.block) {
                continue;
            }
            edges += 1;
            if edges > MAX_PREDS {
                ok = false;
                break;
            }
            let term = p.inst;
            let dests = func.dfg.insts[term].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            let own_arg = match func.dfg.insts[term] {
                InstructionData::Brif { arg, .. } => Some(func.dfg.resolve_aliases(arg)),
                _ => None,
            };
            let at = head.get(&p.block).copied().unwrap_or(NIL);
            for (d, bc) in dests.iter().enumerate() {
                if bc.block(&func.dfg.value_lists) != j {
                    continue;
                }
                let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                let subst = |op: Op| -> Option<Op> {
                    match op {
                        Op::V(v) => match params.iter().position(|&q| q == v) {
                            Some(k) => match args.get(k) {
                                Some(BlockArg::Value(av)) => Some(op_of(func, *av, bits)),
                                _ => None,
                            },
                            None => Some(op),
                        },
                        k => Some(k),
                    }
                };
                let (Some(a2), Some(o2)) = (subst(a), subst(o)) else {
                    ok = false;
                    break 'preds;
                };
                // A `brif` naming `j` on both sides decides nothing.
                let own = own_arg.filter(|_| dests.iter().filter(|x| x.block(&func.dfg.value_lists) == j).count() == 1).map(|c| (c, d == 0));
                match decide(func, &facts, own, at, cc, a2, o2, bits) {
                    Some(r) if verdict.is_none_or(|v| v == r) => verdict = Some(r),
                    _ => {
                        ok = false;
                        break 'preds;
                    }
                }
            }
        }
        let (true, Some(r)) = (ok, verdict) else { continue };
        if !pliron_cfg::bisect("joincmp") {
            continue;
        }
        let ty = func.dfg.value_type(func.dfg.first_result(i));
        func.replace(i).iconst(ty, r as i64);
        n += 1;
    }
    if n > 0 && crate::jumpthread::fold_const_branches(func) > 0 {
        crate::jumpthread::remove_unreachable_blocks(func);
        crate::jumpthread::merge_chains(func, &mut crate::jumpthread::Cd::new());
    }
    n
}
