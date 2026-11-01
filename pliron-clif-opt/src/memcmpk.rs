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

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    BlockArg, ExternalName, FuncRef, Function, Inst, InstBuilder, InstructionData, LibCall, MemFlagsData,
    Opcode, Value, types,
};
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

/// The i32 memcmp result of `a[..n]` vs `b[..n]`, emitted at `pos`;
/// `exact` asks for the sign, else only zero/nonzero.
fn expand(pos: &mut FuncCursor, a: Value, b: Value, n: u64, exact: bool) -> Value {
    let flags = MemFlagsData::new().with_notrap();
    if n == 1 {
        let x = pos.ins().uload8(types::I32, flags, a, 0);
        let y = pos.ins().uload8(types::I32, flags, b, 0);
        return pos.ins().isub(x, y);
    }
    if exact {
        let ty = types::Type::int_with_byte_size(n as u16).unwrap();
        let x = pos.ins().load(ty, flags, a, 0);
        let y = pos.ins().load(ty, flags, b, 0);
        let x = pos.ins().bswap(x);
        let y = pos.ins().bswap(y);
        let gt = pos.ins().icmp(IntCC::UnsignedGreaterThan, x, y);
        let lt = pos.ins().icmp(IntCC::UnsignedLessThan, x, y);
        let gt = pos.ins().uextend(types::I32, gt);
        let lt = pos.ins().uextend(types::I32, lt);
        return pos.ins().isub(gt, lt);
    }
    let mut acc: Option<Value> = None;
    let (mut rem, mut off) = (n, 0i32);
    while rem > 0 {
        let (sz, ty) = [(8u64, types::I64), (4, types::I32), (2, types::I16), (1, types::I8)]
            .into_iter()
            .find(|&(sz, _)| sz <= rem)
            .unwrap();
        let x = pos.ins().load(ty, flags, a, off);
        let y = pos.ins().load(ty, flags, b, off);
        let mut d = pos.ins().bxor(x, y);
        if ty != types::I64 {
            d = pos.ins().uextend(types::I64, d);
        }
        acc = Some(match acc {
            Some(s) => pos.ins().bor(s, d),
            None => d,
        });
        rem -= sz;
        off += sz as i32;
    }
    let ne = pos.ins().icmp_imm_u(IntCC::NotEqual, acc.unwrap(), 0);
    pos.ins().uextend(types::I32, ne)
}

pub fn run(func: &mut Function, fn_names: &[String], fname: &str) -> usize {
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
            let Some(n) = iconst(func, len).and_then(|k| u64::try_from(k).ok()).filter(|n| (1..=16).contains(n)) else {
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
            let exact = matches!(n, 1 | 2 | 4 | 8);
            if !exact && !res.is_some_and(|r| zero_tests_only(func, &users, r)) {
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
            let mut pos = FuncCursor::new(func).at_inst(i);
            let r = expand(&mut pos, a, b2, n, exact);
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
