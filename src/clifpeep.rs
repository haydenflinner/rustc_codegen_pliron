//! Cranelift-IR peepholes its egraph lacks.
//!
//! `(K & (1 << s)) != 0` on i128 constants (rustc's `u128` bitset idiom, e.g.
//! `is_id_continue`'s ASCII mask) becomes an i64 select + shift, like LLVM's
//! lowering, instead of a full i128 shift and mask.
//!
//! `uextend (band|bor|bxor a, b)` on narrow ints becomes the op on extended
//! operands, so `uextend (load.i8)` folds into one `movzx` instead of a byte
//! op followed by another `movzx` (UTF-8 decoding, `u8` flag tests).
//!
//! A `load.i8`/`load.i16` that is also `uextend`ed becomes one zero-extending
//! `uload` to i64; the narrow value and the extensions become `ireduce`s,
//! which are free. x64 only folds `uextend (load)` when the load has a single
//! use, so otherwise it re-extends the already-`movzx`ed register.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
    types,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

/// Tight unsigned ceiling on `v` — its proven maximum value. Defaults to the
/// type's max; narrower through `uextend` (a `u8` index extended to i64 is at
/// most 255), `band` masks, `ushr`, and `urem`. Used to fold `icmp`s whose
/// constant bound lies outside the operand's range (`x8 < 0x100000`).
fn vmax(func: &Function, v: Value) -> u64 {
    let v = func.dfg.resolve_aliases(v);
    let ty = func.dfg.value_type(v);
    let ty_umax = if ty.bits() >= 64 {
        u64::MAX
    } else {
        (1u64 << ty.bits()) - 1
    };
    if !ty.is_int() || ty.is_vector() {
        return ty_umax;
    }
    let mut hi = ty_umax;
    let Some(d) = func.dfg.value_def(v).inst() else {
        return hi;
    };
    match func.dfg.insts[d] {
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => hi = hi.min(vmax(func, arg)),
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => hi = hi.min(imm.bits() as u64 & ty_umax),
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => {
            for a in args {
                if let Some(m) = iconst(func, a) {
                    hi = hi.min(m as u64 & ty_umax);
                }
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Ushr,
            args,
        } => {
            if let Some(s) = iconst(func, args[1]) {
                // CLIF masks the shift amount to the operand width.
                let s = (s as u64) % (ty.bits() as u64);
                hi = hi.min(ty_umax >> s);
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Urem,
            args,
        } => {
            if let Some(d) = iconst(func, args[1])
                && d > 0
            {
                hi = hi.min(d as u64 - 1);
            }
        }
        _ => {}
    }
    hi
}

fn const128(func: &Function, v: Value) -> Option<(i64, i64)> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::Binary {
            opcode: Opcode::Iconcat,
            args,
        } => Some((iconst(func, args[0])?, iconst(func, args[1])?)),
        _ => None,
    }
}

/// `(K, s)` if `v` is `band K, (ishl 1, s)` with a constant i128 `K`.
fn bit_test(func: &Function, v: Value) -> Option<((i64, i64), Value)> {
    let v = func.dfg.resolve_aliases(v);
    let InstructionData::Binary {
        opcode: Opcode::Band,
        args,
    } = func.dfg.insts[func.dfg.value_def(v).inst()?]
    else {
        return None;
    };
    for (k, sh) in [(args[0], args[1]), (args[1], args[0])] {
        let Some(k) = const128(func, k) else { continue };
        let sh = func.dfg.resolve_aliases(sh);
        let Some(i) = func.dfg.value_def(sh).inst() else {
            continue;
        };
        if let InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [one, amt],
        } = func.dfg.insts[i]
            && const128(func, one) == Some((1, 0))
        {
            return Some((k, func.dfg.resolve_aliases(amt)));
        }
    }
    None
}

/// `amt` as i64 when it is `uextend x` or `uextend x & m` (`m` < 2^63).
fn narrow(pos: &mut FuncCursor, amt: Value) -> Option<Value> {
    let f = &*pos.func;
    let (a, m) = match f.dfg.insts[f.dfg.value_def(amt).inst()?] {
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => {
            let (a, b) = (
                f.dfg.resolve_aliases(args[0]),
                f.dfg.resolve_aliases(args[1]),
            );
            match (const128(f, a), const128(f, b)) {
                (_, Some((m, 0))) => (a, Some(m)),
                (Some((m, 0)), _) => (b, Some(m)),
                _ => return None,
            }
        }
        _ => (amt, None),
    };
    let InstructionData::Unary {
        opcode: Opcode::Uextend,
        arg,
    } = f.dfg.insts[f.dfg.value_def(a).inst()?]
    else {
        return None;
    };
    let x = f.dfg.resolve_aliases(arg);
    let xt = f.dfg.value_type(x);
    if xt.is_vector() || !xt.is_int() || xt.bits() > 64 {
        return None;
    }
    let x64 = if xt == types::I64 {
        x
    } else {
        pos.ins().uextend(types::I64, x)
    };
    Some(match m {
        Some(m) => {
            let c = pos.ins().iconst(types::I64, m);
            pos.ins().band(x64, c)
        }
        None => x64,
    })
}

fn widen(pos: &mut FuncCursor, inst: Inst, arg: Value) -> bool {
    let f = &*pos.func;
    let to = f.dfg.value_type(f.dfg.first_result(inst));
    if to.is_vector() || !to.is_int() || to.bits() > 64 {
        return false;
    }
    let arg = f.dfg.resolve_aliases(arg);
    let Some(d) = f.dfg.value_def(arg).inst() else {
        return false;
    };
    let InstructionData::Binary { opcode, args } = f.dfg.insts[d] else {
        return false;
    };
    if !matches!(opcode, Opcode::Band | Opcode::Bor | Opcode::Bxor) {
        return false;
    }
    let a = pos.ins().uextend(to, args[0]);
    let b = pos.ins().uextend(to, args[1]);
    let r = pos.func.replace(inst);
    match opcode {
        Opcode::Band => r.band(a, b),
        Opcode::Bor => r.bor(a, b),
        _ => r.bxor(a, b),
    };
    true
}

fn widen_on() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| crate::pass_enabled("PLIRON_WIDEN"));
    *ON
}

