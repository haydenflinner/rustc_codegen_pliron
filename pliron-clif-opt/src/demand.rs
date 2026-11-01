//! Demanded-bits simplification of xor constants (`PLIRON_DEMAND`).
//!
//! SWAR byte scans test only each byte's top bit: serde_json's
//! `skip_to_escape` builds `(c - 0x20) & !c | (q - 1) & !q | (b - 1) & !b`
//! with `q = c ^ 0x22..`, `b = c ^ 0x5c..`, then masks with `0x8080..`.
//! Earlier folds turn `!q` into `c ^ 0xdd..` and `!b` into `c ^ 0xa3..`;
//! under that mask both equal `!c` (the constants' bit 7 is set in every
//! byte), so the three `and`s share one operand and the egraph factors
//! them into one `bic` — LLVM's loop has one, ours three.
//!
//! A value's demanded bits are the union over its users of the bits each
//! user reads: a constant `band` reads its mask's bits of the demanded
//! result, other bitwise ops (`band`/`bor`/`bxor`/`bnot`) pass the
//! result's demand through, anything else reads every bit. `bxor x, K`
//! whose demand `D` has `K & D == D` is `bnot x` on those bits, and with
//! `K & D == 0` it is `x`.

use cranelift_codegen::ir::{Function, Inst, InstBuilder, InstructionData, Opcode, Value};
use rustc_hash::FxHashMap;

fn iconst(func: &Function, v: Value) -> Option<u64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits() as u64),
        _ => None,
    }
}

struct Demand<'a> {
    func: &'a Function,
    users: &'a FxHashMap<Value, Vec<Inst>>,
    memo: FxHashMap<Value, u64>,
}

impl Demand<'_> {
    fn of(&mut self, v: Value, depth: u32) -> u64 {
        if let Some(&d) = self.memo.get(&v) {
            return d;
        }
        let ty = self.func.dfg.value_type(v);
        let all = if ty.bits() >= 64 { u64::MAX } else { (1u64 << ty.bits()) - 1 };
        if depth > 8 || !ty.is_int() || ty.bits() > 64 {
            return all;
        }
        let mut d = 0u64;
        for &u in self.users.get(&v).map_or(&[][..], |u| &u[..]) {
            let data = &self.func.dfg.insts[u];
            let res = self.func.dfg.inst_results(u).first().copied();
            let part = match (data.opcode(), data) {
                (Opcode::Band, InstructionData::Binary { args, .. }) => {
                    let other = if self.func.dfg.resolve_aliases(args[0]) == v { args[1] } else { args[0] };
                    let rd = self.of(res.unwrap(), depth + 1);
                    iconst(self.func, other).map_or(rd, |c| rd & c)
                }
                (Opcode::Bor | Opcode::Bxor, _) | (Opcode::Bnot, _) => self.of(res.unwrap(), depth + 1),
                _ => all,
            };
            d |= part;
            if d == all {
                break;
            }
        }
        self.memo.insert(v, d);
        d
    }
}

pub fn run(func: &mut Function) -> usize {
    let mut users: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                users.entry(func.dfg.resolve_aliases(v)).or_default().push(i);
            }
        }
    }
    // (xor inst, x, to bnot (else alias to x))
    let mut edits: Vec<(Inst, Value, bool)> = Vec::new();
    {
        let mut dm = Demand { func, users: &users, memo: FxHashMap::default() };
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                let InstructionData::Binary { opcode: Opcode::Bxor, args } = func.dfg.insts[i] else { continue };
                let (x, k) = match (iconst(func, args[0]), iconst(func, args[1])) {
                    (_, Some(k)) => (args[0], k),
                    (Some(k), _) => (args[1], k),
                    _ => continue,
                };
                let r = func.dfg.first_result(i);
                let ty = func.dfg.value_type(r);
                let all = if ty.bits() >= 64 { u64::MAX } else { (1u64 << ty.bits()) - 1 };
                let k = k & all;
                // Already `bnot`/`x` on every bit.
                if k == all || k == 0 {
                    continue;
                }
                let d = dm.of(r, 0);
                if d == all {
                    continue;
                }
                if k & d == d {
                    edits.push((i, x, true));
                } else if k & d == 0 {
                    edits.push((i, x, false));
                }
            }
        }
    }
    let mut n = 0;
    for (i, x, not) in edits {
        if !pliron_cfg::bisect("demand") {
            continue;
        }
        if not {
            func.replace(i).bnot(x);
        } else {
            let r = func.dfg.first_result(i);
            func.layout.remove_inst(i);
            func.dfg.clear_results(i);
            func.dfg.change_to_alias(r, x);
        }
        n += 1;
    }
    if n > 0 {
        n += factor_nots(func);
    }
    n
}

/// `x` when `v` is `bnot x` or `bxor x, -1`.
fn not_of(func: &Function, v: Value) -> Option<Value> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::Unary { opcode: Opcode::Bnot, arg } => Some(func.dfg.resolve_aliases(arg)),
        InstructionData::Binary { opcode: Opcode::Bxor, args } => {
            let ty = func.dfg.value_type(v);
            let all = if ty.bits() >= 64 { u64::MAX } else { (1u64 << ty.bits()) - 1 };
            match (iconst(func, args[0]).map(|k| k & all), iconst(func, args[1]).map(|k| k & all)) {
                (_, Some(k)) if k == all => Some(func.dfg.resolve_aliases(args[0])),
                (Some(k), _) if k == all => Some(func.dfg.resolve_aliases(args[1])),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `band a, !x` (either order, `!x` as `bnot`/`bxor -1`): `(a, x)`.
fn and_not(func: &Function, v: Value) -> Option<(Value, Value)> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    let InstructionData::Binary { opcode: Opcode::Band, args } = func.dfg.insts[i] else { return None };
    if let Some(x) = not_of(func, args[1]) {
        Some((args[0], x))
    } else {
        not_of(func, args[0]).map(|x| (args[1], x))
    }
}

/// `bor (band a, !x), (band b, !x)` -> `band (bor a, b), !x` when both
/// `band`s feed only this `bor`: the compile-time egraph turns each
/// `band _, bnot x` into `band_not` before its own factoring rule could
/// match. Runs only after `run` rewrote something.
fn factor_nots(func: &mut Function) -> usize {
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *uses.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let InstructionData::Binary { opcode: Opcode::Bor, args } = func.dfg.insts[i] else { continue };
            let (p, q) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
            if uses.get(&p) != Some(&1) || uses.get(&q) != Some(&1) {
                continue;
            }
            let (Some((a, x)), Some((bb, y))) = (and_not(func, p), and_not(func, q)) else { continue };
            if x != y {
                continue;
            }
            let nx = {
                let pi = func.dfg.value_def(p).unwrap_inst();
                let pa = func.dfg.inst_args(pi);
                if not_of(func, pa[1]) == Some(x) { pa[1] } else { pa[0] }
            };
            let mut cur = cranelift_codegen::cursor::FuncCursor::new(func).at_inst(i);
            use cranelift_codegen::cursor::Cursor;
            let o = cur.ins().bor(a, bb);
            cur.func.replace(i).band(o, nx);
            // The or now reads `a`/`b` directly; the old `band`s go dead.
            *uses.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            *uses.entry(func.dfg.resolve_aliases(bb)).or_default() += 1;
            n += 1;
        }
    }
    n
}
