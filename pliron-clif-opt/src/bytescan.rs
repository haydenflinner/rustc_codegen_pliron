//! Byte-set scan vectorization (`PLIRON_BYTESCAN`, `=0` disables; aarch64).
//!
//! `iter().position(|&c| TABLE[c as usize] != 0)` over a constant 256-entry
//! table whose non-zero entries are a handful of bytes (HTML escaping stops
//! at `"&<>`) is a memchr. LLVM keeps it scalar (it does not vectorize
//! early-exit loops); this compares 16 bytes per step. The loop, as the
//! pipeline has it before `punroll` (pulldown-cmark's `escape_html_scalar`):
//!
//! ```text
//! H(p, o..):  b = uload8 p (or p + base); [jump chain]
//!             t = load.i8 (T + b); brif t, FOUND, C
//! C:          p' = p + 1; brif (p' != END | p' <u END), H(p', f(p')..), EXIT
//! ```
//!
//! A vector pre-loop runs while more than 16 bytes remain: load 16, compare
//! with each stop byte, movemask (`shrn #4`, 4 bits per byte); no match
//! advances 16, a match re-enters `H` at that byte. With 16 or fewer left
//! it enters `H` where it stands. The scalar loop still takes every exit,
//! so FOUND and EXIT see exactly the original values; the pre-loop only
//! skips bytes whose table entry is zero (the loop must be pure math and
//! loads), and only reads bytes before `END`. Other header params must be
//! `f(p')` on the back edge with entry values matching `f(p0)` (an offset
//! `p - base`); they are recomputed for the new pointer with a copy of `f`.
//!
//! The table's bytes come from `Pipe::const_tables`: immutable globals of
//! this CGU and imported statics rustc evaluated (`State::extern_bytes`).
//! Not handled yet: top-tested loops (`while i < n && T[s[i]] == 0`).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, ExternalName, Function, GlobalValueData, Inst, InstBuilder, InstructionData,
    MemFlagsData, Opcode, Value, types,
};
use rustc_hash::FxHashMap;

use crate::pipeline::{Analysis, CfgDtLa};

/// Most stop bytes compared per 16-byte step.
const MAX_SET: usize = 6;

fn iconst_of(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// `v` without the `imul x, 1` / `uextend` wrappers the frontend leaves on
/// GEP indices (the compile-path egraph removes them later).
fn strip(func: &Function, v: Value) -> Value {
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..4 {
        let Some(i) = func.dfg.value_def(v).inst() else { break };
        match func.dfg.insts[i] {
            InstructionData::Binary { opcode: Opcode::Imul, args } if iconst_of(func, args[1]) == Some(1) => v = func.dfg.resolve_aliases(args[0]),
            InstructionData::Binary { opcode: Opcode::Imul, args } if iconst_of(func, args[0]) == Some(1) => v = func.dfg.resolve_aliases(args[1]),
            _ => break,
        }
    }
    v
}

/// `T` is `symbol_value gv (+ k)`: the table's bytes from `tables`, from
/// offset `k` on.
fn table_bytes<'t>(func: &Function, t: Value, tables: &'t FxHashMap<u32, std::sync::Arc<[u8]>>) -> Option<&'t [u8]> {
    let t = func.dfg.resolve_aliases(t);
    let i = func.dfg.value_def(t).inst()?;
    let (gvv, off) = match func.dfg.insts[i] {
        InstructionData::UnaryGlobalValue { opcode: Opcode::SymbolValue, global_value } => (global_value, 0i64),
        InstructionData::Binary { opcode: Opcode::Iadd, args } => {
            let (sym, k) = if let Some(k) = iconst_of(func, args[1]) { (args[0], k) } else { (args[1], iconst_of(func, args[0])?) };
            let si = func.dfg.value_def(func.dfg.resolve_aliases(sym)).inst()?;
            let InstructionData::UnaryGlobalValue { opcode: Opcode::SymbolValue, global_value } = func.dfg.insts[si] else {
                return None;
            };
            (global_value, k)
        }
        _ => return None,
    };
    let GlobalValueData::Symbol { name: ExternalName::User(r), offset, .. } = &func.global_values[gvv] else {
        return None;
    };
    let uen = &func.params.user_named_funcs()[*r];
    let bytes = tables.get(&uen.index)?;
    let off = usize::try_from(off.checked_add(offset.bits())?).ok()?;
    bytes.get(off..off.checked_add(256)?)
}

