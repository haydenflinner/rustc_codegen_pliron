//! Drop edges into unreachable code on Cranelift IR (LLVM SimplifyCFG's
//! `unreachable` handling). A block that only computes pure values and then
//! hits the `unreachable` trap (or jumps to such a block) can never be
//! entered in a well-defined execution, so a `brif` with one such target
//! becomes a `jump` to the other, and `br_table` entries are redirected.
//! This removes the range/null checks guarding `unreachable_unchecked`,
//! `unwrap_unchecked` and `match` `otherwise: unreachable` arms.

use cranelift_codegen::ir::{Block, Function, InstructionData, Opcode, TrapCode};
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

/// Returns the number of edges removed.
pub fn run(func: &mut Function) -> usize {
    let dead = dead_blocks(func);
    if dead.is_empty() {
        return 0;
    }
    let mut n = 0;
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
