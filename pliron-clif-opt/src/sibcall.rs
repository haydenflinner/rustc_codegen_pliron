//! Sibling calls on the platform ABI (opt-in: `PLIRON_SIBCALL=1`, aarch64).
//!
//! Shape: a call whose results are returned as they are:
//!
//! ```text
//! v2, v3 = call fn7(v0, v1)
//! return v2, v3
//! ```
//!
//! LLVM emits the frame's restores and a `b fn7` for it; we emitted the
//! restores around a `bl fn7` and a `ret`, so a state machine whose states
//! are functions (lol_html's `data_state` → `tag_open_state` → ...) paid a
//! frame, a link-register save and a return per transition. Evidence:
//! html-rewriter's aarch64 binary has 955 `b <function>` in LLVM's code
//! and 278 in ours, with 519 `bl; restores; ret` tails left.
//!
//! Rule: the call becomes `return_call` when
//! * the caller and the callee use the same calling convention: the
//!   `tail` one of the internal functions (which `pliron-clif-lower`
//!   never tail-called unless the source said `become`), or a platform
//!   one (the vendored Cranelift allows `return_call` from `SystemV` and
//!   `AppleAarch64` under `PLIRON_SIBCALL=1`),
//! * neither signature has a stack argument or a stack return area
//!   (checked by [`reg_only`], conservatively: there is no frame to
//!   rewrite and the caller pops nothing),
//! * the signatures return the same types in the same registers, and the
//!   `return` takes the call's results in order,
//! * no stack slot of this function can be seen by the callee: every use
//!   of a slot address (and of the `iadd`s / `uextend`s derived from it)
//!   is the address of a load or a store, a compare, or a `nocap` operand
//!   of a call (so a slot address in a stored value, a branch argument,
//!   an unproven call operand or the callee's own operands keeps the
//!   frame).
//!
//! An indirect call is a candidate on the `tail` convention only (the
//! aarch64 `return_call_indirect` pins its target register to `x1`).
//!
//! A `try_call` is not a candidate (the landing pad needs the frame), and
//! neither is a call followed by anything but the `return`.
//! `fastcc` records nothing for a function with a tail call, so callers
//! assume the platform's full clobber set across it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    ArgumentPurpose, Block, Function, Inst, InstBuilder, InstructionData, Opcode, Signature, Value,
};
use cranelift_codegen::isa::CallConv;
use rustc_hash::{FxHashMap, FxHashSet};

/// Both argument and return values of `sig` fit in registers on aarch64
/// (8 integer and 8 vector registers, 6 integer registers for the `tail`
/// convention, which reserves `x0` and `x1`; an `i128` is counted as
/// three integer registers, covering the 16-byte alignment skip).
pub fn reg_only(sig: &Signature) -> bool {
    let max_ints = if sig.call_conv == CallConv::Tail { 6 } else { 8 };
    let fits = |params: &[cranelift_codegen::ir::AbiParam]| -> bool {
        let (mut ints, mut vecs) = (0u32, 0u32);
        for p in params {
            match p.purpose {
                ArgumentPurpose::Normal => {}
                // x8; no argument register.
                ArgumentPurpose::StructReturn => continue,
                _ => return false,
            }
            let t = p.value_type;
            if t == types::I128 {
                ints += 3;
            } else if t.is_int() {
                ints += 1;
            } else if t.is_float() || t.is_vector() {
                if t.bits() > 128 {
                    return false;
                }
                vecs += 1;
            } else {
                return false;
            }
        }
        ints <= max_ints && vecs <= 8
    };
    fits(&sig.params) && fits(&sig.returns)
}

/// A call directly followed by a `return` of its results.
struct Site {
    call: Inst,
    ret: Inst,
}