struct Shape {
    h: Block,
    c: Block,
    /// Index of the pointer (or index) param among `h`'s params.
    pi: usize,
    /// Index loops: the byte address is `param + base`.
    base: Option<Value>,
    end: Value,
    set: Vec<u8>,
    /// Back-edge arg expression of every other param: `(param index, value)`.
    others: Vec<(usize, Value)>,
    /// `p'` in `c`.
    pnext: Value,
}

/// `v` is built only from `pnext`, constants, values from outside the loop
/// and pure math in `c` (so a copy can recompute it for another pointer).
fn expr_ok(func: &Function, v: Value, c: Block, lb: &[Block], pnext: Value, depth: u32) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if v == pnext {
        return true;
    }
    if depth > 6 {
        return false;
    }
    match func.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Param(b, _) => !lb.contains(&b),
        cranelift_codegen::ir::ValueDef::Result(i, _) => {
            let Some(b) = func.layout.inst_block(i) else { return false };
            if b != c {
                return !lb.contains(&b);
            }
            let op = func.dfg.insts[i].opcode();
            matches!(op, Opcode::Iadd | Opcode::Isub | Opcode::Iconst | Opcode::Imul | Opcode::Ishl | Opcode::Ushr | Opcode::Band)
                && func.dfg.inst_args(i).iter().all(|&a| expr_ok(func, a, c, lb, pnext, depth + 1))
        }
        _ => false,
    }
}

/// Copy `v`'s expression with `pnext` replaced by `np`, before `at`.
fn clone_expr(func: &mut Function, v: Value, c: Block, pnext: Value, np: Value, at: Inst, memo: &mut FxHashMap<Value, Value>) -> Value {
    let v = func.dfg.resolve_aliases(v);
    if v == pnext {
        return np;
    }
    if let Some(&m) = memo.get(&v) {
        return m;
    }
    let cranelift_codegen::ir::ValueDef::Result(i, _) = func.dfg.value_def(v) else { return v };
    if func.layout.inst_block(i) != Some(c) {
        return v;
    }
    let args: Vec<Value> = func.dfg.inst_args(i).to_vec();
    let nargs: Vec<Value> = args.iter().map(|&a| clone_expr(func, a, c, pnext, np, at, memo)).collect();
    let ni = func.dfg.clone_inst(i);
    for (slot, a) in func.dfg.inst_args_mut(ni).iter_mut().zip(nargs) {
        *slot = a;
    }
    func.layout.insert_inst(ni, at);
    let r = func.dfg.first_result(ni);
    memo.insert(v, r);
    r
}

