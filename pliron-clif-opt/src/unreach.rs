//! Drop edges into unreachable code on Cranelift IR (LLVM SimplifyCFG's
//! `unreachable` handling). A block that only computes pure values and then
//! hits the `unreachable` trap (or jumps to such a block) can never be
//! entered in a well-defined execution, so a `brif` with one such target
//! becomes a `jump` to the other, and `br_table` entries are redirected.
//! This removes the range/null checks guarding `unreachable_unchecked`,
//! `unwrap_unchecked` and `match` `otherwise: unreachable` arms.
//!
//! On wasm, before that edge is dropped, a dominated `brif` on the same
//! relational compare is folded too. `assume(i < n)` lowers as a branch
//! to this trap; the slice check on `i < n` is the dominated copy.
//! Native does not fold: that use of the fact was not timed on native
//! (`run`'s `fold_facts` is `Pipe::wasm`).

use cranelift_codegen::ir::{
    Block, Function, InstructionData, Opcode, TrapCode, Value, ValueDef, condcodes::IntCC,
};
use rustc_hash::FxHashSet;

/// The trap code `lower` uses for LLVM `unreachable`.
pub const UNREACHABLE: u8 = 2;

fn quiet(func: &Function, i: cranelift_codegen::ir::Inst) -> bool {
    let op = func.dfg.insts[i].opcode();
    !op.is_call()
        && !op.can_store()
        && !op.can_trap()
        && !op.other_side_effects()
        && !op.is_terminator()
        && !op.is_branch()
}

fn dead_blocks(func: &Function) -> FxHashSet<Block> {
    let mut dead: FxHashSet<Block> = FxHashSet::default();
    loop {
        let mut changed = false;
        for b in func.layout.blocks() {
            if dead.contains(&b) {
                continue;
            }
            let Some(t) = func.layout.last_inst(b) else {
                continue;
            };
            let term_dead = match func.dfg.insts[t] {
                InstructionData::Trap {
                    opcode: Opcode::Trap,
                    code,
                } => code == TrapCode::unwrap_user(UNREACHABLE),
                InstructionData::Jump { destination, .. } => {
                    dead.contains(&destination.block(&func.dfg.value_lists))
                }
                _ => false,
            };
            if term_dead && func.layout.block_insts(b).all(|i| i == t || quiet(func, i)) {
                dead.insert(b);
                changed = true;
            }
        }
        if !changed {
            return dead;
        }
    }
}

/// Some `brif` enters a block that is just the `unreachable` trap: an
/// `assume` (or an `unreachable_unchecked` arm) whose condition is a
/// fact on the other edge. One look at each block's terminator.
pub fn has_assume_edge(func: &Function) -> bool {
    let is_trap = |b: Block| {
        func.layout.first_inst(b).is_some_and(|i| {
            matches!(
                func.dfg.insts[i],
                InstructionData::Trap { opcode: Opcode::Trap, code }
                    if code == TrapCode::unwrap_user(UNREACHABLE)
            )
        })
    };
    func.layout.blocks().any(|b| {
        func.layout.last_inst(b).is_some_and(|t| match func.dfg.insts[t] {
            InstructionData::Brif { blocks, .. } => blocks
                .iter()
                .any(|bc| is_trap(bc.block(&func.dfg.value_lists))),
            _ => false,
        })
    })
}

/// `PLIRON_SWITCH_KEEPDEAD=1`: `br_table`s keep an `unreachable` default
/// (only their entries are redirected), so a table that reaches Cranelift
/// drops its bounds clamp; brchain treats such a default as a don't-care.
pub static KEEP_DEFAULT: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_SWITCH_KEEPDEAD", "1"));

/// An icmp operand: a value, or an `iconst` compared by its bits so two
/// fresh copies of `n` still match before GVN.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Opnd {
    V(Value),
    K(i64),
}

fn opnd(func: &Function, v: Value) -> Opnd {
    let v = func.dfg.resolve_aliases(v);
    if let ValueDef::Result(i, _) = func.dfg.value_def(v)
        && let InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } = func.dfg.insts[i]
    {
        return Opnd::K(imm.bits());
    }
    Opnd::V(v)
}

/// Same relational compare. Equality stays out: this fold is the
/// `assume(i < n)` fact, not a second copy of the eq/ne retest.
fn rel_key(func: &Function, v: Value) -> Option<(IntCC, Opnd, Opnd)> {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::IntCompare { cond, args, .. }
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual) =>
        {
            Some((cond, opnd(func, args[0]), opnd(func, args[1])))
        }
        _ => None,
    }
}