fn uloads(func: &mut Function) -> usize {
    let mut users: rustc_data_structures::fx::FxHashMap<Value, Vec<Inst>> = Default::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } = func.dfg.insts[i]
            {
                let a = func.dfg.resolve_aliases(arg);
                if let Some(d) = func.dfg.value_def(a).inst()
                    && let InstructionData::Load {
                        opcode: Opcode::Load,
                        ..
                    } = func.dfg.insts[d]
                    && matches!(func.dfg.value_type(a), types::I8 | types::I16)
                {
                    users.entry(a).or_default().push(i);
                }
            }
        }
    }
    let n = users.len();
    let mut pos = FuncCursor::new(func);
    for (r, exts) in users {
        let l = pos.func.dfg.value_def(r).inst().unwrap();
        let InstructionData::Load {
            flags, arg, offset, ..
        } = pos.func.dfg.insts[l]
        else {
            unreachable!()
        };
        let nt = pos.func.dfg.value_type(r);
        let flags = pos.func.dfg.mem_flags[flags].clone();
        pos.goto_inst(l);
        let wide = if nt == types::I8 {
            pos.ins().uload8(types::I64, flags, arg, offset)
        } else {
            pos.ins().uload16(types::I64, flags, arg, offset)
        };
        pos.func.replace(l).ireduce(nt, wide);
        for u in exts {
            let res = pos.func.dfg.first_result(u);
            let to = pos.func.dfg.value_type(res);
            if to == types::I64 {
                pos.func.dfg.clear_results(u);
                pos.func.layout.remove_inst(u);
                pos.func.dfg.change_to_alias(res, wide);
            } else if to.bits() > 64 {
                pos.func.replace(u).uextend(to, wide);
            } else {
                pos.func.replace(u).ireduce(to, wide);
            }
        }
    }
    n
}

/// The `(op, x, k)` of `v = op x, k` for constant `k` (either operand).
fn bin_k(func: &Function, v: Value, op: Opcode) -> Option<(Value, i64)> {
    let i = func.dfg.value_def(v).inst()?;
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return None;
    };
    if opcode != op {
        return None;
    }
    let a = args.map(|a| func.dfg.resolve_aliases(a));
    iconst(func, a[1])
        .map(|k| (a[0], k))
        .or_else(|| iconst(func, a[0]).map(|k| (a[1], k)))
}

/// Whether `v` is always 0 or 1.
fn is01(func: &Function, v: Value, depth: u32) -> bool {
    if let Some(i) = func.dfg.value_def(v).inst()
        && matches!(func.dfg.insts[i].opcode(), Opcode::Icmp | Opcode::Fcmp)
    {
        return true;
    }
    bin_k(func, v, Opcode::Band).is_some_and(|(_, k)| k == 1)
        || (depth > 0
            && bin_k(func, v, Opcode::Bxor)
                .is_some_and(|(x, k)| k == 1 && is01(func, x, depth - 1)))
}

/// `v` as a constant lane: `iconst` or `splat (iconst)`, sign-extended.
fn lane_const(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        InstructionData::Unary {
            opcode: Opcode::Splat,
            arg,
        } => iconst(func, arg),
        _ => None,
    }
}

/// Same value, or same constant bit-pattern in `w` bits.
fn same_lane(func: &Function, a: Value, b: Value, w: u32) -> bool {
    let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
    if a == b {
        return true;
    }
    let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    match (lane_const(func, a), lane_const(func, b)) {
        (Some(x), Some(y)) => (x as u64 & mask) == (y as u64 & mask),
        _ => false,
    }
}

/// `v` as `min_or_max(x, k)` → `(op, x, k)` with the constant second.
fn minmax_k(func: &Function, v: Value) -> Option<(Opcode, Value, i64)> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return None;
    };
    if !matches!(
        opcode,
        Opcode::Umin | Opcode::Umax | Opcode::Smin | Opcode::Smax
    ) {
        return None;
    }
    let (a, b) = (args[0], args[1]);
    if let Some(k) = lane_const(func, b) {
        Some((opcode, a, k))
    } else {
        lane_const(func, a).map(|k| (opcode, b, k))
    }
}

/// `select|bitselect (icmp cc x, y) t, f` folds to integer min/max — the
/// vendored egraph knows the one-level forms but not the nested clamp idiom
/// `x < lo ? lo : min(x, hi)` (needs a `lo <= hi` proof), and folding here
/// lets loopvec emit `umin`/`umax` directly instead of `icmp`+`bitselect`.
fn minmax(pos: &mut FuncCursor, inst: Inst) -> bool {
    let f = &*pos.func;
    let InstructionData::Ternary { opcode, args } = f.dfg.insts[inst] else {
        return false;
    };
    if !matches!(opcode, Opcode::Select | Opcode::Bitselect) {
        return false;
    }
    let ty = f.dfg.value_type(f.dfg.first_result(inst));
    let w = if ty.is_vector() {
        ty.lane_bits()
    } else {
        ty.bits()
    };
    if !ty.lane_type().is_int() || w > 64 {
        return false;
    }
    let [c, t, fv] = args;
    let Some(ci) = f.dfg.value_def(f.dfg.resolve_aliases(c)).inst() else {
        return false;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: cc,
        args: [a, b],
    } = f.dfg.insts[ci]
    else {
        return false;
    };
    let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    let ku = |k: i64| k as u64 & mask;
    let ks = |k: i64| ((k as u64 & mask) << (64 - w) as i64) >> (64 - w);

    // select(icmp cc a b, t, f): try both arm orders — swapping arms and
    // complementing cc is the same function.
    for (tt, ff, cc) in [(t, fv, cc), (fv, t, cc.complement())] {
        // select(x < y ? x : y) = min etc.
        let direct = if same_lane(f, tt, a, w) && same_lane(f, ff, b, w) {
            Some(match cc {
                IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual => Opcode::Umin,
                IntCC::UnsignedGreaterThan | IntCC::UnsignedGreaterThanOrEqual => {
                    Opcode::Umax
                }
                IntCC::SignedLessThan | IntCC::SignedLessThanOrEqual => Opcode::Smin,
                IntCC::SignedGreaterThan | IntCC::SignedGreaterThanOrEqual => {
                    Opcode::Smax
                }
                _ => continue,
            })
        } else {
            None
        };
        if let Some(op) = direct {
            let (a, b) = (a, b);
            let r = pos.func.replace(inst);
            match op {
                Opcode::Umin => r.umin(a, b),
                Opcode::Umax => r.umax(a, b),
                Opcode::Smin => r.smin(a, b),
                _ => r.smax(a, b),
            };
            return true;
        }
    }

    // Nested clamp: select(icmp cc x, K1) K1, minmax(x, K2).
    // Normalize so the icmp's constant is the rhs.
    let (mut x, mut k1v, mut cc) = (a, b, cc);
    if lane_const(f, x).is_some() && lane_const(f, k1v).is_none() {
        (x, k1v) = (k1v, x);
        cc = cc.swap_args();
    }
    let Some(k1) = lane_const(f, k1v) else {
        return false;
    };
    for (tt, ff, cc) in [(t, fv, cc), (fv, t, cc.complement())] {
        if lane_const(f, tt).map_or(true, |k| ku(k) != ku(k1)) {
            continue;
        }
        let Some((mop, mx, k2)) = minmax_k(f, ff) else {
            continue;
        };
        if f.dfg.resolve_aliases(mx) != f.dfg.resolve_aliases(x) {
            continue;
        }
        // `x < K1 ? K1 : min(x, K2)` = max(min(x,K2), K1) iff K1 <= K2, etc.
        // The inner min/max must share the icmp's signedness — a mixed
        // signedness changes the result for negative or high-bit lanes.
        let signed = matches!(
            cc,
            IntCC::SignedLessThan
                | IntCC::SignedLessThanOrEqual
                | IntCC::SignedGreaterThan
                | IntCC::SignedGreaterThanOrEqual
        );
        if signed != matches!(mop, Opcode::Smin | Opcode::Smax) {
            continue;
        }
        let less = match cc {
            IntCC::UnsignedLessThan
            | IntCC::UnsignedLessThanOrEqual
            | IntCC::SignedLessThan
            | IntCC::SignedLessThanOrEqual => true,
            IntCC::UnsignedGreaterThan
            | IntCC::UnsignedGreaterThanOrEqual
            | IntCC::SignedGreaterThan
            | IntCC::SignedGreaterThanOrEqual => false,
            _ => continue,
        };
        let (le12, ge12) = if signed {
            (ks(k1) <= ks(k2), ks(k1) >= ks(k2))
        } else {
            (ku(k1) <= ku(k2), ku(k1) >= ku(k2))
        };
        let (min_op, max_op) = if signed {
            (Opcode::Smin, Opcode::Smax)
        } else {
            (Opcode::Umin, Opcode::Umax)
        };
        let want_min = mop == min_op;
        // x<K ? K : min(x,K2) -> max(inner,K)   [K<=K2]
        // x>K ? K : min(x,K2) -> min(x,K)      [K<=K2]
        // x<K ? K : max(x,K2) -> max(x,K)      [K>=K2]
        // x>K ? K : max(x,K2) -> min(inner,K)  [K>=K2]
        let new = if less && want_min && le12 {
            Some((max_op, ff, tt))
        } else if !less && want_min && le12 {
            Some((min_op, x, tt))
        } else if less && !want_min && ge12 {
            Some((max_op, x, tt))
        } else if !less && !want_min && ge12 {
            Some((min_op, ff, tt))
        } else {
            None
        };
        let Some((op, a1, a2)) = new else { continue };
        let r = pos.func.replace(inst);
        match op {
            Opcode::Umin => r.umin(a1, a2),
            Opcode::Umax => r.umax(a1, a2),
            Opcode::Smin => r.smin(a1, a2),
            _ => r.smax(a1, a2),
        };
        return true;
    }
    false
}