/// Turns eligible `call; return` tails into `return_call`. `nocap` is the
/// pipeline's `(call, operand) -> bytes` fact table. Returns the number
/// of calls turned. Linear in the function.
pub fn run(func: &mut Function, nocap: &FxHashMap<(Inst, u32), u64>) -> usize {
    let cc = func.signature.call_conv;
    if !matches!(cc, CallConv::SystemV | CallConv::AppleAarch64 | CallConv::Tail)
        || !reg_only(&func.signature)
        || !func.dynamic_stack_slots.is_empty()
    {
        return 0;
    }
    let mut sites: Vec<Site> = Vec::new();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for &b in &blocks {
        let Some(ret) = func.layout.last_inst(b) else { continue };
        if func.dfg.insts[ret].opcode() != Opcode::Return {
            continue;
        }
        let Some(call) = func.layout.prev_inst(ret) else { continue };
        let sig_ref = match func.dfg.insts[call] {
            InstructionData::Call { opcode: Opcode::Call, func_ref, .. } => {
                func.dfg.ext_funcs[func_ref].signature
            }
            // The aarch64 `return_call_indirect` pins its target to `x1`,
            // which only the `tail` convention keeps out of its arguments.
            InstructionData::CallIndirect { opcode: Opcode::CallIndirect, sig_ref, .. }
                if cc == CallConv::Tail =>
            {
                sig_ref
            }
            _ => continue,
        };
        let callee = &func.dfg.signatures[sig_ref];
        if callee.call_conv != cc || !reg_only(callee) || callee.returns != func.signature.returns {
            continue;
        }
        let results = func.dfg.inst_results(call);
        let rets = func.dfg.inst_args(ret);
        if results.len() != rets.len()
            || !results.iter().zip(rets).all(|(&r, &v)| r == func.dfg.resolve_aliases(v))
        {
            continue;
        }
        sites.push(Site { call, ret });
    }
    if sites.is_empty() {
        return 0;
    }
    let (escaped, tainted) = slot_reach(func, nocap);
    if escaped {
        return 0;
    }
    let mut done = 0;
    for s in sites {
        // The callee's operands must not point into this frame.
        if func
            .dfg
            .inst_args(s.call)
            .iter()
            .any(|&a| tainted.contains(&func.dfg.resolve_aliases(a)))
            || !pliron_cfg::bisect("sibcall")
        {
            continue;
        }
        let args: Vec<Value> = func.dfg.inst_args(s.call).iter().map(|&v| func.dfg.resolve_aliases(v)).collect();
        let data = func.dfg.insts[s.call].clone();
        let loc = func.srcloc(s.call);
        let mut cur = FuncCursor::new(func).at_inst(s.ret);
        let new = match data {
            InstructionData::Call { func_ref, .. } => cur.ins().return_call(func_ref, &args),
            InstructionData::CallIndirect { sig_ref, .. } => {
                cur.ins().return_call_indirect(sig_ref, args[0], &args[1..])
            }
            _ => unreachable!(),
        };
        func.set_srcloc(new, loc);
        func.layout.remove_inst(s.call);
        func.layout.remove_inst(s.ret);
        done += 1;
    }
    done
}

/// Whether any stack slot address of the function escapes (is seen by
/// anything but loads, stores through it, compares and `nocap` call
/// operands), and the set of values derived from slot addresses.
fn slot_reach(func: &Function, nocap: &FxHashMap<(Inst, u32), u64>) -> (bool, FxHashSet<Value>) {
    let mut tainted: FxHashSet<Value> = FxHashSet::default();
    if !func.sized_stack_slots.values().any(|d| d.size > 0) {
        return (false, tainted);
    }
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if matches!(func.dfg.insts[i], InstructionData::StackAddr { .. }) {
                tainted.insert(func.dfg.first_result(i));
            }
        }
    }
    if tainted.is_empty() {
        return (false, tainted);
    }
    // Derived values: address arithmetic. Layout order is not dominance
    // order, so repeat until nothing is added (a few rounds at most).
    let mut rounds = 0;
    loop {
        let mut grew = false;
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                if !propagates(func.dfg.insts[i].opcode()) {
                    continue;
                }
                let any = func.dfg.inst_args(i).iter().any(|&a| tainted.contains(&func.dfg.resolve_aliases(a)));
                if any && tainted.insert(func.dfg.first_result(i)) {
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
        rounds += 1;
        if rounds > 8 {
            return (true, tainted);
        }
    }
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let op = func.dfg.insts[i].opcode();
            if op == Opcode::StackAddr || propagates(op) {
                continue;
            }
            let args = func.dfg.inst_args(i);
            let hit = |v: Value| tainted.contains(&func.dfg.resolve_aliases(v));
            // Branch arguments are not in `inst_args`; `inst_values` has them.
            if !func.dfg.inst_values(i).any(hit) {
                continue;
            }
            let ok = match op {
                // Address operand only (arg 0 of a load, arg 1 of a store).
                Opcode::Load
                | Opcode::Uload8
                | Opcode::Sload8
                | Opcode::Uload16
                | Opcode::Sload16
                | Opcode::Uload32
                | Opcode::Sload32 => true,
                Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32 => !hit(args[0]),
                Opcode::Icmp => true,
                Opcode::Call => args
                    .iter()
                    .enumerate()
                    .all(|(k, &a)| !hit(a) || nocap.contains_key(&(i, k as u32))),
                _ => false,
            };
            if !ok {
                return (true, tainted);
            }
        }
    }
    (false, tainted)
}

