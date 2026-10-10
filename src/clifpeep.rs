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
    BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef, types,
};
use rustc_data_structures::fx::FxHashSet;

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
            // `icmp cc x, bound` folds to a flag constant when `bound` is the
            // operand type's extreme in `cc`'s direction (`x >u MAX`,
            // `x <s MIN`, `x <=u MAX`, ...). MIR emits these from range
            // comparisons like `x > u64::MAX`.
            let ty = pos.func.dfg.value_type(args[0]);
            if ty.is_int() && !ty.is_vector() && ty.bits() <= 64 {
                let (mut cc, mut k) = (cond, args[1]);
                if iconst(pos.func, args[0]).is_some() {
                    cc = cc.swap_args();
                    k = args[0];
                }
                if let Some(k) = iconst(pos.func, k) {
                    let bits = ty.bits() as u32;
                    let umax = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                    let smin = (-1i128 << (bits - 1)) as i64;
                    let smax = ((1i128 << (bits - 1)) - 1) as i64;
                    let ku = k as u64 & umax;
                    let c = match cc {
                        IntCC::UnsignedGreaterThan if ku == umax => Some(0),
                        IntCC::UnsignedLessThanOrEqual if ku == umax => Some(1),
                        IntCC::UnsignedLessThan if ku == 0 => Some(0),
                        IntCC::UnsignedGreaterThanOrEqual if ku == 0 => Some(1),
                        IntCC::SignedGreaterThan if k == smax => Some(0),
                        IntCC::SignedLessThanOrEqual if k == smax => Some(1),
                        IntCC::SignedLessThan if k == smin => Some(0),
                        IntCC::SignedGreaterThanOrEqual if k == smin => Some(1),
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
