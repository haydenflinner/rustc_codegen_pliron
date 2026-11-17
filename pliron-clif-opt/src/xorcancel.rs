//! Cancellation of repeated leaves in xor trees (`PLIRON_XORCANCEL=1`
//! enables).
//!
//! SipHash-1-3's last round ends `v3 = rotl(v3, 21) ^ v0`, and the hash is
//! `v0 ^ v1 ^ v2 ^ v3`: `v0` occurs twice in that tree, so the result is
//! `v1 ^ v2 ^ rotl(v3, 21)`, and `v0`'s last `rotl 32` and `iadd` die with
//! it. LLVM's reassociation finds this; the egraph only cancels `x ^ x`
//! when both operands meet in one `bxor`. Every std `HashMap` lookup with
//! the default hasher runs it (`hmget`: two `bxor`, one `rotl`, one `iadd`
//! per lookup).
//!
//! For each root `bxor` (a `bxor` whose value is not the single use of
//! another `bxor`), the leaves of its tree are collected through internal
//! `bxor` nodes that have exactly one use (each such node belongs to one
//! tree, so every instruction is walked once). A leaf that occurs an even
//! number of times cancels; when any does, the root is rebuilt as the xor
//! of the remaining leaves in their first-occurrence order (`0` when none
//! remain, the leaf itself when one does). The internal nodes become dead
//! and later steps remove them. A tree with more than `MAX_LEAVES` leaves
//! is left alone.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Function, Inst, InstBuilder, InstructionData, Opcode, Value};
use rustc_hash::FxHashMap;

const MAX_LEAVES: usize = 32;

fn bxor_args(func: &Function, inst: Inst) -> Option<[Value; 2]> {
    match func.dfg.insts[inst] {
        InstructionData::Binary { opcode: Opcode::Bxor, args } => Some(args),
        _ => None,
    }
}

/// Rewrites every xor tree with a repeated leaf; returns the trees changed.
pub fn run(func: &mut Function) -> usize {
    // Use counts and, for single-use values, the user.
    let mut uses: FxHashMap<Value, (u32, Option<Inst>)> = FxHashMap::default();
    let mut roots: Vec<Inst> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                let e = uses.entry(v).or_insert((0, None));
                e.0 += 1;
                e.1 = Some(i);
            }
            if bxor_args(func, i).is_some() {
                roots.push(i);
            }
        }
    }
    // An internal node: a `bxor` result whose only use is another `bxor`.
    let internal = |func: &Function, v: Value| -> bool {
        let Some(d) = func.dfg.value_def(v).inst() else {
            return false;
        };
        bxor_args(func, d).is_some()
            && uses
                .get(&v)
                .is_some_and(|&(n, u)| n == 1 && u.is_some_and(|u| bxor_args(func, u).is_some()))
    };
    let mut changed = 0;
    for root in roots {
        let res = func.dfg.first_result(root);
        if internal(func, res) {
            continue;
        }
        let ty = func.dfg.value_type(res);
        // Collect leaves, left to right.
        let mut leaves: Vec<Value> = Vec::new();
        let mut stack: Vec<Value> = bxor_args(func, root).unwrap().iter().rev().copied().collect();
        let mut over = false;
        while let Some(v) = stack.pop() {
            let v = func.dfg.resolve_aliases(v);
            if internal(func, v) {
                let d = func.dfg.value_def(v).inst().unwrap();
                let a = bxor_args(func, d).unwrap();
                stack.push(a[1]);
                stack.push(a[0]);
            } else {
                leaves.push(v);
            }
            if leaves.len() + stack.len() > MAX_LEAVES {
                over = true;
                break;
            }
        }
        if over {
            continue;
        }
        let mut count: FxHashMap<Value, usize> = FxHashMap::default();
        for &l in &leaves {
            *count.entry(l).or_default() += 1;
        }
        if count.values().all(|&n| n < 2) {
            continue;
        }
        let mut keep: Vec<Value> = Vec::new();
        for &l in &leaves {
            if count[&l] % 2 == 1 && !keep.contains(&l) {
                keep.push(l);
            }
        }
        if keep.is_empty() && !ty.is_int() {
            continue;
        }
        let mut pos = FuncCursor::new(func).at_inst(root);
        match keep.len() {
            0 => {
                pos.func.replace(root).iconst(ty, 0);
            }
            1 => {
                // `x | x` is `x`; the egraph folds it to the leaf.
                pos.func.replace(root).bor(keep[0], keep[0]);
            }
            n => {
                let mut acc = keep[0];
                for &l in &keep[1..n - 1] {
                    acc = pos.ins().bxor(acc, l);
                }
                pos.func.replace(root).bxor(acc, keep[n - 1]);
            }
        }
        changed += 1;
    }
    changed
}