fn find(func: &Function, an: &mut Analysis, tables: &FxHashMap<u32, std::sync::Arc<[u8]>>, done: &[Block]) -> Option<Shape> {
    let CfgDtLa { cfg, la, .. } = an.cfg_dt_la(func);
    for lp in la.loops() {
        let h = la.loop_header(lp);
        if done.contains(&h) {
            continue;
        }
        let blocks: Vec<Block> = func.layout.blocks().filter(|&b| la.is_in_loop(b, lp)).collect();
        // Only pure math and loads: the vector pre-loop skips iterations.
        let pure = blocks.iter().all(|&b| {
            func.layout.block_insts(b).all(|i| {
                matches!(
                    func.dfg.insts[i].opcode(),
                    Opcode::Iadd | Opcode::Isub | Opcode::Imul | Opcode::Iconst | Opcode::Icmp | Opcode::Uload8
                        | Opcode::Load | Opcode::Brif | Opcode::Jump | Opcode::Band | Opcode::Uextend
                        | Opcode::Ireduce | Opcode::Select | Opcode::Ishl | Opcode::Ushr
                )
            })
        });
        if !pure {
            continue;
        }
        // H, then empty-param `jump` blocks with one predecessor, end in
        // the table test `brif`; the one remaining block is C.
        let mut tb = h;
        let mut chain = 1;
        while let Some(t) = func.layout.last_inst(tb)
            && let InstructionData::Jump { destination, .. } = func.dfg.insts[t]
        {
            let d = destination.block(&func.dfg.value_lists);
            if d == h || !la.is_in_loop(d, lp) || cfg.pred_iter(d).count() != 1 || !func.dfg.block_params(d).is_empty() || chain > 4 {
                break;
            }
            tb = d;
            chain += 1;
        }
        if blocks.len() != chain + 1 {
            continue;
        }
        let Some(&c) = blocks.iter().find(|&&b| b != h && !is_chain(func, h, tb, b)) else { continue };
        // H: b = uload8 p; a = iadd T, b; t = load.i8/uload8 a; brif t-test.
        let ht = func.layout.last_inst(tb)?;
        let InstructionData::Brif { arg: cond, blocks: hb, .. } = func.dfg.insts[ht] else { continue };
        let (t, ne) = {
            let cv = func.dfg.resolve_aliases(cond);
            match func.dfg.value_def(cv).inst().map(|i| func.dfg.insts[i]) {
                Some(InstructionData::IntCompare { cond: cc, args, .. })
                    if matches!(cc, IntCC::Equal | IntCC::NotEqual) && iconst_of(func, args[1]) == Some(0) =>
                {
                    (args[0], cc == IntCC::NotEqual)
                }
                _ => (cv, true),
            }
        };
        // The edge taken when `t != 0` leaves the loop; the other goes to C.
        let (found_slot, cont_slot) = if ne { (0, 1) } else { (1, 0) };
        let pool = &func.dfg.value_lists;
        if hb[cont_slot].block(pool) != c || la.is_in_loop(hb[found_slot].block(pool), lp) {
            continue;
        }
        let Some(ti) = func.dfg.value_def(func.dfg.resolve_aliases(t)).inst() else { continue };
        let InstructionData::Load { opcode: lop, arg: a, offset: toff, .. } = func.dfg.insts[ti] else { continue };
        if !matches!(lop, Opcode::Load | Opcode::Uload8) || func.dfg.value_type(func.dfg.first_result(ti)).bits() > 64 || i32::from(toff) != 0 {
            continue;
        }
        if lop == Opcode::Load && func.dfg.value_type(func.dfg.first_result(ti)) != types::I8 {
            continue;
        }
        let Some(ai) = func.dfg.value_def(func.dfg.resolve_aliases(a)).inst() else { continue };
        let InstructionData::Binary { opcode: Opcode::Iadd, args: aa } = func.dfg.insts[ai] else { continue };
        let is_byte = |v: Value| -> Option<Value> {
            let i = func.dfg.value_def(strip(func, v)).inst()?;
            match func.dfg.insts[i] {
                InstructionData::Load { opcode: Opcode::Uload8, arg, offset, .. } if i32::from(offset) == 0 => Some(arg),
                _ => None,
            }
        };
        let (tab, p) = match (is_byte(aa[1]), is_byte(aa[0])) {
            (Some(p), _) => (aa[0], p),
            (None, Some(p)) => (aa[1], p),
            _ => continue,
        };
        // The byte address is the param itself (pointer loop) or `i + base`
        // with `base` loop-invariant (index loop).
        let p = func.dfg.resolve_aliases(p);
        let param_of = |v: Value| func.dfg.block_params(h).iter().position(|&x| x == func.dfg.resolve_aliases(v));
        let invariant = |v: Value| match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
            cranelift_codegen::ir::ValueDef::Result(i, _) => func.layout.inst_block(i).is_some_and(|b| !la.is_in_loop(b, lp)),
            cranelift_codegen::ir::ValueDef::Param(b, _) => !la.is_in_loop(b, lp),
            _ => false,
        };
        let (pi, base) = if let Some(pi) = param_of(p) {
            (pi, None)
        } else {
            let Some(xi) = func.dfg.value_def(p).inst() else { continue };
            let InstructionData::Binary { opcode: Opcode::Iadd, args: xa } = func.dfg.insts[xi] else { continue };
            match (param_of(xa[0]), param_of(xa[1])) {
                (Some(k), _) if invariant(xa[1]) => (k, Some(func.dfg.resolve_aliases(xa[1]))),
                (_, Some(k)) if invariant(xa[0]) => (k, Some(func.dfg.resolve_aliases(xa[0]))),
                _ => continue,
            }
        };
        let p = func.dfg.block_params(h)[pi];
        let Some(bytes) = table_bytes(func, tab, tables) else { continue };
        let set: Vec<u8> = (0u16..256).filter(|&c| bytes[c as usize] != 0).map(|c| c as u8).collect();
        if set.is_empty() || set.len() > MAX_SET {
            continue;
        }
        // C: p' = p + 1; brif (p' != END | p' <u END), H(..), EXIT.
        let ct = func.layout.last_inst(c)?;
        let InstructionData::Brif { arg: ccond, blocks: cb, .. } = func.dfg.insts[ct] else { continue };
        let back = (0..2).find(|&k| cb[k].block(pool) == h);
        let Some(back) = back else { continue };
        if la.is_in_loop(cb[1 - back].block(pool), lp) || cfg.pred_iter(c).count() != 1 {
            continue;
        }
        let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(ccond)).inst() else { continue };
        let InstructionData::IntCompare { cond: ccc, args: ca, .. } = func.dfg.insts[ci] else { continue };
        let continue_on_true = back == 0;
        let ok_cc = match (ccc, continue_on_true) {
            (IntCC::NotEqual, true) | (IntCC::Equal, false) | (IntCC::UnsignedLessThan, true) | (IntCC::UnsignedGreaterThanOrEqual, false) => true,
            _ => false,
        };
        if !ok_cc {
            continue;
        }
        let pnext = func.dfg.resolve_aliases(ca[0]);
        let end = func.dfg.resolve_aliases(ca[1]);
        // p' = p + 1.
        let Some(pni) = func.dfg.value_def(pnext).inst() else { continue };
        let InstructionData::Binary { opcode: Opcode::Iadd, args: pa } = func.dfg.insts[pni] else { continue };
        let one = |x: Value| iconst_of(func, x) == Some(1);
        if !((func.dfg.resolve_aliases(pa[0]) == p && one(pa[1])) || (func.dfg.resolve_aliases(pa[1]) == p && one(pa[0]))) {
            continue;
        }
        // END defined outside the loop.
        let end_out = match func.dfg.value_def(end) {
            cranelift_codegen::ir::ValueDef::Result(i, _) => func.layout.inst_block(i).is_some_and(|b| !la.is_in_loop(b, lp)),
            cranelift_codegen::ir::ValueDef::Param(b, _) => !la.is_in_loop(b, lp),
            _ => false,
        };
        if !end_out {
            continue;
        }
        // Back-edge args: the pointer param gets p'; others f(p').
        let bargs: Vec<Value> = match cb[back].args(pool).map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        }).collect::<Option<Vec<_>>>() {
            Some(v) => v,
            None => continue,
        };
        if bargs.len() != func.dfg.block_params(h).len() || bargs[pi] != pnext {
            continue;
        }
        let mut others = Vec::new();
        let mut ok = true;
        for (k, &v) in bargs.iter().enumerate() {
            if k == pi {
                continue;
            }
            if !expr_ok(func, v, c, &blocks, pnext, 0) {
                ok = false;
                break;
            }
            others.push((k, v));
        }
        if !ok {
            continue;
        }
        // Entry edges: each must pass, for every other param, exactly f of
        // its pointer arg. Checked for the `f = p' - X` with entry `X`, `0`
        // shape (an index next to the pointer) and for `f` invariant.
        for pred in cfg.pred_iter(h) {
            if la.is_in_loop(pred.block, lp) {
                continue;
            }
            let dests = func.dfg.insts[pred.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            for bc in dests.iter().filter(|bc| bc.block(pool) == h) {
                let eargs: Option<Vec<Value>> = bc.args(pool).map(|a| match a {
                    BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                    _ => None,
                }).collect();
                let Some(eargs) = eargs else { ok = false; break };
                for &(k, f) in &others {
                    let f = func.dfg.resolve_aliases(f);
                    let good = match func.dfg.value_def(f).inst().map(|i| func.dfg.insts[i]) {
                        // f = p' - X: the entry arg is `p0 - X`, or 0 when X = p0.
                        Some(InstructionData::Binary { opcode: Opcode::Isub, args: sa })
                            if func.dfg.resolve_aliases(sa[0]) == pnext =>
                        {
                            let x = func.dfg.resolve_aliases(sa[1]);
                            !expr_uses(func, x, c, pnext)
                                && ((x == eargs[pi] && iconst_of(func, eargs[k]) == Some(0))
                                    || func.dfg.value_def(eargs[k]).inst().is_some_and(|ei| {
                                        matches!(func.dfg.insts[ei], InstructionData::Binary { opcode: Opcode::Isub, args: ea }
                                            if func.dfg.resolve_aliases(ea[0]) == eargs[pi] && func.dfg.resolve_aliases(ea[1]) == x)
                                    }))
                        }
                        _ => !expr_uses(func, f, c, pnext) && func.dfg.resolve_aliases(eargs[k]) == f,
                    };
                    if !good {
                        ok = false;
                    }
                }
            }
            if !ok {
                break;
            }
        }
        if !ok {
            continue;
        }
        return Some(Shape { h, c, pi, base, end, set, others, pnext });
    }
    None
}