/// `brif (bxor (band c, 1), 1), a, b` (rustc's `!bool` after `i1`
/// normalization) becomes `brif c, b, a`.
fn brif_not(func: &mut Function, inst: Inst) -> bool {
    let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[inst] else {
        return false;
    };
    let (mut v, mut neg) = (func.dfg.resolve_aliases(arg), false);
    for _ in 0..8 {
        match (bin_k(func, v, Opcode::Band), bin_k(func, v, Opcode::Bxor)) {
            (Some((x, 1)), _) if is01(func, x, 4) => v = x,
            (_, Some((x, 1))) if is01(func, x, 4) => {
                v = x;
                neg = !neg;
            }
            _ => break,
        }
    }
    if v == func.dfg.resolve_aliases(arg) {
        return false;
    }
    let blocks = if neg { [blocks[1], blocks[0]] } else { blocks };
    if let InstructionData::Brif {
        arg: a, blocks: bs, ..
    } = &mut func.dfg.insts[inst]
    {
        *a = v;
        *bs = blocks;
    }
    true
}

/// rustc lowers `CheckedBinaryOp` (integer ops under `overflow-checks`) to
/// two-result `*overflow` CLIF ops; when the `Assert` on the flag is later
/// optimized away the flag result stays dead, but the checked op still
/// defeats op-level matching (loopvec reductions, idioms) and pays for the
/// flag test in scalar code. Rewrite dead-flag overflow ops to the plain
/// wrapping op — identical first-result semantics.
fn deflag(func: &mut Function) -> usize {
    // A value is used iff it appears in some inst's args or in a branch
    // destination's args (block-call args aren't part of `inst_args`).
    let mut used: FxHashSet<Value> = FxHashSet::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                used.insert(func.dfg.resolve_aliases(a));
            }
            for a in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .flat_map(|bc| bc.args(&func.dfg.value_lists))
            {
                if let BlockArg::Value(v) = a {
                    used.insert(func.dfg.resolve_aliases(v));
                }
            }
        }
    }
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let plain = match func.dfg.insts[i].opcode() {
                Opcode::SaddOverflow | Opcode::UaddOverflow => Opcode::Iadd,
                Opcode::SsubOverflow | Opcode::UsubOverflow => Opcode::Isub,
                Opcode::SmulOverflow | Opcode::UmulOverflow => Opcode::Imul,
                _ => continue,
            };
            let res = func.dfg.inst_results(i).to_vec();
            let InstructionData::Binary { args, .. } = func.dfg.insts[i] else {
                continue;
            };
            if res.len() != 2 || used.contains(&func.dfg.resolve_aliases(res[1])) {
                continue;
            }
            let fty = func.dfg.value_type(res[1]);
            let mut pos = FuncCursor::new(func).at_inst(i);
            let nv = match plain {
                Opcode::Iadd => pos.ins().iadd(args[0], args[1]),
                Opcode::Isub => pos.ins().isub(args[0], args[1]),
                _ => pos.ins().imul(args[0], args[1]),
            };
            // Detach the old results before aliasing (`change_to_alias`
            // requires unattached values); point the dead flag at a zero
            // const so it isn't left dangling on a removed inst.
            let z = pos.ins().iconst(fty, 0);
            pos.func.dfg.clear_results(i);
            pos.func.dfg.change_to_alias(res[0], nv);
            pos.func.dfg.change_to_alias(res[1], z);
            pos.func.layout.remove_inst(i);
            n += 1;
        }
    }
    n
}

/// A shared cold block with block params makes its incoming edge copies
/// unconditional: regalloc can't place the parallel copy inside the cold
/// successor (other preds feed different values), so every hot
/// predecessor's tail pays the moves — executed even when the cold path
/// is never taken (e.g. punroll's shared `block21(v46) cold` panic
/// blocks). Give each hot->cold param edge a cold adapter block
/// `a: jump C(args)`: the same parallel copy then materializes inside
/// cold code, while the arg vregs are merely live-in to the adapter and
/// the hot edge carries no args at all.
pub fn coldedges(func: &mut Function) -> usize {
    // Collect (pred terminator, edge index, cold target, edge args)
    // first: new blocks are appended while rewriting.
    let mut edges = vec![];
    for b in func.layout.blocks() {
        if func.layout.is_cold(b) {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        for (di, bc) in func
            .dfg
            .insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            let c = bc.block(&func.dfg.value_lists);
            if !func.layout.is_cold(c) || func.dfg.block_params(c).is_empty() {
                continue;
            }
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            if args.iter().all(|a| matches!(a, BlockArg::Value(_)))
                && args.len() == func.dfg.block_params(c).len()
            {
                edges.push((t, di, c, args));
            }
        }
    }
    let mut n = 0;
    for (t, di, c, args) in edges {
        let a = func.dfg.make_block();
        func.layout.set_cold(a);
        func.layout.insert_block(a, c);
        FuncCursor::new(func).at_bottom(a).ins().jump(c, &args);
        let bc = func.dfg.block_call(a, &[]);
        let dfg = &mut func.dfg;
        let dests = dfg
            .insts[t]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
        if let Some(d) = dests.get_mut(di) {
            *d = bc;
        }
        n += 1;
    }
    n
}

