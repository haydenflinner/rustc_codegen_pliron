//! Per-function shared panic tails (`PLIRON_PANICSTUB=1`, opt-in).
//!
//! Every cold block that ends in `call f(args); trap code` — a bounds
//! check, `unwrap`, overflow or slice-index failure after lowering —
//! and names the same callee, trap code and argument types joins one
//! group. The group gets one shared cold block
//! `S(p..): call f(..); trap code`; each member keeps everything in
//! front of its call and ends in `jump S(args)` instead.
//!
//! What this adds over the passes that already exist:
//! - `tailmerge` merges blocks that print identically; `tailmergep`
//!   (default on) merges blocks of one *shape* (same instruction
//!   sequence modulo constants and outside operands). Both key on the
//!   whole block, so `panic_bounds_check(i, 4, loc)` (length is a local
//!   constant) and `panic_bounds_check(i, len, loc)` (length is a hot
//!   value behind a `coldargs` wrapper) stay separate, as does any
//!   block with different code in front of the call. Here only the
//!   `call; trap` suffix has to agree, so these all share one tail,
//!   and `tailmergep`'s own shared blocks for one callee fold together.
//! - `coldargs` wraps each hot integer call argument in
//!   `iadd a, (umulhi ..)` per cold call so the ABI register pin lands
//!   in cold code. A member's wrapper is stripped (the raw value rides
//!   the edge) and the shared block rebinds the parameter once, so the
//!   hot value is still never the fixed-register use.
//!
//! Per argument position the shared block uses: the constant itself
//! when every member passes an equal constant (materialized once), the
//! value itself when every member passes the same value (it dominates
//! every member, hence the shared block), and a block parameter
//! otherwise.
//!
//! Soundness needs no noreturn fact: `call f(a); trap c` in two places
//! and `jump S(a)` to one copy of it run the same call with the same
//! arguments and then the same trap. Only plain `call` qualifies —
//! `try_call` sites keep their own exception edge. The shared call
//! keeps the first member's source location: the other sites lose the
//! per-site debug line of the call instruction (the `&Location` the
//! panic message prints is an argument and is unaffected).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    types::Type, Block, BlockArg, BlockCall, FuncRef, Function, Inst, InstBuilder,
    InstructionData, Opcode, TrapCode, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

struct Site {
    block: Block,
    call: Inst,
    trap: Inst,
    /// The call's own (resolved) arguments.
    orig: Vec<Value>,
    /// The same, with `coldargs` wrappers looked through.
    args: Vec<Value>,
    /// Positions whose wrapper was looked through.
    stripped: Vec<bool>,
}

/// How the shared block obtains one call argument.
enum Arg {
    /// Differs per member: a block parameter.
    Param(Type),
    /// The same value in every member.
    Val(Value),
    /// An equal constant in every member: clone of this inst.
    Const(Inst),
}

/// Constant materializations that are safe to clone anywhere.
fn const_inst(func: &Function, v: Value) -> Option<Inst> {
    let ValueDef::Result(i, 0) = func.dfg.value_def(v) else {
        return None;
    };
    let ok = match func.dfg.insts[i] {
        // i128 `iconst` clones made this late have no isel rule.
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            ..
        } => func.dfg.value_type(v).bits() <= 64,
        InstructionData::UnaryIeee32 { .. } | InstructionData::UnaryIeee64 { .. } => true,
        InstructionData::UnaryGlobalValue {
            opcode: Opcode::SymbolValue | Opcode::FuncAddr | Opcode::TlsValue,
            ..
        } => true,
        _ => false,
    };
    (ok && func.dfg.inst_results(i).len() == 1).then_some(i)
}

