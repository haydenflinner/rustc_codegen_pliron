//! Strength-reduce `base + index * stride` onto a pointer induction variable
//! (`PLIRON_IVSR=1`, opt-in).
//!
//! brotli's shortest-path loop reads `nodes[i + 1]` (20-byte nodes) on
//! every position: `v = (i + 1) * 20 + nodes` each iteration. On aarch64
//! that is a `madd` of the index with the base, plus the base reloaded from
//! its spill slot and the stride rematerialized, where LLVM's loop strength
//! reduction carries the address itself (`add x8, x19, x25`):
//!
//! ```text
//! H(i, ..):   a = iadd B, imul(iadd(i, 1), 20)      H(i, .., p):   a = p
//!             ...                                →                 ...
//!             jump H(i + 1, ..)                                    jump H(i + 1, .., p + 20)
//!   entry:    jump H(i0, ..)                           entry:      jump H(i0, .., B + 20*i0 + 20)
//! ```
//!
//! Rule: a loop header's integer parameter `i` whose every back-edge
//! argument is `i + c` (one constant `c`, as linear forms), and in-loop
//! `iadd X, B` (`B` defined outside the loop) with `X ≡ S*i + k` (linear
//! form with the single leaf `i`, `S` not a power of two: a power of two
//! folds into the address mode already), when no header parameter already
//! steps by `S*c`. One new header parameter `p` per
//! `(i, B, S)` holds `B + S*i + k0` on every trip: the entry edges pass it
//! computed from their `i` argument, the back edges `p + S*c`; each
//! address becomes `p` or `p + (k - k0)`, when one of the group's addresses
//! is computed on every trip (its block dominates each latch). Everything is wrapping
//! arithmetic, so the rewrite is exact. Only headers whose predecessors
//! end in `jump` or `brif`. Linear in the loop bodies.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    types, Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};
use rustc_hash::FxHashMap;

use crate::ivptr::lin;
use crate::pipeline::{Analysis, CfgDtLa};

/// The smallest loop taken (`PLIRON_IVSR_MIN`, instructions, inner loops
/// included; default 0, every loop). aarch64's `madd` computes `i*S + base`
/// in one instruction, so the rewrite pays where the base or the stride
/// were spilled or rematerialized (brotli's per-position loop) and costs a
/// register and the entry's multiply elsewhere (lol_html's attribute loops,
/// html-rewriter +0.10%). Loop size did not separate the two (brotli's
/// `UpdateNodes` win is a 19-instruction loop, the html losers 6 and 18).
const MIN_LOOP: usize = 0;

/// Address instructions rewritten per function.
const MAX_REWRITES: usize = 256;

/// A rewrite group: one new pointer parameter.
struct Group {
    header: Block,
    iv: Value,
    base: Value,
    stride: i64,
    /// The constant part of the first address (`k0`).
    k0: i64,
    /// Back-edge step of `iv`.
    step: i64,
    /// (address instruction, its constant part `k`).
    uses: Vec<(Inst, i64)>,
    /// Instructions in the loop (for the trace).
    lsize: usize,
}

fn is_pow2(s: i64) -> bool {
    s != 0 && (s.unsigned_abs() & (s.unsigned_abs() - 1)) == 0
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    run_with(func, an, pliron_cfg::env_get("PLIRON_IVSR_MIN").and_then(|v| v.parse().ok()).unwrap_or(MIN_LOOP))
}

