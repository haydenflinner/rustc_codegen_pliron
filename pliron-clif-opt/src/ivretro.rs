//! Retrofit affine edge args to stepped-param form (`PLIRON_IVRETRO`, opt-in).
//!
//! indvars grows a stepped param `p = B + iv*K` while the loop is still
//! multi-block; the iv's own edge arg is not yet `iv + s` in shape, so
//! `edge_value` falls back to the literal `B + arg*K` — an `imul` plus
//! `iadd` materialized on every backedge. Later cfg cleanup collapses
//! the latch until `arg_iv = iv + s`, at which point the arg could have
//! been `p + s*K` all along: one add, and no multiply sitting on the
//! carried dependency chain. On targets without scaled addressing
//! (wasm, riscv) the literal multiply is real code, not an amode fold.
//!
//! For each header param pair `(iv, p)`, every edge into `h` must carry
//! `arg_p = B + arg_iv*K` for the same resolved `B`/`K` — the invariant
//! `p = B + iv*K` is then inductive (entry sets it, each backedge
//! preserves it) and an edge whose iv arg is `iadd iv, s` may forward
//! `arg_p := p + s*K`; `arg_iv = iv` forwards `p` itself. Ring
//! arithmetic makes `B + iv*K + s*K = B + (iv+s)*K` exact without
//! nowrap. Edges supplying a constant-folded affine form (bare `B`
//! beside an `iv = 0` arg, `B + c*K` beside a constant iv) keep their
//! literal args: they still satisfy the invariant, they just have no
//! `iv + s` to key the rewrite on.
//!
//! `PLIRON_IVRETRO=0` disables it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value, ValueDef,
};
use rustc_hash::FxHashMap;

use crate::pipeline::Analysis;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
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

/// `iconst`'s immediate must fit the type's unsigned range; masking is
/// free since the consumers wrap anyway.
fn iconst_masked(pos: &mut FuncCursor, t: Type, k: i64) -> Value {
    let mask = if t.bits() < 64 {
        (1i64 << t.bits()) - 1
    } else {
        -1
    };
    pos.ins().iconst(t, k & mask)
}

/// Type-width mask for comparing folded constants (`i32` iconst
/// immediates are stored sign-extended; products computed in `i64`
/// must be compared in the value's own ring).
fn ty_mask(func: &Function, v: Value) -> i64 {
    let b = func.dfg.value_type(v).bits();
    if b < 64 { (1i64 << b) - 1 } else { -1 }
}

/// `v` as `a*k` for constant `k` (`imul a, c` / `ishl a, c`), or `None`.
fn scaled(func: &Function, v: Value) -> Option<(Value, i64)> {
    let ValueDef::Result(i, _) = func.dfg.value_def(func.dfg.resolve_aliases(v)) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args,
        } => iconst(func, args[1])
            .map(|c| (func.dfg.resolve_aliases(args[0]), c))
            .or_else(|| iconst(func, args[0]).map(|c| (func.dfg.resolve_aliases(args[1]), c))),
        InstructionData::Binary {
            opcode: Opcode::Ishl,
            args,
        } => iconst(func, args[1]).and_then(|c| {
            (0..63)
                .contains(&c)
                .then_some((func.dfg.resolve_aliases(args[0]), 1i64 << c))
        }),
        _ => None,
    }
}

/// `arg` split as `base + a*k + c0`: `base` a non-const resolved value
/// (or `None`), `a` the scaled operand (or `None`), `c0` folded
/// constants masked to `arg`'s type width. Ambiguous `x + y` sums
/// return `None` — the rewrite needs to know which operand is the
/// iv-affine term.
fn affine(func: &Function, arg: Value) -> Option<(Option<Value>, Option<(Value, i64)>, i64)> {
    let v = func.dfg.resolve_aliases(arg);
    let mask = ty_mask(func, v);
    if let Some(c) = iconst(func, v) {
        return Some((None, None, c & mask));
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return Some((Some(v), None, 0));
    };
    let (opcode, args) = match func.dfg.insts[i] {
        InstructionData::Binary { opcode, args } => (opcode, args),
        _ => return Some((Some(v), None, 0)),
    };
    if opcode == Opcode::Imul || opcode == Opcode::Ishl {
        let (a, k) = scaled(func, v)?;
        return Some((None, Some((a, k)), 0));
    }
    let neg = opcode == Opcode::Isub;
    if !neg && opcode != Opcode::Iadd {
        return Some((Some(v), None, 0));
    }
    // Two-operand form: classify each side as scaled / const / atom.
    let mut base = None;
    let mut sc = None;
    let mut c0 = 0i64;
    for (n, &o) in args.iter().enumerate() {
        let o = func.dfg.resolve_aliases(o);
        let sign = if neg && n == 1 { -1i64 } else { 1 };
        if let Some(c) = iconst(func, o) {
            c0 = (c0.wrapping_add(sign.wrapping_mul(c))) & mask;
        } else if let Some((a, k)) = scaled(func, o) {
            if sc.is_some() {
                return None;
            }
            sc = Some((a, sign.wrapping_mul(k)));
        } else {
            if base.is_some() || sign < 0 {
                return None;
            }
            base = Some(o);
        }
    }
    Some((base, sc, c0))
}

/// Does `arg` evaluate to `base + bc0 + ck`? The folded shapes a
/// literal edge arg takes are bare `base` (`ck == 0`), `base + m`
/// (`m == ck`), or an iconst total when the base is absent.
fn folded_eq(func: &Function, arg: Value, base: Option<Value>, bc0: i64, ck: i64) -> bool {
    let Some((b, a, c0)) = affine(func, arg) else {
        return false;
    };
    if a.is_some() {
        return false;
    }
    let mask = ty_mask(func, arg);
    let want = bc0.wrapping_add(ck) & mask;
    match (base, b) {
        (Some(x), Some(y)) => x == y && c0 == want,
        (None, None) => c0 == want,
        _ => false,
    }
}

