//! Height reassociation of straight-line chains (`PLIRON_HREASSOC=1`
//! enables).
//!
//! ```text
//! t = iadd a, b        ; b: just computed, deep
//! u = iadd t, m        ; m: loaded early, shallow
//! ```
//!
//! puts two dependent adds on `b`'s path. With `t = iadd a, m; u = iadd
//! t, b`, `b` waits for one. blake3's G function is `a + b + m` four times
//! per round with `b` fresh off a rotate each time: LLVM's reassociation
//! (operand ranks) adds the message word first, and the extra add on every
//! `a` update was ~40 cycles of each 64-byte compress (blake3 1.16x).
//!
//! Per block, a value's height is its dependency depth inside the block
//! (outside values and params 0; loads 4, `imul` 3, anything else 1 on top
//! of its deepest operand). For `u = t ⊕ z` with `t = x ⊕ y` the same
//! associative op, in the same block and used only by `u`: when the deeper
//! of `x`/`y` is deeper than `z`, `u` becomes `(shallow ⊕ z) ⊕ deep`, the
//! new inner op placed right before `u` and `t` removed. Wrapping integer
//! `iadd`, `imul`, `band`, `bor`, `bxor` only, and no chain with a `bnot`
//! leaf (its `bic`/`orn`/`eon` fusion needs a register partner) or whose
//! result is a load/store address (amode shapes).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef};
use rustc_hash::FxHashMap;

fn assoc(op: Opcode) -> bool {
    matches!(op, Opcode::Iadd | Opcode::Imul | Opcode::Band | Opcode::Bor | Opcode::Bxor)
}

