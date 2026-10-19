//! Critical-path reassociation of accumulator chains
//! (`PLIRON_REASSOC=1` enables).
//!
//! ```text
//! h(v_acc): ... t1 = iadd v_acc, a
//!              t2 = iadd t1, b
//!              t3 = iadd t2, c
//!              jump h(t3)
//! ```
//!
//! A straight-line sum `((r + a) + b) + c` puts `k` dependent adds on
//! `r`'s path; when `r` is a loop-carried block param the chain
//! latency compounds every iteration. Reassociating to
//! `r + ((a + b) + c)` leaves one add on `r`'s edge — the `x` tree is
//! intra-iteration work that schedules in parallel. LLVM's isel
//! produces that shape on its own (eqbug's `f8+f16+fu8` accumulator:
//! depth-2 tree vs our depth-3 serial chain, ~1 cycle/iter ≈ +9%).
//!
//! Rewrite rules: the chain lives in one block, every intermediate is
//! single-use, and the head's non-chain operand is a block param of
//! that block. The new `x` adds are emitted at the chain end (every
//! operand's def precedes it) and the dead intermediates are removed.
//! Only associative integer ops — `iadd`, `imul`, `band`, `bor`,
//! `bxor`, `smin`, `smax`, `umin`, `umax`. Saturating/float adds are
//! not associative and never match.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Block, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef};

/// Associative integer ops whose argument order is free.
fn assoc(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Iadd
            | Opcode::Imul
            | Opcode::Band
            | Opcode::Bor
            | Opcode::Bxor
            | Opcode::Smin
            | Opcode::Smax
            | Opcode::Umin
            | Opcode::Umax
    )
}

/// `v` is defined by same-opcode inst `d` sitting in `block`, and `d`'s
/// result's only use site is `user` — `d` is a chain link.
fn link(
    func: &Function,
    uses: &crate::uses::Uses,
    op: Opcode,
    block: Block,
    user: Inst,
    v: Value,
) -> Option<Inst> {
    let ValueDef::Result(d, _) = func.dfg.value_def(v) else {
        return None;
    };
    if func.dfg.insts[d].opcode() != op || func.layout.inst_block(d) != Some(block) {
        return None;
    }
    let sites = uses.sites(v);
    (sites.len() == 1 && sites[0].1 == user).then_some(d)
}

/// A plan to rebuild `end` as `r ⊕ ((x0 + x1) + x2) …` where `r` is a
/// block param and `links` are the dead single-use intermediates.
struct Plan {
    op: Opcode,
    end: Inst,
    links: Vec<Inst>,
    r: Value,
    xs: Vec<Value>,
}

/// Decompose the single-use same-op chain ending at `end`. Returns
/// `None` for a bare `r ⊕ x` (nothing to shorten) or when neither head
/// operand is a block param of `block`.
fn plan(func: &Function, uses: &crate::uses::Uses, block: Block, end: Inst) -> Option<Plan> {
    let op = func.dfg.insts[end].opcode();
    if !assoc(op) {
        return None;
    }
    // Skip non-maximal ends: when `end`'s result feeds a same-block
    // same-op inst, the walk from there covers this chain.
    let res = func.dfg.inst_results(end)[0];
    if uses
        .sites(res)
        .iter()
        .any(|&(b, i)| b == block && func.dfg.insts[i].opcode() == op)
    {
        return None;
    }

    let mut links = Vec::new();
    let mut xs = Vec::new();
    let mut cur = end;
    loop {
        let args = func.dfg.inst_args(cur).to_vec();
        let mut next = None;
        for (pos, &a) in args.iter().enumerate() {
            let a = func.dfg.resolve_aliases(a);
            if let Some(d) = link(func, uses, op, block, cur, a) {
                next = Some((d, args[1 - pos]));
                break;
            }
        }
        match next {
            Some((d, x)) => {
                links.push(d);
                xs.push(x);
                cur = d;
            }
            None => break,
        }
    }
    if links.is_empty() {
        return None;
    }
    // `cur` is the chain head `r ⊕ x0`. Prefer a block-param operand
    // for `r`; either position commutes.
    let args = func.dfg.inst_args(cur).to_vec();
    let (r, x0) = match args.as_slice() {
        &[a, b] if matches!(func.dfg.value_def(a), ValueDef::Param(b_, _) if b_ == block) => (a, b),
        &[a, b] if matches!(func.dfg.value_def(b), ValueDef::Param(b_, _) if b_ == block) => (b, a),
        _ => return None,
    };
    xs.push(x0);
    xs.reverse(); // collected deepest-first; emit shallow first
    Some(Plan {
        op,
        end,
        links,
        r,
        xs,
    })
}

