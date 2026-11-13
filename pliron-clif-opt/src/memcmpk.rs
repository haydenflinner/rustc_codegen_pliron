//! Small constant-length `memcmp`/`bcmp` calls inline (`PLIRON_MEMCMPK`,
//! `=0` disables).
//!
//! `<[u8]>::eq` and `str::ends_with(char)` reach `memcmp(a, b, len)` with
//! `len` a runtime value at the intrinsic; the slice-length test makes it
//! a constant only at CLIF level (pulldown-cmark's `HtmlWriter::run`
//! calls `memcmp(.., .., 1)` ~30 times per event, through `try_call`).
//! LLVM expands these. For `n` in {1, 2, 4, 8} the expansion is exact:
//! big-endian (byte-swapped) loads compared unsigned give memcmp's sign,
//! and for `n == 1` the byte difference itself. Other `n <= 16` expand
//! only when every use of the result tests it against zero (`eq`/`ne`):
//! the xor of each chunk pair, or-ed together, is nonzero exactly when
//! the ranges differ. A `try_call` becomes a `jump` to its normal
//! destination; memcmp never unwinds.
//!
//! When the result is only tested against zero, the power-of-two sizes
//! take the equality form too (`PLIRON_MEMCMPK_EQ=0` restores the
//! three-way form): `starts_with(b"\xFF\xFE")` is one 16-bit compare, not
//! two byte swaps and two compares. A side that is a constant data object
//! (`Pipe::const_tables`) is read at compile time
//! (`PLIRON_MEMCMPK_CONST=0` off), so that compare is against an
//! immediate (encoding_rs's `Encoding::for_bom`, `tests/parity/bom.rs`).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    BlockArg, ExternalName, FuncRef, Function, GlobalValueData, Inst, InstBuilder, InstructionData, LibCall,
    MemFlagsData, Opcode, Value, types,
};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Block;
use rustc_hash::FxHashMap;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// `fref` is libc's `memcmp`/`bcmp` (a libcall, or a user name the
/// module's symbol table spells so).
fn is_memcmp(func: &Function, fn_names: &[String], fref: FuncRef) -> bool {
    match func.dfg.ext_funcs[fref].name {
        ExternalName::LibCall(LibCall::Memcmp) => true,
        ExternalName::User(u) => {
            let un = &func.params.user_named_funcs()[u];
            un.namespace == 0
                && fn_names.get(un.index as usize).is_some_and(|s| s == "memcmp" || s == "bcmp")
        }
        _ => false,
    }
}

/// Every use of `v` is `icmp eq/ne v, 0` (either operand order).
fn zero_tests_only(func: &Function, users: &FxHashMap<Value, Vec<Inst>>, v: Value) -> bool {
    users.get(&v).is_some_and(|us| {
        us.iter().all(|&u| match func.dfg.insts[u] {
            InstructionData::IntCompare { cond: IntCC::Equal | IntCC::NotEqual, args, .. } => {
                let (a, b) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
                (a == v && iconst(func, b) == Some(0)) || (b == v && iconst(func, a) == Some(0))
            }
            _ => false,
        })
    })
}

/// Constant data tables by `DataId` index (`Pipe::const_tables`).
pub type Tables = FxHashMap<u32, std::sync::Arc<[u8]>>;