/// An integer argument of a call in a cold block that is defined in a hot
/// block pins its virtual register to a fixed ABI register across the whole
/// hot region: the register allocator honors the call's operand constraint
/// at the value's only fixed use, so loop-carried operands like a
/// bounds-check `idx`/`len` get permanently assigned to x0/x1 and every
/// other user — and every backedge — pays a move to shuffle them. Rebind
/// each such arg to a fresh identity def inside the cold block: the copy
/// is still emitted, but at the call site in cold code instead of at
/// every hot use (LLVM places the same `mov`s in the panic block).
///
/// The wrap can't be `iadd x, 0`: cranelift's egraph folds it back to
/// `x` (`iadd_x_plus_zero`), re-attaching the fixed use to the hot
/// range. `sadd_overflow x, 0` computes the same value, has no
/// `simplify`/`simplify_skeleton` rule, and its dead overflow-flag
/// result is removed by vcode DCE — leaving a single `adds` in the
/// cold block.
pub fn coldargs(func: &mut Function) -> usize {
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        if !func.layout.is_cold(b) {
            continue;
        }
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            if !func.dfg.insts[i].opcode().is_call() {
                continue;
            }
            let mut reb = vec![];
            for &a in func.dfg.inst_args(i) {
                let ty = func.dfg.value_type(a);
                if !ty.is_int() {
                    continue;
                }
                let hot = match func.dfg.value_def(a) {
                    ValueDef::Result(di, _) => func
                        .layout
                        .inst_block(di)
                        .is_some_and(|db| !func.layout.is_cold(db)),
                    // A block param's bundle merges with its incoming
                    // edge sources (typically hot values), so a
                    // fixed-reg call use on the param reaches back
                    // into the hot source bundle and fragments it.
                    ValueDef::Param(..) => true,
                    _ => false,
                };
                if hot {
                    reb.push((a, ty));
                }
            }
            if reb.is_empty() {
                continue;
            }
            let mut pairs = vec![];
            {
                let mut pos = FuncCursor::new(func).at_inst(i);
                for &(a, ty) in &reb {
                    let z = pos.ins().iconst(ty, 0);
                    // `sadd_overflow` is pure but has no egraph rule, and
                    // the flag result is dead so vcode DCE leaves one
                    // `adds` in the cold block.
                    let nv = pos.ins().sadd_overflow(a, z).0;
                    pairs.push((a, nv));
                }
            }
            for a in func.dfg.inst_args_mut(i) {
                for &(old, nv) in &pairs {
                    if *a == old {
                        *a = nv;
                    }
                }
            }
            n += pairs.len();
        }
    }
    n
}