fn emit_op<'b>(
    pos: &mut FuncCursor<'b>,
    op: Opcode,
    a: Value,
    b: Value,
) -> Value {
    let ins = pos.ins();
    match op {
        Opcode::Iadd => ins.iadd(a, b),
        Opcode::Imul => ins.imul(a, b),
        Opcode::Band => ins.band(a, b),
        Opcode::Bor => ins.bor(a, b),
        Opcode::Bxor => ins.bxor(a, b),
        Opcode::Smin => ins.smin(a, b),
        Opcode::Smax => ins.smax(a, b),
        Opcode::Umin => ins.umin(a, b),
        Opcode::Umax => ins.umax(a, b),
        _ => unreachable!("reassoc: not an assoc op"),
    }
}

fn apply(func: &mut Function, pl: &Plan) {
    let mut acc = pl.xs[0];
    {
        let mut pos = FuncCursor::new(func).at_inst(pl.end);
        for &x in &pl.xs[1..] {
            acc = emit_op(&mut pos, pl.op, acc, x);
        }
    }
    match &mut func.dfg.insts[pl.end] {
        InstructionData::Binary { args, .. } => *args = [pl.r, acc],
        _ => unreachable!("reassoc: assoc ops are Binary"),
    }
    for &l in &pl.links {
        func.dfg.clear_results(l);
        func.layout.remove_inst(l);
    }
}

pub fn run(func: &mut Function, an: &mut crate::pipeline::Analysis) -> usize {
    let uses = an.uses(func);
    let mut plans = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let Some(pl) = plan(func, &uses, b, i) {
                plans.push(pl);
            }
        }
    }
    let n = plans.len();
    for pl in &plans {
        tracing::debug!(target: "pliron::reassoc", "reassoc {:?}: {} adds", pl.end, pl.xs.len());
        apply(func, pl);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::types::I64;
    use cranelift_codegen::ir::{
        AbiParam, BlockArg, Function, InstBuilder, Signature, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(
            cranelift_codegen::settings::builder(),
        );
        cranelift_codegen::verify_function(func, &flags).unwrap();
    }

    /// h(v_acc): t = ((v_acc + a) + b) + c; jump h(t) — plus a use of
    /// the block param elsewhere so it isn't dead.
    fn chain_func() -> (Function, Value) {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let h = func.dfg.make_block();
        for bl in [entry, h] {
            func.layout.append_block(bl);
        }
        let v0 = func.dfg.append_block_param(entry, I64);
        let acc = func.dfg.append_block_param(h, I64);
        let t;
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(entry);
            pos.ins().jump(h, &[BlockArg::from(v0)]);
        }
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(h);
            let a = pos.ins().iconst(I64, 7);
            let b = pos.ins().iconst(I64, 11);
            let c = pos.ins().iconst(I64, 13);
            let t1 = pos.ins().iadd(acc, a);
            let t2 = pos.ins().iadd(t1, b);
            t = pos.ins().iadd(t2, c);
            pos.ins().jump(h, &[BlockArg::from(t)]);
        }
        (func, t)
    }

    #[test]
    fn reassociates_loop_carried_chain() {
        let (mut func, t) = chain_func();
        let mut an = crate::pipeline::Analysis::default();
        let n = run(&mut func, &mut an);
        assert_eq!(n, 1);
        verify(&func);
        // The end inst now adds the block param to a fresh x-tree.
        let ValueDef::Result(ti, _) = func.dfg.value_def(t) else {
            panic!("t should be an inst result")
        };
        let h = func.dfg.value_def(func.dfg.inst_args(ti)[0]);
        assert!(matches!(h, ValueDef::Param(..)));
    }

    #[test]
    fn skips_chain_without_param_root() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let v0 = func.dfg.append_block_param(entry, I64);
        {
            let mut pos = FuncCursor::new(&mut func).at_bottom(entry);
            let a = pos.ins().iconst(I64, 3);
            // head is `a + a` — no block-param operand, so no rewrite.
            let b = pos.ins().iadd(a, a);
            let c = pos.ins().iadd(b, v0);
            let d = pos.ins().iadd(c, v0);
            pos.ins().return_(&[d]);
        }
        let mut an = crate::pipeline::Analysis::default();
        let n = run(&mut func, &mut an);
        assert_eq!(n, 0);
        verify(&func);
    }
}
