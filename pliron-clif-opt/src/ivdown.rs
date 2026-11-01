//! Exit-only counters count down to zero (`PLIRON_IVDOWN`, `=0` disables).
//!
//! Once `ivptr` has moved the addressing onto pointer params, the
//! `TrustedRandomAccess` zip's index survives only as a trip counter:
//!
//! ```text
//! H(.., i):  ...  i' = iadd i, c;  t = icmp ne i', n;  brif t, H(.., i'), EXIT
//! ```
//!
//! with `n` loop-invariant and `i`/`i'` used nowhere else. Carrying
//! `r = n - i` instead (entry `n - i0`, step `-c`) gives `r' = n - i'`,
//! so `i' == n` exactly when `r' == 0`, wrapping included: the compare
//! loses its loop-invariant operand and aarch64 tests the counter with
//! `cbnz` (`sub; cbnz` per trip instead of `add; cmp; b.ne`), LLVM's
//! shape for num-bigint-dig's `add_mul_vvw` in blind-sig's montgomery.
//! The rewrite is in place: the param, the step and the compare keep
//! their values, only the entry args and two operands change.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef};
use rustc_hash::FxHashMap;

use crate::pipeline::{Analysis, CfgDtLa};

fn iconst_of(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// `v = iadd x, c` (either order): `(x, c)`.
fn step_of(func: &Function, v: Value) -> Option<(Value, i64)> {
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::Binary { opcode: Opcode::Iadd, args } => match (iconst_of(func, args[0]), iconst_of(func, args[1])) {
            (_, Some(c)) => Some((func.dfg.resolve_aliases(args[0]), c)),
            (Some(c), _) => Some((func.dfg.resolve_aliases(args[1]), c)),
            _ => None,
        },
        _ => None,
    }
}

