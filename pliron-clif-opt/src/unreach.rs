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

/// Returns the number of edges removed.
///
/// `fold_facts` also folds a dominated copy of the compare. The pipeline
/// passes `Pipe::wasm`. Native stays the edge-drop only: the fold was
/// not timed there.
pub fn run(func: &mut Function, fold_facts: bool) -> usize {
    let dead = dead_blocks(func);
    if dead.is_empty() {
        return 0;
    }
    let mut n = 0;
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
}
