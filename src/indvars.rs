//! Induction-variable strength reduction on CLIF (LLVM indvars' narrow
//! slice): `base + iv*K` and bare `iv*K` inside a loop become new header
//! block params, stepped by `K*step` per back edge instead of multiplied.
//! Wrapping integer arithmetic makes the recurrence exact even on overflow,
//! so no legality check is needed beyond shape-matching the step.
//! `PLIRON_INDUCT=0` disables it; `PLIRON_INDUCT_DEBUG` logs rewrites.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value,
    ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

const MAX_LOOPS: usize = 32;
const MAX_PARAMS: usize = 6;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits()),
            _ => None,
        },
        _ => None,
    }
}

fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

/// The multiplicand of an `iv` term: a constant, or a loop-invariant value
/// (e.g. `iv * n` in `a[k*n+j]` — reduced to `np += n` per iteration).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Mul {
    K(i64),
    V(Value),
}

/// `v` as `iv*K`: `imul(iv, K)`, `ishl(iv, log2K)`, or bare `iv` (K = 1).
/// `K` may be a constant or a value defined outside `body`.
fn scaled(
    func: &Function,
    iv: Value,
    v: Value,
    body: &FxHashSet<Block>,
) -> Option<Mul> {
    let v = func.dfg.resolve_aliases(v);
    if v == iv {
        return Some(Mul::K(1));
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    let mul_of = |x: Value| {
        if let Some(k) = iconst(func, x) {
            return Some(Mul::K(k));
        }
        let x = func.dfg.resolve_aliases(x);
        if func.dfg.value_type(x) == func.dfg.value_type(iv)
            && def_block(func, x).is_some_and(|d| !body.contains(&d))
        {
            Some(Mul::V(x))
        } else {
            None
        }
    };
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args: [a, b],
        } => {
            if func.dfg.resolve_aliases(a) == iv {
                mul_of(b)
            } else if func.dfg.resolve_aliases(b) == iv {
                mul_of(a)
            } else {
                None
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [a, b],
        } if func.dfg.resolve_aliases(a) == iv => {
            iconst(func, b).and_then(|s| (0..64).contains(&s).then(|| Mul::K(1i64 << s)))
        }
        _ => None,
    }
}

/// One predecessor edge into `h`: the branch instruction and its `slot`th
/// destination, which must be `h`. The argument for param `idx` is the value
/// the edge passes for it.
struct Edge {
    inst: Inst,
    slot: usize,
}

fn edges_to(func: &Function, cfg: &ControlFlowGraph, h: Block) -> Vec<Edge> {
    let mut out = Vec::new();
    for p in cfg.pred_iter(h) {
        for (slot, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) == h {
                out.push(Edge {
                    inst: p.inst,
                    slot,
                });
            }
        }
    }
    out
}

fn edge_arg(func: &Function, e: &Edge, idx: usize) -> Option<Value> {
    let bc = func.dfg.insts[e.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[e.slot];
    match bc.args(&func.dfg.value_lists).nth(idx)? {
        BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
        _ => None,
    }
}

/// `iconst`'s immediate must fit the type's unsigned range; masking is free
/// since the consumers wrap anyway.
fn iconst_masked(pos: &mut FuncCursor, t: Type, k: i64) -> Value {
    if t == types::I128 {
        // `iconst` supports up to i64 only; iconcat the two halves (the hi
        // half sign-extends `k`, matching i64 -> i128 sign extension).
        let lo = pos.ins().iconst(types::I64, k);
        let hi = pos.ins().iconst(types::I64, k >> 63);
        return pos.ins().iconcat(lo, hi);
    }
    let mask = if t.bits() < 64 { (1i64 << t.bits()) - 1 } else { -1 };
    pos.ins().iconst(t, k & mask)
}

/// `p + K*step` materialized at the end of `inst`'s block.
fn emit_add(func: &mut Function, inst: Inst, p: Value, mul: Mul, step: i64) -> Value {
    let t = func.dfg.value_type(p);
    let mut pos = FuncCursor::new(func).at_inst(inst);
    let d = match mul {
        Mul::K(k) => iconst_masked(&mut pos, t, k.wrapping_mul(step)),
        Mul::V(x) if step == 1 => x,
        Mul::V(x) => {
            let c = iconst_masked(&mut pos, t, step);
            pos.ins().imul(x, c)
        }
    };
    pos.ins().iadd(p, d)
}

/// `base + v*K` materialized at the end of `inst`'s block.
fn emit_lin(func: &mut Function, inst: Inst, base: Option<Value>, v: Value, mul: Mul) -> Value {
    let t = func.dfg.value_type(v);
    let mut pos = FuncCursor::new(func).at_inst(inst);
    let s = match mul {
        Mul::K(k) => {
            let c = iconst_masked(&mut pos, t, k);
            pos.ins().imul(v, c)
        }
        Mul::V(x) => pos.ins().imul(v, x),
    };
    match base {
        Some(b) => pos.ins().iadd(b, s),
        None => s,
    }
}

/// If `inst`'s terminator argument for iv-param `idx` is `iv + step`, the new
/// param's argument is `np + K*step`; otherwise recompute `base + arg*K`.
fn edge_value(
    func: &mut Function,
    iv: Value,
    np: Value,
    base: Option<Value>,
    mul: Mul,
    e: &Edge,
    arg: Value,
) -> Value {
    // Step form: `iadd(iv, C)`/`iadd_imm`/`isub(iv, C)` feeding this edge.
    let step = match func.dfg.value_def(arg) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::Binary {
                opcode: Opcode::Iadd,
                args: [a, b],
            } if func.dfg.resolve_aliases(a) == iv => iconst(func, b),
            InstructionData::Binary {
                opcode: Opcode::Iadd,
                args: [a, b],
            } if func.dfg.resolve_aliases(b) == iv => iconst(func, a),
            InstructionData::Binary {
                opcode: Opcode::Isub,
                args: [a, b],
            } if func.dfg.resolve_aliases(a) == iv => {
                iconst(func, b).map(|x| x.wrapping_neg())
            }
            _ => None,
        },
        _ => None,
    };
    // Wrapping arithmetic keeps the recurrence exact even when `K*step`
    // overflows `i64`.
    match step {
        Some(s) => emit_add(func, e.inst, np, mul, s),
        None => emit_lin(func, e.inst, base, arg, mul),
    }
}

