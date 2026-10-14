//! Scalarize `extractlane`/`insertlane` on fixed vectors narrower than
//! the target's minimum legal vector width (x64: 128 bits) — ISLE only
//! lowers the Xmm forms. Rewrites through an integer bitcast so the
//! results are plain scalar ops:
//!
//!   extractlane.vNxM  →  bitcast.iB v; ushr_imm lane*esz; ireduce.N
//!                       (+ bitcast.fN for float lanes)
//!   insertlane.vNxM   →  widen lane into iB at lane*esz, band/bor mask,
//!                        bitcast back

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Endianness, MemFlagsData};
use cranelift_codegen::ir::{self, InstBuilder, InstructionData, Opcode, Type, Value};

fn int_ty(bits: u16) -> Type {
    Type::int(bits).expect("integer type")
}

/// True when `ty` is a fixed vector the ISA can't lower ops on.
fn narrow_vec(ty: Type, max_bits: u32) -> bool {
    ty.is_vector() && !ty.is_dynamic_vector() && ty.bits() < max_bits
}

/// Scalarize extractlane/insertlane touching vectors < `max_bits`.
/// Returns the number of instructions rewritten.
pub fn run(func: &mut ir::Function, max_bits: u32) -> usize {
    let mut jobs = Vec::new();
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let op = func.dfg.insts[inst].opcode();
            match op {
                Opcode::Extractlane | Opcode::Insertlane | Opcode::Store => {
                    let v = func.dfg.resolve_aliases(func.dfg.inst_args(inst)[0]);
                    if narrow_vec(func.dfg.value_type(v), max_bits) {
                        jobs.push((inst, op));
                    }
                }
                Opcode::Load => {
                    let res = func.dfg.inst_results(inst)[0];
                    if narrow_vec(func.dfg.value_type(res), max_bits) {
                        jobs.push((inst, op));
                    }
                }
                _ => {}
            }
        }
    }
    let n = jobs.len();
    for (inst, op) in jobs {
        // Read phase.
        let args: Vec<Value> = func
            .dfg
            .inst_args(inst)
            .iter()
            .map(|a| func.dfg.resolve_aliases(*a))
            .collect();
        let (lane, mf, off) = match func.dfg.insts[inst] {
            InstructionData::BinaryImm8 { imm, .. }
            | InstructionData::TernaryImm8 { imm, .. } =>(u8::from(imm) as u64, None, ir::immediates::Offset32::from(0)),
            InstructionData::Store { flags, offset, .. } => (0, Some(flags), offset),
            InstructionData::Load { flags, offset, .. } => (0, Some(flags), offset),
            _ => panic!("veclegalize: unexpected data"),
        };
        let res = func.dfg.inst_results(inst).first().copied();
        // Vector type involved: the stored/loaded result for memory ops,
        // arg0 for extract/insert.
        let vt = match op {
            Opcode::Load => func.dfg.value_type(res.unwrap()),
            _ => func.dfg.value_type(args[0]),
        };
        let lt = match op {
            Opcode::Extractlane => func.dfg.value_type(res.unwrap()),
            Opcode::Insertlane => func.dfg.value_type(args[1]),
            _ => vt.lane_type(),
        };
        let it = int_ty(vt.bits() as u16);
        let esz = lt.bits() as u64;
        let sh = lane * esz;
        // Mutate phase: build the scalar sequence before `inst`.
        let le = MemFlagsData::new().with_endianness(Endianness::Little);
        let mut pos = FuncCursor::new(func).at_inst(inst);
        match op {
            Opcode::Extractlane => {
                let v = pos.ins().bitcast(it, le, args[0]);
                let v = if sh > 0 { pos.ins().ushr_imm(v, sh as i64) } else { v };
                // Last op in place so `res` stays attached.
                if lt.is_float() {
                    let t = if it.bits() > lt.bits() {
                        pos.ins().ireduce(int_ty(lt.bits() as u16), v)
                    } else {
                        v
                    };
                    pos.func.replace(inst).bitcast(lt, le, t);
                } else if it.bits() > lt.bits() {
                    pos.func.replace(inst).ireduce(lt, v);
                } else {
                    pos.func.replace(inst).iadd_imm(v, 0);
                }
            }
            Opcode::Insertlane => {
                // Splice the lane's bits into the vector's integer view.
                let v = pos.ins().bitcast(it, le, args[0]);
                let st = lt;
                let s = if st.is_float() {
                    let t = pos.ins().bitcast(int_ty(st.bits() as u16), le, args[1]);
                    if it.bits() > st.bits() { pos.ins().uextend(it, t) } else { t }
                } else if it.bits() > st.bits() {
                    pos.ins().uextend(it, args[1])
                } else {
                    args[1]
                };
                let mask: u64 = if esz >= 64 {
                    0
                } else {
                    !(((1u64 << esz) - 1) << sh)
                };
                let mc = pos.ins().iconst(it, mask as i64);
                let masked = pos.ins().band(v, mc);
                let sin = if sh > 0 { pos.ins().ishl_imm_u(s, sh as i64) } else { s };
                let r = pos.ins().bor(masked, sin);
                pos.func.replace(inst).bitcast(vt, le, r);
            }
            Opcode::Store => {
                let flags = pos.func.dfg.mem_flags[mf.unwrap()];
                let v = pos.ins().bitcast(it, le, args[0]);
                pos.ins().store(flags, v, args[1], off);
                pos.func.layout.remove_inst(inst);
            }
            Opcode::Load => {
                let flags = pos.func.dfg.mem_flags[mf.unwrap()];
                let w = pos.ins().load(it, flags, args[0], off);
                pos.func.replace(inst).bitcast(vt, le, w);
            }
            _ => unreachable!(),
        }
    }
    n
}
