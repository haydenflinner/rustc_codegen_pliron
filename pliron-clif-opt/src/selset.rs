//! Set membership held in a value → range check + bit test
//! (default on; `PLIRON_SELSET=0` off).
//!
//! `bittab` folds membership *trees* of `icmp`/`brif` blocks. Once
//! `ifconv` has run, a predicate such as
//! `c == b'\t' || c == 0x0b || c == 0x0c || c == b' '` is no longer a
//! tree: the first test is a branch and the rest a chain of selects,
//!
//! ```text
//! P:  brif (icmp eq x, 9), hit, B
//! B:  v = select (icmp eq x, 11), 1, (select (icmp eq x, 12), 1, (icmp eq x, 32))
//!     brif v, hit, miss
//! ```
//!
//! four compares, three conditional selects and two branches (ten
//! instructions on aarch64) where LLVM emits `cmp x, #32; b.hi miss;
//! lsr m, mask, x; tbz m, #0, miss`. pulldown-cmark's scanners test a byte
//! like this on every iteration (`scanners.rs` runs 1.6x LLVM's
//! instructions; `is_ascii_whitespace_no_nl` under `scan_while`).
//!
//! The pass reads the set a `brif`'s condition accepts — `icmp eq x, K`
//! leaves joined by `select c, 1, rest`, `bor`, and 0/1-preserving
//! `band _, 1` / extensions — and, when it spans at most 64 values,
//! rewrites the block's end to
//!
//! ```text
//! B:  d = x - lo                     ; `x` itself when the set sits in 0..=63
//!     brif (icmp ule d, hi - lo), T, miss
//! T:  brif (band (ushr mask, d), 1), hit, miss
//! ```
//!
//! A predecessor that tests one more constant of the same `x` and goes
//! to the same `hit` (the branch `ifconv` left in front) joins the set
//! when `B` holds nothing but the predicate; it then just jumps to `B`.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value};
use std::collections::BTreeSet;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// Zero-extended constant of `x`'s type.
fn zext(func: &Function, x: Value, k: i64) -> u64 {
    let bits = func.dfg.value_type(x).bits();
    if bits >= 64 { k as u64 } else { (k as u64) & ((1u64 << bits) - 1) }
}

/// The values of `x` for which `v` is non-zero, when `v` is a 0/1
/// membership expression over one `x`. `x` is fixed by the first leaf.
fn set_of(func: &Function, v: Value, x: &mut Option<Value>, out: &mut BTreeSet<u64>, fuel: &mut u32) -> bool {
    if *fuel == 0 {
        return false;
    }
    *fuel -= 1;
    let v = func.dfg.resolve_aliases(v);
    let Some(i) = func.dfg.value_def(v).inst() else { return false };
    match func.dfg.insts[i] {
        InstructionData::IntCompare { cond: IntCC::Equal, args, .. } => {
            let (a, b) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
            let (var, k) = match (iconst(func, a), iconst(func, b)) {
                (None, Some(k)) => (a, k),
                (Some(k), None) => (b, k),
                _ => return false,
            };
            let ty = func.dfg.value_type(var);
            if !ty.is_int() || ty.bits() > 64 {
                return false;
            }
            match *x {
                Some(x0) if x0 != var => return false,
                _ => *x = Some(var),
            }
            out.insert(zext(func, var, k));
            true
        }
        // `c || rest` after if-conversion.
        InstructionData::Ternary { opcode: Opcode::Select, args } => {
            iconst(func, args[1]).is_some_and(|k| k & 1 == 1 && (k == 1 || func.dfg.value_type(args[1]).bits() == 8 && k & 0xff == 1))
                && set_of(func, args[0], x, out, fuel)
                && set_of(func, args[2], x, out, fuel)
        }
        InstructionData::Binary { opcode: Opcode::Bor, args } => {
            set_of(func, args[0], x, out, fuel) && set_of(func, args[1], x, out, fuel)
        }
        // 0/1 in, 0/1 out.
        InstructionData::Binary { opcode: Opcode::Band, args } => {
            if iconst(func, args[1]) == Some(1) {
                set_of(func, args[0], x, out, fuel)
            } else if iconst(func, args[0]) == Some(1) {
                set_of(func, args[1], x, out, fuel)
            } else {
                false
            }
        }
        InstructionData::Unary { opcode: Opcode::Uextend | Opcode::Ireduce, arg } => set_of(func, arg, x, out, fuel),
        _ => false,
    }
}

fn dest(func: &Function, bc: cranelift_codegen::ir::BlockCall) -> (Block, Vec<BlockArg>) {
    (bc.block(&func.dfg.value_lists), bc.args(&func.dfg.value_lists).collect())
}