/// `arg` as `iv + s` for constant `s` (`isub iv, c` is `s = -c`), or
/// `s = 0` when the arg is `iv` itself.
fn step_of(func: &Function, arg: Value, iv: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(arg);
    if v == iv {
        return Some(0);
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return None;
    };
    let (a, b) = (
        func.dfg.resolve_aliases(args[0]),
        func.dfg.resolve_aliases(args[1]),
    );
    match opcode {
        Opcode::Iadd if a == iv => iconst(func, b),
        Opcode::Iadd if b == iv => iconst(func, a),
        Opcode::Isub if a == iv => iconst(func, b).map(|c| c.wrapping_neg()),
        _ => None,
    }
}

/// The `idx`-th `Value` arg of edge `(inst, di)`, alias-resolved.
fn edge_arg(func: &Function, inst: Inst, di: usize, idx: usize) -> Option<Value> {
    let bc = func.dfg.insts[inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[di];
    match bc.args(&func.dfg.value_lists).nth(idx)? {
        BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
        _ => None,
    }
}

/// Edges into each block: `(terminator, dest_index)` per destination.
fn edges_to(func: &Function) -> FxHashMap<Block, Vec<(Inst, usize)>> {
    let mut m: FxHashMap<Block, Vec<(Inst, usize)>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for (d, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                m.entry(bc.block(&func.dfg.value_lists))
                    .or_default()
                    .push((i, d));
            }
        }
    }
    m
}

/// One retrofit: `p`'s arg on edge `(inst, di)` becomes `p + s*K`
/// (`p` itself when `s*K == 0`).
struct Hit {
    inst: Inst,
    di: usize,
    pi: usize,
    p: Value,
    sk: i64,
}

pub fn run(func: &mut Function, _an: &mut Analysis) -> usize {
    let edges = edges_to(func);
    let mut hits = Vec::new();
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let params: Vec<Value> = func.dfg.block_params(b).to_vec();
        if params.len() < 2 {
            continue;
        }
        let Some(es) = edges.get(&b) else {
            continue;
        };
        if es.len() < 2 {
            continue;
        }
        // Per-edge `(arg_pi decomp, arg_ii)` per (pi, ii) pair would
        // recompute the same decompositions; params are few enough that
        // the straightforward double loop stays linear.
        for (pi, &p) in params.iter().enumerate() {
            if !func.dfg.value_type(p).is_int() {
                continue;
            }
            'anchor: for ii in 0..params.len() {
                if ii == pi {
                    continue;
                }
                // Pass 1: derive `(B, c0, K)` from canonical
                // `B + arg_ii*K` edges. Decomposition failures and
                // scaled-term mismatches are deferred to pass 2 — a
                // folded edge is only provable once `(B, K)` is known.
                let mut bk: Option<(Option<Value>, i64, i64)> = None;
                let mut saw_affine = false;
                let mut decomps = Vec::with_capacity(es.len());
                for &(e_inst, e_di) in es {
                    let (Some(a), Some(iv_a)) = (
                        edge_arg(func, e_inst, e_di, pi),
                        edge_arg(func, e_inst, e_di, ii),
                    ) else {
                        continue 'anchor;
                    };
                    let Some(d) = affine(func, a) else {
                        continue 'anchor;
                    };
                    if let Some((av, k)) = d.1 {
                        if av != iv_a {
                            continue 'anchor;
                        }
                        match bk {
                            None => bk = Some((d.0, d.2, k)),
                            Some((b0, c00, k0)) => {
                                if b0 != d.0 || c00 != d.2 || k0 != k {
                                    continue 'anchor;
                                }
                            }
                        }
                        saw_affine = true;
                    }
                    decomps.push((e_inst, e_di, a, iv_a, d));
                }
                let (Some((b0, c00, k0)), true) = (bk, saw_affine) else {
                    continue;
                };
                // Pass 2: every edge must satisfy the invariant — the
                // folded ones against `B + c*K` — then stepped edges
                // take their rewrite.
                let mut pending = Vec::new();
                for &(e_inst, e_di, a, iv_a, ref d) in &decomps {
                    if d.1.is_none() {
                        let Some(c) = iconst(func, iv_a) else {
                            continue 'anchor;
                        };
                        if !folded_eq(func, a, b0, c00, c.wrapping_mul(k0)) {
                            continue 'anchor;
                        }
                    }
                    if let Some(s) = step_of(func, iv_a, params[ii]) {
                        pending.push(Hit {
                            inst: e_inst,
                            di: e_di,
                            pi,
                            p,
                            sk: s.wrapping_mul(k0),
                        });
                    }
                }
                hits.extend(pending);
                break;
            }
        }
    }
    let n = hits.len();
    for h in hits {
        let arg = if h.sk == 0 {
            h.p
        } else {
            let t = func.dfg.value_type(h.p);
            let mut pos = FuncCursor::new(func).at_inst(h.inst);
            let c = iconst_masked(&mut pos, t, h.sk);
            pos.ins().iadd(h.p, c)
        };
        let dfg = &mut func.dfg;
        let dests =
            dfg.insts[h.inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
        let mut idx = 0;
        dests[h.di].update_args(&mut dfg.value_lists, |a| {
            let r = if idx == h.pi { BlockArg::Value(arg) } else { a };
            idx += 1;
            r
        });
    }
    n
}
