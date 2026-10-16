//! Fold `icmp CC (select c, K, x), K` through the select.
//!
//! rustc's `Option::is_some` niche test lowers to
//! `select c, 0, ptr; icmp eq _, 0` — the merge block then pays
//! `mov 0; cmp; csel; cmp` (plus a second `csel` for the payload
//! advance) where LLVM keeps one `cmp; b.eq`. When the live arm's
//! compare `x CC K` is provably constant — `x` non-null vs `K=0`, or
//! `x <u 0` / `x >=u 0` which need no proof — the whole compare
//! collapses to `c`, `!c`, or a constant and the select dies.
//!
//! `PLIRON_NFOLD=0` disables it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};

use crate::jumpthread::NonNull;

fn const64(func: &Function, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits() as u64),
        _ => None,
    }
}

fn sext(x: u64, bits: u32) -> i64 {
    let sh = 64 - bits.min(64);
    ((x << sh) as i64) >> sh
}

fn icmp_cc(cc: IntCC, a: u64, b: u64, bits: u32) -> bool {
    let m = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let (a, b) = (a & m, b & m);
    let (sa, sb) = (sext(a, bits), sext(b, bits));
    match cc {
        IntCC::Equal => a == b,
        IntCC::NotEqual => a != b,
        IntCC::SignedLessThan => sa < sb,
        IntCC::SignedGreaterThanOrEqual => sa >= sb,
        IntCC::SignedGreaterThan => sa > sb,
        IntCC::SignedLessThanOrEqual => sa <= sb,
        IntCC::UnsignedLessThan => a < b,
        IntCC::UnsignedGreaterThanOrEqual => a >= b,
        IntCC::UnsignedGreaterThan => a > b,
        IntCC::UnsignedLessThanOrEqual => a <= b,
    }
}

/// `x CC k` provably constant: unsigned bounds vs 0 need no proof;
/// `x != 0` for a non-null `x`.
fn provable_cmp(func: &Function, nn: &NonNull, cc: IntCC, x: Value, k: u64) -> Option<bool> {
    if k != 0 {
        return None;
    }
    match cc {
        IntCC::UnsignedLessThan => Some(false),
        IntCC::UnsignedGreaterThanOrEqual => Some(true),
        IntCC::Equal | IntCC::UnsignedLessThanOrEqual if nn.is(func, x) => Some(false),
        IntCC::NotEqual | IntCC::UnsignedGreaterThan if nn.is(func, x) => Some(true),
        _ => None,
    }
}

/// The folded result: replace with `c`, `!c`, or a constant.
enum Fold {
    C(Value),
    NotC(Value),
    K(u64),
}

/// `v` already carries the icmp 0/1 convention, so it can stand in for a
/// compare result: icmp/fcmp results, `x & 1`, and select/const bools.
fn canonical_bool(func: &Function, v: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                ..
            }
            | InstructionData::FloatCompare { .. } => true,
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => const64(func, args[0]) == Some(1) || const64(func, args[1]) == Some(1),
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => matches!(imm.bits() as u64, 0 | 1),
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } => canonical_bool(func, args[1]) && canonical_bool(func, args[2]),
            _ => false,
        },
        _ => false,
    }
}

