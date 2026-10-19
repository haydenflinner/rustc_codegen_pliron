//! Commute operands of `iadd`/`imul`/`band`/`bor`/`bxor`/`fadd`/`fmul`/
//! `{u,s}{min,max}` so the operand with FEWER uses sits first — the slot
//! two-address x64 isel overwrites. With `fmul splat, b` (splat live
//! across every unrolled `mulpd`, `b` a one-use load) regalloc must copy
//! the splat into a scratch it can clobber; `fmul b, splat` lets it
//! destroy `b`'s reg in place and the copy disappears (matmul's inner
//! loop sheds a `movdqa` per f64x2 lane-group).
//!
//! Never touches an instruction whose second operand is a constant —
//! immediates only fold into the source slot, so keep them there. Ties
//! and equal counts leave the order alone.

use cranelift_codegen::entity::SecondaryMap;
use cranelift_codegen::ir::{Function, Inst, InstructionData, Opcode, Value};

/// Whether `v` resolves to the result of a materialized-constant
/// instruction — such a value must remain the second operand so the x64
/// backend can fold it into the op's source slot.
fn is_const(func: &Function, v: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    let Some(d) = func.dfg.value_def(v).inst() else {
        return false;
    };
    matches!(
        func.dfg.insts[d].opcode(),
        Opcode::Iconst | Opcode::F32const | Opcode::F64const | Opcode::F128const | Opcode::Vconst
    )
}

/// Swap the operands of `inst` (a `Binary`) and count it.
fn swap(func: &mut Function, inst: Inst) {
    func.dfg.inst_args_mut(inst).swap(0, 1);
}

pub fn run(func: &mut Function) -> usize {
    // Count each (alias-resolved) value's argument occurrences across all
    // instructions — `inst_args` covers the whole value list, including
    // branch destination args, so block-param (phi) uses count too. A
    // local histogram beat sharing `Uses` (measured: its CSR build is
    // heavier and nothing else consumes it at this position).
    let mut uses = SecondaryMap::<Value, u32>::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                uses[func.dfg.resolve_aliases(a)] += 1;
            }
        }
    }
    let mut n = 0;
    let insts: Vec<Inst> = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .collect();
    for i in insts {
        let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
            continue;
        };
        if !matches!(
            opcode,
            Opcode::Iadd
                | Opcode::Imul
                | Opcode::Band
                | Opcode::Bor
                | Opcode::Bxor
                | Opcode::Fadd
                | Opcode::Fmul
                | Opcode::Umin
                | Opcode::Umax
                | Opcode::Smin
                | Opcode::Smax
        ) {
            continue;
        }
        let [a, b] = args;
        let (ua, ub) = (
            uses[func.dfg.resolve_aliases(a)],
            uses[func.dfg.resolve_aliases(b)],
        );
        if a == b || is_const(func, b) || ub >= ua {
            continue;
        }
        swap(func, i);
        n += 1;
    }
    n
}