/// `min_size`: the smallest loop (instructions, inner loops included) taken.
pub fn run_with(func: &mut Function, an: &mut Analysis, min_size: usize) -> usize {
    let mut groups: Vec<Group> = Vec::new();
    {
        let CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
        // Instructions per loop, inner loops included: each block adds its
        // count to its innermost loop and every loop around it.
        let mut size: FxHashMap<cranelift_codegen::loop_analysis::Loop, usize> = FxHashMap::default();
        for b in func.layout.blocks() {
            let n = func.layout.block_insts(b).count();
            let mut l = la.innermost_loop(b);
            while let Some(x) = l {
                *size.entry(x).or_default() += n;
                l = la.loop_parent(x);
            }
        }
        for lp in la.loops() {
            let h = la.loop_header(lp);
            let lsize = size.get(&lp).copied().unwrap_or(0);
            if lsize < min_size {
                continue;
            }
            // Header predecessors: all `jump`/`brif`; split in/out of loop.
            let mut back: Vec<Block> = Vec::new();
            let mut entry: Vec<Block> = Vec::new();
            let mut ok = true;
            for pred in cfg.pred_iter(h) {
                let t = pred.inst;
                if !matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif) {
                    ok = false;
                    break;
                }
                if la.is_in_loop(pred.block, lp) {
                    back.push(pred.block);
                } else {
                    entry.push(pred.block);
                }
            }
            if !ok || back.is_empty() || entry.is_empty() {
                continue;
            }
            // Induction params: every back-edge arg is `param + c`.
            let params: Vec<Value> = func.dfg.block_params(h).to_vec();
            let mut step: FxHashMap<Value, i64> = FxHashMap::default();
            'p: for (k, &pv) in params.iter().enumerate() {
                if func.dfg.value_type(pv) != types::I64 {
                    continue;
                }
                let mut c: Option<i64> = None;
                for &b in &back {
                    let t = func.layout.last_inst(b).unwrap();
                    for bc in func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                        if bc.block(&func.dfg.value_lists) != h {
                            continue;
                        }
                        let Some(BlockArg::Value(a)) = bc.args(&func.dfg.value_lists).nth(k) else { continue 'p };
                        let l = lin(func, a, 0);
                        if l.t.len() != 1 || l.t[0] != (pv, 1) {
                            continue 'p;
                        }
                        match c {
                            None => c = Some(l.k),
                            Some(x) if x == l.k => {}
                            _ => continue 'p,
                        }
                    }
                }
                if let Some(c) = c {
                    if c != 0 {
                        step.insert(pv, c);
                    }
                }
            }
            if step.is_empty() {
                continue;
            }
            let in_loop = |v: Value, func: &Function| -> bool {
                let b = match func.dfg.value_def(v) {
                    ValueDef::Result(i, _) => func.layout.inst_block(i),
                    ValueDef::Param(b, _) => Some(b),
                    _ => None,
                };
                b.is_some_and(|b| la.is_in_loop(b, lp))
            };
            // Values passed back to the header: an address among them is a
            // pointer the loop already carries, only rebuilt from the index
            // each trip (lol_html's selector VM: `p' = base + 120*(i + 1)`).
            // Replacing it adds a second register for the same pointer.
            let mut carried: rustc_hash::FxHashSet<Value> = rustc_hash::FxHashSet::default();
            for &b in &back {
                let t = func.layout.last_inst(b).unwrap();
                for bc in func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                    if bc.block(&func.dfg.value_lists) == h {
                        for a in bc.args(&func.dfg.value_lists) {
                            if let BlockArg::Value(a) = a {
                                carried.insert(func.dfg.resolve_aliases(a));
                            }
                        }
                    }
                }
            }
            // Candidates in the loop's blocks.
            let first_group = groups.len();
            let mut local: FxHashMap<(Value, Value, i64), usize> = FxHashMap::default();
            for b in func.layout.blocks() {
                if !la.is_in_loop(b, lp) {
                    continue;
                }
                // Only the innermost loop's own blocks: an inner loop's
                // header handles its own.
                if la.innermost_loop(b) != Some(lp) {
                    continue;
                }
                for inst in func.layout.block_insts(b) {
                    let InstructionData::Binary { opcode: Opcode::Iadd, args } = func.dfg.insts[inst] else { continue };
                    if func.dfg.value_type(func.dfg.first_result(inst)) != types::I64
                        || carried.contains(&func.dfg.first_result(inst))
                    {
                        continue;
                    }
                    for (bi, xi) in [(0, 1), (1, 0)] {
                        let base = func.dfg.resolve_aliases(args[bi]);
                        if in_loop(base, func) {
                            continue;
                        }
                        let l = lin(func, args[xi], 0);
                        if l.t.len() != 1 {
                            continue;
                        }
                        let (iv, s) = l.t[0];
                        let Some(&c) = step.get(&iv) else { continue };
                        if is_pow2(s) || s == 1 || s == -1 {
                            continue;
                        }
                        // A header parameter already stepping by `S*c` is
                        // the pointer IV (lol_html's selector VM carries
                        // its 120-byte stack cursor): a second one is a
                        // register more for the same work.
                        let sc = s.wrapping_mul(c);
                        if step.values().any(|&x| x == sc) {
                            continue;
                        }
                        let key = (iv, base, s);
                        let g = *local.entry(key).or_insert_with(|| {
                            groups.push(Group { header: h, iv, base, stride: s, k0: l.k, step: c, uses: Vec::new(), lsize });
                            groups.len() - 1
                        });
                        groups[g].uses.push((inst, l.k));
                        break;
                    }
                }
            }
            // The new parameter is live on every trip, so a group pays only
            // when one of its addresses is computed on every trip too (its
            // block dominates each latch): an address in a conditional arm
            // saved its multiply on that arm's trips alone, and the extra
            // register cost spills elsewhere (html-rewriter +0.07%).
            let every_trip = |b: Block| back.iter().all(|&l| dt.block_dominates(b, l));
            let mut k = first_group;
            for g in first_group..groups.len() {
                let keep = groups[g].uses.iter().any(|&(i, _)| func.layout.inst_block(i).is_some_and(|b| every_trip(b)));
                if keep {
                    groups.swap(k, g);
                    k += 1;
                }
            }
            groups.truncate(k);
        }
    }
    let mut n = 0;
    for g in groups {
        if n + g.uses.len() > MAX_REWRITES || !pliron_cfg::bisect("ivsr") {
            break;
        }
        tracing::debug!(target: "pliron::ivsr", "header {} iv {} base {} stride {} step {} uses {} loop {}", g.header, g.iv, g.base, g.stride, g.step, g.uses.len(), g.lsize);
        rewrite(func, &g);
        n += g.uses.len();
    }
    if n > 0 {
        an.invalidate(func);
    }
    n
}

