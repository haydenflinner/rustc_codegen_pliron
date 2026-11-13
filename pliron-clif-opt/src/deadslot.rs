//! Stack slots nothing refers to any more give up their frame bytes
//! (default on; `PLIRON_DEADSLOT=0` off).
//!
//! Shape: a function whose last use of an explicit slot went away
//! after `slotmerge` ran — the slot keeps its size, so the function
//! keeps a frame for it:
//!
//! ```text
//! ss0 = explicit_slot 1          ; no `stack_addr ss0` left
//! ...
//! sub sp, sp, #0x10              ; and `add sp, sp, #0x10` at each return
//! ```
//!
//! Evidence: pulldown-cmark's `scan_closing_code_fence`
//! (`tests/parity/closing.rs`). The closure passed to `take_while`
//! captures the fence byte by reference, so the byte lives in a slot
//! until `loadfwd2` forwards it and `dse3` deletes the store — after
//! `slotmerge`, the only step that zeroes unreferenced slots. Two
//! instructions on every call of a function that executes about forty.
//!
//! Rule: at the end of the pipeline, a sized slot with no `stack_addr`
//! whose result is used gets size 0. (`stack_addr` is the only
//! instruction that names a slot; a `stack_addr` nothing reads is left
//! for Cranelift's own dead-code pass.)

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::{Function, InstructionData, StackSlot, Value};
use rustc_hash::FxHashMap;

/// Returns the number of slots zeroed.
pub fn run(func: &mut Function) -> usize {
    if !func.sized_stack_slots.values().any(|d| d.size > 0) {
        return 0;
    }
    let mut addr: FxHashMap<Value, StackSlot> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::StackAddr { stack_slot, .. } = func.dfg.insts[i] {
                addr.insert(func.dfg.first_result(i), stack_slot);
            }
        }
    }
    let mut live = vec![false; func.sized_stack_slots.len()];
    if !addr.is_empty() {
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    if let Some(s) = addr.get(&func.dfg.resolve_aliases(v)) {
                        live[s.index()] = true;
                    }
                }
            }
        }
    }
    let dead: Vec<StackSlot> = func
        .sized_stack_slots
        .iter()
        .filter(|(s, d)| d.size > 0 && !live[s.index()])
        .map(|(s, _)| s)
        .collect();
    if dead.is_empty() || !pliron_cfg::bisect("deadslot") {
        return 0;
    }
    for &s in &dead {
        func.sized_stack_slots[s].size = 0;
    }
    dead.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::types::I64;
    use cranelift_codegen::ir::{AbiParam, InstBuilder, Signature, StackSlotData, StackSlotKind, UserFuncName};
    use cranelift_codegen::isa::CallConv;

    #[test]
    fn zeroes_only_unreferenced_slots() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let dead = func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let unread = func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let used = func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let b = func.dfg.make_block();
        func.layout.append_block(b);
        let x = func.dfg.append_block_param(b, I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(b);
        // An address nothing reads does not keep its slot.
        cur.ins().stack_addr(I64, unread, 0);
        let a = cur.ins().stack_addr(I64, used, 0);
        let y = cur.ins().iadd(a, x);
        cur.ins().return_(&[y]);
        assert_eq!(run(&mut func), 2);
        assert_eq!(func.sized_stack_slots[dead].size, 0);
        assert_eq!(func.sized_stack_slots[unread].size, 0);
        assert_eq!(func.sized_stack_slots[used].size, 8);
        assert_eq!(run(&mut func), 0);
    }
}
