//! Stack slots whose address never leaves `iadd` chains, and which
//! values are such an address.
//!
//! `loadfwd::isolated_escapes` calls a slot escaped as soon as its
//! address is indexed by a variable (`stack_addr ss + i`): the result
//! keys under a root of its own, so the same-root rules no longer see
//! every access of the slot. One such `buf[i]` then makes every store to
//! the slot a possible write of every other pointer in the function
//! (hmget inlined into its caller: SipHasher's tail buffer is read at a
//! variable offset, and the stores to it kept the map's header loads in
//! the inner loop).
//!
//! This analysis answers the narrower question those cases need: are two
//! roots in *different objects*? A slot is **contained** when every use
//! of every value in its class `D(S)` is one of
//!
//! * the address of a `load` or a `store`,
//! * an `icmp` operand,
//! * an `iadd` operand whose other operand cannot hold a pointer; the sum
//!   joins `D(S)`.
//!
//! `D(S)` starts at the `stack_addr`s of `S`. Any other use (a call
//! operand, a stored value, a branch argument, `isub`, `select`, ...)
//! opens the slot, and nothing is claimed about an open slot.
//!
//! For a contained slot every value that points into it is in `D(S)`:
//! its address was never written to memory, passed to a call or fed to
//! an operation outside the list, so no other value can be computed from
//! it. A root outside `D(S)` therefore addresses another object, and no
//! call can reach the slot at all.
//!
//! The other half is that an access through a `D(S)` value stays inside
//! `S`. CLIF has no `getelementptr`: `iadd(a, b)` does not say which
//! operand is the pointer, and `heap.wrapping_add(&local as usize)` is a
//! defined program whose result points into the heap object. So the
//! other operand of each `iadd` must be an integer that cannot carry an
//! address ([`mayptr`]): one computed from constants, comparisons and
//! block parameters fed by such values only (loop counters). An index
//! that was loaded, passed in or returned by a call leaves the slot open.

use cranelift_codegen::entity::EntityRef as _;
use cranelift_codegen::ir::{Function, InstructionData, Opcode, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::loadfwd::Root;
use crate::uses::Uses;

#[derive(Default)]
pub struct SlotProv {
    /// Resolved value → the slot whose address plus integers it is.
    base: FxHashMap<Value, StackSlot>,
    /// Slots with a use the module comment does not list.
    open: FxHashSet<StackSlot>,
}

impl SlotProv {
    /// The contained slot `r` points into, if it is known to.
    pub fn slot_of(&self, func: &Function, r: Root) -> Option<StackSlot> {
        let s = match r {
            Root::S(s) => s,
            Root::V(v) => *self.base.get(&func.dfg.resolve_aliases(v))?,
        };
        (!self.open.contains(&s)).then_some(s)
    }

    /// Accesses rooted at `a` and at `b` touch different objects.
    pub fn apart(&self, func: &Function, a: Root, b: Root) -> bool {
        match (self.slot_of(func, a), self.slot_of(func, b)) {
            (Some(x), Some(y)) => x != y,
            (None, None) => false,
            _ => true,
        }
    }

    /// Slots tracked and not open, for the trace line.
    pub fn contained(&self) -> Vec<StackSlot> {
        let mut v: Vec<StackSlot> = self
            .base
            .values()
            .copied()
            .filter(|s| !self.open.contains(s))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// Integer operations whose result holds an address only if an operand
/// does. Everything else (loads, calls, parameters, `stack_addr`,
/// `symbol_value`, float and vector code, bit casts) is a source.
fn int_pure(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Iconst
            | Opcode::Iadd
            | Opcode::Isub
            | Opcode::Ineg
            | Opcode::Imul
            | Opcode::Udiv
            | Opcode::Sdiv
            | Opcode::Urem
            | Opcode::Srem
            | Opcode::Band
            | Opcode::Bor
            | Opcode::Bxor
            | Opcode::Bnot
            | Opcode::Ishl
            | Opcode::Ushr
            | Opcode::Sshr
            | Opcode::Rotl
            | Opcode::Rotr
            | Opcode::Uextend
            | Opcode::Sextend
            | Opcode::Ireduce
            | Opcode::Umin
            | Opcode::Umax
            | Opcode::Smin
            | Opcode::Smax
            | Opcode::Select
            | Opcode::Icmp
    )
}

/// `mp[v.index()]`: resolved value `v` may hold an address. Forward from
/// every source through [`int_pure`] results and block parameters; a
/// comparison's result never does (one bit, as `isolated_escapes` already
/// assumes). `None` when the walk runs over budget.
fn mayptr(func: &Function, uses: &Uses) -> Option<Vec<bool>> {
    let n = func.dfg.num_values();
    let mut mp = vec![false; n];
    let mut work: Vec<Value> = Vec::new();
    let mark = |v: Value, mp: &mut Vec<bool>, work: &mut Vec<Value>| {
        let v = func.dfg.resolve_aliases(v);
        if !mp[v.index()] {
            mp[v.index()] = true;
            work.push(v);
        }
    };
    if let Some(e) = func.layout.entry_block() {
        for &p in func.dfg.block_params(e) {
            mark(p, &mut mp, &mut work);
        }
    }
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let data = &func.dfg.insts[i];
            if !int_pure(data.opcode()) {
                for &r in func.dfg.inst_results(i) {
                    mark(r, &mut mp, &mut work);
                }
            }
            // A `try_call`'s results and exception payload reach their
            // blocks as non-value edge arguments.
            for bc in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                let dst = bc.block(&func.dfg.value_lists);
                for (pos, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    if a.as_value().is_none() {
                        mark(func.dfg.block_params(dst)[pos], &mut mp, &mut work);
                    }
                }
            }
        }
    }
    let budget = 16 * n + 4096;
    let mut steps = 0usize;
    while let Some(v) = work.pop() {
        for &(_, i) in uses.sites(v) {
            let data = &func.dfg.insts[i];
            let op = data.opcode();
            if int_pure(op) && op != Opcode::Icmp {
                steps += 1;
                for &r in func.dfg.inst_results(i) {
                    mark(r, &mut mp, &mut work);
                }
            }
            for bc in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                let dst = bc.block(&func.dfg.value_lists);
                for (pos, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    steps += 1;
                    if a.as_value().map(|a| func.dfg.resolve_aliases(a)) == Some(v) {
                        mark(func.dfg.block_params(dst)[pos], &mut mp, &mut work);
                    }
                }
            }
            if steps > budget {
                return None;
            }
        }
    }
    Some(mp)
}