/// The bytes at `p` when `p` is `symbol_value gv (+ k)` of a constant
/// data object in `tables`, from that address to the object's end.
fn const_bytes<'t>(func: &Function, p: Value, tables: &'t Tables) -> Option<&'t [u8]> {
    if tables.is_empty() {
        return None;
    }
    let p = func.dfg.resolve_aliases(p);
    let i = func.dfg.value_def(p).inst()?;
    let (gvv, off) = match func.dfg.insts[i] {
        InstructionData::UnaryGlobalValue { opcode: Opcode::SymbolValue, global_value } => (global_value, 0i64),
        InstructionData::Binary { opcode: Opcode::Iadd, args } => {
            let (sym, k) = if let Some(k) = iconst(func, args[1]) {
                (args[0], k)
            } else {
                (args[1], iconst(func, args[0])?)
            };
            let si = func.dfg.value_def(func.dfg.resolve_aliases(sym)).inst()?;
            let InstructionData::UnaryGlobalValue { opcode: Opcode::SymbolValue, global_value } = func.dfg.insts[si]
            else {
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
    if uen.namespace != 1 {
        return None;
    }
    let bytes = tables.get(&uen.index)?;
    let off = usize::try_from(off.checked_add(offset.bits())?).ok()?;
    bytes.get(off..)
}

/// `load.ty p+off`, or the constant it reads when `kb` holds `p`'s bytes
/// (little-endian targets only: `kb` is `None` otherwise).
fn ld(pos: &mut FuncCursor, ty: types::Type, p: Value, off: i32, kb: Option<&[u8]>) -> Value {
    let sz = ty.bytes() as usize;
    if let Some(b) = kb.and_then(|b| b.get(off as usize..off as usize + sz)) {
        let mut v = 0u64;
        for (k, &x) in b.iter().enumerate() {
            v |= (x as u64) << (8 * k);
        }
        // `iconst` takes the value sign-extended from the type's width.
        let sh = 64 - 8 * sz as u32;
        return pos.ins().iconst(ty, ((v << sh) as i64) >> sh);
    }
    pos.ins().load(ty, MemFlagsData::new().with_notrap(), p, off)
}

/// The i32 memcmp result of `a[..n]` vs `b[..n]`, emitted at `pos`;
/// `exact` asks for the sign, else only zero/nonzero. `ka`/`kb`: the
/// bytes of a side that is constant data. With the zero/nonzero form
/// also the two tests themselves, `(differ, equal)` as `icmp` results,
/// for the caller to hand to the result's `ne 0` / `eq 0` users.
fn expand(
    pos: &mut FuncCursor,
    a: Value,
    b: Value,
    n: u64,
    exact: bool,
    ka: Option<&[u8]>,
    kb: Option<&[u8]>,
) -> (Value, Option<(Value, Value)>) {
    let flags = MemFlagsData::new().with_notrap();
    if n == 1 {
        let x = pos.ins().uload8(types::I32, flags, a, 0);
        let y = pos.ins().uload8(types::I32, flags, b, 0);
        return (pos.ins().isub(x, y), None);
    }
    if exact {
        let ty = types::Type::int_with_byte_size(n as u16).unwrap();
        let x = ld(pos, ty, a, 0, ka);
        let y = ld(pos, ty, b, 0, kb);
        let x = pos.ins().bswap(x);
        let y = pos.ins().bswap(y);
        let gt = pos.ins().icmp(IntCC::UnsignedGreaterThan, x, y);
        let lt = pos.ins().icmp(IntCC::UnsignedLessThan, x, y);
        let gt = pos.ins().uextend(types::I32, gt);
        let lt = pos.ins().uextend(types::I32, lt);
        return (pos.ins().isub(gt, lt), None);
    }
    // Against a constant: the chunks' compares combined (`or` of the
    // `ne`s, `and` of the `eq`s), each on a 32- or 64-bit value so
    // aarch64 fuses them (`cmp; ccmp`) with the constant as an
    // immediate. Else the xor of the chunks, or-ed.
    let chain = ka.is_some() || kb.is_some();
    let mut acc: Option<Value> = None;
    let mut tests: Option<(Value, Value)> = None;
    let (mut rem, mut off) = (n, 0i32);
    while rem > 0 {
        let (sz, ty) = [(8u64, types::I64), (4, types::I32), (2, types::I16), (1, types::I8)]
            .into_iter()
            .find(|&(sz, _)| sz <= rem)
            .unwrap();
        let x = ld(pos, ty, a, off, ka);
        let y = ld(pos, ty, b, off, kb);
        if chain || sz == n {
            let wide = if !chain || sz == 8 { ty } else { types::I32 };
            let (x, y) = if ty == wide {
                (x, y)
            } else {
                (pos.ins().uextend(wide, x), pos.ins().uextend(wide, y))
            };
            let ne = pos.ins().icmp(IntCC::NotEqual, x, y);
            let eq = pos.ins().icmp(IntCC::Equal, x, y);
            tests = Some(match tests {
                Some((n0, e0)) => (pos.ins().bor(n0, ne), pos.ins().band(e0, eq)),
                None => (ne, eq),
            });
        } else {
            let mut d = pos.ins().bxor(x, y);
            if ty != types::I64 {
                d = pos.ins().uextend(types::I64, d);
            }
            acc = Some(match acc {
                Some(s) => pos.ins().bor(s, d),
                None => d,
            });
        }
        rem -= sz;
        off += sz as i32;
    }
    let tests = tests.unwrap_or_else(|| {
        let acc = acc.unwrap();
        (pos.ins().icmp_imm_u(IntCC::NotEqual, acc, 0), pos.ins().icmp_imm_u(IntCC::Equal, acc, 0))
    });
    (pos.ins().uextend(types::I32, tests.0), Some(tests))
}

/// A constant `n` must equal on entry to `b`: the edge into `b`'s single-
/// predecessor chain is the equal arm of `brif (icmp eq n, K)` (or the
/// unequal arm of `ne`). serde's derived field visitors compare the
/// length against each name's and call `memcmp(s, "title", len)` with the
/// variable length on the equal arm.
fn pinned_by_guard(func: &Function, cfg: &ControlFlowGraph, mut b: Block, n: Value) -> Option<i64> {
    let n = func.dfg.resolve_aliases(n);
    for _ in 0..8 {
        let mut preds = cfg.pred_iter(b);
        let (Some(p), None) = (preds.next(), preds.next()) else { return None };
        if let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst]
            && let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst()
            && let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[ci]
        {
            let (a0, a1) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
            let k = if a0 == n { iconst(func, a1) } else if a1 == n { iconst(func, a0) } else { None };
            let taken_true = blocks[0].block(&func.dfg.value_lists) == b;
            let taken_false = blocks[1].block(&func.dfg.value_lists) == b;
            if let Some(k) = k
                && taken_true != taken_false
                && ((cond == IntCC::Equal && taken_true) || (cond == IntCC::NotEqual && taken_false))
            {
                return Some(k);
            }
        }
        b = p.block;
    }
    None
}

pub fn run(func: &mut Function, fn_names: &[String], fname: &str) -> usize {
    run_with(func, fn_names, fname, &Tables::default(), true)
}

/// [`run`] with the module's constant data (`tables`); `little`: the
/// target is little-endian (constants are only read then).
pub fn run_with(func: &mut Function, fn_names: &[String], fname: &str, tables: &Tables, little: bool) -> usize {
    let eq_form = !pliron_cfg::env_is("PLIRON_MEMCMPK_EQ", "0");
    let fold = little && !pliron_cfg::env_is("PLIRON_MEMCMPK_CONST", "0");
    let cfg = ControlFlowGraph::with_function(func);
    if ["memcmp", "bcmp"].iter().any(|s| fname.contains(s)) {
        return 0;
    }
    let mut users: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                users.entry(func.dfg.resolve_aliases(v)).or_default().push(i);
            }
        }
    }
    let mut done = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let (fref, try_call) = match func.dfg.insts[i] {
                InstructionData::Call { func_ref, .. } => (func_ref, false),
                InstructionData::TryCall { func_ref, .. } => (func_ref, true),
                _ => continue,
            };
            if !is_memcmp(func, fn_names, fref) {
                continue;
            }
            let sig = func.dfg.ext_funcs[fref].signature;
            if func.dfg.signatures[sig].returns.first().map(|r| r.value_type) != Some(types::I32) {
                continue;
            }
            let args = func.dfg.inst_args(i).to_vec();
            let [a, b2, len] = args[..] else { continue };
            let Some(n) = iconst(func, len)
                .or_else(|| pinned_by_guard(func, &cfg, b, len))
                .and_then(|k| u64::try_from(k).ok())
                .filter(|n| (1..=16).contains(n))
            else {
                continue;
            };
            // The value carrying the result: the call's own, or the normal
            // destination's param bound to `ret0`.
            let (res, normal) = if try_call {
                let InstructionData::TryCall { exception, .. } = func.dfg.insts[i] else { unreachable!() };
                let bc = *func.dfg.exception_tables[exception].normal_return();
                let dest = bc.block(&func.dfg.value_lists);
                let bargs: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                let slot = bargs.iter().position(|x| matches!(x, BlockArg::TryCallRet(0)));
                let res = slot.map(|k| func.dfg.block_params(dest)[k]);
                (res, Some((dest, bargs)))
            } else {
                (func.dfg.inst_results(i).first().copied(), None)
            };
            let zero_tested = res.is_some_and(|r| zero_tests_only(func, &users, r));
            let exact = matches!(n, 1 | 2 | 4 | 8) && !(eq_form && zero_tested && n > 1);
            if !exact && !zero_tested {
                continue;
            }
            if !pliron_cfg::bisect("memcmpk") {
                continue;
            }
            // A `try_call` destination arg other than a plain value or
            // `ret0` would name a result memcmp doesn't have.
            if let Some((_, bargs)) = &normal
                && !bargs.iter().all(|x| matches!(x, BlockArg::Value(_) | BlockArg::TryCallRet(0)))
            {
                continue;
            }
            let (ka, kb) = if fold && n > 1 {
                let k = |p: Value| const_bytes(func, p, tables).filter(|b| b.len() as u64 >= n).map(|b| b.to_vec());
                (k(a), k(b2))
            } else {
                (None, None)
            };
            let mut pos = FuncCursor::new(func).at_inst(i);
            let (r, tests) = expand(&mut pos, a, b2, n, exact, ka.as_deref(), kb.as_deref());
            match normal {
                Some((dest, bargs)) => {
                    let mapped: Vec<BlockArg> = bargs
                        .iter()
                        .map(|x| match *x {
                            BlockArg::TryCallRet(_) => BlockArg::Value(r),
                            other => other,
                        })
                        .collect();
                    pos.func.replace(i).jump(dest, &mapped);
                }
                None => {
                    let res = pos.func.dfg.inst_results(i).to_vec();
                    pos.func.dfg.clear_results(i);
                    if let Some(&v) = res.first() {
                        // The result's zero tests are the expansion's own
                        // compares (defined here, ahead of every user).
                        if let (Some((ne, eq)), true) = (tests, eq_form) {
                            for &u in users.get(&v).map_or(&[][..], |us| &us[..]) {
                                let InstructionData::IntCompare { cond, .. } = pos.func.dfg.insts[u] else {
                                    continue;
                                };
                                if pos.func.layout.inst_block(u).is_none() {
                                    continue;
                                }
                                let ur = pos.func.dfg.first_result(u);
                                pos.func.dfg.clear_results(u);
                                pos.func.dfg.change_to_alias(ur, if cond == IntCC::Equal { eq } else { ne });
                                pos.func.layout.remove_inst(u);
                            }
                        }
                        pos.func.dfg.change_to_alias(v, r);
                    }
                    pos.func.layout.remove_inst(i);
                }
            }
            done += 1;
        }
    }
    done
}