/// Fold multi-pred forwarder blocks edgefwd leaves behind.
///
/// edgefwd retargets edges past `jump`-only forwarders but skips a block
/// wholesale when any of its params escape into a dominated block, and
/// never touches `brif` forwarders. Handle both leftovers:
///
/// - `b(p0..pk): jump T(a..)` with escaping params: retarget each pred
///   edge that still substitutes cleanly. This is unsafe only when a
///   param is used in a block reachable from T without passing through
///   b — the new edge would reach that use without the param being
///   defined. Uses whose only b-free access goes through b stay sound:
///   the traversal still enters via b, and if b loses every pred it is
///   removed along with everything it dominated.
/// - `b(p0..pk): brif c, T1(a..), T2(d..)`: a `jump`-terminated pred
///   `P: jump b(x..)` absorbs the branch, becoming
///   `P: brif c', T1(a'..), T2(d'..)` with b's params replaced by the
///   edge's args. This duplicates the conditional into each such pred
///   but removes the forwarder and its edge copies entirely — the same
///   fold LLVM's simplifycfg applies to trivial diamonds. Preds with
///   wider terminators (brif/br_table/try_call) can't absorb a second
///   edge and are left alone. Same escape rule, checked against both
///   targets' b-free reach.
///
/// Cold blocks are never folded: coldedges adapters are single-`jump`
/// cold blocks by construction and must survive to emission. Cold preds
/// may still absorb a hot forwarder (the marking follows the block).
/// Runs to a small fixpoint so chains (`P -> bF -> bG -> T`) collapse.
pub fn foldforwarders(func: &mut Function) -> usize {
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    use cranelift_codegen::ir::{Block, BlockCall};
    use rustc_data_structures::fx::FxHashMap;

    enum Rw {
        /// Retarget edge `di` of `pinst` to a new block call.
        Edge { pinst: Inst, di: usize, call: BlockCall },
        /// Replace `pinst` (a `jump`) with a cloned `brif` in `pb`.
        Brif { pb: Block, pinst: Inst, data: InstructionData },
    }

    // Substitute a forwarder-internal value `v` onto the edge
    // (pb, pinst): a forwarder param maps to that edge's arg at the same
    // position; anything else must be a `Value` already valid on the
    // edge — i.e. dominate the predecessor terminator.
    let subst = |func: &Function,
                 domtree: &DominatorTree,
                 params: &[Value],
                 pargs: &[BlockArg],
                 v: Value,
                 pb: Block,
                 pinst: Inst|
     -> Option<BlockArg> {
        let v = func.dfg.resolve_aliases(v);
        if let Some(k) = params.iter().position(|&p| p == v) {
            return Some(pargs[k]);
        }
        let ok = match func.dfg.value_def(v) {
            ValueDef::Result(i, _) => {
                i != pinst && domtree.dominates(i, pinst, &func.layout)
            }
            ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
            _ => false,
        };
        ok.then_some(BlockArg::Value(v))
    };

    let subst_args = |func: &Function,
                      domtree: &DominatorTree,
                      params: &[Value],
                      pargs: &[BlockArg],
                      bc: BlockCall,
                      pb: Block,
                      pinst: Inst|
     -> Option<Vec<BlockArg>> {
        let pool = &func.dfg.value_lists;
        let mut out = Vec::new();
        for a in bc.args(pool) {
            match a {
                BlockArg::Value(v) => {
                    out.push(subst(func, domtree, params, pargs, v, pb, pinst)?)
                }
                // Pseudo-values can't be introduced onto a `jump` edge,
                // and we only call this for jump preds anyway.
                _ => return None,
            }
        }
        Some(out)
    };

    let mut total = 0;
    for _ in 0..8 {
        let cfg = ControlFlowGraph::with_function(func);
        let domtree = DominatorTree::with_function(func, &cfg);
        let entry = func.layout.entry_block();
        // Every block that uses a param of another block. A forwarder
        // fold is unsafe if a new edge can reach such a use without
        // traversing the param's defining block: the use would lose its
        // definition. (Coarse: any param's escape use counts — a reach
        // path that still passes through the defining block is harmless
        // in principle, but that precision isn't worth it.)
        let mut escape_blocks: FxHashSet<Block> = FxHashSet::default();
        for x in func.layout.blocks() {
            for i in func.layout.block_insts(x) {
                let mut note = |v: Value| {
                    let v = func.dfg.resolve_aliases(v);
                    if let ValueDef::Param(d, _) = func.dfg.value_def(v)
                        && d != x
                    {
                        escape_blocks.insert(x);
                    }
                };
                for &v in func.dfg.inst_args(i) {
                    note(v);
                }
                for bc in func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            note(v);
                        }
                    }
                }
            }
        }
        // True if folding `b` is unsafe: some escaping-param use is
        // reachable from `targets` without traversing b, so the new
        // edges could reach that use while its param is undefined.
        // Cheap when nothing escapes at all.
        let escapes_hazard = |cfg: &ControlFlowGraph,
                              b: Block,
                              targets: &[Block],
                              esc: &FxHashSet<Block>| {
            if esc.is_empty() {
                return false;
            }
            let mut reach = FxHashSet::default();
            let mut work: Vec<Block> = targets.to_vec();
            while let Some(x) = work.pop() {
                if x == b || !reach.insert(x) {
                    continue;
                }
                for s in cfg.succ_iter(x) {
                    work.push(s);
                }
            }
            reach.iter().any(|u| esc.contains(u))
        };
        let mut rws: Vec<Rw> = Vec::new();
        // A `jump` can only carry one edge, so at most one Brif rewrite
        // per inst — dedup defensively.
        let mut brifed: FxHashMap<Inst, ()> = FxHashMap::default();
        for b in func.layout.blocks().collect::<Vec<_>>() {
            if Some(b) == entry || func.layout.is_cold(b) {
                continue;
            }
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            if insts.len() != 1 {
                continue;
            }
            let term = insts[0];
            let params = func.dfg.block_params(b).to_vec();
            match func.dfg.insts[term] {
                InstructionData::Jump { destination, .. } => {
                    let t = destination.block(&func.dfg.value_lists);
                    if t == b {
                        continue;
                    }
                    let mut targs = Vec::new();
                    let mut ok = true;
                    for a in destination.args(&func.dfg.value_lists) {
                        match a {
                            BlockArg::Value(v) => {
                                targs.push(func.dfg.resolve_aliases(v))
                            }
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok || targs.len() != func.dfg.num_block_params(t) {
                        continue;
                    }
                    if escapes_hazard(&cfg, b, &[t], &escape_blocks) {
                        continue;
                    }
                    for (pb, pinst) in
                        cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect::<Vec<_>>()
                    {
                        if pb == b {
                            continue;
                        }
                        let dests = func.dfg.insts[pinst]
                            .branch_destination(
                                &func.dfg.jump_tables,
                                &func.dfg.exception_tables,
                            )
                            .to_vec();
                        for (di, bc) in dests.iter().enumerate() {
                            if bc.block(&func.dfg.value_lists) != b {
                                continue;
                            }
                            let pargs: Vec<BlockArg> =
                                bc.args(&func.dfg.value_lists).collect();
                            if pargs.len() != params.len() {
                                continue;
                            }
                            let mut new = Vec::with_capacity(targs.len());
                            let mut ok = true;
                            for &v in &targs {
                                if let Some(k) =
                                    params.iter().position(|&p| p == v)
                                {
                                    // Verbatim: valid on this same edge
                                    // even if a try_call pseudo-value.
                                    new.push(pargs[k]);
                                    continue;
                                }
                                let dom = match func.dfg.value_def(v) {
                                    ValueDef::Result(i, _) => {
                                        i != pinst
                                            && domtree.dominates(
                                                i,
                                                pinst,
                                                &func.layout,
                                            )
                                    }
                                    ValueDef::Param(d, _) => domtree
                                        .block_dominates(d, pb),
                                    _ => false,
                                };
                                if !dom {
                                    ok = false;
                                    break;
                                }
                                new.push(BlockArg::Value(v));
                            }
                            if !ok {
                                continue;
                            }
                            let call =
                                BlockCall::new(t, new, &mut func.dfg.value_lists);
                            rws.push(Rw::Edge {
                                pinst,
                                di,
                                call,
                            });
                        }
                    }
                }
                InstructionData::Brif { arg: c, blocks, .. } => {
                    let t1 = blocks[0].block(&func.dfg.value_lists);
                    let t2 = blocks[1].block(&func.dfg.value_lists);
                    if escapes_hazard(&cfg, b, &[t1, t2], &escape_blocks) {
                        continue;
                    }
                    for (pb, pinst) in
                        cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect::<Vec<_>>()
                    {
                        if pb == b || brifed.contains_key(&pinst) {
                            continue;
                        }
                        // Only a lone `jump` pred can absorb the two-edge
                        // branch without growing a fresh block.
                        let InstructionData::Jump {
                            destination: pd, ..
                        } = func.dfg.insts[pinst]
                        else {
                            continue;
                        };
                        if pd.block(&func.dfg.value_lists) != b {
                            continue;
                        }
                        let pargs: Vec<BlockArg> =
                            pd.args(&func.dfg.value_lists).collect();
                        if pargs.len() != params.len() {
                            continue;
                        }
                        let Some(BlockArg::Value(cv)) = subst(
                            func, &domtree, &params, &pargs, c, pb, pinst,
                        ) else {
                            continue;
                        };
                        let mut nb = Vec::with_capacity(2);
                        let mut ok = true;
                        for bc in blocks {
                            let Some(new) = subst_args(
                                func, &domtree, &params, &pargs, bc, pb,
                                pinst,
                            ) else {
                                ok = false;
                                break;
                            };
                            nb.push(BlockCall::new(
                                bc.block(&func.dfg.value_lists),
                                new,
                                &mut func.dfg.value_lists,
                            ));
                        }
                        if !ok || nb.len() != 2 {
                            continue;
                        }
                        let data = InstructionData::Brif {
                            opcode: Opcode::Brif,
                            arg: cv,
                            blocks: [nb[0], nb[1]],
                        };
                        rws.push(Rw::Brif { pb, pinst, data });
                        brifed.insert(pinst, ());
                    }
                }
                _ => continue,
            }
        }
        if rws.is_empty() {
            break;
        }
        let mut applied = 0;
        for rw in rws {
            match rw {
                Rw::Edge { pinst, di, call } => {
                    let dfg = &mut func.dfg;
                    let dests = dfg.insts[pinst].branch_destination_mut(
                        &mut dfg.jump_tables,
                        &mut dfg.exception_tables,
                    );
                    if let Some(d) = dests.get_mut(di) {
                        *d = call;
                        applied += 1;
                    }
                }
                Rw::Brif { pb, pinst, data } => {
                    let ni = func.dfg.make_inst(data);
                    func.layout.remove_inst(pinst);
                    func.layout.append_inst(ni, pb);
                    applied += 1;
                }
            }
        }
        total += applied;
        if applied == 0 {
            break;
        }
    }
    crate::jumpthread::remove_unreachable_blocks(func);
    total
}