/// `b` is on the `jump` chain from `h` to `tb`.
fn is_chain(func: &Function, h: Block, tb: Block, b: Block) -> bool {
    let mut x = h;
    loop {
        if x == b {
            return true;
        }
        if x == tb {
            return false;
        }
        let Some(t) = func.layout.last_inst(x) else { return false };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[t] else { return false };
        x = destination.block(&func.dfg.value_lists);
    }
}

fn expr_uses(func: &Function, v: Value, c: Block, pnext: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if v == pnext {
        return true;
    }
    match func.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Result(i, _) if func.layout.inst_block(i) == Some(c) => {
            func.dfg.inst_args(i).iter().any(|&a| expr_uses(func, a, c, pnext))
        }
        _ => false,
    }
}

fn rewrite(func: &mut Function, an: &mut Analysis, s: &Shape) {
    let ptr = func.dfg.value_type(func.dfg.block_params(s.h)[s.pi]);
    let entries: Vec<(Inst, usize)> = {
        let CfgDtLa { cfg, la, .. } = an.cfg_dt_la(func);
        let lp = la.innermost_loop(s.h).unwrap();
        let mut v = Vec::new();
        for pred in cfg.pred_iter(s.h) {
            if la.is_in_loop(pred.block, lp) {
                continue;
            }
            let n = func.dfg.insts[pred.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables).len();
            for k in 0..n {
                let d = func.dfg.insts[pred.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[k];
                if d.block(&func.dfg.value_lists) == s.h {
                    v.push((pred.inst, k));
                }
            }
        }
        v
    };
    let vh = func.dfg.make_block();
    let vb = func.dfg.make_block();
    let vn = func.dfg.make_block();
    let vf = func.dfg.make_block();
    let toh = func.dfg.make_block();
    for b in [vh, vb, vn, vf, toh] {
        func.layout.insert_block(b, s.h);
    }
    let vp = func.dfg.append_block_param(vh, ptr);
    let fp = func.dfg.append_block_param(vf, ptr);
    let fbits = func.dfg.append_block_param(vf, types::I64);
    let tp = func.dfg.append_block_param(toh, ptr);
    // Entry edges: H(p0, ..) -> VH(p0).
    for (inst, k) in entries {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[k];
        let p0 = match bc.args(&dfg.value_lists).nth(s.pi) {
            Some(BlockArg::Value(v)) => v,
            _ => unreachable!(),
        };
        *bc = cranelift_codegen::ir::BlockCall::new(vh, [BlockArg::Value(p0)], &mut dfg.value_lists);
    }
    // In bounds (more than 16 bytes left), not necessarily aligned.
    let flags = MemFlagsData::new().with_notrap();
    let mut cur = FuncCursor::new(func).at_bottom(vh);
    // VH: more than 16 bytes left?
    let rem = cur.ins().isub(s.end, vp);
    let more = cur.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, rem, 16);
    cur.ins().brif(more, vb, &[], toh, &[BlockArg::Value(vp)]);
    // VB: 16 bytes vs each stop byte -> 4 bits per byte.
    cur.goto_bottom(vb);
    let addr = match s.base {
        Some(b) => cur.ins().iadd(vp, b),
        None => vp,
    };
    let v = cur.ins().load(types::I8X16, flags, addr, 0);
    let mut m: Option<Value> = None;
    for &c in &s.set {
        let k = cur.ins().iconst(types::I8, i64::from(c));
        let sp = cur.ins().splat(types::I8X16, k);
        let e = cur.ins().icmp(IntCC::Equal, v, sp);
        m = Some(match m {
            None => e,
            Some(x) => cur.ins().bor(x, e),
        });
    }
    let m = m.unwrap();
    let le = MemFlagsData::new().with_endianness(cranelift_codegen::ir::Endianness::Little);
    let m16 = cur.ins().bitcast(types::I16X8, le, m);
    let four = cur.ins().iconst(types::I16, 4);
    let sh = cur.ins().ushr(m16, four);
    let ff = cur.ins().iconst(types::I16, 0xff);
    let ffs = cur.ins().splat(types::I16X8, ff);
    let lo = cur.ins().band(sh, ffs);
    let n = cur.ins().unarrow(lo, lo);
    let q = cur.ins().bitcast(types::I64X2, le, n);
    let bits = cur.ins().extractlane(q, 0);
    cur.ins().brif(bits, vf, &[BlockArg::Value(vp), BlockArg::Value(bits)], vn, &[]);
    // VN: no stop byte in these 16.
    cur.goto_bottom(vn);
    let vp2 = cur.ins().iadd_imm_s(vp, 16);
    cur.ins().jump(vh, &[BlockArg::Value(vp2)]);
    // VF: the first stop byte is at lane ctz(bits) / 4.
    cur.goto_bottom(vf);
    let tz = cur.ins().ctz(fbits);
    let lane = cur.ins().ushr_imm_s(tz, 2);
    let np = cur.ins().iadd(fp, lane);
    cur.ins().jump(toh, &[BlockArg::Value(np)]);
    // TOH: enter H at `tp`, other params recomputed as f(tp).
    let func = cur.func;
    let j = FuncCursor::new(func).at_bottom(toh).ins().jump(s.h, &[]);
    let nparams = func.dfg.block_params(s.h).len();
    let mut args = vec![tp; nparams];
    let mut memo = FxHashMap::default();
    for &(k, f) in &s.others {
        args[k] = clone_expr(func, f, s.c, s.pnext, tp, j, &mut memo);
    }
    let dfg = &mut func.dfg;
    let bc = &mut dfg.insts[j].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[0];
    *bc = cranelift_codegen::ir::BlockCall::new(s.h, args.into_iter().map(BlockArg::Value), &mut dfg.value_lists);
    an.invalidate(func);
}

pub fn run(func: &mut Function, an: &mut Analysis, tables: &FxHashMap<u32, std::sync::Arc<[u8]>>) -> usize {
    if tables.is_empty() {
        return 0;
    }
    let mut n = 0;
    let mut done = Vec::new();
    while n < 8 {
        let Some(s) = find(func, an, tables, &done) else { break };
        done.push(s.h);
        if !pliron_cfg::bisect("bytescan") {
            break;
        }
        rewrite(func, an, &s);
        n += 1;
    }
    n
}
