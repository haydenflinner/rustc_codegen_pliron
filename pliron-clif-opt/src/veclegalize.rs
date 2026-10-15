//! Scalarize ops on a fixed vector type the ISA can't emit (x64: widths
//! below 128 bits, e.g. `i8x8` from rustc's core_simd/mask codegen).
//! Driven from lower.rs: compile is attempted, the error/panic names a
//! vector type, every inst touching that type is scalarized, and compile
//! is retried.
//!
//! View: a narrow-vector value becomes an integer of the same width via
//! `bitcast` (`i8x8`→`i64`). Rebuilds:
//!   - bitwise lane ops (band/bor/bxor/bnot) run unchanged on the int
//!   - lane-wise ALU ops expand to one scalar op per lane (same opcode)
//!   - icmp/fcmp lanes get `ineg` to rebuild the all-ones mask
//!   - splat = lane * 0x0101…01 (SWAR broadcast)
//!   - extractlane/insertlane/iconcat/isplit/load/store/valltrue/
//!     vanytrue/vhigh_bits get bespoke bit-splice rewrites
//!   - outputs of non-target (wide) vector types are assembled with an
//!     insertlane chain instead of a bitcast
//! Anything left over (calls, swizzle) still fails at emit and names the
//! inst.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{
    self, Endianness, InstBuilder, InstructionData, MemFlagsData, Opcode, StackSlot,
    StackSlotData, StackSlotKind, Type, Value,
};

/// Materialize a narrow-vector result from an int view: no CLIF op
/// produces a narrow vector on x64 except `load` (64-bit xmm helpers),
/// so spill the int to a slot and reload it as the vector type.
fn emit_vec_result(
    pos: &mut FuncCursor,
    inst: ir::Inst,
    ss: StackSlot,
    int_v: Value,
    target: Type,
) {
    let it = int_ty(target.bits() as u16);
    pos.ins().stack_store(it, int_v, ss, 0);
    let a = pos.ins().stack_addr(ir::types::I64, ss, 0);
    pos.func.replace(inst).load(
        target,
        MemFlagsData::trusted(),
        a,
        ir::immediates::Offset32::from(0),
    );
}

fn int_ty(bits: u16) -> Type {
    Type::int(bits).expect("integer type")
}

fn le_flags() -> MemFlagsData {
    MemFlagsData::new().with_endianness(Endianness::Little)
}

fn bitwise_ok(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Band | Opcode::Bor | Opcode::Bxor | Opcode::Bnot | Opcode::Select | Opcode::Bitcast
    )
}

fn lanewise(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Iadd
            | Opcode::Isub
            | Opcode::Imul
            | Opcode::Udiv
            | Opcode::Sdiv
            | Opcode::Urem
            | Opcode::Srem
            | Opcode::Umin
            | Opcode::Umax
            | Opcode::Smin
            | Opcode::Smax
            | Opcode::Rotl
            | Opcode::Rotr
            | Opcode::Ishl
            | Opcode::Ushr
            | Opcode::Sshr
            | Opcode::Ineg
            | Opcode::Iabs
            | Opcode::Umulhi
            | Opcode::Smulhi
            | Opcode::Fadd
            | Opcode::Fsub
            | Opcode::Fmul
            | Opcode::Fdiv
            | Opcode::Fmin
            | Opcode::Fmax
            | Opcode::Fma
            | Opcode::Fabs
            | Opcode::Fneg
            | Opcode::Sqrt
            | Opcode::Ceil
            | Opcode::Floor
            | Opcode::Trunc
            | Opcode::Nearest
            | Opcode::Uextend
            | Opcode::Sextend
            | Opcode::Ireduce
            | Opcode::Fpromote
            | Opcode::Fdemote
            | Opcode::FcvtToUint
            | Opcode::FcvtToSint
            | Opcode::FcvtFromUint
            | Opcode::FcvtFromSint
    )
}

fn cmp_op(op: Opcode) -> bool {
    matches!(op, Opcode::Icmp | Opcode::Fcmp)
}