/// Splice a `jump`'s target into its predecessor when the target has no
/// other predecessors. Cranelift's register allocator splits live ranges
/// at block boundaries, so straight-line code chopped into blocks (e.g.
/// bcheck's versioned clones) pays a parallel-copy shuffle on every edge.
/// A block can only have one terminator, so only unconditional edges can
/// be merged: move the target's instructions in, alias its params to the
/// edge args, and the per-edge copies disappear. Repeat to fixpoint: each
/// fusion can expose the next block of the same chain.
pub fn fusechains(func: &mut Function) -> usize {
    use rustc_data_structures::fx::FxHashMap;
    let mut n = 0;
    loop {
        // Incoming-edge count per block (self-edges included).
        let mut preds: FxHashMap<cranelift_codegen::ir::Block, u32> = FxHashMap::default();
        for b in func.layout.blocks() {
            let Some(lt) = func.layout.last_inst(b) else {
                continue;
            };
            for bc in func.dfg.insts[lt].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            ) {
                *preds.entry(bc.block(&func.dfg.value_lists)).or_default() += 1;
            }
        }
        let mut fused = false;
        for b in func.layout.blocks().collect::<Vec<_>>() {
            if func.layout.is_cold(b) {
                continue;
            }
            let Some(lt) = func.layout.last_inst(b) else {
                continue;
            };
            if func.dfg.insts[lt].opcode() != Opcode::Jump {
                continue;
            }
            let dests = func.dfg.insts[lt]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .to_vec();
            let pick = dests.iter().find(|bc| {
                let t = bc.block(&func.dfg.value_lists);
                t != b
                    && !func.layout.is_cold(t)
                    && preds.get(&t).copied().unwrap_or(0) == 1
            });
            let Some(bc) = pick else { continue };
            let t = bc.block(&func.dfg.value_lists);
            // Edge args must all be plain values (no try_call exn markers)
            // so the successor's params can be aliased to them.
            let params = func.dfg.block_params(t).to_vec();
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            let mut pairs = Vec::with_capacity(args.len());
            let mut ok = args.len() == params.len();
            for (p, a) in params.iter().zip(args) {
                if let BlockArg::Value(v) = a {
                    pairs.push((*p, v));
                } else {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            // Detach t's params so they can be aliased to the edge args;
            // every use of a param (in t or anywhere t dominated) then
            // resolves to the value the predecessor passed.
            func.dfg.detach_block_params(t);
            for (p, v) in pairs {
                func.dfg.change_to_alias(p, v);
            }
            let tinsts: Vec<Inst> = func.layout.block_insts(t).collect();
            func.layout.remove_inst(lt);
            for i in tinsts {
                func.layout.remove_inst(i);
                func.layout.append_inst(i, b);
            }
            func.layout.remove_block(t);
            n += 1;
            fused = true;
            break;
        }
        if !fused {
            break;
        }
    }
    n
}

/// Block params that receive the same value on every incoming edge are
/// pure copy overhead: regalloc materializes a parallel copy per arg on
/// each conditional pred (that's most of the `mov;b` edge-split blocks).
/// When every pred passes `v` for param `p`, `v` is defined on every pred
/// edge and so dominates the block: drop `p`, rewrite its uses to `v`,
/// and strip that arg slot from every edge — an arg-free conditional edge
/// needs no split at all. Duplicate params (the same arg value on every
/// edge) merge the same way. Jumpthread/merge-shaped CFGs leave thousands
/// of these; iterate to a fixpoint since each drop can expose more.
pub fn sameargs(func: &mut Function) -> usize {
    let mut n = 0;
    for _ in 0..8 {
        let k = sameargs_round(func);
        n += k;
        if k == 0 {
            break;
        }
    }
    n
}

fn sameargs_round(func: &mut Function) -> usize {
    // Every edge target -> [(inst, dest index)] — jump/brif/br_table and
    // try_call exception edges all appear in `branch_destination`.
    let mut edges: FxHashMap<Block, Vec<(Inst, usize)>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for (d, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                edges
                    .entry(bc.block(&func.dfg.value_lists))
                    .or_default()
                    .push((i, d));
            }
        }
    }
    // param Value -> replacement, drop-index sets, and per-edge arg drops.
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut arg_drops: Vec<(Inst, usize, Vec<usize>)> = vec![];
    let mut param_drops: Vec<Value> = vec![];
    for (&b, es) in &edges {
        let ps = func.dfg.block_params(b);
        if ps.is_empty() || es.is_empty() {
            continue;
        }
        // Resolve every edge's args once, keeping pseudo-args distinct:
        // `TryCallRet(0)` and `TryCallRet(1)` are different incoming
        // values — collapsing them to one "not a Value" marker makes the
        // duplicate-param merge below fuse a call's distinct results.
        let mut edge_args: Vec<Vec<BlockArg>> = Vec::with_capacity(es.len());
        let mut uniform = true;
        for &(i, d) in es {
            let bc = &func.dfg.insts[i].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            )[d];
            let args: Vec<BlockArg> = bc
                .args(&func.dfg.value_lists)
                .map(|a| match a {
                    BlockArg::Value(v) => BlockArg::Value(func.dfg.resolve_aliases(v)),
                    _ => a,
                })
                .collect();
            if args.len() != ps.len() {
                uniform = false;
                break;
            }
            edge_args.push(args);
        }
        if !uniform {
            continue;
        }
        let mut repl: Vec<(usize, Value)> = vec![];
        for ix in 0..ps.len() {
            let mut same: Option<Value> = None;
            let mut differs = false;
            for a in &edge_args {
                match a[ix] {
                    BlockArg::Value(v) => {
                        if same.is_none() {
                            same = Some(v);
                        } else if same != Some(v) {
                            differs = true;
                            break;
                        }
                    }
                    _ => {
                        differs = true;
                        break;
                    }
                }
            }
            let mut found = false;
            if !differs {
                let v = same.unwrap();
                // `v` dominates `b` unless it's defined in `b` itself
                // (a self-edge passing an in-block def/param).
                let in_b = match func.dfg.value_def(v) {
                    ValueDef::Result(i, _) => func.layout.inst_block(i) == Some(b),
                    ValueDef::Param(bb, _) => bb == b,
                    _ => true,
                };
                if !in_b {
                    repl.push((ix, v));
                    found = true;
                }
            }
            if !found {
                // Duplicate-param merge: an earlier param with the
                // identical arg vector (resolving through drops).
                for jx in 0..ix {
                    if edge_args.iter().all(|a| a[jx] == a[ix]) {
                        let rep = repl
                            .iter()
                            .find(|(k, _)| *k == jx)
                            .map(|(_, v)| *v)
                            .unwrap_or(ps[jx]);
                        repl.push((ix, rep));
                        break;
                    }
                }
            }
        }
        if repl.is_empty() {
            continue;
        }
        let mut ixs: Vec<usize> = repl.iter().map(|(ix, _)| *ix).collect();
        ixs.sort_unstable_by(|x, y| y.cmp(x));
        for &(i, d) in es {
            arg_drops.push((i, d, ixs.clone()));
        }
        for (ix, v) in repl {
            pmap.insert(ps[ix], v);
            param_drops.push(ps[ix]);
        }
    }
    if param_drops.is_empty() {
        return 0;
    }
    // Flatten alias chains into inst data first: a `change_to_alias`'d
    // value (coldargs' `sadd_overflow` rebinds, deflag results) can point
    // at a param we're about to detach — uses through it wouldn't be
    // rewritten and the alias would dangle.
    func.dfg.resolve_all_aliases();
    // Replacements may themselves be dropped params: chase to a survivor.
    let keys: Vec<Value> = pmap.keys().copied().collect();
    for p in keys {
        let mut w = pmap[&p];
        let mut seen: FxHashSet<Value> = FxHashSet::default();
        while let Some(&u) = pmap.get(&w) {
            if !seen.insert(w) {
                break;
            }
            w = u;
        }
        pmap.insert(p, w);
    }
    // One global use rewrite (inst args and every BlockCall's args).
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let dfg = &mut func.dfg;
            let mut data = dfg.insts[i];
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |x| pmap.get(&x).copied().unwrap_or(x),
            );
            dfg.insts[i] = data;
        }
    }
    // Strip the dropped arg slots from every pred edge (descending so
    // earlier indices stay valid), then drop the params themselves.
    for (i, d, ixs) in arg_drops {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[i].branch_destination_mut(
            &mut dfg.jump_tables,
            &mut dfg.exception_tables,
        )[d];
        for &ix in &ixs {
            bc.remove(ix, &mut dfg.value_lists);
        }
    }
    for p in &param_drops {
        func.dfg.remove_block_param(*p);
    }
    param_drops.len()
}