fn rewrite(func: &mut Function, g: &Group) {
    let h = g.header;
    let iv_idx = func.dfg.block_params(h).iter().position(|&p| p == g.iv).expect("iv is a header param");
    let p = func.dfg.append_block_param(h, types::I64);
    // Every predecessor terminator passes the new argument.
    let preds: Vec<Inst> = {
        let mut v = Vec::new();
        for b in func.layout.blocks() {
            if let Some(t) = func.layout.last_inst(b) {
                if func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .any(|bc| bc.block(&func.dfg.value_lists) == h)
                {
                    v.push(t);
                }
            }
        }
        v
    };
    for t in preds {
        let n_dests = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables).len();
        for d in 0..n_dests {
            let bc = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
            if bc.block(&func.dfg.value_lists) != h {
                continue;
            }
            let Some(BlockArg::Value(a)) = bc.args(&func.dfg.value_lists).nth(iv_idx) else {
                unreachable!("induction args are values")
            };
            let mut cur = FuncCursor::new(func).at_inst(t);
            // A back edge passes `iv + step`: the new value is `p + S*step`.
            // An entry edge: `base + S*a + k0`.
            let la = lin(cur.func, a, 0);
            let val = if la.t.len() == 1 && la.t[0] == (g.iv, 1) {
                cur.ins().iadd_imm(p, g.stride.wrapping_mul(g.step))
            } else {
                let m = cur.ins().imul_imm(a, g.stride);
                let m = if g.k0 != 0 { cur.ins().iadd_imm(m, g.k0) } else { m };
                cur.ins().iadd(g.base, m)
            };
            let dfg = &mut func.dfg;
            let bcm = &mut dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
            bcm.append_argument(val, &mut dfg.value_lists);
        }
    }
    // The addresses.
    for &(inst, k) in &g.uses {
        let d = k.wrapping_sub(g.k0);
        if d == 0 {
            let r = func.dfg.first_result(inst);
            func.layout.remove_inst(inst);
            func.dfg.clear_results(inst);
            func.dfg.change_to_alias(r, p);
        } else {
            func.replace(inst).iadd_imm(p, d);
        }
    }
}