/// Extract lane `i` (scalar type `lt`) of `v`, a value of vector type
/// `vt`. Narrow vectors go through the int view; wide ones use
/// `extractlane` (lowerable).
fn get_lane(pos: &mut FuncCursor, v: Value, vt: Type, i: u64, target: Type) -> Value {
    let lt = vt.lane_type();
    let esz = lt.bits() as u64;
    let le = le_flags();
    if vt == target || (vt.is_vector() && vt.bits() < 128) {
        let it = int_ty(vt.bits() as u16);
        let w = pos.ins().bitcast(it, le, v);
        let w = if i * esz > 0 {
            pos.ins().ushr_imm_u(w, (i * esz) as i64)
        } else {
            w
        };
        if lt.is_float() {
            let t = if it.bits() as u64 > esz {
                pos.ins().ireduce(int_ty(esz as u16), w)
            } else {
                w
            };
            pos.ins().bitcast(lt, le, t)
        } else if it.bits() as u64 > esz {
            pos.ins().ireduce(lt, w)
        } else {
            w
        }
    } else {
        pos.ins().extractlane(v, i as u8)
    }
}

/// Widen lane scalar `s` (type `lt`) to integer `it` keeping its bits.
fn lane_to_int(pos: &mut FuncCursor, s: Value, lt: Type, it: Type) -> Value {
    let le = le_flags();
    if lt.is_float() {
        let t = pos.ins().bitcast(int_ty(lt.bits() as u16), le, s);
        if it.bits() > lt.bits() {
            pos.ins().uextend(it, t)
        } else {
            t
        }
    } else if it.bits() > lt.bits() {
        pos.ins().uextend(it, s)
    } else {
        s
    }
}

fn iconst_wide(pos: &mut FuncCursor, it: Type, imm: u128) -> Value {
    if it.bits() <= 64 {
        pos.ins().iconst(it, imm as u64 as i64)
    } else {
        let lo = pos.ins().iconst(ir::types::I64, imm as u64 as i64);
        let hi = pos.ins().iconst(ir::types::I64, (imm >> 64) as u64 as i64);
        let lo = pos.ins().uextend(it, lo);
        let hi = pos.ins().uextend(it, hi);
        let hi = pos.ins().ishl_imm_u(hi, 64);
        pos.ins().bor(lo, hi)
    }
}

fn vt_of(pos: &FuncCursor, v: Value) -> Type {
    pos.func.dfg.value_type(v)
}

/// Scalarize every inst touching vector type `target`. Returns the
/// number rewritten.
/// Replace `inst`'s single result with `v`. Scalar ints keep the old
/// `iadd_imm v, 0` copy; float lanes (f32x2 extractlane) can't take an
/// integer add, so they alias directly.
fn copy_to(pos: &mut FuncCursor, inst: ir::Inst, v: ir::Value) {
    if pos.func.dfg.value_type(v).is_int() {
        pos.func.replace(inst).iadd_imm(v, 0);
        return;
    }
    let r = pos.func.dfg.first_result(inst);
    pos.func.layout.remove_inst(inst);
    pos.func.dfg.clear_results(inst);
    pos.func.dfg.change_to_alias(r, v);
}