/// Returns the number of rewrites.
pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    if crate::pass_enabled("PLIRON_DEFLAG") {
        n += deflag(func);
    }
    if crate::pass_enabled("PLIRON_ULOAD") {
        n += uloads(func);
    }
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            if pos.func.dfg.insts[inst].opcode() == Opcode::Brif {
                if crate::pass_enabled("PLIRON_BRIFNOT") && brif_not(pos.func, inst) {
                    n += 1;
                }
                continue;
            }
            if let InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } = pos.func.dfg.insts[inst]
            {
                if widen_on() && widen(&mut pos, inst, arg) {
                    n += 1;
                }
                continue;
            }
            // `urem x, 2^k` = `x & (2^k - 1)`: scalar urem is a libcall-ish
            // divide, and the mask form reaches loopvec's cond trees.
            if let InstructionData::Binary {
                opcode: Opcode::Urem,
                args,
            } = pos.func.dfg.insts[inst]
            {
                let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
                if let Some(k) = iconst(pos.func, args[1])
                    && ty.is_int()
                    && !ty.is_vector()
                    && ty.bits() <= 64
                    && k > 0
                    && (k as u64).is_power_of_two()
                    && (ty.bits() == 64 || k < (1i64 << ty.bits()))
                {
                    let m = pos.ins().iconst(ty, k - 1);
                    pos.func.replace(inst).band(args[0], m);
                    n += 1;
                }
                continue;
            }
            if matches!(
                pos.func.dfg.insts[inst].opcode(),
                Opcode::Select | Opcode::Bitselect
            ) {
                if crate::pass_enabled("PLIRON_MINMAX") && minmax(&mut pos, inst) {
                    n += 1;
                }
                continue;
            }
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                cond,
                args,
            } = pos.func.dfg.insts[inst]
            else {
                continue;
            };
            // `icmp cc x, bound` folds to a flag constant when `bound` is
            // outside `x`'s proven range — either the operand type's extreme
            // (`x >u MAX`, `x <s MIN`, `x <=u MAX`, ...) or a tighter ceiling
            // from `vmax` (`uextend`/`band`/`ushr`/`urem`-bounded values, like
            // `x8 <u 0x100000` bounds checks on narrow indexes).
            let ty = pos.func.dfg.value_type(args[0]);
            if ty.is_int() && !ty.is_vector() && ty.bits() <= 64 {
                let (mut cc, mut k, mut x) = (cond, args[1], args[0]);
                if iconst(pos.func, args[0]).is_some() {
                    cc = cc.swap_args();
                    k = args[0];
                    x = args[1];
                }
                if let Some(k) = iconst(pos.func, k) {
                    let bits = ty.bits() as u32;
                    let umax = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                    let smin = (-1i128 << (bits - 1)) as i64;
                    let smax = ((1i128 << (bits - 1)) - 1) as i64;
                    let ku = k as u64 & umax;
                    // `mx`: proven unsigned ceiling on the non-const operand;
                    // `nn`: x's range stays in the signed-positive half, so
                    // signed compares against `k` behave like unsigned ones.
                    let (mx, nn) = if crate::pass_enabled("PLIRON_VMAX") {
                        let m = vmax(pos.func, x);
                        (m, m <= smax as u64)
                    } else {
                        // Type max never fits the signed-positive half.
                        (umax, false)
                    };
                    let mx = mx as i64;
                    let c = match cc {
                        IntCC::UnsignedGreaterThan if ku == umax => Some(0),
                        IntCC::UnsignedLessThanOrEqual if ku == umax => Some(1),
                        IntCC::UnsignedLessThan if ku == 0 => Some(0),
                        IntCC::UnsignedGreaterThanOrEqual if ku == 0 => Some(1),
                        IntCC::SignedGreaterThan if k == smax => Some(0),
                        IntCC::SignedLessThanOrEqual if k == smax => Some(1),
                        IntCC::SignedLessThan if k == smin => Some(0),
                        IntCC::SignedGreaterThanOrEqual if k == smin => Some(1),
                        IntCC::Equal if ku > mx as u64 => Some(0),
                        IntCC::NotEqual if ku > mx as u64 => Some(1),
                        IntCC::UnsignedLessThan if ku > mx as u64 => Some(1),
                        IntCC::UnsignedLessThanOrEqual if ku >= mx as u64 => Some(1),
                        IntCC::UnsignedGreaterThan if ku >= mx as u64 => Some(0),
                        IntCC::UnsignedGreaterThanOrEqual if ku > mx as u64 => Some(0),
                        IntCC::SignedLessThan if nn && k > mx => Some(1),
                        IntCC::SignedLessThan if nn && k <= 0 => Some(0),
                        IntCC::SignedLessThanOrEqual if nn && k >= mx => Some(1),
                        IntCC::SignedLessThanOrEqual if nn && k < 0 => Some(0),
                        IntCC::SignedGreaterThan if nn && k >= mx => Some(0),
                        IntCC::SignedGreaterThan if nn && k < 0 => Some(1),
                        IntCC::SignedGreaterThanOrEqual if nn && k > mx => Some(0),
                        IntCC::SignedGreaterThanOrEqual if nn && k <= 0 => Some(1),
                        _ => None,
                    };
                    if let Some(c) = c {
                        pos.func.replace(inst).iconst(types::I8, c);
                        n += 1;
                    }
                }
            }
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual)
                || pos.func.dfg.value_type(args[0]) != types::I128
            {
                continue;
            }
            let (x, z) = if const128(pos.func, args[1]) == Some((0, 0)) {
                (args[0], args[1])
            } else {
                (args[1], args[0])
            };
            if const128(pos.func, z) != Some((0, 0)) {
                continue;
            }
            let Some(((lo, hi), amt)) = bit_test(pos.func, x) else {
                continue;
            };
            // i128 `ishl` masks its amount to 0..128: bit 6 picks the half.
            let s = match pos.func.dfg.value_type(amt) {
                types::I128 => match narrow(&mut pos, amt) {
                    Some(s) => s,
                    None => pos.ins().isplit(amt).0,
                },
                types::I64 => amt,
                _ => continue,
            };
            let c64 = pos.ins().iconst(types::I64, 64);
            let half = pos.ins().band(s, c64);
            let l = pos.ins().iconst(types::I64, lo);
            let h = pos.ins().iconst(types::I64, hi);
            let w = pos.ins().select(half, h, l);
            let sh = pos.ins().ushr(w, s);
            let one = pos.ins().iconst(types::I64, 1);
            let b = pos.ins().band(sh, one);
            let zero = pos.ins().iconst(types::I64, 0);
            pos.func.replace(inst).icmp(cond, b, zero);
            n += 1;
        }
    }
    n
}