/// `v` is a bitwise NOT: `bnot`, or `bxor` with an all-ones constant.
fn is_not(func: &Function, v: Value) -> bool {
    let ValueDef::Result(d, _) = func.dfg.value_def(v) else { return false };
    let ones = |x: Value| {
        let x = func.dfg.resolve_aliases(x);
        let ValueDef::Result(c, _) = func.dfg.value_def(x) else { return false };
        let bits = func.dfg.value_type(x).bits();
        matches!(func.dfg.insts[c], InstructionData::UnaryImm { opcode: Opcode::Iconst, imm }
            if (imm.bits() as u64) & mask(bits) == mask(bits))
    };
    match func.dfg.insts[d] {
        InstructionData::Unary { opcode: Opcode::Bnot, .. } => true,
        InstructionData::Binary { opcode: Opcode::Bxor, args } => ones(args[0]) || ones(args[1]),
        _ => false,
    }
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

fn latency(op: Opcode) -> u32 {
    if op.can_load() {
        4
    } else if op == Opcode::Imul {
        3
    } else {
        1
    }
}

pub fn run(func: &mut Function) -> usize {
    // Use counts over the whole function (block-call args included).
    let mut nuse: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *nuse.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    // Values used as a load/store address: address chains are short and
    // their shape is what isel folds into `[base, idx, lsl #k]` amodes.
    let mut addr: rustc_hash::FxHashSet<Value> = Default::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            match func.dfg.insts[i] {
                InstructionData::Load { arg, .. } => {
                    addr.insert(func.dfg.resolve_aliases(arg));
                }
                InstructionData::Store { args, .. } => {
                    addr.insert(func.dfg.resolve_aliases(args[1]));
                }
                _ => {}
            }
        }
    }
    // `PLIRON_HREASSOC_MIN=<k>` (default 4): rewrite only when the deep
    // operand is at least k levels above `z`.
    let min_gain: u32 = crate::config::env_get("PLIRON_HREASSOC_MIN").and_then(|v| v.parse().ok()).unwrap_or(4);
    let mut n = 0;
    let blocks: Vec<_> = func.layout.blocks().collect();
    for b in blocks {
        let mut h: FxHashMap<Value, u32> = FxHashMap::default();
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for u in insts {
            let op = func.dfg.insts[u].opcode();
            let height = |h: &FxHashMap<Value, u32>, v: Value| h.get(&v).copied().unwrap_or(0);
            if assoc(op)
                && let InstructionData::Binary { args, .. } = func.dfg.insts[u]
                && !func.dfg.value_type(func.dfg.inst_results(u)[0]).is_vector()
                && !addr.contains(&func.dfg.inst_results(u)[0])
            {
                let args = args.map(|a| func.dfg.resolve_aliases(a));
                // The single-use same-op operand `t` and the other one `z`.
                let pick = (0..2).find_map(|k| {
                    let ValueDef::Result(t, _) = func.dfg.value_def(args[k]) else { return None };
                    (func.dfg.insts[t].opcode() == op
                        && func.layout.inst_block(t) == Some(b)
                        && nuse.get(&args[k]).copied() == Some(1)
                        && args[k] != args[1 - k])
                        .then_some((t, args[1 - k]))
                });
                if let Some((t, z)) = pick {
                    let ta = func.dfg.inst_args(t).to_vec();
                    let (x, y) = (func.dfg.resolve_aliases(ta[0]), func.dfg.resolve_aliases(ta[1]));
                    let (deep, shallow) = if height(&h, x) >= height(&h, y) { (x, y) } else { (y, x) };
                    // A `bnot` leaf fuses into its user (`bic`/`orn`/`eon`)
                    // only when paired with a register: regrouped with a
                    // constant it becomes `mvn` + `and #imm` (memchr's
                    // SWAR `(x - LO) & !x & HI` lost 20 `bic`s, regex +1.5%).
                    let bnot = |v: Value| is_not(func, v);
                    if height(&h, deep) >= height(&h, z) + min_gain && ![x, y, z].into_iter().any(bnot) {
                        let inner = {
                            let mut pos = FuncCursor::new(func).at_inst(u);
                            let ins = pos.ins();
                            match op {
                                Opcode::Iadd => ins.iadd(shallow, z),
                                Opcode::Imul => ins.imul(shallow, z),
                                Opcode::Band => ins.band(shallow, z),
                                Opcode::Bor => ins.bor(shallow, z),
                                _ => ins.bxor(shallow, z),
                            }
                        };
                        let iv = func.dfg.inst_results(func.dfg.value_def(inner).unwrap_inst())[0];
                        h.insert(iv, height(&h, shallow).max(height(&h, z)) + latency(op));
                        nuse.insert(iv, 1);
                        if let InstructionData::Binary { args, .. } = &mut func.dfg.insts[u] {
                            *args = [inner, deep];
                        }
                        func.dfg.clear_results(t);
                        func.layout.remove_inst(t);
                        n += 1;
                    }
                }
            }
            let base = func.dfg.inst_args(u).iter().map(|&a| height(&h, func.dfg.resolve_aliases(a))).max().unwrap_or(0);
            for &r in func.dfg.inst_results(u) {
                h.insert(r, base + latency(op));
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::types::I32;
    use cranelift_codegen::ir::{AbiParam, Function, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;

    /// `u = (a + rotl(b0, 7)) + m` becomes `(a + m) + rotl(b0, 7)`.
    #[test]
    fn deep_operand_moves_last() {
        let mut sig = Signature::new(CallConv::SystemV);
        for _ in 0..3 {
            sig.params.push(AbiParam::new(I32));
        }
        sig.returns.push(AbiParam::new(I32));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        func.layout.append_block(b);
        let a = func.dfg.append_block_param(b, I32);
        let b0 = func.dfg.append_block_param(b, I32);
        let m = func.dfg.append_block_param(b, I32);
        let (deep, u) = {
            let mut pos = FuncCursor::new(&mut func).at_bottom(b);
            let mut deep = pos.ins().bxor(b0, a);
            for _ in 0..4 {
                deep = pos.ins().rotl_imm(deep, 7);
            }
            let t = pos.ins().iadd(a, deep);
            let u = pos.ins().iadd(t, m);
            pos.ins().return_(&[u]);
            (deep, u)
        };
        assert_eq!(run(&mut func), 1);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        cranelift_codegen::verify_function(&func, &flags).unwrap();
        let ui = func.dfg.value_def(u).unwrap_inst();
        assert_eq!(func.dfg.inst_args(ui)[1], deep, "{}", func.display());
    }

    /// `(deep & !x) & K` keeps its shape: the NOT fuses into `bic` only
    /// beside a register, not beside the constant.
    #[test]
    fn not_leaf_blocks_rewrite() {
        let mut sig = Signature::new(CallConv::SystemV);
        for _ in 0..2 {
            sig.params.push(AbiParam::new(I32));
        }
        sig.returns.push(AbiParam::new(I32));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        func.layout.append_block(b);
        let x = func.dfg.append_block_param(b, I32);
        let y = func.dfg.append_block_param(b, I32);
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(b);
            let mut deep = pos.ins().iadd(x, y);
            for _ in 0..6 {
                deep = pos.ins().rotl_imm(deep, 3);
            }
            let ones = pos.ins().iconst(I32, -1);
            let nx = pos.ins().bxor(x, ones);
            let k = pos.ins().iconst(I32, 0x80808080u32 as i64);
            let t = pos.ins().band(deep, nx);
            let u = pos.ins().band(t, k);
            pos.ins().return_(&[u]);
        }
        assert_eq!(run(&mut func), 0, "{}", func.display());
    }

    /// Already shallow-first: nothing to do.
    #[test]
    fn shallow_first_chain_stays() {
        let mut sig = Signature::new(CallConv::SystemV);
        for _ in 0..3 {
            sig.params.push(AbiParam::new(I32));
        }
        sig.returns.push(AbiParam::new(I32));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        func.layout.append_block(b);
        let a = func.dfg.append_block_param(b, I32);
        let b0 = func.dfg.append_block_param(b, I32);
        let m = func.dfg.append_block_param(b, I32);
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(b);
            let x = pos.ins().bxor(b0, a);
            let deep = pos.ins().rotl_imm(x, 7);
            let t = pos.ins().iadd(a, m);
            let u = pos.ins().iadd(t, deep);
            pos.ins().return_(&[u]);
        }
        assert_eq!(run(&mut func), 0);
    }
}
