//! Affine address rebase onto a canonical pointer IV (`PLIRON_LSR=1`).
//!
//! The UTF-8 validator's 128-byte scan carries two index params 16 bytes
//! apart; every 16-byte slot forms its address as `iadd (i + K), base` —
//! two adds per slot in emitted code, where LLVM's LSR carries one
//! pointer and folds each slot into `[p, #K]`:
//!
//! ```text
//! H(i1, i2):   p0 = iadd i1, b            H(i1, i2):   p0 = iadd i1, b
//!              a2 = iadd i2, b        →                a2 = iadd p0, 16
//!              a3 = iadd (i2 + 16), b                  a3 = iadd p0, 32
//!              (i2 = i1 + 16 invariant)                ...
//! ```
//!
//! `p_m = p_r + D` is provable by induction without knowing either
//! recurrence: the difference is a constant `D` on every entry edge and
//! `lin(back_m) - lin(back_r)` reduces to the same `D` on every back
//! edge (param leaves cancel symbolically). Normalizing each address
//! addend's lin form through those relations exposes `root + K` forms;
//! `iadd X, b` with `X ≡ root + K` under a dominating
//! `c = iadd root, b` becomes `iadd c, K`. Everything is wrapping-add
//! reassociation — no nowrap premise — so the rewrite is always sound;
//! the win is the folded address mode and the shared pointer register.
//!
//! Scoped to `i64` `iadd X, b` where `b` is defined outside the loop
//! (an invariant pointer/integer — `iconst` bases are `rebase`'s) and
//! `X` normalizes to exactly `root + K`: a lone root leaf, no others.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    types, Block, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
    ValueDef,
};
use cranelift_codegen::ir::BlockArg;
use rustc_hash::FxHashMap;

use crate::ivptr::{lin, Lin};
use crate::pipeline::{Analysis, CfgDtLa};

/// Cap on canonical candidates tested per rewrite — dominance checks are
/// O(group); a degenerate loop body stays linear.
const MAX_CANDS: usize = 32;

/// The owning block of a value's def: inst's block, or the param's block.
fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

fn is_iconst(func: &Function, v: Value) -> bool {
    func.dfg
        .value_def(func.dfg.resolve_aliases(v))
        .inst()
        .is_some_and(|i| {
            matches!(
                func.dfg.insts[i],
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    ..
                }
            )
        })
}

/// Union-find with offsets: `p_x = p_root(x) + off(x)`.
struct Uf {
    parent: FxHashMap<Value, (Value, i64)>,
}