/// `umin`/`umax`/`smin`/`smax` on scalar ints wider than a register (i128):
/// no Cranelift target lowers them — the egraph's `select(icmp)->min` rules
/// don't restrict by width, and loopvec's scalar reduction epilogue emits
/// `umin` for `u128` accumulators — so split them back into `icmp` + `select`
/// (which do lower on i128: cmp/sbcs + a pair of csel).
pub fn wide_minmax(func: &mut Function) -> usize {
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            let InstructionData::Binary { opcode, args } = pos.func.dfg.insts[inst] else {
                continue;
            };
            let cc = match opcode {
                Opcode::Umin => IntCC::UnsignedLessThan,
                Opcode::Umax => IntCC::UnsignedGreaterThan,
                Opcode::Smin => IntCC::SignedLessThan,
                Opcode::Smax => IntCC::SignedGreaterThan,
                _ => continue,
            };
            let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
            if ty.is_vector() || !ty.is_int() || ty.bits() <= 64 {
                continue;
            }
            let [x, y] = args;
            let c = pos.ins().icmp(cc, x, y);
            pos.func.replace(inst).select(c, x, y);
            n += 1;
        }
    }
    n
}

/// Loads within the dereferenceable bytes of a frozen param (rustc `noalias
/// readonly` `&T`: immutable and live for the whole call) become `readonly
/// can_move`, which Cranelift's egraph treats as pure: it merges repeats and
/// hoists them out of loops, as LLVM's LICM/GVN do with the same facts.
pub fn frozen_loads(
    func: &mut Function,
    frozen: &rustc_data_structures::fx::FxHashMap<Value, u64>,
) -> usize {
    let mut n = 0;
    let mut hoist = Vec::new();
    let insts: Vec<Inst> = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .collect();
    for i in insts {
        let InstructionData::Load {
            opcode,
            arg,
            flags,
            offset,
        } = func.dfg.insts[i]
        else {
            continue;
        };
        let bytes = match opcode {
            Opcode::Load => func.dfg.value_type(func.dfg.first_result(i)).bytes(),
            Opcode::Uload8 | Opcode::Sload8 => 1,
            Opcode::Uload16 | Opcode::Sload16 => 2,
            Opcode::Uload32 | Opcode::Sload32 => 4,
            _ => continue,
        };
        let a = func.dfg.resolve_aliases(arg);
        let (base, k) = if frozen.contains_key(&a) {
            (a, 0)
        } else {
            match bin_k(func, a, Opcode::Iadd) {
                Some(xk) => xk,
                None => continue,
            }
        };
        let Some(&size) = frozen.get(&base) else {
            continue;
        };
        let lo = k + i64::from(offset);
        if lo < 0 || lo as u64 + u64::from(bytes) > size {
            continue;
        }
        let d = func.dfg.mem_flags[flags];
        if !d.notrap() || (d.readonly() && d.can_move()) {
            continue;
        }
        let nf = func
            .dfg
            .mem_flags
            .insert_unchecked(d.with_readonly().with_can_move());
        if let InstructionData::Load { flags, .. } = &mut func.dfg.insts[i] {
            *flags = nf;
        }
        n += 1;
        if let Ok(off) = i32::try_from(lo) {
            // Without `can_move` the egraph keeps the load where we put it
            // instead of sinking it back to its (in-loop) uses.
            let hf = func.dfg.mem_flags.insert_unchecked(d.with_readonly());
            hoist.push((i, opcode, base, off, hf));
        }
    }
    // Opt-in: on hm.rs it pins the key's fields in callee-saved registers
    // across the probe loop and costs +7% instructions.
    if !hoist.is_empty() && std::env::var("PLIRON_FROZEN_HOIST").is_ok_and(|v| v == "1") {
        hoist_frozen(func, hoist);
    }
    n
}

/// Frozen memory is dereferenceable and immutable for the whole call, so a
/// load of it inside a loop can be done once in the entry block. Cranelift's
/// own LICM skips these when they are `uload8/16` (not "pure" for its egraph).
fn hoist_frozen(
    func: &mut Function,
    hoist: Vec<(Inst, Opcode, Value, i32, cranelift_codegen::ir::MemFlags)>,
) {
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    use cranelift_codegen::loop_analysis::LoopAnalysis;
    let Some(entry) = func.layout.entry_block() else {
        return;
    };
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let mut done: rustc_data_structures::fx::FxHashMap<
        (Opcode, cranelift_codegen::ir::Type, Value, i32),
        Value,
    > = Default::default();
    for (i, opcode, base, off, flags) in hoist {
        let Some(b) = func.layout.inst_block(i) else {
            continue;
        };
        if la.innermost_loop(b).is_none() || !dt.is_reachable(b) {
            continue;
        }
        let r = func.dfg.first_result(i);
        let ty = func.dfg.value_type(r);
        let nv = *done.entry((opcode, ty, base, off)).or_insert_with(|| {
            let mut pos = FuncCursor::new(func).at_first_insertion_point(entry);
            let (ni, dfg) = pos.ins().Load(opcode, ty, flags, off.into(), base);
            dfg.first_result(ni)
        });
        func.layout.remove_inst(i);
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(r, nv);
    }
}