struct Edit {
    /// (pred terminator, dest index) of each entry edge.
    entries: Vec<(Inst, usize)>,
    slot: usize,
    step: Inst,
    i: Value,
    c: i64,
    cmp: Inst,
    /// The compare's counter operand (`i` or `i'`).
    x: Value,
    cc: IntCC,
    n: Value,
    /// Users of the compare's result.
    cmp_users: Vec<Inst>,
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    // `ivptr` leaves the index's old address math behind, dead but
    // still a use until something sweeps it. Pure insts only: the CFG
    // and the cached analyses stay valid.
    crate::uses::worklist_dce(func, crate::jumpthread::pure_op);
    // Users per value; edge args count (as uses by the terminator).
    let mut users: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for inst in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(inst) {
                users.entry(func.dfg.resolve_aliases(v)).or_default().push(inst);
            }
        }
    }
    let nuse = |v: Value| users.get(&v).map_or(0, |u| u.len());
    let mut edits: Vec<Edit> = Vec::new();
    {
        let CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
        let def_block = |v: Value| -> Option<Block> {
            match func.dfg.value_def(v) {
                ValueDef::Result(d, _) => func.layout.inst_block(d),
                ValueDef::Param(b, _) => Some(b),
                _ => None,
            }
        };
        for lp in la.loops() {
            let h = la.loop_header(lp);
            let params: Vec<Value> = func.dfg.block_params(h).to_vec();
            // Every edge into `h`: (terminator, dest index, from inside the loop).
            let mut edges: Vec<(Inst, usize, bool)> = Vec::new();
            for pred in cfg.pred_iter(h) {
                let inside = la.is_in_loop(pred.block, lp);
                let dests = func.dfg.insts[pred.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                for (d, bc) in dests.iter().enumerate() {
                    if bc.block(&func.dfg.value_lists) == h {
                        edges.push((pred.inst, d, inside));
                    }
                }
            }
            let arg = |inst: Inst, d: usize, k: usize| -> Option<Value> {
                let dests = func.dfg.insts[inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                match dests[d].args(&func.dfg.value_lists).nth(k)? {
                    BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                    _ => None,
                }
            };
            for (k, &i) in params.iter().enumerate() {
                // The back edges all carry one `i' = i + c`.
                let mut inc = None;
                let mut nback = 0;
                let mut ok = true;
                for &(inst, d, inside) in &edges {
                    let a = arg(inst, d, k);
                    if inside {
                        nback += 1;
                        if inc.is_none() {
                            inc = a;
                        }
                        ok &= a.is_some() && a == inc;
                    } else {
                        ok &= a.is_some();
                    }
                }
                let Some(inc) = inc.filter(|_| ok && nback > 0 && nback < edges.len()) else { continue };
                let Some((base, c)) = step_of(func, inc) else { continue };
                if base != i || c == 0 {
                    continue;
                }
                let step = func.dfg.value_def(inc).unwrap_inst();
                // One compare against an invariant `n`, on `i` or `i'`;
                // nothing else reads either.
                let (x, cmp) = match (nuse(i), nuse(inc)) {
                    (1, m) if m == nback + 1 => {
                        let Some(&c) = users[&inc].iter().find(|&&u| func.dfg.insts[u].opcode() == Opcode::Icmp) else { continue };
                        (inc, c)
                    }
                    (2, m) if m == nback => {
                        let Some(&c) = users[&i].iter().find(|&&u| u != step) else { continue };
                        (i, c)
                    }
                    _ => continue,
                };
                let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[cmp] else { continue };
                if !matches!(cond, IntCC::Equal | IntCC::NotEqual) {
                    continue;
                }
                let (a0, a1) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
                let n = if a0 == x && a1 != x {
                    a1
                } else if a1 == x && a0 != x {
                    a0
                } else {
                    continue;
                };
                // `n` is defined outside the loop and available at the end
                // of every entry pred.
                let Some(nb) = def_block(n) else { continue };
                if la.is_in_loop(nb, lp) {
                    continue;
                }
                let entries: Vec<(Inst, usize)> = edges.iter().filter(|e| !e.2).map(|&(inst, d, _)| (inst, d)).collect();
                if !entries.iter().all(|&(inst, _)| {
                    func.layout.inst_block(inst).is_some_and(|pb| dt.block_dominates(nb, pb))
                }) {
                    continue;
                }
                if pliron_cfg::bisect("ivdown") {
                    let t = func.dfg.first_result(cmp);
                    let cmp_users = users.get(&t).cloned().unwrap_or_default();
                    edits.push(Edit { entries, slot: k, step, i, c, cmp, x, cc: cond, n, cmp_users });
                }
            }
        }
    }
    let k = edits.len();
    for e in edits {
        for &(inst, d) in &e.entries {
            let mut cur = FuncCursor::new(func).at_inst(inst);
            let dests = cur.func.dfg.insts[inst].branch_destination(&cur.func.dfg.jump_tables, &cur.func.dfg.exception_tables);
            let i0 = match dests[d].args(&cur.func.dfg.value_lists).nth(e.slot) {
                Some(BlockArg::Value(v)) => v,
                _ => unreachable!("checked above"),
            };
            let r0 = cur.ins().isub(e.n, i0);
            let func = cur.func;
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
            let blk = bc.block(&dfg.value_lists);
            let mut args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
            args[e.slot] = BlockArg::Value(r0);
            *bc = cranelift_codegen::ir::BlockCall::new(blk, args, &mut dfg.value_lists);
        }
        let ty = func.dfg.value_type(e.i);
        let mut cur = FuncCursor::new(func).at_inst(e.step);
        let neg = cur.ins().iconst(ty, e.c.wrapping_neg());
        cur.func.replace(e.step).iadd(e.i, neg);
        // A compare feeding only a `brif` becomes the counter itself as
        // the condition (nonzero = `ne`; `eq` swaps the targets): the
        // egraph would turn `icmp ne (iadd r, -1), 0` into `icmp ne r, 1`,
        // keeping `r` and `r'` both live (an extra `mov` per trip).
        let t = func.dfg.first_result(e.cmp);
        let only_brif = match e.cmp_users[..] {
            [u] => match func.dfg.insts[u] {
                InstructionData::Brif { arg, .. } => func.dfg.resolve_aliases(arg) == t,
                _ => false,
            },
            _ => false,
        };
        if only_brif {
            let u = e.cmp_users[0];
            if let InstructionData::Brif { arg, blocks, .. } = &mut func.dfg.insts[u] {
                *arg = e.x;
                if e.cc == IntCC::Equal {
                    blocks.swap(0, 1);
                }
            }
        } else {
            let mut cur = FuncCursor::new(func).at_inst(e.cmp);
            let zero = cur.ins().iconst(ty, 0);
            cur.func.replace(e.cmp).icmp(e.cc, e.x, zero);
        }
    }
    if k > 0 {
        an.invalidate(func);
    }
    k
}