fn iconst_of(func: &Function, v: Value) -> Option<i64> {
    let ValueDef::Result(i, _) = func.dfg.value_def(func.dfg.resolve_aliases(v)) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

/// `v` is `iadd a, z` defined in `b` with `z = umulhi _, 0` /
/// `umulhi 0, _` (`coldargs`' rebind of a hot argument; `z` is zero
/// whatever the other operand is): returns `a`.
fn unwrap_coldarg(func: &Function, b: Block, v: Value) -> Option<Value> {
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    if func.layout.inst_block(i) != Some(b) {
        return None;
    }
    let InstructionData::Binary {
        opcode: Opcode::Iadd,
        args: [a, z],
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let ValueDef::Result(zi, _) = func.dfg.value_def(func.dfg.resolve_aliases(z)) else {
        return None;
    };
    let InstructionData::Binary {
        opcode: Opcode::Umulhi,
        args: [x, y],
    } = func.dfg.insts[zi]
    else {
        return None;
    };
    (iconst_of(func, x) == Some(0) || iconst_of(func, y) == Some(0))
        .then(|| func.dfg.resolve_aliases(a))
}

/// Remove now-unused argument materialization (`coldargs` wrappers,
/// constants) from member block `b`, starting from `roots`.
fn sweep(func: &mut Function, uses: &mut FxHashMap<Value, u32>, b: Block, roots: &[Value]) {
    let mut work: Vec<Value> = roots.to_vec();
    while let Some(v) = work.pop() {
        if uses.get(&v).copied().unwrap_or(0) != 0 {
            continue;
        }
        let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
            continue;
        };
        if func.layout.inst_block(i) != Some(b) || func.dfg.inst_results(i).len() != 1 {
            continue;
        }
        let dead = const_inst(func, v).is_some()
            || matches!(func.dfg.insts[i].opcode(), Opcode::Iconst | Opcode::Iadd | Opcode::Umulhi);
        if !dead {
            continue;
        }
        for &a in func.dfg.inst_args(i) {
            let a = func.dfg.resolve_aliases(a);
            if let Some(n) = uses.get_mut(&a) {
                *n = n.saturating_sub(1);
            }
            work.push(a);
        }
        func.layout.remove_inst(i);
    }
}

/// Returns the number of panic sites redirected to a shared tail.
/// `copy` selects `coldargs`' `PLIRON_COLDCOPY` form for the rebind in
/// the shared block.
pub fn run(func: &mut Function, copy: bool) -> usize {
    let entry = func.layout.entry_block();
    // Candidate sites, grouped in first-seen order.
    let mut index: FxHashMap<(FuncRef, TrapCode, Vec<Type>), usize> = FxHashMap::default();
    let mut groups: Vec<(FuncRef, TrapCode, Vec<Site>)> = vec![];
    for b in func.layout.blocks() {
        if Some(b) == entry || !func.layout.is_cold(b) {
            continue;
        }
        let Some(trap) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Trap {
            opcode: Opcode::Trap,
            code,
        } = func.dfg.insts[trap]
        else {
            continue;
        };
        let Some(call) = func.layout.prev_inst(trap) else {
            continue;
        };
        let InstructionData::Call {
            opcode: Opcode::Call,
            func_ref,
            ..
        } = func.dfg.insts[call]
        else {
            continue;
        };
        let orig: Vec<Value> = func
            .dfg
            .inst_args(call)
            .iter()
            .map(|&v| func.dfg.resolve_aliases(v))
            .collect();
        let mut args = orig.clone();
        let mut stripped = vec![false; args.len()];
        for (j, a) in args.iter_mut().enumerate() {
            if let Some(raw) = unwrap_coldarg(func, b, *a) {
                *a = raw;
                stripped[j] = true;
            }
        }
        let tys: Vec<Type> = args.iter().map(|&v| func.dfg.value_type(v)).collect();
        let gi = *index.entry((func_ref, code, tys)).or_insert_with(|| {
            groups.push((func_ref, code, vec![]));
            groups.len() - 1
        });
        groups[gi].2.push(Site {
            block: b,
            call,
            trap,
            orig,
            args,
            stripped,
        });
    }
    if !groups.iter().any(|g| g.2.len() >= 2) {
        return 0;
    }
    // Total uses per value (`inst_values` covers branch args).
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *uses.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    let mut n = 0;
    // Members that ended up as `M(p..): jump S(own params)`: block ->
    // (shared block, param index feeding each shared param).
    let mut fwd: FxHashMap<Block, (Block, Vec<usize>)> = FxHashMap::default();
    for (func_ref, code, mut sites) in groups {
        // A call result used anywhere would dangle (nothing can follow
        // the trap, so this is defensive).
        sites.retain(|s| {
            func.dfg
                .inst_results(s.call)
                .iter()
                .all(|r| uses.get(r).copied().unwrap_or(0) == 0)
        });
        if sites.len() < 2 || !pliron_cfg::bisect("panicstub") {
            continue;
        }
        let s0 = &sites[0];
        let nargs = s0.args.len();
        let members: FxHashSet<Block> = sites.iter().map(|s| s.block).collect();
        let kinds: Vec<Arg> = (0..nargs)
            .map(|j| {
                let v0 = s0.args[j];
                let ty = func.dfg.value_type(v0);
                // One value everywhere: it is defined outside every
                // member (a member ends in a trap and dominates
                // nothing), so it dominates the shared block.
                let outside = match func.dfg.value_def(v0) {
                    ValueDef::Result(i, _) => func
                        .layout
                        .inst_block(i)
                        .is_some_and(|d| !members.contains(&d)),
                    ValueDef::Param(d, _) => !members.contains(&d),
                    _ => false,
                };
                if outside && sites.iter().all(|s| s.args[j] == v0) {
                    return Arg::Val(v0);
                }
                if let Some(c0) = const_inst(func, v0)
                    && sites.iter().all(|s| {
                        const_inst(func, s.args[j])
                            .is_some_and(|c| func.dfg.insts[c] == func.dfg.insts[c0])
                    })
                {
                    return Arg::Const(c0);
                }
                Arg::Param(ty)
            })
            .collect();
        // The shared block goes after the group's last member.
        let last = sites.last().unwrap().block;
        let sb = func.dfg.make_block();
        func.layout.insert_block_after(sb, last);
        func.layout.set_cold(sb);
        let srcloc = func.srcloc(s0.call);
        let mut ops: Vec<Value> = Vec::with_capacity(nargs);
        for k in &kinds {
            ops.push(match *k {
                Arg::Param(ty) => func.dfg.append_block_param(sb, ty),
                Arg::Val(v) => v,
                Arg::Const(c) => {
                    let ni = func.dfg.clone_inst(c);
                    func.layout.append_inst(ni, sb);
                    func.dfg.first_result(ni)
                }
            });
        }
        {
            // Rebind what any member had rebound (see `coldargs`): the
            // fixed-register use must be a value born in cold code.
            let mut zeros: FxHashMap<Type, Value> = FxHashMap::default();
            let mut pos = FuncCursor::new(func).at_bottom(sb);
            for j in 0..nargs {
                let v = ops[j];
                let ty = pos.func.dfg.value_type(v);
                if matches!(kinds[j], Arg::Const(_))
                    || !sites.iter().any(|s| s.stripped[j])
                    || !ty.is_int()
                    || ty.bits() > 64
                {
                    continue;
                }
                let z = match zeros.get(&ty) {
                    Some(&z) => z,
                    None => {
                        let c0 = pos.ins().iconst(ty, 0);
                        let z = if copy {
                            let c1 = pos.ins().iconst(ty, 1);
                            pos.ins().umulhi(c0, c1)
                        } else {
                            pos.ins().umulhi(v, c0)
                        };
                        zeros.insert(ty, z);
                        z
                    }
                };
                ops[j] = pos.ins().iadd(v, z);
                // Over-counting only keeps a dead def alive.
                *uses.entry(v).or_default() += 2;
            }
            let ci = pos.ins().call(func_ref, &ops);
            pos.func.set_srcloc(ci, srcloc);
            pos.ins().trap(code);
        }
        for k in &kinds {
            if let Arg::Val(v) = *k {
                *uses.entry(v).or_default() += 1;
            }
        }
        for s in &sites {
            func.layout.remove_inst(s.trap);
            func.layout.remove_inst(s.call);
            for v in &s.orig {
                if let Some(c) = uses.get_mut(v) {
                    *c = c.saturating_sub(1);
                }
            }
            let pass: Vec<Value> = (0..nargs)
                .filter(|&j| matches!(kinds[j], Arg::Param(_)))
                .map(|j| s.args[j])
                .collect();
            for &v in &pass {
                *uses.entry(v).or_default() += 1;
            }
            let bargs: Vec<BlockArg> = pass.iter().map(|&v| BlockArg::from(v)).collect();
            FuncCursor::new(func).at_bottom(s.block).ins().jump(sb, &bargs);
            sweep(func, &mut uses, s.block, &s.orig);
            n += 1;
            // Nothing left but the jump, and every arg is one of the
            // block's own params: preds can target `sb` directly.
            if func.layout.first_inst(s.block) == func.layout.last_inst(s.block) {
                let params = func.dfg.block_params(s.block);
                let map: Option<Vec<usize>> = pass
                    .iter()
                    .map(|v| params.iter().position(|p| func.dfg.resolve_aliases(*p) == *v))
                    .collect();
                if let Some(map) = map {
                    fwd.insert(s.block, (sb, map));
                }
            }
        }
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::panicstub", "panicstub: {} sites of {func_ref} -> {sb}", sites.len());
        }
    }
    if fwd.is_empty() {
        return n;
    }
    // Retarget every edge into a forwarding member; one terminator scan.
    let mut edges: Vec<(Inst, usize, Block)> = vec![];
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        for (di, bc) in func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            let d = bc.block(&func.dfg.value_lists);
            if fwd.contains_key(&d) {
                if bc.len(&func.dfg.value_lists) == func.dfg.num_block_params(d) {
                    edges.push((t, di, d));
                } else {
                    // Keep the forwarder for an edge we cannot rewrite.
                    fwd.remove(&d);
                }
            }
        }
    }
    let mut moved = false;
    for (t, di, d) in edges {
        let Some((sb, map)) = fwd.get(&d) else {
            continue;
        };
        let dfg = &mut func.dfg;
        let old: Vec<BlockArg> = dfg.insts[t]
            .branch_destination(&dfg.jump_tables, &dfg.exception_tables)[di]
            .args(&dfg.value_lists)
            .collect();
        let new = BlockCall::new(*sb, map.iter().map(|&k| old[k]), &mut dfg.value_lists);
        dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
            new;
        moved = true;
    }
    if moved {
        crate::jumpthread::remove_unreachable_blocks(func);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::types::I64;
    use cranelift_codegen::ir::{
        AbiParam, ExtFuncData, ExternalName, Signature, UserExternalName, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(func, &flags) {
            panic!("{e}\n{}", func.display());
        }
    }

    fn count(func: &Function, op: Opcode) -> usize {
        func.layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b))
            .filter(|&i| func.dfg.insts[i].opcode() == op)
            .count()
    }

    /// fn(i, len, k): three bounds checks against the same `len`, with
    /// three argument shapes: a `coldargs`-wrapped hot index, a local
    /// constant index, and a bare hot index.
    fn build(code: [TrapCode; 3]) -> (Function, FuncRef) {
        let mut sig = Signature::new(CallConv::SystemV);
        for _ in 0..3 {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let mut psig = Signature::new(CallConv::SystemV);
        for _ in 0..3 {
            psig.params.push(AbiParam::new(I64));
        }
        let psig = func.import_signature(psig);
        let name = func.declare_imported_user_function(UserExternalName::new(0, 1));
        let f = func.import_function(ExtFuncData {
            name: ExternalName::user(name),
            signature: psig,
            colocated: true,
            patchable: false,
        });
        let blocks: Vec<Block> = (0..7).map(|_| func.dfg.make_block()).collect();
        for &b in &blocks {
            func.layout.append_block(b);
        }
        let [b0, b1, b2, ok, c0, c1, c2] = blocks[..] else {
            unreachable!()
        };
        let i = func.dfg.append_block_param(b0, I64);
        let len = func.dfg.append_block_param(b0, I64);
        let k = func.dfg.append_block_param(b0, I64);
        for c in [c0, c1, c2] {
            func.layout.set_cold(c);
        }
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        cur.ins().brif(i, b1, &[], c0, &[]);
        cur.goto_bottom(b1);
        cur.ins().brif(len, b2, &[], c1, &[]);
        cur.goto_bottom(b2);
        cur.ins().brif(k, ok, &[], c2, &[]);
        cur.goto_bottom(ok);
        cur.ins().return_(&[i]);
        // c0: both hot args rebound the `coldargs` way.
        cur.goto_bottom(c0);
        let z0 = cur.ins().iconst(I64, 0);
        let z1 = cur.ins().iconst(I64, 1);
        let z = cur.ins().umulhi(z0, z1);
        let wi = cur.ins().iadd(i, z);
        let wl = cur.ins().iadd(len, z);
        let loc = cur.ins().iconst(I64, 100);
        cur.ins().call(f, &[wi, wl, loc]);
        cur.ins().trap(code[0]);
        // c1: constant index.
        cur.goto_bottom(c1);
        let seven = cur.ins().iconst(I64, 7);
        let z0 = cur.ins().iconst(I64, 0);
        let z1 = cur.ins().iconst(I64, 1);
        let z = cur.ins().umulhi(z0, z1);
        let wl = cur.ins().iadd(len, z);
        let loc = cur.ins().iconst(I64, 200);
        cur.ins().call(f, &[seven, wl, loc]);
        cur.ins().trap(code[1]);
        // c2: bare args.
        cur.goto_bottom(c2);
        let loc = cur.ins().iconst(I64, 300);
        cur.ins().call(f, &[k, len, loc]);
        cur.ins().trap(code[2]);
        (func, f)
    }

    #[test]
    fn merges_across_shapes() {
        let nr = TrapCode::NORETURN_CALL;
        let (mut func, _) = build([nr, nr, nr]);
        verify(&func);
        assert_eq!(run(&mut func, true), 3);
        verify(&func);
        assert_eq!(count(&func, Opcode::Call), 1, "{}", func.display());
        assert_eq!(count(&func, Opcode::Trap), 1);
        // One rebind (of the shared `len`) survives, in the shared
        // block; the per-site wrappers are gone.
        assert_eq!(count(&func, Opcode::Umulhi), 1, "{}", func.display());
        // Shared block: index and location are params, `len` is not.
        let sb = func.layout.blocks().last().unwrap();
        assert!(func.layout.is_cold(sb));
        assert_eq!(func.dfg.num_block_params(sb), 2, "{}", func.display());
        // Second run finds nothing.
        assert_eq!(run(&mut func, true), 0);
    }

    #[test]
    fn trap_codes_stay_apart() {
        let nr = TrapCode::NORETURN_CALL;
        let (mut func, _) = build([nr, TrapCode::unwrap_user(2), TrapCode::unwrap_user(3)]);
        assert_eq!(run(&mut func, true), 0);
        assert_eq!(count(&func, Opcode::Call), 3);
    }

    /// Two `tailmergep`-style shared blocks for one callee fold into
    /// one and their preds retarget past them.
    #[test]
    fn forwarders_retarget() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let mut psig = Signature::new(CallConv::SystemV);
        psig.params.push(AbiParam::new(I64));
        psig.params.push(AbiParam::new(I64));
        let psig = func.import_signature(psig);
        let name = func.declare_imported_user_function(UserExternalName::new(0, 1));
        let f = func.import_function(ExtFuncData {
            name: ExternalName::user(name),
            signature: psig,
            colocated: true,
            patchable: false,
        });
        let b0 = func.dfg.make_block();
        let m1 = func.dfg.make_block();
        let m2 = func.dfg.make_block();
        for b in [b0, m1, m2] {
            func.layout.append_block(b);
        }
        func.layout.set_cold(m1);
        func.layout.set_cold(m2);
        let x = func.dfg.append_block_param(b0, I64);
        let p1 = func.dfg.append_block_param(m1, I64);
        let q1 = func.dfg.append_block_param(m1, I64);
        let p2 = func.dfg.append_block_param(m2, I64);
        let q2 = func.dfg.append_block_param(m2, I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let one = cur.ins().iconst(I64, 1);
        cur.ins().brif(
            x,
            m1,
            &[BlockArg::Value(x), BlockArg::Value(one)],
            m2,
            &[BlockArg::Value(one), BlockArg::Value(x)],
        );
        cur.goto_bottom(m1);
        cur.ins().call(f, &[p1, q1]);
        cur.ins().trap(TrapCode::NORETURN_CALL);
        // m2 passes its params swapped.
        cur.goto_bottom(m2);
        cur.ins().call(f, &[q2, p2]);
        cur.ins().trap(TrapCode::NORETURN_CALL);
        verify(&func);
        assert_eq!(run(&mut func, false), 2);
        verify(&func);
        assert_eq!(func.layout.blocks().count(), 2, "{}", func.display());
        assert_eq!(count(&func, Opcode::Call), 1);
        // Both edges now carry (x, 1) straight into the shared block.
        let t = func.layout.last_inst(b0).unwrap();
        let dests = func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        for d in dests {
            let a: Vec<BlockArg> = d.args(&func.dfg.value_lists).collect();
            assert!(matches!(a[..], [BlockArg::Value(a0), BlockArg::Value(a1)] if a0 == x && a1 == one));
        }
    }
}
