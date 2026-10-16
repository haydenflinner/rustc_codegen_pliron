//! Accumulator tail-recursion elimination (LLVM's TRE-for-accumulators):
//! `return op(expr, f(args))` / `return op(f(a), f(b))` with `op`
//! associative+commutative becomes `acc op= expr; jump header(args)` — a
//! loop instead of the second call, which for binary recursion
//! (`fib(n-1) + fib(n-2)`) roughly halves the call count.
//!
//! The loop can't re-enter the entry block: its params are signature-bound
//! and can't carry the accumulator. Instead the entry block's insts move
//! into a new header block `h` with one extra `acc` param, entry becomes
//! `jump h(args, identity)`, and entry params are renamed to `h`'s —
//! `h` dominates every original use, so `acc` reaches every `return` as
//! an ordinary SSA operand. A cold all-zero `return` is the emulated-EH
//! eject: its value is dead once the flag is set, so it's left alone.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    BlockArg, ExternalName, FuncRef, Function, Inst, InstBuilder, InstructionData, Opcode, Type,
    Value, types,
};
use cranelift_module::FuncId;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.resolved_value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

/// Identity of an associative+commutative integer op.
fn accop(op: Opcode, t: Type) -> Option<i64> {
    let bits = t.bits() as i64;
    Some(match op {
        Opcode::Iadd | Opcode::Bor | Opcode::Bxor | Opcode::Umax => 0,
        Opcode::Imul => 1,
        Opcode::Band | Opcode::Umin => -1,
        Opcode::Smin => (1i64 << (bits - 1)) - 1,
        Opcode::Smax => -1i64 << (bits - 1),
        _ => return None,
    })
}

fn emit_op(pos: &mut FuncCursor, op: Opcode, x: Value, y: Value) -> Value {
    match op {
        Opcode::Iadd => pos.ins().iadd(x, y),
        Opcode::Bor => pos.ins().bor(x, y),
        Opcode::Bxor => pos.ins().bxor(x, y),
        Opcode::Imul => pos.ins().imul(x, y),
        Opcode::Umin => pos.ins().umin(x, y),
        Opcode::Umax => pos.ins().umax(x, y),
        Opcode::Band => pos.ins().band(x, y),
        Opcode::Smin => pos.ins().smin(x, y),
        Opcode::Smax => pos.ins().smax(x, y),
        _ => unreachable!(),
    }
}

/// Number of times `v` is used as an operand (inst args + branch args).
fn uses(func: &Function, v: Value) -> usize {
    let v = func.dfg.resolve_aliases(v);
    func.layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .map(|i| {
            func.dfg
                .inst_values(i)
                .filter(|&u| func.dfg.resolve_aliases(u) == v)
                .count()
        })
        .sum()
}

/// Rewrite every use of `old` to `new` — inst args, branch args, and
/// exception-table contexts alike.
fn rename(func: &mut Function, old: Value, new: Value) {
    let insts: Vec<Inst> = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .collect();
    for i in insts {
        func.dfg
            .map_inst_values(i, |v| if v == old { new } else { v });
    }
}

/// `ir::FuncRef` whose user name refers back to `id`, if one is declared.
fn self_ref(func: &Function, id: FuncId) -> Option<FuncRef> {
    func.dfg.ext_funcs.iter().find_map(|(fr, d)| {
        let ExternalName::User(u) = &d.name else {
            return None;
        };
        let n = &func.params.user_named_funcs()[*u];
        (n.namespace == 0 && n.index == id.as_u32()).then_some(fr)
    })
}

fn is_self_call(func: &Function, i: Inst, fr: FuncRef) -> bool {
    matches!(
        func.dfg.insts[i],
        InstructionData::Call { func_ref, .. } if func_ref == fr
    )
}

