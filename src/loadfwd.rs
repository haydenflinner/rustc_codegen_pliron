//! Cross-block load forwarding on Cranelift IR (LLVM GVN's load elimination,
//! minus PRE). A location is a root (SSA pointer or stack slot) plus a
//! constant offset and a type. Unlike Cranelift's alias analysis, a store
//! only kills locations it may overlap: same root and overlapping bytes, or
//! any location when the roots may alias. Calls, atomics, fences and
//! trapping (volatile) stores kill everything; only `notrap` loads/stores
//! (all non-volatile accesses at -O) are forwarded.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, Function, Inst, InstructionData, Opcode, StackSlot, Type, Value,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

const MAX_ENTRIES: usize = 64;
const MAX_ITERS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Root {
    V(Value),
    S(StackSlot),
}

type Loc = (Root, i64, Type);
type Avail = FxHashMap<Loc, Value>;

fn root(func: &Function, v: Value) -> (Root, i64) {
    let mut v = func.dfg.resolve_aliases(v);
    let mut off = 0i64;
    for _ in 0..8 {
        let Some(i) = func.dfg.value_def(v).inst() else {
            break;
        };
        match func.dfg.insts[i] {
            InstructionData::StackAddr {
                stack_slot, offset, ..
            } => {
                return (
                    Root::S(stack_slot),
                    off.wrapping_add(i64::from(i32::from(offset))),
                );
            }
            InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            } => {
                let c = |x: Value| match &func.dfg.insts[func.dfg.value_def(x).inst()?] {
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                    } => Some(imm.bits()),
                    _ => None,
                };
                let (a, b) = (
                    func.dfg.resolve_aliases(args[0]),
                    func.dfg.resolve_aliases(args[1]),
                );
                if let Some(k) = c(b) {
                    off = off.wrapping_add(k);
                    v = a;
                } else if let Some(k) = c(a) {
                    off = off.wrapping_add(k);
                    v = b;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    (Root::V(v), off)
}

fn notrap(func: &Function, i: Inst) -> bool {
    func.dfg.insts[i].memflags().is_some()
        && func.dfg.insts[i].memflags_trap_code(&func.dfg).is_none()
}

/// Kill the locations a store of `ty` at `(r, o)` may overwrite.
fn kill(av: &mut Avail, r: Root, o: i64, ty: Type) {
    let n = i64::from(ty.bytes());
    av.retain(|&(r2, o2, t2), _| match (r, r2) {
        _ if r == r2 => o2 >= o + n || o >= o2 + i64::from(t2.bytes()),
        (Root::S(_), Root::S(_)) => true,
        _ => false,
    });
}

/// Run `b`'s transfer function; with `rw`, record forwarded loads.
fn transfer(func: &Function, b: Block, av: &mut Avail, rw: Option<&mut Vec<(Inst, Value)>>) {
    let mut rw = rw;
    for i in func.layout.block_insts(b) {
        let op = func.dfg.insts[i].opcode();
        match func.dfg.insts[i] {
            InstructionData::Load {
                opcode: Opcode::Load,
                arg,
                offset,
                ..
            } if notrap(func, i) => {
                let res = func.dfg.first_result(i);
                let ty = func.dfg.value_type(res);
                let (r, o) = root(func, arg);
                let loc = (r, o.wrapping_add(i64::from(i32::from(offset))), ty);
                if let Some(&v) = av.get(&loc) {
                    if let Some(rw) = rw.as_deref_mut() {
                        rw.push((i, v));
                    }
                } else if av.len() < MAX_ENTRIES {
                    av.insert(loc, res);
                }
            }
            InstructionData::Store {
                opcode: Opcode::Store,
                args,
                offset,
                ..
            } if notrap(func, i) => {
                let val = func.dfg.resolve_aliases(args[0]);
                let ty = func.dfg.value_type(val);
                let (r, o) = root(func, args[1]);
                let o = o.wrapping_add(i64::from(i32::from(offset)));
                kill(av, r, o, ty);
                if av.len() < MAX_ENTRIES {
                    av.insert((r, o, ty), val);
                }
            }
            _ if op.is_call()
                || op.can_store()
                || op.other_side_effects()
                || matches!(op, Opcode::AtomicLoad | Opcode::Fence) =>
            {
                av.clear();
            }
            _ => {}
        }
    }
}

fn meet(acc: &mut Option<Avail>, x: &Avail) {
    match acc {
        None => *acc = Some(x.clone()),
        Some(a) => a.retain(|k, v| x.get(k) == Some(v)),
    }
}

/// Forward loads in `func`; returns the number removed.
pub fn run(func: &mut Function) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let cfg = ControlFlowGraph::with_function(func);
    // Reverse postorder.
    let mut post = Vec::new();
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut stack = vec![(entry, false)];
    while let Some((b, done)) = stack.pop() {
        if done {
            post.push(b);
            continue;
        }
        if !seen.insert(b) {
            continue;
        }
        stack.push((b, true));
        for s in cfg.succ_iter(b) {
            if !seen.contains(&s) {
                stack.push((s, false));
            }
        }
    }
    post.reverse();
    let rpo = post;
    let mut out: FxHashMap<Block, Avail> = FxHashMap::default();
    let input = |out: &FxHashMap<Block, Avail>, b: Block| -> Option<Avail> {
        if b == entry {
            return Some(Avail::default());
        }
        let mut acc = None;
        for p in cfg.pred_iter(b) {
            if let Some(o) = out.get(&p.block) {
                meet(&mut acc, o);
            }
        }
        acc
    };
    let mut stable = false;
    for _ in 0..MAX_ITERS {
        let mut changed = false;
        for &b in &rpo {
            let Some(mut av) = input(&out, b) else {
                continue;
            };
            transfer(func, b, &mut av, None);
            if out.get(&b) != Some(&av) {
                out.insert(b, av);
                changed = true;
            }
        }
        if !changed {
            stable = true;
            break;
        }
    }
    if !stable {
        return 0;
    }
    let mut fwd = Vec::new();
    for &b in &rpo {
        if let Some(mut av) = input(&out, b) {
            transfer(func, b, &mut av, Some(&mut fwd));
        }
    }
    for &(i, v) in &fwd {
        let r = func.dfg.first_result(i);
        func.layout.remove_inst(i);
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(r, v);
    }
    fwd.len()
}
