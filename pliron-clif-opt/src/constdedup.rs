//! Constant dedup — alias duplicate constants to their dominating
//! definition (`PLIRON_CSTDEDUP`).
//!
//! Block-cloning passes (`punroll`, `edgespec`, `jumpthread`) stamp a
//! fresh `iconst.i8 0` per copy; downstream that is one `mov w8, #0`
//! before every unrolled `strb` instead of a single shared register for
//! the whole loop (LLVM keeps the zero in a pinned reg or folds to
//! `wzr`). Scoped CSE over the dominator tree: on block entry the
//! block's constants are recorded, on exit dropped; a duplicate in a
//! dominated block aliases to the dominating value and dies.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Opcode, Type, Value, types};
use rustc_hash::FxHashMap;

/// Walk every reachable block; per-block work is trivial.
const MAX_BLOCKS: usize = 16384;

/// (result type, bit pattern) of a rematerializable constant.
type Key = (Type, u64);

fn key(func: &Function, i: Inst) -> Option<Key> {
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some((
            func.dfg.value_type(func.dfg.first_result(i)),
            imm.bits() as u64,
        )),
        InstructionData::UnaryIeee32 { imm, .. } => Some((types::F32, u64::from(imm.bits()))),
        InstructionData::UnaryIeee64 { imm, .. } => Some((types::F64, imm.bits())),
        _ => None,
    }
}

pub fn run(func: &mut Function, dt: &DominatorTree) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    if func.layout.blocks().count() > MAX_BLOCKS || !dt.is_reachable(entry) {
        return 0;
    }
    let mut map: FxHashMap<Key, Value> = FxHashMap::default();
    let mut scope: Vec<Vec<Key>> = Vec::new();
    let mut stack: Vec<(Block, bool)> = vec![(entry, false)];
    let mut n = 0;
    while let Some((b, exiting)) = stack.pop() {
        if exiting {
            for k in scope.pop().unwrap() {
                map.remove(&k);
            }
            continue;
        }
        // Exit marker goes under the children, so its scope pop happens
        // only after every dominated subtree is done.
        stack.push((b, true));
        scope.push(Vec::new());
        let added = scope.last_mut().unwrap();
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let Some(k) = key(func, i) else {
                continue;
            };
            let res = func.dfg.first_result(i);
            match map.get(&k) {
                Some(&v) => {
                    func.layout.remove_inst(i);
                    func.dfg.clear_results(i);
                    func.dfg.change_to_alias(res, v);
                    n += 1;
                }
                None => {
                    map.insert(k, res);
                    added.push(k);
                }
            }
        }
        for c in dt.children(b) {
            stack.push((c, false));
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    use cranelift_codegen::ir::{AbiParam, BlockArg, InstBuilder, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;

    fn dedup(func: &mut Function) -> usize {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        run(func, &dt)
    }

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(func, &flags) {
            panic!("constdedup produced invalid IR: {e}\n{}", func.display());
        }
    }

    fn iconsts(func: &Function) -> usize {
        let mut n = 0;
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                if let InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    ..
                } = func.dfg.insts[i]
                {
                    n += 1;
                }
            }
        }
        n
    }

    /// Two identical iconsts in one block merge; the surviving result is
    /// used by both consumers.
    #[test]
    fn same_block_merges() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        func.layout.append_block(b0);
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().iconst(types::I64, 0);
        let b = cur.ins().iconst(types::I64, 0);
        let s = cur.ins().iadd(a, b);
        cur.ins().return_(&[s]);
        assert_eq!(dedup(&mut func), 1);
        assert_eq!(iconsts(&func), 1);
        verify(&func);
    }

    /// A constant in a dominating block covers a dominated use; a
    /// same-valued constant in a SIBLING (non-dominating) block is left
    /// alone.
    #[test]
    fn dominance_scoped() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let l = func.dfg.make_block();
        let r = func.dfg.make_block();
        let join = func.dfg.make_block();
        for b in [b0, l, r, join] {
            func.layout.append_block(b);
        }

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let cz = cur.ins().iconst(types::I64, 0);
        let c1 = cur.ins().iconst(types::I8, 1);
        cur.ins().brif(c1, l, &[], r, &[]);

        // Dominated block: its iconst(0) aliases to b0's.
        let mut cur = FuncCursor::new(&mut func).at_bottom(l);
        let lz = cur.ins().iconst(types::I64, 0);
        let ls = cur.ins().iadd(lz, cz);
        cur.ins().jump(join, &[BlockArg::Value(ls)]);

        // Sibling block: its own iconst(7) is NOT dominated by a def,
        // so it stays.
        let mut cur = FuncCursor::new(&mut func).at_bottom(r);
        let rz = cur.ins().iconst(types::I64, 7);
        cur.ins().jump(join, &[BlockArg::Value(rz)]);

        let p = func.dfg.append_block_param(join, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(join);
        cur.ins().return_(&[p]);

        assert_eq!(dedup(&mut func), 1);
        verify(&func);
    }
}