/// `brif c, live, unreach` makes `c` known on `live`. A later `brif` on
/// the same compare, in a block `live` dominates, takes that side.
/// The source branch itself is left for the edge-drop below: its block
/// is not dominated by `live`.
fn fold_dominated(func: &mut Function, dead: &FxHashSet<Block>) -> usize {
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    let mut facts = Vec::new();
    for b in func.layout.blocks() {
        if dead.contains(&b) {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif {
            arg,
            blocks: [then, els],
            ..
        } = func.dfg.insts[t]
        else {
            continue;
        };
        let (tb, eb) = (
            then.block(&func.dfg.value_lists),
            els.block(&func.dfg.value_lists),
        );
        let (live, holds) = match (dead.contains(&tb), dead.contains(&eb)) {
            (false, true) => (tb, true),
            (true, false) => (eb, false),
            _ => continue,
        };
        let Some(k) = rel_key(func, arg) else {
            continue;
        };
        facts.push((live, k, holds));
    }
    if facts.is_empty() {
        return 0;
    }
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        if dead.contains(&b) {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif {
            arg,
            blocks: [then, els],
            ..
        } = func.dfg.insts[t]
        else {
            continue;
        };
        let (tb, eb) = (
            then.block(&func.dfg.value_lists),
            els.block(&func.dfg.value_lists),
        );
        if dead.contains(&tb) || dead.contains(&eb) {
            continue;
        }
        let Some(k) = rel_key(func, arg) else {
            continue;
        };
        let Some(holds) = facts
            .iter()
            .find(|(live, fk, _)| *fk == k && dom.try_dominates(*live, b, &func.layout))
            .map(|(_, _, h)| *h)
        else {
            continue;
        };
        let keep = if holds { then } else { els };
        func.dfg.insts[t] = InstructionData::Jump {
            opcode: Opcode::Jump,
            destination: keep,
        };
        n += 1;
    }
    n
}

/// Bits of `v`'s type, for the constant evaluator (ints up to 64 bits).
fn vbits(func: &Function, v: Value) -> Option<u32> {
    let t = func.dfg.value_type(v);
    (t.is_int() && t.bits() <= 64).then(|| t.bits())
}

fn mask(x: u64, bits: u32) -> u64 {
    if bits >= 64 { x } else { x & ((1u64 << bits) - 1) }
}

fn sext(x: u64, bits: u32) -> i64 {
    if bits >= 64 {
        x as i64
    } else {
        ((x << (64 - bits)) as i64) >> (64 - bits)
    }
}

/// The value of `v` when the block params in `env` hold the given
/// constants; `None` when it depends on anything else. `fuel` bounds
/// the walk (shared across the whole evaluation).
fn ceval(func: &Function, v: Value, env: &[(Value, u64)], fuel: &mut u32) -> Option<u64> {
    if *fuel == 0 {
        return None;
    }
    *fuel -= 1;
    let v = func.dfg.resolve_aliases(v);
    let bits = vbits(func, v)?;
    if let Some(&(_, k)) = env.iter().find(|(p, _)| *p == v) {
        return Some(mask(k, bits));
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    let r = match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => imm.bits() as u64,
        InstructionData::IntCompare { cond, args, .. } => {
            let ob = vbits(func, args[0])?;
            let a = ceval(func, args[0], env, fuel)?;
            let b = ceval(func, args[1], env, fuel)?;
            let (sa, sb) = (sext(a, ob), sext(b, ob));
            let r = match cond {
                IntCC::Equal => a == b,
                IntCC::NotEqual => a != b,
                IntCC::UnsignedLessThan => a < b,
                IntCC::UnsignedLessThanOrEqual => a <= b,
                IntCC::UnsignedGreaterThan => a > b,
                IntCC::UnsignedGreaterThanOrEqual => a >= b,
                IntCC::SignedLessThan => sa < sb,
                IntCC::SignedLessThanOrEqual => sa <= sb,
                IntCC::SignedGreaterThan => sa > sb,
                IntCC::SignedGreaterThanOrEqual => sa >= sb,
            };
            r as u64
        }
        InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } => {
            let c = ceval(func, args[0], env, fuel)?;
            ceval(func, if c != 0 { args[1] } else { args[2] }, env, fuel)?
        }
        InstructionData::Binary { opcode, args } => {
            let a = ceval(func, args[0], env, fuel)?;
            let b = ceval(func, args[1], env, fuel)?;
            match opcode {
                Opcode::Band => a & b,
                Opcode::Bor => a | b,
                Opcode::Bxor => a ^ b,
                Opcode::Iadd => a.wrapping_add(b),
                Opcode::Isub => a.wrapping_sub(b),
                _ => return None,
            }
        }
        InstructionData::Unary { opcode, arg } => {
            let ab = vbits(func, arg)?;
            let a = ceval(func, arg, env, fuel)?;
            match opcode {
                Opcode::Ireduce | Opcode::Uextend => a,
                Opcode::Sextend => sext(a, ab) as u64,
                Opcode::Bnot => !a,
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(mask(r, bits))
}

/// Edges that pass a constant into a block param and thereby decide a
/// `brif` toward unreachable code are themselves unreachable: redirect
/// them straight to the dead block, so the caller's `dead_blocks` /
/// edge-drop removes the test that guarded them too.
///
/// `unwrap_unchecked` on an `Option<&T>` built by a branch is the shape:
/// `P(p = phi(null, ptr))`, `brif p != 0, live, dead`. The `null` edge
/// cannot be taken, so the `ptr == end` branch selecting it folds.
///
/// Between the param block and the `brif` only quiet instructions run
/// (a chain of single-predecessor jumps), so entering along the edge
/// reaches the trap with no effect in between. One pass over the
/// blocks; each evaluation has a fixed fuel.
fn thread_const_edges(func: &mut Function, dead: &FxHashSet<Block>) -> usize {
    use cranelift_codegen::ir::Inst;
    // Predecessor edges: (pred terminator, slot in its `blocks`).
    let mut preds: cranelift_codegen::entity::SecondaryMap<Block, Vec<(Inst, u8)>> =
        cranelift_codegen::entity::SecondaryMap::new();
    let mut table_target: FxHashSet<Block> = FxHashSet::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => {
                preds[destination.block(&func.dfg.value_lists)].push((t, 0));
            }
            InstructionData::Brif { blocks, .. } => {
                for (k, bc) in blocks.iter().enumerate() {
                    preds[bc.block(&func.dfg.value_lists)].push((t, k as u8));
                }
            }
            InstructionData::BranchTable { table, .. } => {
                for bc in func.dfg.jump_tables[table].all_branches() {
                    table_target.insert(bc.block(&func.dfg.value_lists));
                }
            }
            _ => {}
        }
    }
    let all_quiet = |func: &Function, b: Block| {
        let t = func.layout.last_inst(b);
        func.layout.block_insts(b).all(|i| Some(i) == t || quiet(func, i))
    };
    // (pred terminator, slot, dead block to enter instead)
    let mut redirects: Vec<(Inst, u8, Block)> = Vec::new();
    for b in func.layout.blocks() {
        if dead.contains(&b) {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif {
            arg,
            blocks: [then, els],
            ..
        } = func.dfg.insts[t]
        else {
            continue;
        };
        let (tb, eb) = (
            then.block(&func.dfg.value_lists),
            els.block(&func.dfg.value_lists),
        );
        // `dead_when`: the condition value that enters the dead block.
        let (d, dcall, dead_when) = match (dead.contains(&tb), dead.contains(&eb)) {
            (true, false) => (tb, then, true),
            (false, true) => (eb, els, false),
            _ => continue,
        };
        if !dcall.args(&func.dfg.value_lists).next().is_none() || !all_quiet(func, b) {
            continue;
        }
        // The join the test hangs off: `b`, or up a short chain of
        // single-predecessor quiet jumps.
        let mut pb = b;
        let mut ok = true;
        for _ in 0..4 {
            // A join: its params, or its edges' own branch conditions,
            // are what the test can depend on.
            let ps = &preds[pb];
            if !func.dfg.block_params(pb).is_empty() || ps.len() > 1 {
                break;
            }
            if ps.len() != 1 || table_target.contains(&pb) {
                ok = false;
                break;
            }
            let (pt, _) = ps[0];
            if !matches!(func.dfg.insts[pt], InstructionData::Jump { .. }) {
                ok = false;
                break;
            }
            let Some(q) = func.layout.inst_block(pt) else {
                ok = false;
                break;
            };
            if q == pb || !all_quiet(func, q) {
                ok = false;
                break;
            }
            pb = q;
        }
        if !ok
            || (func.dfg.block_params(pb).is_empty() && preds[pb].len() < 2)
            || table_target.contains(&pb)
        {
            continue;
        }
        let params: Vec<Value> = func.dfg.block_params(pb).to_vec();
        let mut env: Vec<(Value, u64)> = Vec::new();
        for &(pt, slot) in &preds[pb] {
            let bc = match func.dfg.insts[pt] {
                InstructionData::Jump { destination, .. } => destination,
                InstructionData::Brif { blocks, .. } => blocks[slot as usize],
                _ => continue,
            };
            env.clear();
            for (p, a) in params.iter().zip(bc.args(&func.dfg.value_lists)) {
                let Some(a) = a.as_value() else { continue };
                if let Opnd::K(k) = opnd(func, a) {
                    env.push((*p, k as u64));
                }
            }
            // Branch conditions known along the edge: the edge's own
            // `brif`, and those up the single-predecessor chain above it
            // (jumpthread turns `phi(null, p) == null` into the test that
            // chose the `null` arm, so the fact is that test's value).
            let mut cur = Some((pt, slot));
            for _ in 0..4 {
                let Some((ct, cs)) = cur else { break };
                if let InstructionData::Brif { arg: c2, .. } = func.dfg.insts[ct] {
                    let c2 = func.dfg.resolve_aliases(c2);
                    let is_cmp = matches!(
                        func.dfg.value_def(c2),
                        ValueDef::Result(ci, _)
                            if matches!(func.dfg.insts[ci], InstructionData::IntCompare { .. })
                    );
                    if cs == 1 {
                        env.push((c2, 0));
                    } else if is_cmp {
                        env.push((c2, 1));
                    }
                }
                let Some(q) = func.layout.inst_block(ct) else { break };
                cur = match preds[q].as_slice() {
                    [one] if !table_target.contains(&q) && q != pb => Some(*one),
                    _ => None,
                };
            }
            if env.is_empty() {
                continue;
            }
            let mut fuel = 48;
            if let Some(c) = ceval(func, arg, &env, &mut fuel)
                && (c != 0) == dead_when
            {
                redirects.push((pt, slot, d));
            }
        }
    }
    let n = redirects.len();
    for (pt, slot, d) in redirects {
        let call = cranelift_codegen::ir::BlockCall::new(d, [], &mut func.dfg.value_lists);
        match &mut func.dfg.insts[pt] {
            InstructionData::Jump { destination, .. } => *destination = call,
            InstructionData::Brif { blocks, .. } => blocks[slot as usize] = call,
            _ => unreachable!(),
        }
    }
    n
}

/// Returns the number of edges removed.
///
/// `fold_facts` also folds a dominated copy of the compare. The pipeline
/// passes `Pipe::wasm`. Native stays the edge-drop only: the fold was
/// not timed there.
pub fn run(func: &mut Function, fold_facts: bool) -> usize {
    run_with(func, fold_facts, false)
}

/// [`run`] with the constant-edge threading chosen by the caller.
pub fn run_with(func: &mut Function, fold_facts: bool, phi: bool) -> usize {
    let mut dead = dead_blocks(func);
    if dead.is_empty() {
        return 0;
    }
    let mut n = 0;
    // `phi` (`PLIRON_UNREACH_PHI`): constant edges that decide a test toward
    // unreachable code. A redirected edge can make its source block dead
    // in turn, so the dead set is recomputed (bounded rounds).
    if phi {
        for _ in 0..3 {
            let k = thread_const_edges(func, &dead);
            if k == 0 {
                break;
            }
            n += k;
            dead = dead_blocks(func);
        }
    }
    if fold_facts {
        n += fold_dominated(func, &dead);
    }
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for b in blocks {
        if dead.contains(&b) {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        match func.dfg.insts[t] {
            InstructionData::Brif {
                opcode: Opcode::Brif,
                blocks: [then, els],
                ..
            } => {
                let (tb, eb) = (
                    then.block(&func.dfg.value_lists),
                    els.block(&func.dfg.value_lists),
                );
                let keep = match (dead.contains(&tb), dead.contains(&eb)) {
                    (true, false) => els,
                    (false, true) => then,
                    _ => continue,
                };
                func.dfg.insts[t] = InstructionData::Jump {
                    opcode: Opcode::Jump,
                    destination: keep,
                };
                n += 1;
            }
            InstructionData::BranchTable { table, .. } => {
                let jt = &func.dfg.jump_tables[table];
                let pool = &func.dfg.value_lists;
                let Some(live) = jt
                    .all_branches()
                    .iter()
                    .find(|bc| !dead.contains(&bc.block(pool)))
                    .copied()
                else {
                    continue;
                };
                let live = live.deep_clone(&mut func.dfg.value_lists);
                let dfg = &mut func.dfg;
                let pool = &mut dfg.value_lists;
                let jt = &mut dfg.jump_tables[table];
                let branches = if *KEEP_DEFAULT { jt.as_mut_slice() } else { jt.all_branches_mut() };
                for bc in branches {
                    if dead.contains(&bc.block(pool)) {
                        *bc = live.deep_clone(pool);
                        n += 1;
                    }
                }
            }
            _ => {}
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::{
        AbiParam, Function, InstBuilder, Signature, UserFuncName, types,
    };
    use cranelift_codegen::isa::CallConv;

    fn brifs(func: &Function) -> usize {
        func.layout
            .blocks()
            .filter(|&b| {
                func.layout.last_inst(b).is_some_and(|t| {
                    matches!(func.dfg.insts[t], InstructionData::Brif { .. })
                })
            })
            .count()
    }

    /// `assume(i < assume_k)` as a trap edge, then a slice check on a
    /// fresh `iconst check_k`. Returns `(func, mid, ok)`.
    fn assume_then_check(assume_k: i64, check_k: i64) -> (Function, Block, Block) {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I32));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let mid = func.dfg.make_block();
        let ok = func.dfg.make_block();
        let panicb = func.dfg.make_block();
        let dead = func.dfg.make_block();
        for b in [entry, mid, ok, panicb, dead] {
            func.layout.append_block(b);
        }
        let i = func.dfg.append_block_param(entry, types::I32);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let k1 = cur.ins().iconst(types::I32, assume_k);
        let c1 = cur.ins().icmp(IntCC::UnsignedLessThan, i, k1);
        cur.ins().brif(c1, mid, &[], dead, &[]);
        FuncCursor::new(&mut func)
            .at_bottom(dead)
            .ins()
            .trap(TrapCode::unwrap_user(UNREACHABLE));
        let mut cur = FuncCursor::new(&mut func).at_bottom(mid);
        let k2 = cur.ins().iconst(types::I32, check_k);
        let c2 = cur.ins().icmp(IntCC::UnsignedLessThan, i, k2);
        cur.ins().brif(c2, ok, &[], panicb, &[]);
        FuncCursor::new(&mut func)
            .at_bottom(panicb)
            .ins()
            .trap(TrapCode::unwrap_user(1));
        FuncCursor::new(&mut func).at_bottom(ok).ins().return_(&[]);
        (func, mid, ok)
    }

    /// `assume(i < 54)` as a trap edge, then the slice check on a second
    /// `iconst 54`. The check becomes a jump; the assume edge does too.
    #[test]
    fn assume_ult_folds_the_later_check() {
        let (mut func, mid, ok) = assume_then_check(54, 54);
        assert_eq!(brifs(&func), 2);
        run(&mut func, true);
        assert_eq!(brifs(&func), 0, "assume and the dominated check both go");
        let mid_term = func.layout.last_inst(mid).unwrap();
        match func.dfg.insts[mid_term] {
            InstructionData::Jump { destination, .. } => {
                assert_eq!(destination.block(&func.dfg.value_lists), ok);
            }
            _ => panic!("check was not folded to the in-bounds arm"),
        }
    }

    /// A different bound is not the assume's fact.
    #[test]
    fn assume_ult_leaves_a_different_bound() {
        let (mut func, _, _) = assume_then_check(54, 7);
        run(&mut func, true);
        assert_eq!(brifs(&func), 1, "the other bound stays");
    }
    fn jumps_to(func: &Function, b: Block) -> Option<Block> {
        match func.dfg.insts[func.layout.last_inst(b)?] {
            InstructionData::Jump { destination, .. } => {
                Some(destination.block(&func.dfg.value_lists))
            }
            _ => None,
        }
    }

    /// `unwrap_unchecked` on `if p == e { None } else { Some(p) }`:
    /// `join(q = phi(0, p))`, `brif q, live, dead`. `store_first` puts a
    /// store ahead of the test in `join`. Returns `(func, entry, some)`.
    fn phi_null(store_first: bool) -> (Function, Block, Block) {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(types::I32));
        sig.returns.push(AbiParam::new(types::I32));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let [entry, none, some, join, live, dead] = [(); 6].map(|_| func.dfg.make_block());
        for b in [entry, none, some, join, live, dead] {
            func.layout.append_block(b);
        }
        let p = func.dfg.append_block_param(entry, types::I32);
        let e = func.dfg.append_block_param(entry, types::I32);
        let q = func.dfg.append_block_param(join, types::I32);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let c = cur.ins().icmp(IntCC::Equal, p, e);
        cur.ins().brif(c, none, &[], some, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(none);
        let z = cur.ins().iconst(types::I32, 0);
        cur.ins().jump(join, &[z.into()]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(some);
        cur.ins().jump(join, &[p.into()]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(join);
        if store_first {
            cur.ins().store(cranelift_codegen::ir::MemFlagsData::trusted(), q, e, 0);
        }
        // The frontend's shape: `band (ireduce (select (q == 0), 0, 1)), 1`.
        let z = cur.ins().iconst(types::I32, 0);
        let isnull = cur.ins().icmp(IntCC::Equal, q, z);
        let one = cur.ins().iconst(types::I32, 1);
        let sel = cur.ins().select(isnull, z, one);
        let r = cur.ins().ireduce(types::I8, sel);
        let k1 = cur.ins().iconst(types::I8, 1);
        let m = cur.ins().band(r, k1);
        cur.ins().brif(m, live, &[], dead, &[]);
        FuncCursor::new(&mut func)
            .at_bottom(dead)
            .ins()
            .trap(TrapCode::unwrap_user(UNREACHABLE));
        let mut cur = FuncCursor::new(&mut func).at_bottom(live);
        let v = cur.ins().load(types::I32, cranelift_codegen::ir::MemFlagsData::trusted(), q, 0);
        cur.ins().return_(&[v]);
        (func, entry, some)
    }

    /// The `null` edge decides the test toward the trap, so the branch
    /// that chose it goes: `entry` jumps to the `Some` arm.
    #[test]
    fn phi_null_edge_drops_the_guard() {
        let (mut func, entry, some) = phi_null(false);
        assert_eq!(brifs(&func), 2);
        run_with(&mut func, false, true);
        assert_eq!(brifs(&func), 0);
        assert_eq!(jumps_to(&func, entry), Some(some));
    }

    /// Without the threading only the test at the join goes.
    #[test]
    fn phi_null_edge_off_keeps_the_guard() {
        let (mut func, entry, _) = phi_null(false);
        run_with(&mut func, false, false);
        assert_eq!(brifs(&func), 1);
        assert_eq!(jumps_to(&func, entry), None);
    }

    /// A store between the join and the test is an effect on the way to
    /// the trap: the edge is left alone.
    #[test]
    fn phi_null_edge_keeps_an_effect() {
        let (mut func, entry, _) = phi_null(true);
        run_with(&mut func, false, true);
        assert_eq!(jumps_to(&func, entry), None);
    }

    /// jumpthread's form: the join tests the condition that chose the
    /// arm (`brif c, none, some` ... `brif c, dead, live`).
    #[test]
    fn same_condition_edge_drops_the_guard() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(types::I32));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let [entry, none, some, join, live, dead] = [(); 6].map(|_| func.dfg.make_block());
        for b in [entry, none, some, join, live, dead] {
            func.layout.append_block(b);
        }
        let p = func.dfg.append_block_param(entry, types::I32);
        let e = func.dfg.append_block_param(entry, types::I32);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let c = cur.ins().icmp(IntCC::Equal, p, e);
        cur.ins().brif(c, none, &[], some, &[]);
        FuncCursor::new(&mut func).at_bottom(none).ins().jump(join, &[]);
        FuncCursor::new(&mut func).at_bottom(some).ins().jump(join, &[]);
        FuncCursor::new(&mut func).at_bottom(join).ins().brif(c, dead, &[], live, &[]);
        FuncCursor::new(&mut func)
            .at_bottom(dead)
            .ins()
            .trap(TrapCode::unwrap_user(UNREACHABLE));
        FuncCursor::new(&mut func).at_bottom(live).ins().return_(&[]);
        run_with(&mut func, false, true);
        assert_eq!(brifs(&func), 0);
        assert_eq!(jumps_to(&func, entry), Some(some));
    }
}