pub fn run(func: &mut ir::Function, target: Type) -> usize {
    let mut jobs = Vec::new();
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let op = func.dfg.insts[inst].opcode();
            let has = func.dfg.inst_args(inst).iter().any(|a| {
                func.dfg.value_type(func.dfg.resolve_aliases(*a)) == target
            }) || func.dfg.inst_results(inst).iter().any(|r| {
                func.dfg.value_type(*r) == target
            });
            if !has {
                continue;
            }
            match op {
                Opcode::Extractlane
                | Opcode::Insertlane
                | Opcode::Load
                | Opcode::Store
                | Opcode::Splat
                | Opcode::Iconcat
                | Opcode::Isplit
                | Opcode::VallTrue
                | Opcode::VanyTrue
                | Opcode::VhighBits => jobs.push(inst),
                _ if bitwise_ok(op) || lanewise(op) || cmp_op(op) => jobs.push(inst),
                _ => {}
            }
        }
    }
    let n = jobs.len();
    for inst in jobs {
        let op = func.dfg.insts[inst].opcode();
        let args: Vec<Value> = func
            .dfg
            .inst_args(inst)
            .iter()
            .map(|a| func.dfg.resolve_aliases(*a))
            .collect();
        let arg_ts: Vec<Type> = args.iter().map(|a| func.dfg.value_type(*a)).collect();
        let results: Vec<Value> = func.dfg.inst_results(inst).to_vec();
        let res_ts: Vec<Type> = results.iter().map(|r| func.dfg.value_type(*r)).collect();
        let (lane, mfo) = match func.dfg.insts[inst] {
            InstructionData::BinaryImm8 { imm, .. }
            | InstructionData::TernaryImm8 { imm, .. } => (u8::from(imm) as u64, None),
            InstructionData::Store { flags, .. } | InstructionData::Load { flags, .. } => {
                (0, Some(flags))
            }
            _ => (0, None),
        };
        let off = match func.dfg.insts[inst] {
            InstructionData::Store { offset, .. } | InstructionData::Load { offset, .. } => {
                offset
            }
            _ => Offset32::from(0),
        };
        let it = int_ty(target.bits() as u16);
        let lt = target.lane_type();
        let esz = lt.bits() as u64;
        let lanes = (target.bits() as u64) / esz;
        let le = le_flags();
        let mut pos = FuncCursor::new(func).at_inst(inst);
        let ss = pos.func.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            target.bits() / 8,
            3,
        ));
        match op {
            Opcode::Extractlane => {
                let out = get_lane(&mut pos, args[0], target, lane, target);
                copy_to(&mut pos, inst, out);
            }
            Opcode::Insertlane => {
                let v = pos.ins().bitcast(it, le, args[0]);
                let s = lane_to_int(&mut pos, args[1], lt, it);
                let sh = lane * esz;
                let mask: u64 = !(((1u64 << esz) - 1) << sh);
                let mc = pos.ins().iconst(it, mask as i64);
                let masked = pos.ins().band(v, mc);
                let sin = if sh > 0 { pos.ins().ishl_imm_u(s, sh as i64) } else { s };
                let r = pos.ins().bor(masked, sin);
                emit_vec_result(&mut pos, inst, ss, r, target);
            }
            Opcode::Store => {
                let flags = pos.func.dfg.mem_flags[mfo.unwrap()];
                let v = pos.ins().bitcast(it, le, args[0]);
                pos.ins().store(flags, v, args[1], off);
                pos.func.layout.remove_inst(inst);
            }
            Opcode::Load => {
                let flags = pos.func.dfg.mem_flags[mfo.unwrap()];
                let w = pos.ins().load(it, flags, args[0], off);
                emit_vec_result(&mut pos, inst, ss, w, target);
            }
            Opcode::Splat => {
                let s = lane_to_int(&mut pos, args[0], lt, it);
                let mut mult: u128 = 0;
                for i in 0..lanes {
                    mult |= 1u128 << (i * esz);
                }
                let m = iconst_wide(&mut pos, it, mult);
                let r = pos.ins().imul(s, m);
                emit_vec_result(&mut pos, inst, ss, r, target);
            }
            Opcode::Iconcat => {
                // concat(lo, hi): result lanes = lo lanes then hi lanes.
                let lo_bits = arg_ts[0].bits() as u64;
                let lo = pos.ins().bitcast(int_ty(lo_bits as u16), le, args[0]);
                let hi = pos.ins().bitcast(int_ty(lo_bits as u16), le, args[1]);
                let lo = pos.ins().uextend(it, lo);
                let hi = pos.ins().uextend(it, hi);
                let hi = pos.ins().ishl_imm_u(hi, lo_bits as i64);
                let r = pos.ins().bor(lo, hi);
                emit_vec_result(&mut pos, inst, ss, r, target);
            }
            Opcode::Isplit => {
                let v = pos.ins().bitcast(it, le, args[0]);
                let out_t = res_ts[0];
                let ob = out_t.bits() as u64;
                let lo = pos.ins().ireduce(int_ty(ob as u16), v);
                let lo = pos.ins().bitcast(out_t, le, lo);
                let hi = pos.ins().ushr_imm_u(v, ob as i64);
                let hi = pos.ins().ireduce(int_ty(ob as u16), hi);
                let hi = pos.ins().bitcast(out_t, le, hi);
                pos.func.dfg.clear_results(inst);
                for (r, v) in results.into_iter().zip([lo, hi]) {
                    pos.func.dfg.change_to_alias(r, v);
                }
                pos.func.layout.remove_inst(inst);
            }
            Opcode::VanyTrue => {
                let v = pos.ins().bitcast(it, le, args[0]);
                let c = pos.ins().icmp_imm(ir::condcodes::IntCC::NotEqual, v, 0);
                copy_to(&mut pos, inst, c);
            }
            Opcode::VallTrue => {
                let v = pos.ins().bitcast(it, le, args[0]);
                let mut acc: Option<Value> = None;
                for i in 0..lanes {
                    let w = if i * esz > 0 {
                        pos.ins().ushr_imm_u(v, (i * esz) as i64)
                    } else {
                        v
                    };
                    let l = pos.ins().ireduce(lt, w);
                    let c = pos.ins().icmp_imm(ir::condcodes::IntCC::NotEqual, l, 0);
                    acc = Some(match acc {
                        None => c,
                        Some(a) => pos.ins().band(a, c),
                    });
                }
                copy_to(&mut pos, inst, acc.unwrap());
            }
            Opcode::VhighBits => {
                // result: bitmask of lane sign bits (scalar int).
                let v = pos.ins().bitcast(it, le, args[0]);
                let mut acc = pos.ins().iconst(ir::types::I64, 0);
                for i in 0..lanes {
                    let w = if i * esz > 0 {
                        pos.ins().ushr_imm_u(v, (i * esz) as i64)
                    } else {
                        v
                    };
                    let l = pos.ins().ireduce(lt, w);
                    let s = pos.ins().ushr_imm_u(l, (esz - 1) as i64);
                    let s = pos.ins().uextend(ir::types::I64, s);
                    let s = if i > 0 { pos.ins().ishl_imm_u(s, i as i64) } else { s };
                    acc = pos.ins().bor(acc, s);
                }
                let rt = res_ts[0];
                let r = if rt.bits() < 64 { pos.ins().ireduce(rt, acc) } else { acc };
                copy_to(&mut pos, inst, r);
            }
            _ if bitwise_ok(op) => {
                let iargs: Vec<Value> = args
                    .iter()
                    .zip(&arg_ts)
                    .map(|(a, t)| {
                        if *t == target {
                            pos.ins().bitcast(it, le, *a)
                        } else {
                            *a
                        }
                    })
                    .collect();
                let r = match iargs.as_slice() {
                    [a] => match op {
                        Opcode::Bnot => pos.ins().bnot(*a),
                        Opcode::Bitcast => *a,
                        _ => unreachable!(),
                    },
                    [a, b] => match op {
                        Opcode::Band => pos.ins().band(*a, *b),
                        Opcode::Bor => pos.ins().bor(*a, *b),
                        Opcode::Bxor => pos.ins().bxor(*a, *b),
                        Opcode::Bitcast => *a,
                        _ => unreachable!(),
                    },
                    [c, a, b] if op == Opcode::Select => pos.ins().select(*c, *a, *b),
                    _ => unreachable!(),
                };
                let out_t = res_ts[0];
                if out_t == target || out_t.is_vector() {
                    emit_vec_result(&mut pos, inst, ss, r, out_t);
                } else {
                    copy_to(&mut pos, inst, r);
                }
            }
            _ => {
                // Lane-wise ALU + comparisons.
                let out_vec = res_ts.first().is_some_and(|t| t.is_vector());
                let out_lt = res_ts.first().map(|t| t.lane_type()).unwrap_or(lt);
                // Per-lane scalar results.
                let mut outs: Vec<Value> = Vec::with_capacity(lanes as usize);
                for i in 0..lanes {
                    let ops: Vec<Value> = args
                        .iter()
                        .zip(&arg_ts)
                        .map(|(a, t)| {
                            if t.is_vector() {
                                get_lane(&mut pos, *a, *t, i, target)
                            } else {
                                *a
                            }
                        })
                        .collect();
                    let r = emit_scalar(&mut pos, inst, op, &ops, out_lt);
                    if cmp_op(op) {
                        // scalar cmp gives i8 0/1; mask lane is 0/-1.
                        outs.push(pos.ins().ineg(r));
                    } else {
                        outs.push(r);
                    }
                }
                if !out_vec {
                    if res_ts.is_empty() {
                        pos.func.layout.remove_inst(inst);
                    } else {
                        copy_to(&mut pos, inst, outs[0]);
                    }
                    continue;
                }
                let out_vt = res_ts[0];
                if out_vt == target || out_vt.bits() < 128 {
                    // Assemble into the int view, bitcast back.
                    let oit = int_ty(out_vt.bits() as u16);
                    let oesz = out_lt.bits() as u64;
                    let mut acc = iconst_wide(&mut pos, oit, 0);
                    for (i, s) in outs.into_iter().enumerate() {
                        let w = lane_to_int(&mut pos, s, out_lt, oit);
                        let w = if i > 0 {
                            pos.ins().ishl_imm_u(w, (i as u64 * oesz) as i64)
                        } else {
                            w
                        };
                        acc = pos.ins().bor(acc, w);
                    }
                    emit_vec_result(&mut pos, inst, ss, acc, out_vt);
                } else {
                    // Wide output: seed with splat(0), chain insertlane.
                    let z = pos.ins().iconst(int_ty(out_lt.bits() as u16), 0);
                    let z = if out_lt.is_float() {
                        pos.ins().bitcast(out_lt, le, z)
                    } else {
                        z
                    };
                    let mut v = pos.ins().splat(out_vt, z);
                    for (i, s) in outs.into_iter().enumerate() {
                        v = pos.ins().insertlane(v, s, i as u8);
                    }
                    copy_to(&mut pos, inst, v);
                }
            }
        }
    }
    n
}