impl Uf {
    fn find(&mut self, x: Value) -> (Value, i64) {
        match self.parent.get(&x).copied() {
            None => (x, 0),
            Some((p, d)) => {
                let (r, dp) = self.find(p);
                let d = d.wrapping_add(dp);
                self.parent.insert(x, (r, d));
                (r, d)
            }
        }
    }
    /// Record `x = y + d`.
    fn union(&mut self, x: Value, y: Value, d: i64) {
        let (rx, dx) = self.find(x);
        let (ry, dy) = self.find(y);
        if rx != ry {
            // x = rx + dx and y = ry + dy, so rx = ry + (d + dy - dx).
            self.parent
                .insert(rx, (ry, d.wrapping_add(dy).wrapping_sub(dx)));
        }
    }
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    // (inst to rewrite, canonical value, new offset)
    let mut edits: Vec<(Inst, Value, i64)> = Vec::new();
    {
        let CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
        // Blocks per loop (nested included), one layout walk.
        let loops: Vec<_> = la.loops().collect();
        let mut members: FxHashMap<_, Vec<Block>> = FxHashMap::default();
        for b in func.layout.blocks() {
            let mut l = la.innermost_loop(b);
            while let Some(x) = l {
                members.entry(x).or_default().push(b);
                l = la.loop_parent(x);
            }
        }
        for lp in loops {
            let h = la.loop_header(lp);
            let params: Vec<Value> = func.dfg.block_params(h).to_vec();
            if params.len() < 2 {
                continue;
            }
            // Edge args into `h` per predecessor arm; all must be plain
            // values matching the param count.
            let mut edges: Vec<(Vec<Value>, bool)> = Vec::new();
            let mut ok = true;
            for p in cfg.pred_iter(h) {
                for bc in func.dfg.insts[p.inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .filter(|bc| bc.block(&func.dfg.value_lists) == h)
                {
                    let args = bc
                        .args(&func.dfg.value_lists)
                        .map(|a| match a {
                            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>();
                    match args {
                        Some(a) if a.len() == params.len() => {
                            edges.push((a, la.is_in_loop(p.block, lp)))
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
            if !ok || !edges.iter().any(|e| e.1) || !edges.iter().any(|e| !e.1) {
                continue;
            }
            // Param-difference invariants: `p_m = p_r + D` with `D` a
            // constant identical on every edge into `h` — entry edges
            // establish it, and on a back edge `lin(back_m) - lin(back_r)`
            // reduces to `D` with all param leaves cancelling.
            let i64p: Vec<usize> = (0..params.len())
                .filter(|&i| func.dfg.value_type(params[i]) == types::I64)
                .collect();
            let mut uf = Uf {
                parent: FxHashMap::default(),
            };
            for &r in &i64p {
                for &m in &i64p {
                    if m == r {
                        continue;
                    }
                    let mut d: Option<i64> = None;
                    let mut good = true;
                    for (args, _) in &edges {
                        let df = lin(func, args[m], 0).add(&lin(func, args[r], 0), -1);
                        match (df.t.is_empty().then_some(df.k), d) {
                            (Some(k), None) => d = Some(k),
                            (Some(k), Some(x)) if k == x => {}
                            _ => {
                                good = false;
                                break;
                            }
                        }
                    }
                    if let Some(k) = d.filter(|_| good) {
                        uf.union(params[m], params[r], k);
                    }
                }
            }
            // `iadd X, b` in the loop, i64 result, `b` defined outside —
            // group by (b, root) after param substitution.
            let mut groups: FxHashMap<(Value, Value), Vec<(Inst, Value, i64)>> =
                FxHashMap::default();
            let pset: rustc_hash::FxHashSet<Value> = params.iter().copied().collect();
            let Some(blks) = members.get(&lp) else { continue };
            for &blk in blks {
                for i in func.layout.block_insts(blk) {
                    let InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args,
                    } = func.dfg.insts[i]
                    else {
                        continue;
                    };
                    let Some(&rv) = func.dfg.inst_results(i).first() else {
                        continue;
                    };
                    if func.dfg.value_type(rv) != types::I64 {
                        continue;
                    }
                    for side in 0..2 {
                        let (x, b) = (args[side], args[side ^ 1]);
                        let b = func.dfg.resolve_aliases(b);
                        if is_iconst(func, b)
                            || !def_block(func, b).is_some_and(|bb| !la.is_in_loop(bb, lp))
                        {
                            continue;
                        }
                        // Substitute each header-param leaf with `root + d`.
                        let f0 = lin(func, x, 0);
                        let mut f = Lin {
                            k: f0.k,
                            t: vec![],
                        };
                        for (lv, lc) in f0.t {
                            let (r, d) = if pset.contains(&lv) {
                                uf.find(lv)
                            } else {
                                (lv, 0)
                            };
                            f = f.add(
                                &Lin {
                                    k: d.wrapping_mul(lc),
                                    t: vec![(r, lc)],
                                },
                                1,
                            );
                        }
                        let [(root, 1)] = f.t[..] else { continue };
                        groups.entry((b, root)).or_default().push((i, rv, f.k));
                    }
                }
            }
            for members in groups.into_values() {
                for &(ri, _, rk) in &members {
                    let mut best: Option<(Inst, Value, i64)> = None;
                    for &(ci, cv, ck) in members.iter().take(MAX_CANDS) {
                        if ci == ri || !dt.dominates(ci, ri, &func.layout) {
                            continue;
                        }
                        // Prefer the K=0 canonical (a bare `iadd root, b`).
                        if ck == 0 {
                            best = Some((ci, cv, ck));
                            break;
                        }
                        if best.is_none() {
                            best = Some((ci, cv, ck));
                        }
                    }
                    if let Some((_, cv, ck)) = best
                        && pliron_cfg::bisect("lsr")
                    {
                        edits.push((ri, cv, rk.wrapping_sub(ck)));
                    }
                }
            }
        }
    }
    let n = edits.len();
    for (i, c, k) in edits {
        if k == 0 {
            let x = func.dfg.inst_results(i)[0];
            func.layout.remove_inst(i);
            func.dfg.detach_inst_results(i);
            func.dfg.change_to_alias(x, c);
            continue;
        }
        let mut pos = FuncCursor::new(func).at_inst(i);
        let kk = pos.ins().iconst(types::I64, k);
        pos.func.dfg.insts[i] = InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [c, kk],
        };
    }
    if n > 0 {
        an.invalidate(func);
    }
    n
}