fn run_loop(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    debug: bool,
) -> usize {
    let h = la.loop_header(lp);
    if !dt.is_reachable(h) {
        return 0;
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    let edges = edges_to(func, cfg, h);
    let params = func.dfg.block_params(h).to_vec();
    let mut n = 0;
    // Every `iv*K` in the loop, and every `invariant + iv*K`, is a candidate.
    for (idx, &iv) in params.iter().enumerate() {
        if n >= MAX_PARAMS {
            break;
        }
        if !func.dfg.value_type(iv).is_int() {
            continue;
        }
        // Collect (iv*K insts) and (base + iv*K insts) inside the loop.
        let mut targets: FxHashMap<(Option<Value>, Mul), Vec<Inst>> = FxHashMap::default();
        for &b in &body {
            for i in func.layout.block_insts(b) {
                match func.dfg.insts[i] {
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args: [a, b2],
                    } => {
                        for (x, y) in [(a, b2), (b2, a)] {
                            if let Some(mul) = scaled(func, iv, y, &body)
                                && def_block(func, x).is_some_and(|d| !body.contains(&d))
                            {
                                targets.entry((Some(x), mul)).or_default().push(i);
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(&r) = func.dfg.inst_results(i).first()
                    && let Some(mul) = scaled(func, iv, r, &body)
                    && mul != Mul::K(1)
                {
                    targets.entry((None, mul)).or_default().push(i);
                }
            }
        }
        if targets.is_empty() {
            continue;
        }
        for (&(base, mul), insts) in &targets {
            if n >= MAX_PARAMS {
                break;
            }
            // `base + iv` and `iv*1` are already a single cheap add; turning
            // them into a stepped param only buys regalloc parallel-copy movs.
            if mul == Mul::K(0) || mul == Mul::K(1) {
                continue;
            }
            let ty: Type = base
                .map(|b| func.dfg.value_type(b))
                .unwrap_or_else(|| func.dfg.value_type(iv));
            if !ty.is_int() {
                continue;
            }
            // Every edge must pass a plain value for the iv param, else the
            // new param can't be filled in.
            let args: Option<Vec<Value>> = edges
                .iter()
                .map(|e| edge_arg(func, e, idx))
                .collect();
            let Some(args) = args else {
                continue;
            };
            // Compute the new param's argument on every edge into `h`.
            let np = func.dfg.append_block_param(h, ty);
            for (e, arg) in edges.iter().zip(args) {
                let v = edge_value(func, iv, np, base, mul, e, arg);
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[e.inst].branch_destination_mut(
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                )[e.slot];
                bc.append_argument(v, &mut dfg.value_lists);
            }
            for &i in insts {
                let r = func.dfg.first_result(i);
                func.layout.remove_inst(i);
                func.dfg.clear_results(i);
                func.dfg.change_to_alias(r, np);
            }
            if debug {
                eprintln!("indvars {h}: base {base:?} mul {mul:?} -> {np} ({} insts)", insts.len());
            }
            n += 1;
        }
    }
    n
}

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let debug = std::env::var_os("PLIRON_INDUCT_DEBUG").is_some();
    let loops: Vec<Loop> = la.loops().collect();
    let mut n = 0;
    for lp in loops.into_iter().take(MAX_LOOPS) {
        n += run_loop(func, &cfg, &dt, &la, lp, debug);
    }
    n
}