/// Emit one scalar lane op with the same opcode (and immediates) as
/// `inst`, producing a lane-typed scalar.
fn emit_scalar(
    pos: &mut FuncCursor,
    inst: ir::Inst,
    op: Opcode,
    args: &[Value],
    lt: Type,
) -> Value {
    let f = pos.func.dfg.insts[inst];
    macro_rules! b {
        ($m:ident) => {
            pos.ins().$m(args[0], args[1])
        };
    }
    macro_rules! u {
        ($m:ident) => {
            pos.ins().$m(args[0])
        };
    }
    match op {
        Opcode::Iadd => b!(iadd),
        Opcode::Isub => b!(isub),
        Opcode::Imul => b!(imul),
        Opcode::Udiv => b!(udiv),
        Opcode::Sdiv => b!(sdiv),
        Opcode::Urem => b!(urem),
        Opcode::Srem => b!(srem),
        Opcode::Umin => b!(umin),
        Opcode::Umax => b!(umax),
        Opcode::Smin => b!(smin),
        Opcode::Smax => b!(smax),
        Opcode::Rotl => b!(rotl),
        Opcode::Rotr => b!(rotr),
        Opcode::Ishl => b!(ishl),
        Opcode::Ushr => b!(ushr),
        Opcode::Sshr => b!(sshr),
        Opcode::Ineg => u!(ineg),
        Opcode::Iabs => u!(iabs),
        Opcode::Umulhi => b!(umulhi),
        Opcode::Smulhi => b!(smulhi),
        Opcode::Fadd => b!(fadd),
        Opcode::Fsub => b!(fsub),
        Opcode::Fmul => b!(fmul),
        Opcode::Fdiv => b!(fdiv),
        Opcode::Fmin => b!(fmin),
        Opcode::Fmax => b!(fmax),
        Opcode::Fma => pos.ins().fma(args[0], args[1], args[2]),
        Opcode::Fabs => u!(fabs),
        Opcode::Fneg => u!(fneg),
        Opcode::Sqrt => u!(sqrt),
        Opcode::Ceil => u!(ceil),
        Opcode::Floor => u!(floor),
        Opcode::Trunc => u!(trunc),
        Opcode::Nearest => u!(nearest),
        Opcode::Uextend => pos.ins().uextend(lt, args[0]),
        Opcode::Sextend => pos.ins().sextend(lt, args[0]),
        Opcode::Ireduce => pos.ins().ireduce(lt, args[0]),
        Opcode::Fpromote => pos.ins().fpromote(lt, args[0]),
        Opcode::Fdemote => pos.ins().fdemote(lt, args[0]),
        Opcode::FcvtToUint => pos.ins().fcvt_to_uint(lt, args[0]),
        Opcode::FcvtToSint => pos.ins().fcvt_to_sint(lt, args[0]),
        Opcode::FcvtFromUint => pos.ins().fcvt_from_uint(lt, args[0]),
        Opcode::FcvtFromSint => pos.ins().fcvt_from_sint(lt, args[0]),
        Opcode::Icmp => {
            let cc = match f {
                InstructionData::IntCompare { cond, .. } => cond,
                _ => panic!("icmp data"),
            };
            pos.ins().icmp(cc, args[0], args[1])
        }
        Opcode::Fcmp => {
            let cc = match f {
                InstructionData::FloatCompare { cond, .. } => cond,
                _ => panic!("fcmp data"),
            };
            pos.ins().fcmp(cc, args[0], args[1])
        }
        _ => panic!("veclegalize: no lane emit for {op}"),
    }
}