/// Only instructions with no effect and no trap: `b` may then run on a
/// path that used to skip it.
fn pure_block(func: &Function, b: Block, term: Inst) -> bool {
    func.layout.block_insts(b).all(|i| {
        i == term
            || matches!(
                func.dfg.insts[i].opcode(),
                Opcode::Iconst
                    | Opcode::Icmp
                    | Opcode::Select
                    | Opcode::Band
                    | Opcode::Bor
                    | Opcode::Uextend
                    | Opcode::Ireduce
            )
    })
}

/// Returns the number of predicates rewritten.
pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut n = 0;
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let InstructionData::Brif { arg, blocks: [bt, bf], .. } = func.dfg.insts[t] else { continue };
        let (mut x, mut set, mut fuel) = (None, BTreeSet::new(), 24u32);
        if !set_of(func, arg, &mut x, &mut set, &mut fuel) {
            continue;
        }
        let Some(x) = x else { continue };
        let (hit, miss) = (dest(func, bt), dest(func, bf));
        if hit == miss {
            continue;
        }
        // The branch if-conversion left in front: `P: brif (x == K), hit, b`.
        let mut front: Option<(Inst, u64)> = None;
        if Some(b) != entry && func.dfg.block_params(b).is_empty() && pure_block(func, b, t) {
            let mut ps = cfg.pred_iter(b);
            if let (Some(p), None) = (ps.next(), ps.next())
                && let InstructionData::Brif { arg: pa, blocks: [pt, pf], .. } = func.dfg.insts[p.inst]
            {
                let (mut px, mut pset, mut pfuel) = (None, BTreeSet::new(), 4u32);
                if set_of(func, pa, &mut px, &mut pset, &mut pfuel)
                    && px == Some(x)
                    && pset.len() == 1
                    && dest(func, pt) == hit
                    && dest(func, pf) == (b, Vec::new())
                {
                    front = Some((p.inst, *pset.iter().next().unwrap()));
                } else if let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(pa)).inst()
                    && let InstructionData::IntCompare { cond: IntCC::NotEqual, args, .. } = func.dfg.insts[ci]
                    && dest(func, pf) == hit
                    && dest(func, pt) == (b, Vec::new())
                {
                    // The same branch, inverted: `brif (x != K), b, hit`.
                    let (a0, a1) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
                    if a0 == x && let Some(k) = iconst(func, a1) {
                        front = Some((p.inst, zext(func, x, k)));
                    } else if a1 == x && let Some(k) = iconst(func, a0) {
                        front = Some((p.inst, zext(func, x, k)));
                    }
                }
            }
        }
        let mut all = set.clone();
        if let Some((_, k)) = front {
            all.insert(k);
        }
        // Worth it from three values on: a compare and a select each
        // against one compare, one shift and one bit test in total.
        if all.len() < 3 {
            continue;
        }
        let (lo, hi) = (*all.iter().next().unwrap(), *all.iter().next_back().unwrap());
        if hi - lo > 63 {
            continue;
        }
        // No subtraction when the whole set already sits in 0..=63.
        let lo = if hi <= 63 { 0 } else { lo };
        let mask = all.iter().fold(0u64, |m, &k| m | (1u64 << (k - lo)));
        if !pliron_cfg::bisect("selset") {
            continue;
        }
        let ty = func.dfg.value_type(x);
        let tb = func.dfg.make_block();
        func.layout.insert_block_after(tb, b);
        func.layout.remove_inst(t);
        let mut pos = FuncCursor::new(func).at_bottom(b);
        let d = if lo == 0 {
            x
        } else {
            let k = pos.ins().iconst(ty, lo as i64);
            pos.ins().isub(x, k)
        };
        let span = pos.ins().iconst(ty, (hi - lo) as i64);
        let inr = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, d, span);
        pos.ins().brif(inr, tb, &[], miss.0, &miss.1);
        let mut pos = FuncCursor::new(func).at_bottom(tb);
        let d64 = if ty.bits() < 64 { pos.ins().uextend(types::I64, d) } else { d };
        let m = pos.ins().iconst(types::I64, mask as i64);
        let sh = pos.ins().ushr(m, d64);
        let one = pos.ins().iconst(types::I64, 1);
        let bit = pos.ins().band(sh, one);
        pos.ins().brif(bit, hit.0, &hit.1, miss.0, &miss.1);
        if let Some((pi, _)) = front {
            let pb = func.layout.inst_block(pi).unwrap();
            func.layout.remove_inst(pi);
            FuncCursor::new(func).at_bottom(pb).ins().jump(b, &[]);
        }
        n += 1;
    }
    n
}