/// `None` when a walk runs over budget: the caller then claims nothing.
pub fn compute(func: &Function, uses: &Uses) -> Option<SlotProv> {
    let mut sp = SlotProv::default();
    let mut work: Vec<Value> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::StackAddr { stack_slot, .. } = func.dfg.insts[i]
            {
                let r = func.dfg.resolve_aliases(func.dfg.first_result(i));
                sp.base.insert(r, stack_slot);
                work.push(r);
            }
        }
    }
    let is_const = |v: Value| {
        func.dfg
            .value_def(v)
            .inst()
            .is_some_and(|d| func.dfg.insts[d].opcode() == Opcode::Iconst)
    };
    // Built at the first variable index: most functions only ever add
    // constants to a slot address.
    let mut mp: Option<Vec<bool>> = None;
    let budget = 16 * func.dfg.num_values() + 4096;
    let mut steps = 0usize;
    while let Some(v) = work.pop() {
        let s = sp.base[&v];
        for &(_, i) in uses.sites(v) {
            steps += 1;
            if steps > budget {
                return None;
            }
            let data = &func.dfg.insts[i];
            let args = func.dfg.inst_args(i);
            let ok = match data {
                // The address is a load's only operand.
                InstructionData::Load { .. } => true,
                // Operand 0 is the value written.
                InstructionData::Store { .. } => func.dfg.resolve_aliases(args[0]) != v,
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    ..
                } => {
                    let a = func.dfg.resolve_aliases(args[0]);
                    let b = func.dfg.resolve_aliases(args[1]);
                    let o = if a == v { b } else { a };
                    let int = if a == b {
                        false
                    } else if is_const(o) {
                        true
                    } else {
                        if mp.is_none() {
                            mp = Some(mayptr(func, uses)?);
                        }
                        !mp.as_ref().unwrap()[o.index()]
                    };
                    int && {
                        let r = func.dfg.resolve_aliases(func.dfg.first_result(i));
                        match sp.base.insert(r, s) {
                            None => {
                                work.push(r);
                                true
                            }
                            Some(s2) if s2 != s => {
                                sp.open.insert(s2);
                                false
                            }
                            Some(_) => true,
                        }
                    }
                }
                _ => data.opcode() == Opcode::Icmp,
            };
            if !ok {
                sp.open.insert(s);
            }
        }
    }
    Some(sp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::{
        types, AbiParam, InstBuilder, MemFlagsData, Signature, StackSlotData, StackSlotKind,
        UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    /// `fn(p: i64, n: i64)`: a loop over `i` reading `ss[i]`, with the
    /// index built by `idx(cursor, i, p, n)`.
    fn indexed(idx: impl Fn(&mut FuncCursor, Value, Value, Value) -> Value) -> (Function, StackSlot, Value) {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let h = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, h, done] {
            func.layout.append_block(b);
        }
        let ss = func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 64, 3));
        let p = func.dfg.append_block_param(b0, types::I64);
        let n = func.dfg.append_block_param(b0, types::I64);
        let i = func.dfg.append_block_param(h, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let zero = cur.ins().iconst(types::I64, 0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(MemFlagsData::new(), zero, a, 0);
        cur.ins().jump(h, &[zero.into()]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let x = idx(&mut cur, i, p, n);
        let q = cur.ins().iadd(a, x);
        cur.ins().load(types::I8, MemFlagsData::new(), q, 0);
        let one = cur.ins().iconst(types::I64, 1);
        let i2 = cur.ins().iadd(i, one);
        let c = cur.ins().icmp(IntCC::UnsignedLessThan, i2, n);
        cur.ins().brif(c, h, &[i2.into()], done, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        cur.ins().return_(&[]);
        (func, ss, q)
    }

    #[test]
    fn counter_index_keeps_the_slot_contained() {
        let (func, ss, q) = indexed(|cur, i, _, _| {
            let three = cur.ins().iconst(types::I64, 3);
            cur.ins().ishl(i, three)
        });
        let sp = compute(&func, &Uses::new(&func)).unwrap();
        assert_eq!(sp.slot_of(&func, Root::V(q)), Some(ss), "{}", func.display());
        let p = func.dfg.block_params(func.layout.entry_block().unwrap())[0];
        assert!(sp.apart(&func, Root::V(q), Root::V(p)));
        assert!(sp.apart(&func, Root::S(ss), Root::V(p)));
        assert!(!sp.apart(&func, Root::S(ss), Root::V(q)));
    }

    /// `heap.wrapping_add(&local as usize)` shape: the other operand may
    /// be a pointer, so the sum may point into its object.
    #[test]
    fn pointer_index_opens_the_slot() {
        let (func, ss, q) = indexed(|_, _, p, _| p);
        let sp = compute(&func, &Uses::new(&func)).unwrap();
        assert_eq!(sp.slot_of(&func, Root::V(q)), None, "{}", func.display());
        assert_eq!(sp.slot_of(&func, Root::S(ss)), None);
        let p = func.dfg.block_params(func.layout.entry_block().unwrap())[0];
        assert!(!sp.apart(&func, Root::S(ss), Root::V(p)));
    }

    /// A counter that a parameter was added to is no longer a pure
    /// counter; neither is one a loaded value flows into.
    #[test]
    fn tainted_counter_opens_the_slot() {
        let (func, ss, _) = indexed(|cur, i, p, _| cur.ins().iadd(i, p));
        let sp = compute(&func, &Uses::new(&func)).unwrap();
        assert_eq!(sp.slot_of(&func, Root::S(ss)), None, "{}", func.display());
        let (func, ss, _) = indexed(|cur, i, p, _| {
            let l = cur.ins().load(types::I64, MemFlagsData::new(), p, 0);
            cur.ins().band(i, l)
        });
        let sp = compute(&func, &Uses::new(&func)).unwrap();
        assert_eq!(sp.slot_of(&func, Root::S(ss)), None, "{}", func.display());
    }

    /// The address written to memory, subtracted, or passed on an edge.
    #[test]
    fn other_uses_open_the_slot() {
        for kind in 0..3 {
            let (mut func, ss, q) = indexed(|cur, i, _, _| {
                let three = cur.ins().iconst(types::I64, 3);
                cur.ins().ishl(i, three)
            });
            let at = func.dfg.value_def(q).inst().unwrap();
            let p = func.dfg.block_params(func.layout.entry_block().unwrap())[0];
            let mut cur = FuncCursor::new(&mut func).after_inst(at);
            match kind {
                0 => {
                    cur.ins().store(MemFlagsData::new(), q, p, 0);
                }
                1 => {
                    cur.ins().isub(p, q);
                }
                _ => {
                    cur.ins().select(p, q, p);
                }
            }
            let sp = compute(&func, &Uses::new(&func)).unwrap();
            assert_eq!(sp.slot_of(&func, Root::S(ss)), None, "kind {kind}\n{}", func.display());
            assert!(!sp.apart(&func, Root::S(ss), Root::V(p)), "kind {kind}");
        }
    }
}