/// Opcodes whose result is an address derived from an operand.
fn propagates(op: Opcode) -> bool {
    matches!(op, Opcode::Iadd | Opcode::Isub | Opcode::Uextend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::types::I64;
    use cranelift_codegen::ir::{AbiParam, MemFlagsData, StackSlotData, StackSlotKind, UserFuncName};

    fn sig(cc: CallConv) -> Signature {
        let mut s = Signature::new(cc);
        s.params.push(AbiParam::new(I64));
        s.returns.push(AbiParam::new(I64));
        s
    }

    /// `f(x) { return g(x) }`, with a slot address optionally handed to
    /// `g` or stored somewhere first.
    fn build(cc: CallConv, slot_arg: bool) -> (Function, Block) {
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig(cc));
        let ss = func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let sr = func.import_signature(sig(cc));
        let uref = func.declare_imported_user_function(cranelift_codegen::ir::UserExternalName::new(0, 1));
        let g = func.import_function(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::user(uref),
            signature: sr,
            colocated: true,
            patchable: false,
        });
        let b = func.dfg.make_block();
        func.layout.append_block(b);
        let x = func.dfg.append_block_param(b, I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(b);
        let a = cur.ins().stack_addr(I64, ss, 0);
        cur.ins().store(MemFlagsData::new().with_notrap(), x, a, 0);
        let arg = if slot_arg { a } else { x };
        let call = cur.ins().call(g, &[arg]);
        let r = cur.func.dfg.first_result(call);
        cur.ins().return_(&[r]);
        (func, b)
    }

    fn count(func: &Function, b: Block, op: Opcode) -> usize {
        func.layout.block_insts(b).filter(|&i| func.dfg.insts[i].opcode() == op).count()
    }

    #[test]
    fn call_before_return_becomes_return_call() {
        let (mut func, b) = build(CallConv::SystemV, false);
        assert_eq!(run(&mut func, &Default::default()), 1);
        assert_eq!(count(&func, b, Opcode::ReturnCall), 1);
        assert_eq!(count(&func, b, Opcode::Call), 0);
        assert_eq!(count(&func, b, Opcode::Return), 0);
    }

    /// The callee must not be handed a pointer into the dying frame.
    #[test]
    fn slot_address_argument_keeps_the_call() {
        let (mut func, b) = build(CallConv::SystemV, true);
        assert_eq!(run(&mut func, &Default::default()), 0);
        assert_eq!(count(&func, b, Opcode::Call), 1);
        assert_eq!(count(&func, b, Opcode::Return), 1);
    }

    /// Internal functions (the `tail` convention) are tail-called too.
    #[test]
    fn tail_convention_converts() {
        let (mut func, b) = build(CallConv::Tail, false);
        assert_eq!(run(&mut func, &Default::default()), 1);
        assert_eq!(count(&func, b, Opcode::ReturnCall), 1);
    }

    /// A callee on another convention is not a candidate.
    #[test]
    fn mixed_conventions_keep_the_call() {
        let (mut func, b) = build(CallConv::SystemV, false);
        let sr = func.import_signature(sig(CallConv::Tail));
        let call = func.layout.block_insts(b).find(|&i| func.dfg.insts[i].opcode() == Opcode::Call).unwrap();
        let InstructionData::Call { func_ref, .. } = func.dfg.insts[call] else { unreachable!() };
        func.dfg.ext_funcs[func_ref].signature = sr;
        assert_eq!(run(&mut func, &Default::default()), 0);
        assert_eq!(count(&func, b, Opcode::Call), 1);
    }

    #[test]
    fn stack_arguments_are_not_register_only() {
        let mut s = Signature::new(CallConv::SystemV);
        for _ in 0..9 {
            s.params.push(AbiParam::new(I64));
        }
        assert!(!reg_only(&s));
        s.params.pop();
        assert!(reg_only(&s));
    }
}
