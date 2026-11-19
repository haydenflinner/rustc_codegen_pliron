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
//!
//! A *write-only* slot goes the same way: every use of every address of
//! it is the address operand of a plain `notrap` store, so nothing can
//! read the bytes (the address never escapes into a call, a load, an
//! `iadd` or a stored value). Its stores are deleted first, which leaves
//! it unreferenced. The shape: memchr's splat of the needle written to a
//! slot (`stur q16, [sp]`, never read) after `dse3` ran and a later fold
//! removed the last reader, which cost a frame in `data_state` of every
//! lol_html tag scanner (default on since 2026-11-19:
//! `PLIRON_DEADSLOT_WRITEONLY=0` off).

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::{Function, Inst, InstructionData, Opcode, StackSlot, Value};
use rustc_hash::FxHashMap;

/// Returns the number of slots zeroed (plus stores deleted, with the
/// write-only rule, `PLIRON_DEADSLOT_WRITEONLY`, default on).
pub fn run(func: &mut Function) -> usize {
    run_with(func, pliron_cfg::env_is("PLIRON_DEADSLOT_WRITEONLY", "1"))
}

/// [`run`] with the write-only rule chosen by the caller.
pub fn run_with(func: &mut Function, write_only: bool) -> usize {
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
    let removed = if addr.is_empty() || !write_only {
        0
    } else {
        remove_write_only_stores(func, &addr)
    };
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
        return removed;
    }
    for &s in &dead {
        func.sized_stack_slots[s].size = 0;
    }
    dead.len() + removed
}

/// Deletes the stores into slots that nothing reads: every use of every
/// `stack_addr` of the slot is the address of a plain `notrap` store.
/// Returns the number of stores deleted. Linear in the function.
fn remove_write_only_stores(func: &mut Function, addr: &FxHashMap<Value, StackSlot>) -> usize {
    let n = func.sized_stack_slots.len();
    let mut escapes = vec![false; n];
    let mut stores: Vec<Vec<Inst>> = vec![Vec::new(); n];
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            // A plain notrap store whose *address* is a slot address and
            // whose value is not one.
            let store_to = match func.dfg.insts[i] {
                InstructionData::Store { opcode, args, .. }
                    if matches!(
                        opcode,
                        Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32
                    ) && crate::loadfwd::notrap(func, i) =>
                {
                    addr.get(&func.dfg.resolve_aliases(args[1]))
                        .copied()
                        .filter(|_| addr.get(&func.dfg.resolve_aliases(args[0])).is_none())
                }
                _ => None,
            };
            if let Some(s) = store_to {
                stores[s.index()].push(i);
                continue;
            }
            for v in func.dfg.inst_values(i) {
                if let Some(s) = addr.get(&func.dfg.resolve_aliases(v)) {
                    escapes[s.index()] = true;
                }
            }
        }
    }
    let mut removed = 0;
    for (s, list) in stores.iter().enumerate() {
        if escapes[s] || list.is_empty() || !pliron_cfg::bisect("deadslot-wo") {
            continue;
        }
        for &i in list {
            func.layout.remove_inst(i);
            removed += 1;
        }
    }
    removed
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
        assert_eq!(run_with(&mut func, false), 2);
        assert_eq!(func.sized_stack_slots[dead].size, 0);
        assert_eq!(func.sized_stack_slots[unread].size, 0);
        assert_eq!(func.sized_stack_slots[used].size, 8);
        assert_eq!(run_with(&mut func, false), 0);
    }

    /// A slot that is only stored to loses its stores and its frame bytes;
    /// one that is also loaded, or whose address escapes, keeps both.
    #[test]
    fn write_only_slot_loses_its_stores() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let wo = func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let rd = func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let esc = func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let b = func.dfg.make_block();
        func.layout.append_block(b);
        let x = func.dfg.append_block_param(b, I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(b);
        let flags = cranelift_codegen::ir::MemFlagsData::new().with_notrap();
        let a0 = cur.ins().stack_addr(I64, wo, 0);
        cur.ins().store(flags, x, a0, 0);
        let a1 = cur.ins().stack_addr(I64, rd, 0);
        cur.ins().store(flags, x, a1, 0);
        let y = cur.ins().load(I64, flags, a1, 0);
        // The address itself stored: it escapes.
        let a2 = cur.ins().stack_addr(I64, esc, 0);
        cur.ins().store(flags, a2, a1, 0);
        cur.ins().return_(&[y]);
        let stores_before = func.layout.block_insts(b).filter(|&i| func.dfg.insts[i].opcode() == Opcode::Store).count();
        assert_eq!(stores_before, 3);
        // Without the rule every store stays.
        run_with(&mut func, false);
        assert_eq!(func.layout.block_insts(b).filter(|&i| func.dfg.insts[i].opcode() == Opcode::Store).count(), 3);
        assert_eq!(func.sized_stack_slots[wo].size, 8);
        run_with(&mut func, true);
        let stores_after = func.layout.block_insts(b).filter(|&i| func.dfg.insts[i].opcode() == Opcode::Store).count();
        assert_eq!(stores_after, 2);
        assert_eq!(func.sized_stack_slots[wo].size, 0);
        assert_eq!(func.sized_stack_slots[rd].size, 8);
        assert_eq!(func.sized_stack_slots[esc].size, 8);
    }
}