pub fn run(func: &mut Function, id: FuncId) -> usize {
    if func.signature.returns.len() != 1 {
        return 0;
    }
    let rty = func.signature.returns[0].value_type;
    if !rty.is_int() || rty == types::I128 {
        return 0;
    }
    let Some(sref) = self_ref(func, id) else {
        return 0;
    };
    // Find `call self; ..; acc_op(expr, r); return`: the call, the
    // accumulate, and the return must be the block's last three insts so
    // nothing observes the removed call between them.
    let mut cand = None;
    'blk: for b in func.layout.blocks() {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        let n = insts.len();
        if n < 3 {
            continue;
        }
        let (ci, bi, ri) = (insts[n - 3], insts[n - 2], insts[n - 1]);
        if func.dfg.insts[ri].opcode() != Opcode::Return
            || func.dfg.inst_args(ri).len() != 1
            || !is_self_call(func, ci, sref)
            || func.dfg.inst_args(ci).len() != func.signature.params.len()
        {
            continue;
        }
        let InstructionData::Binary { opcode, args } = func.dfg.insts[bi] else {
            continue;
        };
        let Some(ident) = accop(opcode, rty) else {
            continue;
        };
        let rv = func.dfg.resolve_aliases(func.dfg.inst_args(ri)[0]);
        if func.dfg.value_def(rv).inst() != Some(bi) {
            continue;
        }
        // The accumulated side can be any value (a second self call in
        // `fib`); the looped side must be this call's result used nowhere
        // else.
        let &[rc] = func.dfg.inst_results(ci) else {
            continue;
        };
        let (a0, a1) = (
            func.dfg.resolve_aliases(args[0]),
            func.dfg.resolve_aliases(args[1]),
        );
        let eix = if a0 == rc {
            1
        } else if a1 == rc {
            0
        } else {
            continue;
        };
        if func.dfg.value_type(args[eix]) != rty || uses(func, rc) != 1 {
            continue;
        }
        cand = Some((bi, ri, opcode, ident, eix, ci));
        break 'blk;
    }
    let Some((bi, ri, opcode, ident, eix, ci)) = cand else {
        return 0;
    };
    let entry = func.layout.entry_block().unwrap();
    let eparams = func.dfg.block_params(entry).to_vec();
    // Split the header out of entry: h takes (sig params.., acc).
    let h = func.dfg.make_block();
    func.layout.insert_block_after(h, entry);
    let hparams: Vec<Value> = eparams
        .iter()
        .map(|&p| {
            let t = func.dfg.value_type(p);
            func.dfg.append_block_param(h, t)
        })
        .collect();
    let acc = func.dfg.append_block_param(h, rty);
    for i in func.layout.block_insts(entry).collect::<Vec<_>>() {
        func.layout.remove_inst(i);
        func.layout.append_inst(i, h);
    }
    for (&p, &hp) in eparams.iter().zip(&hparams) {
        rename(func, p, hp);
    }
    // Re-read the candidate's operands after the rename: entry-param
    // operands now carry h's params, not the stale originals.
    let expr = func.dfg.inst_args(bi)[eix];
    let call_args: Vec<Value> = func.dfg.inst_args(ci).to_vec();
    {
        let mut pos = FuncCursor::new(func).at_bottom(entry);
        let k = pos.ins().iconst(rty, ident);
        let mut ja: Vec<BlockArg> = eparams.iter().copied().map(BlockArg::Value).collect();
        ja.push(BlockArg::Value(k));
        pos.ins().jump(h, &ja);
    }
    // Every other `return v` yields `op(v, acc)`; a cold all-zero return is
    // the emulated-EH eject — its value is dead once the flag is set.
    let rets: Vec<Inst> = func
        .layout
        .blocks()
        .filter_map(|b| func.layout.last_inst(b))
        .filter(|&i| {
            i != ri
                && func.dfg.insts[i].opcode() == Opcode::Return
                && func.dfg.inst_args(i).len() == 1
        })
        .collect();
    for r in rets {
        let b = func.layout.inst_block(r).unwrap();
        let rv = func.dfg.inst_args(r)[0];
        if func.layout.is_cold(b) && iconst(func, rv) == Some(0) {
            continue;
        }
        let mut pos = FuncCursor::new(func).at_inst(r);
        let nv = emit_op(&mut pos, opcode, rv, acc);
        for x in func.dfg.inst_args_mut(r) {
            *x = nv;
        }
    }
    // In the candidate block: acc op= expr, then loop on the call's args.
    {
        let mut pos = FuncCursor::new(func).at_inst(ri);
        let acc2 = emit_op(&mut pos, opcode, acc, expr);
        let mut ja: Vec<BlockArg> = call_args.iter().copied().map(BlockArg::Value).collect();
        ja.push(BlockArg::Value(acc2));
        pos.ins().jump(h, &ja);
    }
    func.layout.remove_inst(ci);
    func.layout.remove_inst(bi);
    func.layout.remove_inst(ri);
    1
}