fn fold_of(func: &Function, nn: &NonNull, i: Inst) -> Option<Fold> {
    let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] else {
        return None;
    };
    let (sv, kv) = (args[0], args[1]);
    let k = const64(func, kv)?;
    let bits = func.dfg.value_type(sv).bits();
    let si = func.dfg.value_def(func.dfg.resolve_aliases(sv)).inst()?;
    let InstructionData::Ternary {
        opcode: Opcode::Select,
        args: sa,
    } = func.dfg.insts[si]
    else {
        return None;
    };
    let (c, t, f) = (
        func.dfg.resolve_aliases(sa[0]),
        func.dfg.resolve_aliases(sa[1]),
        func.dfg.resolve_aliases(sa[2]),
    );
    // Exactly one arm must be the compared constant; the live arm's
    // compare must also be provably constant so the whole select
    // collapses to `c ? A : B` on constants.
    let (x, const_res, const_is_t) = match (const64(func, t), const64(func, f)) {
        (Some(kt), None) => (f, icmp_cc(cond, kt, k, bits), true),
        (None, Some(kf)) => (t, icmp_cc(cond, kf, k, bits), false),
        _ => return None,
    };
    let xcmp = provable_cmp(func, nn, cond, x, k);
    if xcmp.is_none() && tracing::enabled!(target: "pliron::niche", tracing::Level::DEBUG) {
        tracing::debug!(target: "pliron::niche", "nfold reject i{i}: cond {cond:?} x {x} k {k}");
        if let ValueDef::Param(pb, _) = func.dfg.value_def(x) {
            tracing::debug!(target: "pliron::niche", "  param-in-set={}", nn.params_debug(func, x));
            for b in func.layout.blocks().collect::<Vec<_>>() {
                for i in func.layout.block_insts(b).collect::<Vec<_>>() {
                    for (ei, bc) in func.dfg.insts[i]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .into_iter()
                        .enumerate()
                    {
                        if bc.block(&func.dfg.value_lists) == pb {
                            for a in bc.args(&func.dfg.value_lists) {
                                tracing::debug!(target: "pliron::niche", "  edge i{i} ei{ei} arg {a:?} nn={} en={}",
                                    a.as_value().is_some_and(|v| nn.is(func, v)),
                                    a.as_value().is_some_and(|v| nn.is_edge(func, i, ei, v)),
                                );
                                if let Some(v) = a.as_value() {
                                    let mut w = func.dfg.resolve_aliases(v);
                                    for _ in 0..6 {
                                        match nn.derived_debug(func, w) {
                                            Some(bb) => {
                                                tracing::debug!(target: "pliron::niche", "    derived[{w}] = {bb} real={}",
                                                    func.dfg.value_is_real(bb)
                                                );
                                                w = func.dfg.resolve_aliases(bb);
                                            }
                                            None => break,
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let xcmp = xcmp?;
    let (a, b2) = if const_is_t {
        (const_res, xcmp)
    } else {
        (xcmp, const_res)
    };
    match (a, b2) {
        (true, false) => Some(Fold::C(c)),
        (false, true) => Some(Fold::NotC(c)),
        (true, true) => Some(Fold::K(1)),
        (false, false) => Some(Fold::K(0)),
    }
}

pub fn run(func: &mut Function, nn: &NonNull) -> usize {
    let mut n = 0;
    let mut rewrites: Vec<(Inst, Fold)> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let Some(f) = fold_of(func, nn, i) {
                rewrites.push((i, f));
            }
        }
    }
    for (i, f) in rewrites {
        let res = func.dfg.first_result(i);
        let t = match f {
            Fold::C(c) => format!("c {c}"),
            Fold::NotC(c) => format!("!c {c}"),
            Fold::K(k) => format!("k {k}"),
        };
        if tracing::enabled!(target: "pliron::niche", tracing::Level::DEBUG) {
            tracing::debug!(target: "pliron::niche", "nfold fold i{i} -> {t}");
        }
        // `c` is a select condition — any nonzero truthy value, not the
        // 0/1 an icmp result must be — so canonicalize with a compare
        // rather than aliasing or `bnot` (which keeps `!1 = 0xFE` truthy
        // for a bit-flag cond). A canonical bool can be aliased directly,
        // which also keeps edge-cond folds (`selargs`) matching.
        match f {
            Fold::C(c)
                if canonical_bool(func, c)
                    && func.dfg.value_type(c) == func.dfg.value_type(res) =>
            {
                func.dfg.clear_results(i);
                func.layout.remove_inst(i);
                func.dfg.change_to_alias(res, c);
            }
            Fold::C(c) | Fold::NotC(c) => {
                let ty = func.dfg.value_type(c);
                let z = FuncCursor::new(&mut *func).at_inst(i).ins().iconst(ty, 0);
                func.dfg.insts[i] = InstructionData::IntCompare {
                    opcode: Opcode::Icmp,
                    cond: if matches!(f, Fold::C(_)) {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    },
                    args: [c, z],
                };
            }
            Fold::K(k) => {
                func.dfg.insts[i] = InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm: (k as i64).into(),
                };
            }
        }
        n += 1;
    }
    n
}
