//! Value-range propagation (`PLIRON_VRP=1`): the first consumer of
//! [`crate::ranges`]. Folds what the ranges decide:
//!
//! - an `icmp` whose operands' ranges (at its block) settle it becomes a
//!   constant, and a `brif` on a constant becomes a `jump`;
//! - `band x, K` where `x`'s bits already lie within `K` is `x`;
//! - `uextend (ireduce x)` and `sextend (ireduce x)` back to the wide
//!   type are `x` when `x` fits the narrow type;
//! - `urem x, K` with `x < K` is `x`; `udiv x, K` with `x < K` is `0`;
//! - `uadd_overflow a, b` whose sum cannot wrap is `iadd` with a zero flag,
//!   and the hand-written carry test `icmp ult (iadd a, b), a` is `0`.
//!
//! Every fold is a value rewrite (`change_to_alias`) or a terminator
//! rewrite; unreachable code is left for the later steps. Returns the
//! number of folds. Ranges are rebuilt per run, so one pass over the
//! function; `PLIRON_VRP_STATS` is folded into `PLIRON_STATS`.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef};

use crate::pipeline::{Analysis, CfgDtLa};
use crate::ranges::{Range, Ranges, bits_of, decide};

enum Fix {
    /// Replace result `v` by `w`.
    Alias(Value, Value),
    /// Replace result `v` by constant `k` of its type.
    Const(Value, i64),
    /// `brif` at `inst` always takes arm `arm`.
    Jump(Inst, usize),
    /// `uadd_overflow` at `inst` never carries.
    NoCarry(Inst),
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let mut fixes: Vec<Fix> = Vec::new();
    {
        let CfgDtLa { cfg, dt, .. } = an.cfg_dt_la(func);
        let mut rs = Ranges::new(func, cfg);
        for b in func.layout.blocks() {
            if !dt.is_reachable(b) {
                continue;
            }
            for inst in func.layout.block_insts(b) {
                let data = func.dfg.insts[inst];
                match data {
                    InstructionData::IntCompare { opcode: Opcode::Icmp, cond, args: [x, y] } => {
                        let r = func.dfg.first_result(inst);
                        let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
                        if bits_of(func.dfg.value_type(x)).is_none() {
                            continue;
                        }
                        let a = rs.at(func, dt, x, b);
                        let c = rs.at(func, dt, y, b);
                        if let Some(t) = decide(cond, &a, &c) {
                            fixes.push(Fix::Const(r, t as i64));
                            continue;
                        }
                        // `icmp ult (iadd a, b), a`: carry test.
                        if cond == IntCC::UnsignedLessThan
                            && let ValueDef::Result(si, _) = func.dfg.value_def(x)
                            && let InstructionData::Binary { opcode: Opcode::Iadd, args: [p, q] } = func.dfg.insts[si]
                        {
                            let (p, q) = (func.dfg.resolve_aliases(p), func.dfg.resolve_aliases(q));
                            if p == y || q == y {
                                let ra = rs.at(func, dt, p, b);
                                let rb = rs.at(func, dt, q, b);
                                if no_carry(&ra, &rb) {
                                    fixes.push(Fix::Const(r, 0));
                                }
                            }
                        }
                    }
                    InstructionData::Brif { arg, blocks, .. } => {
                        let c = func.dfg.resolve_aliases(arg);
                        if blocks[0].block(&func.dfg.value_lists) == blocks[1].block(&func.dfg.value_lists) {
                            continue;
                        }
                        let r = rs.at(func, dt, c, b);
                        if let Some(k) = r.is_const() {
                            fixes.push(Fix::Jump(inst, if k != 0 { 0 } else { 1 }));
                        }
                    }
                    InstructionData::Binary { opcode, args: [x, y] } => {
                        let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
                        let res = func.dfg.first_result(inst);
                        // Constants are stored sign-extended: mask to the width.
                        let Some(wb) = bits_of(func.dfg.value_type(x)) else { continue };
                        let wmask = if wb >= 64 { u64::MAX } else { (1u64 << wb) - 1 };
                        match opcode {
                            Opcode::Band => {
                                let (v, k) = match (crate::loopidiom::iconst(func, x), crate::loopidiom::iconst(func, y)) {
                                    (_, Some(k)) => (x, k as u64 & wmask),
                                    (Some(k), _) => (y, k as u64 & wmask),
                                    _ => continue,
                                };
                                let r = rs.at(func, dt, v, b);
                                if cover(r.uhi) & !k == 0 {
                                    fixes.push(Fix::Alias(res, v));
                                }
                            }
                            Opcode::Urem | Opcode::Udiv => {
                                let Some(k) = crate::loopidiom::iconst(func, y) else { continue };
                                let k = k as u64 & wmask;
                                if k == 0 {
                                    continue;
                                }
                                let r = rs.at(func, dt, x, b);
                                if r.uhi < k {
                                    if opcode == Opcode::Urem {
                                        fixes.push(Fix::Alias(res, x));
                                    } else {
                                        fixes.push(Fix::Const(res, 0));
                                    }
                                }
                            }
                            Opcode::UaddOverflow => {
                                let ra = rs.at(func, dt, x, b);
                                let rb = rs.at(func, dt, y, b);
                                if no_carry(&ra, &rb) {
                                    fixes.push(Fix::NoCarry(inst));
                                }
                            }
                            _ => {}
                        }
                    }
                    InstructionData::Unary { opcode: op @ (Opcode::Uextend | Opcode::Sextend), arg } => {
                        // `uextend.T (ireduce.N x)` with x: T fitting N.
                        let a = func.dfg.resolve_aliases(arg);
                        let ValueDef::Result(ri, _) = func.dfg.value_def(a) else { continue };
                        let InstructionData::Unary { opcode: Opcode::Ireduce, arg: wide } = func.dfg.insts[ri] else { continue };
                        let wide = func.dfg.resolve_aliases(wide);
                        let res = func.dfg.first_result(inst);
                        if func.dfg.value_type(wide) != func.dfg.value_type(res) {
                            continue;
                        }
                        let Some(nb) = bits_of(func.dfg.value_type(a)) else { continue };
                        if nb >= 64 || bits_of(func.dfg.value_type(wide)).is_none() {
                            continue;
                        }
                        let r = rs.at(func, dt, wide, b);
                        let fits = match op {
                            Opcode::Uextend => r.uhi < (1u64 << nb),
                            _ => r.slo >= -(1i64 << (nb - 1)) && r.shi < (1i64 << (nb - 1)),
                        };
                        if fits {
                            fixes.push(Fix::Alias(res, wide));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if fixes.is_empty() {
        return 0;
    }
    let n = fixes.len();
    let mut cfg_changed = false;
    for f in fixes {
        match f {
            Fix::Alias(v, w) => {
                let Some(inst) = func.dfg.value_def(v).inst() else { continue };
                if v != w && func.dfg.inst_results(inst).len() == 1 {
                    func.dfg.clear_results(inst);
                    func.layout.remove_inst(inst);
                    func.dfg.change_to_alias(v, w);
                }
            }
            Fix::Const(v, k) => {
                let ty = func.dfg.value_type(v);
                let Some(inst) = func.dfg.value_def(v).inst() else { continue };
                if func.dfg.inst_results(inst).len() != 1 {
                    continue;
                }
                let mut pos = FuncCursor::new(func).at_inst(inst);
                let c = pos.ins().iconst(ty, k);
                func.dfg.clear_results(inst);
                func.layout.remove_inst(inst);
                func.dfg.change_to_alias(v, c);
            }
            Fix::Jump(inst, arm) => {
                let InstructionData::Brif { blocks, .. } = func.dfg.insts[inst] else { continue };
                let dest = blocks[arm];
                let block = dest.block(&func.dfg.value_lists);
                let args: Vec<cranelift_codegen::ir::BlockArg> = dest.args(&func.dfg.value_lists).collect();
                func.dfg.insts[inst] = InstructionData::Jump {
                    opcode: Opcode::Jump,
                    destination: cranelift_codegen::ir::BlockCall::new(block, args.iter().copied(), &mut func.dfg.value_lists),
                };
                cfg_changed = true;
            }
            Fix::NoCarry(inst) => {
                let InstructionData::Binary { args: [x, y], .. } = func.dfg.insts[inst] else { continue };
                let results: Vec<Value> = func.dfg.inst_results(inst).to_vec();
                let [sum, flag] = results[..] else { continue };
                let ty = func.dfg.value_type(sum);
                let mut pos = FuncCursor::new(func).at_inst(inst);
                let s = pos.ins().iadd(x, y);
                let z = pos.ins().iconst(cranelift_codegen::ir::types::I8, 0);
                let _ = ty;
                func.dfg.clear_results(inst);
                func.layout.remove_inst(inst);
                func.dfg.change_to_alias(sum, s);
                func.dfg.change_to_alias(flag, z);
            }
        }
    }
    if cfg_changed {
        an.invalidate(func);
    }
    n
}

/// `a + b` cannot carry out of the width.
fn no_carry(a: &Range, b: &Range) -> bool {
    let m = if a.bits >= 64 { u64::MAX } else { (1u64 << a.bits) - 1 };
    a.uhi.checked_add(b.uhi).is_some_and(|s| s <= m)
}

/// Every bit a value `<= uhi` can have set: all bits up to `uhi`'s top
/// bit (`uhi = 0x100` allows `0xff`, so the bound alone says nothing about
/// the low bits).
fn cover(uhi: u64) -> u64 {
    if uhi == 0 { 0 } else { u64::MAX >> uhi.leading_zeros() }
}

#[allow(dead_code)]
fn block_of(func: &Function, inst: Inst) -> Option<Block> {
    func.layout.inst_block(inst)
}
