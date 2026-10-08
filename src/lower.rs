//! pliron LLVM dialect -> Cranelift IR -> object file.
//!
//! Aggregates are kept as SSA "leaf lists" (one Cranelift value per scalar
//! leaf), so extract/insertvalue are free and loads/stores are per-leaf.

use std::sync::Arc;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::{Ieee32, Ieee64};
use cranelift_codegen::ir::{
    self, AbiParam, ArgumentPurpose, Block, FuncRef, GlobalValue, InstBuilder, MemFlagsData,
    Signature, StackSlotData, StackSlotKind, TrapCode, Type as ClType, types as clt,
};
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module, default_libcall_names};
use cranelift_object::{ObjectBuilder, ObjectModule};
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;
use pliron_llvm::attributes::{FCmpPredicateAttr, ICmpPredicateAttr};
use pliron_llvm::ops::*;
use rustc_codegen_ssa::common::AtomicRmwBinOp;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use smallvec::{SmallVec, smallvec};

use crate::context::{ArgExt, ConstVal, Exts, State, mask};
use crate::types::{TyK, classify, leaf_range, leaves, size_align, struct_offsets};

type Vals = SmallVec<[ir::Value; 2]>;

#[derive(Clone, Copy)]
enum Sym {
    F(FuncId, TypeHandle),
    D(DataId, bool),
}

pub fn make_sig(ctx: &Context, fn_ty: TypeHandle, exts: &Exts, cc: CallConv) -> Signature {
    let TyK::Func(ret, args, _) = classify(ctx, fn_ty) else {
        panic!("not a function type")
    };
    let mut sig = Signature::new(cc);
    let apply = |p: AbiParam, e: ArgExt, t: ClType| match e {
        ArgExt::SRet => AbiParam::special(t, ArgumentPurpose::StructReturn),
        ArgExt::ByVal(n) => AbiParam::special(t, ArgumentPurpose::StructArgument(n)),
        ArgExt::Zext if t.is_int() && t.bits() < 32 => p.uext(),
        ArgExt::Sext if t.is_int() && t.bits() < 32 => p.sext(),
        _ => p,
    };
    for (i, a) in args.iter().enumerate() {
        let lv = leaves(ctx, *a);
        let e = exts.params.get(i).copied().unwrap_or_default();
        let single = lv.len() == 1;
        for (_, t) in lv {
            let p = AbiParam::new(t);
            sig.params.push(if single { apply(p, e, t) } else { p });
        }
    }
    let lv = leaves(ctx, ret);
    let single = lv.len() == 1;
    for (_, t) in lv {
        let p = AbiParam::new(t);
        sig.returns
            .push(if single { apply(p, exts.ret, t) } else { p });
    }
    sig
}

/// Local, non-variadic functions whose address never escapes and that are
/// never invoked: only called directly from this module, so they may use Cranelift's `tail` ABI (up to
/// 8 int + 8 float return registers instead of SysV's 2+2).
pub(crate) fn internal_fns(
    ctx: &Context,
    st: &State<'_>,
) -> rustc_data_structures::fx::FxHashSet<String> {
    use rustc_data_structures::fx::FxHashSet;
    if !crate::pass_enabled("PLIRON_TAILCC") {
        return FxHashSet::default();
    }
    let mut taken = address_taken(ctx, st);
    // A `try_call` into a tail-CC callee clobbers every register, so callees
    // reached by an invoke keep the platform ABI.
    taken.extend(
        st.invokes
            .keys()
            .filter_map(|&op| crate::inline::direct_callee(ctx, st, op))
            .cloned(),
    );
    st.funcs
        .iter()
        .filter(|(n, f)| {
            f.linkage == Linkage::Local
                && !taken.contains(n.as_str())
                && !matches!(classify(ctx, f.ty), TyK::Func(_, _, true))
                && has_body(ctx, f.op)
        })
        .map(|(n, _)| n.clone())
        .collect()
}

/// Functions whose address is used other than as a direct callee: symbol
/// constants that are used or sit in global/aggregate initializers, and names
/// mentioned in asm.
pub(crate) fn address_taken(
    ctx: &Context,
    st: &State<'_>,
) -> rustc_data_structures::fx::FxHashSet<String> {
    use rustc_data_structures::fx::FxHashSet;
    let mut escaped: FxHashSet<Value> = st.globals.values().filter_map(|g| g.init).collect();
    for c in st.consts.values() {
        if let ConstVal::Agg(vs) = c {
            escaped.extend(vs.iter().copied());
        }
    }
    let mut taken = FxHashSet::default();
    for (v, c) in &st.consts {
        if let ConstVal::Sym { sym, .. } = c {
            if v.is_used(ctx) || escaped.contains(v) {
                taken.insert(sym.clone());
            }
        }
    }
    taken.extend(
        st.funcs
            .keys()
            .filter(|n| st.asm.contains(n.as_str()))
            .cloned(),
    );
    taken
}

pub(crate) fn has_body(ctx: &Context, f: Ptr<Operation>) -> bool {
    Operation::get_op::<FuncOp>(f, ctx)
        .unwrap()
        .get_entry_block(ctx)
        .is_some()
}

pub fn lower_to_object(
    unwind: bool,
    hot: bool,
    ctx: &Context,
    st: &State<'_>,
    isa: Arc<dyn TargetIsa>,
    name: &str,
) -> Vec<u8> {
    let mut b = ObjectBuilder::new(isa.clone(), name.to_string(), default_libcall_names()).unwrap();
    b.per_function_section(true);
    b.per_data_object_section(true);
    let mut m = ObjectModule::new(b);
    let cc = isa.default_call_conv();
    let internal = internal_fns(ctx, st);
    let cc_of = |n: &str| {
        if internal.contains(n) {
            CallConv::Tail
        } else {
            cc
        }
    };
    let mut ids: FxHashMap<String, Sym> = FxHashMap::default();
    let mut hot_bodies: FxHashMap<String, FuncId> = FxHashMap::default();
    let mut hot_asm = String::new();
    for (n, f) in &st.funcs {
        if st.dead_fns.contains(n) {
            continue;
        }
        let sig = make_sig(ctx, f.ty, &f.exts, cc_of(n));
        let mut l = f.linkage;
        if l == Linkage::Import && has_body(ctx, f.op) {
            l = Linkage::Export;
        }
        if hot && has_body(ctx, f.op) && crate::hot::patchable(n) {
            let body = m
                .declare_function(&crate::hot::body_name(n), Linkage::Hidden, &sig)
                .unwrap();
            let thunk = m.declare_function(n, Linkage::Import, &sig).unwrap();
            hot_bodies.insert(n.clone(), body);
            hot_asm.push_str(&crate::hot::thunk_asm(n, l));
            ids.insert(n.clone(), Sym::F(thunk, f.ty));
            continue;
        }
        let id = m
            .declare_function(n, l, &sig)
            .unwrap_or_else(|e| panic!("declare {n}: {e}"));
        ids.insert(n.clone(), Sym::F(id, f.ty));
    }
    for (n, g) in &st.globals {
        if ids.contains_key(n) {
            continue;
        }
        let l = if g.linkage == Linkage::Import && g.init.is_some() {
            Linkage::Export
        } else {
            g.linkage
        };
        let id = m
            .declare_data(n, l, g.mutable, g.tls)
            .unwrap_or_else(|e| panic!("declare {n}: {e}"));
        ids.insert(n.clone(), Sym::D(id, g.tls));
    }

    let mut eh = crate::eh::UnwindContext::new(&mut m, true, unwind);
    let cfg = m.target_config();
    let mut fbc = FunctionBuilderContext::new();
    let mut clctx = m.make_context();
    let mut threaded = 0usize;
    let mut merged = 0usize;
    let mut unreached = 0usize;
    let mut dupd = 0usize;
    let mut forwarded = 0usize;
    let mut peeped = 0usize;
    for (n, f) in &st.funcs {
        if !has_body(ctx, f.op) || st.dead_fns.contains(n) {
            continue;
        }
        CUR_FN.with(|c| c.borrow_mut().clone_from(n));
        let Sym::F(id, _) = ids[n] else {
            unreachable!()
        };
        let id = hot_bodies.get(n).copied().unwrap_or(id);
        clctx.func.signature = make_sig(ctx, f.ty, &f.exts, cc_of(n));
        let nonnull: FxHashSet<cranelift_codegen::ir::Value>;
        let derived: FxHashMap<cranelift_codegen::ir::Value, cranelift_codegen::ir::Value>;
        let frozen: FxHashMap<cranelift_codegen::ir::Value, u64>;
        {
            let b = FunctionBuilder::new(&mut clctx.func, &mut fbc);
            let mut fl = FnLower {
                ctx,
                st,
                m: &mut m,
                ids: &ids,
                b,
                vals: FxHashMap::default(),
                cconst: FxHashMap::default(),
                blocks: FxHashMap::default(),
                frefs: FxHashMap::default(),
                gvs: FxHashMap::default(),
                terminated: false,
                cc,
                internal: &internal,
                exn: None,
                vars: FxHashMap::default(),
                nonnull: FxHashSet::default(),
                bool01: FxHashSet::default(),
                derived: FxHashMap::default(),
                frozen: FxHashMap::default(),
            };
            fl.lower(f.op);
            fl.b.finalize(cfg);
            nonnull = std::mem::take(&mut fl.nonnull);
            derived = std::mem::take(&mut fl.derived);
            frozen = std::mem::take(&mut fl.frozen);
        }
        let dump = std::env::var("PLIRON_CLIF").is_ok_and(|f| n.contains(f.as_str()));
        if dump {
            eprintln!("==== clif {n} ====\n{}", clctx.func.display());
        }
        if st.jumpthread {
            threaded += crate::jumpthread::run(&mut clctx.func, &nonnull, &derived);
            if dump {
                eprintln!(
                    "==== clif {n} after jumpthread ====\n{}",
                    clctx.func.display()
                );
            }
            if std::env::var_os("PLIRON_VERIFY").is_some()
                && let Err(e) = cranelift_codegen::verify_function(&clctx.func, isa.flags())
            {
                panic!("jumpthread broke `{n}`: {e}\n{}", clctx.func.display());
            }
        }
        if st.unreach {
            unreached += crate::unreach::run(&mut clctx.func);
            if std::env::var_os("PLIRON_VERIFY").is_some()
                && let Err(e) = cranelift_codegen::verify_function(&clctx.func, isa.flags())
            {
                panic!("unreach broke `{n}`: {e}\n{}", clctx.func.display());
            }
        }
        if st.peep {
            peeped += crate::clifpeep::run(&mut clctx.func);
        }
        if st.tailmerge {
            merged += crate::tailmerge::run(&mut clctx.func);
            if std::env::var_os("PLIRON_VERIFY").is_some()
                && let Err(e) = cranelift_codegen::verify_function(&clctx.func, isa.flags())
            {
                panic!("tailmerge broke `{n}`: {e}\n{}", clctx.func.display());
            }
        }
        if st.taildup {
            dupd += crate::taildup::run(&mut clctx.func);
            if std::env::var_os("PLIRON_VERIFY").is_some()
                && let Err(e) = cranelift_codegen::verify_function(&clctx.func, isa.flags())
            {
                panic!("taildup broke `{n}`: {e}\n{}", clctx.func.display());
            }
        }
        if st.loadfwd {
            forwarded += crate::loadfwd::run(&mut clctx.func);
            if dump {
                eprintln!("==== clif {n} after loadfwd ====\n{}", clctx.func.display());
            }
            if std::env::var_os("PLIRON_VERIFY").is_some()
                && let Err(e) = cranelift_codegen::verify_function(&clctx.func, isa.flags())
            {
                panic!("loadfwd broke `{n}`: {e}\n{}", clctx.func.display());
            }
        }
        if std::env::var("PLIRON_CONSTBR").is_ok_and(|v| v == "1") {
            let k = crate::jumpthread::fold_const_branches(&mut clctx.func);
            if k > 0 && std::env::var_os("PLIRON_CONSTBR_DEBUG").is_some() {
                eprintln!("constbr {k} {n}");
            }
            if std::env::var_os("PLIRON_VERIFY").is_some()
                && let Err(e) = cranelift_codegen::verify_function(&clctx.func, isa.flags())
            {
                panic!("constbr broke `{n}`: {e}\n{}", clctx.func.display());
            }
        }
        if crate::pass_enabled("PLIRON_LOOPROT") {
            crate::looprot::run(&mut clctx.func);
            if dump {
                eprintln!("==== clif {n} after looprot ====\n{}", clctx.func.display());
            }
            if std::env::var_os("PLIRON_VERIFY").is_some()
                && let Err(e) = cranelift_codegen::verify_function(&clctx.func, isa.flags())
            {
                panic!("looprot broke `{n}`: {e}\n{}", clctx.func.display());
            }
        }
        if !frozen.is_empty() {
            let k = crate::clifpeep::frozen_loads(&mut clctx.func, &frozen);
            if dump {
                eprintln!(
                    "==== clif {n}: {k} frozen loads, params {:?} ====\n{}",
                    frozen,
                    clctx.func.display()
                );
            }
        }
        if let Err(e) = m.define_function(id, &mut clctx) {
            panic!("cranelift rejected `{n}`: {e:?}\n{}", clctx.func.display());
        }
        eh.add_function(&mut m, id, &clctx);
        m.clear_context(&mut clctx);
    }

    if st.loadfwd && std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!("loadfwd {name}: {forwarded} loads forwarded, {peeped} peepholes");
    }
    if st.jumpthread && std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!(
            "jumpthread {name}: {threaded} edges threaded, {merged} exit blocks merged, {unreached} unreachable edges, {dupd} returns duplicated"
        );
    }
    for (n, g) in &st.globals {
        let Some(init) = g.init else { continue };
        let Some(Sym::D(id, _)) = ids.get(n).copied() else {
            continue;
        };
        let (size, _) = size_align(ctx, init.get_type(ctx));
        let mut bytes = vec![0u8; size as usize];
        let mut relocs = Vec::new();
        write_const(ctx, st, init, 0, &mut bytes, &mut relocs);
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        // LLVM places constants in 16-byte-capped mergeable/aligned sections;
        // crates that reinterpret `&[u8]` statics rely on that by accident.
        let mut align = g.align.max(1);
        if !g.mutable && g.section.is_none() {
            let pref = if size > 16 { 16 } else if size.is_power_of_two() { size } else { 1 };
            align = align.max(pref);
        }
        desc.set_align(align);
        if g.used {
            desc.set_used(true);
        }
        if let Some(sec) = &g.section {
            desc.set_custom_section(sec);
        }
        for (off, sym, addend) in relocs {
            match ids.get(&sym).copied() {
                Some(Sym::F(fid, _)) => {
                    let fr = m.declare_func_in_data(fid, &mut desc);
                    desc.write_function_addr(off as u32, fr);
                }
                Some(Sym::D(did, _)) => {
                    let gv = m.declare_data_in_data(did, &mut desc);
                    desc.write_data_addr(off as u32, gv, addend);
                }
                None => panic!("unknown symbol {sym} in initializer of {n}"),
            }
        }
        m.define_data(id, &desc)
            .unwrap_or_else(|e| panic!("define data {n}: {e}"));
    }
    let mut product = m.finish();
    eh.emit(&mut product);
    for (alias, target, weak) in &st.aliases {
        let tsym = match ids.get(target).copied() {
            Some(Sym::F(id, _)) => product.function_symbol(id),
            Some(Sym::D(id, _)) => product.data_symbol(id),
            None => continue,
        };
        let t = product.object.symbol(tsym);
        let (value, size, kind, section) = (t.value, t.size, t.kind, t.section);
        if !matches!(section, object::write::SymbolSection::Section(_)) {
            continue;
        }
        let scope = match t.scope {
            object::SymbolScope::Compilation => object::SymbolScope::Linkage,
            s => s,
        };
        let obj = &mut product.object;
        let id = obj.symbol_id(alias.as_bytes()).unwrap_or_else(|| {
            obj.add_symbol(object::write::Symbol {
                name: alias.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind,
                scope,
                weak: *weak,
                section: object::write::SymbolSection::Undefined,
                flags: object::SymbolFlags::None,
            })
        });
        let s = obj.symbol_mut(id);
        (s.value, s.size, s.kind, s.scope, s.weak, s.section) =
            (value, size, kind, scope, *weak, section);
    }
    if !st.asm.is_empty() || !hot_asm.is_empty() {
        let x86 = isa.triple().architecture == target_lexicon::Architecture::X86_64;
        let asm = format!("{}\n{hot_asm}", st.asm);
        crate::objmerge::assemble_into(&mut product.object, &asm, x86);
    }
    product.emit().unwrap()
}

pub(crate) fn write_const(
    ctx: &Context,
    st: &State<'_>,
    v: Value,
    off: u64,
    bytes: &mut [u8],
    relocs: &mut Vec<(u64, String, i64)>,
) {
    let ty = v.get_type(ctx);
    match st
        .consts
        .get(&v)
        .unwrap_or_else(|| panic!("non-constant value in initializer"))
    {
        ConstVal::Bits(b) => {
            let (s, _) = size_align(ctx, ty);
            let le = b.to_le_bytes();
            bytes[off as usize..(off + s) as usize].copy_from_slice(&le[..s as usize]);
        }
        ConstVal::Zero | ConstVal::Undef => {}
        ConstVal::Bytes(bs) => bytes[off as usize..off as usize + bs.len()].copy_from_slice(bs),
        ConstVal::Agg(elems) => {
            let offs: Vec<u64> = match classify(ctx, ty) {
                TyK::Struct(fs, packed) => struct_offsets(ctx, &fs, packed).0,
                TyK::Array(e, n) => (0..n).map(|i| i * size_align(ctx, e).0).collect(),
                TyK::Vector(e, n) => (0..n as u64).map(|i| i * size_align(ctx, e).0).collect(),
                k => panic!("aggregate constant of {k:?}"),
            };
            for (e, o) in elems.iter().zip(offs) {
                write_const(ctx, st, *e, off + o, bytes, relocs);
            }
        }
        ConstVal::Sym { sym, off: a } => relocs.push((off, sym.clone(), *a)),
    }
}

thread_local! {
    /// Symbol being lowered, for panic messages.
    static CUR_FN: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

struct FnLower<'a, 'b, 'tcx> {
    ctx: &'a Context,
    st: &'a State<'tcx>,
    m: &'a mut ObjectModule,
    ids: &'a FxHashMap<String, Sym>,
    b: FunctionBuilder<'b>,
    vals: FxHashMap<Value, Vals>,
    cconst: FxHashMap<Value, Vals>,
    blocks: FxHashMap<Ptr<BasicBlock>, Block>,
    frefs: FxHashMap<FuncId, FuncRef>,
    gvs: FxHashMap<DataId, GlobalValue>,
    terminated: bool,
    cc: CallConv,
    /// Local functions using our internal ABI (`CallConv::Tail`).
    internal: &'a rustc_data_structures::fx::FxHashSet<String>,
    exn: Option<cranelift_frontend::Variable>,
    /// Promoted allocas (sroa.rs): one variable per scalar leaf.
    vars: FxHashMap<Value, Vec<(cranelift_frontend::Variable, ClType)>>,
    /// Results of `!nonnull` memory loads.
    nonnull: FxHashSet<cranelift_codegen::ir::Value>,
    /// Results of `bool` loads known to be 0 or 1.
    bool01: FxHashSet<cranelift_codegen::ir::Value>,
    /// Inbounds GEP result → base (null only if the base is).
    derived: FxHashMap<cranelift_codegen::ir::Value, cranelift_codegen::ir::Value>,
    /// Entry params pointing at frozen memory → dereferenceable bytes.
    frozen: FxHashMap<cranelift_codegen::ir::Value, u64>,
}

impl<'a, 'b, 'tcx> FnLower<'a, 'b, 'tcx> {
    /// Flags for a plain load/store. Non-volatile accesses to invalid memory are UB
    /// (as in LLVM), so at -O they need no trap record and may be removed or reordered.
    fn plain_mf(&self, op: Ptr<Operation>) -> MemFlagsData {
        if self.st.notrap && !self.st.volatile.contains(&op) {
            MemFlagsData::new().with_notrap()
        } else {
            MemFlagsData::new()
        }
    }

    fn lower(&mut self, f: Ptr<Operation>) {
        let ctx = self.ctx;
        let region = f.deref(ctx).get_region(0);
        let pblocks: Vec<Ptr<BasicBlock>> = region.deref(ctx).iter(ctx).collect();
        for pb in &pblocks {
            let cb = self.b.create_block();
            self.blocks.insert(*pb, cb);
        }
        let entry = self.blocks[&pblocks[0]];
        self.b.append_block_params_for_function_params(entry);
        if crate::pass_enabled("PLIRON_COLD") {
            for pb in cold_blocks(ctx, self.st, &pblocks) {
                self.b.set_cold_block(self.blocks[&pb]);
            }
        }
        for pb in &pblocks[1..] {
            let args: Vec<Value> = pb.deref(ctx).arguments().collect();
            for a in args {
                let cb = self.blocks[pb];
                let vs: Vals = leaves(ctx, a.get_type(ctx))
                    .into_iter()
                    .map(|(_, t)| self.b.append_block_param(cb, t))
                    .collect();
                self.vals.insert(a, vs);
            }
        }
        let pt = self.m.target_config().pointer_type();
        let exn = self.b.declare_var(pt);
        self.exn = Some(exn);
        for op in crate::sroa::allocas(ctx, f) {
            let a = op.deref(ctx).get_result(0);
            if let Some(&ty) = self.st.promoted.get(&a) {
                let vs = leaves(ctx, ty)
                    .into_iter()
                    .map(|(_, t)| (self.b.declare_var(t), t))
                    .collect();
                self.vars.insert(a, vs);
            }
        }
        let params = self.b.block_params(entry).to_vec();
        let mut i = 0;
        let args: Vec<Value> = pblocks[0].deref(ctx).arguments().collect();
        for arg in args {
            let n = leaves(ctx, arg.get_type(ctx)).len();
            if n == 1
                && let Some(&s) = self.st.frozen.get(&arg)
            {
                self.frozen.insert(params[i], s);
            }
            self.vals.insert(arg, params[i..i + n].into());
            i += n;
        }
        let (order, live) = rpo_split(ctx, self.st, &pblocks);
        for (k, pb) in order.into_iter().enumerate() {
            self.b.switch_to_block(self.blocks[&pb]);
            // Unreachable blocks may use values from each other in any order
            // (e.g. after inlining into dead code); they never run.
            if k >= live {
                self.b.ins().trap(TrapCode::unwrap_user(1));
                continue;
            }
            if pb == pblocks[0] {
                let z = self.b.ins().iconst(pt, 0);
                self.b.def_var(exn, z);
            }
            self.cconst.clear();
            self.terminated = false;
            let ops: Vec<Ptr<Operation>> = pb.deref(ctx).iter(ctx).collect();
            for op in ops {
                if self.terminated {
                    let d = self.b.create_block();
                    self.b.switch_to_block(d);
                    self.cconst.clear();
                    self.terminated = false;
                }
                self.lower_op(op);
            }
            if !self.terminated {
                self.b.ins().trap(TrapCode::unwrap_user(1));
            }
        }
        self.b.seal_all_blocks();
    }

    /// Cranelift x64 has no f16/f128 `fneg`; flip the sign bit instead.
    fn fneg(&mut self, x: ir::Value) -> ir::Value {
        let t = self.b.func.dfg.value_type(x);
        let mf = MemFlagsData::new();
        if t == clt::F16 {
            let i = self.b.ins().bitcast(clt::I16, mf, x);
            let r = self.b.ins().bxor_imm_u(i, 0x8000);
            self.b.ins().bitcast(t, mf, r)
        } else if t == clt::F128 {
            let i = self.b.ins().bitcast(clt::I128, mf, x);
            let (lo, hi) = self.b.ins().isplit(i);
            let hi = self.b.ins().bxor_imm_s(hi, i64::MIN);
            let r = self.b.ins().iconcat(lo, hi);
            self.b.ins().bitcast(t, mf, r)
        } else {
            self.b.ins().fneg(x)
        }
    }

    fn ty_leaves(&self, t: TypeHandle) -> Vec<(u64, ClType)> {
        leaves(self.ctx, t)
    }

    fn get(&mut self, v: Value) -> Vals {
        if let Some(x) = self.vals.get(&v) {
            return x.clone();
        }
        if let Some(x) = self.cconst.get(&v) {
            return x.clone();
        }
        let Some(cv) = self.st.consts.get(&v).cloned() else {
            let why = match v.defining_op() {
                Some(op) => {
                    let blk = op.deref(self.ctx).get_parent_block();
                    format!(
                        "defined by `{}` in block {:?} (known block: {})",
                        Operation::get_opid(op, self.ctx),
                        blk,
                        blk.is_some_and(|b| self.blocks.contains_key(&b))
                    )
                }
                None => "a block argument".to_string(),
            };
            let f = CUR_FN.with(|c| c.borrow().clone());
            panic!("value used before its definition was lowered in {f}: {why}");
        };
        let r = self.mat(v.get_type(self.ctx), cv);
        self.cconst.insert(v, r.clone());
        r
    }

    fn block_args(&mut self, vs: &[Value]) -> Vec<ir::BlockArg> {
        vs.iter()
            .flat_map(|v| self.get(*v))
            .map(ir::BlockArg::Value)
            .collect()
    }

    /// One `br_table` over `[min, max]` (holes go to `d`) when the cases are
    /// at least 10% dense (LLVM's `-O` jump-table density). Cranelift's
    /// `Switch` splits on every hole and binary-searches between the runs.
    fn dense_switch(&mut self, x: ir::Value, vals: &[(u128, Block)], d: Block) -> bool {
        static ON: std::sync::LazyLock<bool> =
            std::sync::LazyLock::new(|| crate::pass_enabled("PLIRON_SWITCH_DENSE"));
        let ty = self.b.func.dfg.value_type(x);
        if !*ON || vals.len() < 4 || ty.bits() > 64 {
            return false;
        }
        let lo = vals.iter().map(|v| v.0).min().unwrap();
        let hi = vals.iter().map(|v| v.0).max().unwrap();
        let span = hi - lo + 1;
        if span > 4096 || span > vals.len() as u128 * 10 {
            return false;
        }
        let mut idx = if lo == 0 {
            x
        } else {
            self.b.ins().iadd_imm_s(x, (lo as i64).wrapping_neg())
        };
        if ty.bits() > 32 {
            let ok = self.b.create_block();
            let oob = self
                .b
                .ins()
                .icmp_imm_u(IntCC::UnsignedGreaterThan, idx, (span - 1) as i64);
            self.b.ins().brif(oob, d, &[], ok, &[]);
            self.b.switch_to_block(ok);
            idx = self.b.ins().ireduce(clt::I32, idx);
        } else if ty.bits() < 32 {
            idx = self.b.ins().uextend(clt::I32, idx);
        }
        let mut table = vec![d; span as usize];
        for &(v, b) in vals {
            table[(v - lo) as usize] = b;
        }
        let pool = &mut self.b.func.dfg.value_lists;
        let def = ir::BlockCall::new(d, std::iter::empty(), pool);
        let entries: Vec<ir::BlockCall> = table
            .iter()
            .map(|&b| ir::BlockCall::new(b, std::iter::empty(), pool))
            .collect();
        let jt = self
            .b
            .create_jump_table(ir::JumpTableData::new(def, &entries));
        self.b.ins().br_table(idx, jt);
        true
    }

    fn get1(&mut self, v: Value) -> ir::Value {
        let x = self.get(v);
        assert_eq!(x.len(), 1, "expected a scalar");
        x[0]
    }

    fn const_int(&self, v: Value) -> Option<i128> {
        let ConstVal::Bits(b) = self.st.consts.get(&v)? else {
            return if matches!(self.st.consts.get(&v)?, ConstVal::Zero) {
                Some(0)
            } else {
                None
            };
        };
        let w = match classify(self.ctx, v.get_type(self.ctx)) {
            TyK::Int(w) => w,
            _ => 64,
        };
        let b = *b;
        Some(if w < 128 && (b >> (w - 1)) & 1 == 1 {
            (b | (!0u128 << w)) as i128
        } else {
            b as i128
        })
    }

    fn iconst_any(&mut self, t: ClType, bits: u128) -> ir::Value {
        if t.is_vector() {
            let bytes = bits.to_le_bytes()[..t.bytes() as usize].to_vec();
            let c = self.b.func.dfg.constants.insert(bytes.into());
            return self.b.ins().vconst(t, c);
        }
        if t == clt::I128 {
            let lo = self.b.ins().iconst(clt::I64, bits as u64 as i64);
            let hi = self.b.ins().iconst(clt::I64, (bits >> 64) as u64 as i64);
            self.b.ins().iconcat(lo, hi)
        } else if t.is_int() {
            self.b.ins().iconst(t, mask(bits, t.bits()) as u64 as i64)
        } else if t == clt::F32 {
            self.b.ins().f32const(Ieee32::with_bits(bits as u32))
        } else if t == clt::F64 {
            self.b.ins().f64const(Ieee64::with_bits(bits as u64))
        } else {
            let it = ClType::int(t.bits() as u16).unwrap();
            let i = self.iconst_any(it, bits);
            self.b.ins().bitcast(t, MemFlagsData::new(), i)
        }
    }

    fn bitcast_to(&mut self, t: ClType, x: ir::Value) -> ir::Value {
        let from = self.b.func.dfg.value_type(x);
        let mf = if t.is_vector() || from.is_vector() {
            MemFlagsData::new().with_endianness(ir::Endianness::Little)
        } else {
            MemFlagsData::new()
        };
        self.b.ins().bitcast(t, mf, x)
    }

    /// The native SIMD leaf type of `ty` and how many parts it splits into.
    fn native_leaf(&self, ty: TypeHandle) -> Option<(ClType, usize)> {
        match classify(self.ctx, ty) {
            TyK::Vector(..) => {
                let l = self.ty_leaves(ty);
                let t = l.first()?.1;
                (t.is_vector() && l.iter().all(|x| x.1 == t)).then_some((t, l.len()))
            }
            _ => None,
        }
    }

    /// Build a native vector from per-lane values (a `vconst` when all are constant).
    fn pack_vec(&mut self, t: ClType, elems: &[Value]) -> ir::Value {
        let lb = t.lane_bits() as usize / 8;
        let mut bytes = vec![0u8; t.bytes() as usize];
        let mut all_const = true;
        for (i, e) in elems.iter().enumerate() {
            match self.st.consts.get(e) {
                Some(ConstVal::Bits(b)) => {
                    bytes[i * lb..(i + 1) * lb].copy_from_slice(&b.to_le_bytes()[..lb])
                }
                Some(ConstVal::Zero | ConstVal::Undef) => {}
                _ => all_const = false,
            }
        }
        let c = self.b.func.dfg.constants.insert(bytes.into());
        let mut v = self.b.ins().vconst(t, c);
        if !all_const {
            for (i, e) in elems.iter().enumerate() {
                let x = self.get1(*e);
                v = self.b.ins().insertlane(v, x, i as u8);
            }
        }
        v
    }

    fn mat(&mut self, ty: TypeHandle, cv: ConstVal) -> Vals {
        if let Some((t, k)) = self.native_leaf(ty) {
            match &cv {
                ConstVal::Agg(elems) => {
                    let per = elems.len() / k;
                    return elems.chunks(per).map(|es| self.pack_vec(t, es)).collect();
                }
                ConstVal::Bytes(bs) => {
                    let mut bytes = bs.clone();
                    bytes.resize(t.bytes() as usize * k, 0);
                    return bytes
                        .chunks(t.bytes() as usize)
                        .map(|c| {
                            let c = self.b.func.dfg.constants.insert(c.to_vec().into());
                            self.b.ins().vconst(t, c)
                        })
                        .collect();
                }
                _ => {}
            }
        }
        match cv {
            ConstVal::Bits(bits) => {
                let lv = self.ty_leaves(ty);
                assert_eq!(lv.len(), 1);
                smallvec![self.iconst_any(lv[0].1, bits)]
            }
            ConstVal::Zero | ConstVal::Undef => self
                .ty_leaves(ty)
                .into_iter()
                .map(|(_, t)| self.iconst_any(t, 0))
                .collect(),
            ConstVal::Bytes(bs) => bs
                .iter()
                .map(|b| self.b.ins().iconst(clt::I8, *b as i64))
                .collect(),
            ConstVal::Agg(elems) => elems.iter().flat_map(|e| self.get(*e)).collect(),
            ConstVal::Sym { sym, off } => {
                let base = self.sym_addr(&sym);
                smallvec![if off != 0 {
                    self.b.ins().iadd_imm_s(base, off)
                } else {
                    base
                }]
            }
        }
    }

    fn fref(&mut self, id: FuncId) -> FuncRef {
        if let Some(f) = self.frefs.get(&id) {
            return *f;
        }
        let f = self.m.declare_func_in_func(id, self.b.func);
        self.frefs.insert(id, f);
        f
    }

    fn sym_addr(&mut self, sym: &str) -> ir::Value {
        match self
            .ids
            .get(sym)
            .copied()
            .unwrap_or_else(|| panic!("unknown symbol {sym}"))
        {
            Sym::F(id, _) => {
                let fr = self.fref(id);
                self.b.ins().func_addr(clt::I64, fr)
            }
            Sym::D(id, tls) => {
                let gv = *self
                    .gvs
                    .entry(id)
                    .or_insert_with(|| self.m.declare_data_in_func(id, self.b.func));
                if tls {
                    self.b.ins().tls_value(clt::I64, gv)
                } else {
                    self.b.ins().symbol_value(clt::I64, gv)
                }
            }
        }
    }

    fn libcall(
        &mut self,
        name: &str,
        params: &[ClType],
        rets: &[ClType],
        args: &[ir::Value],
    ) -> Vec<ir::Value> {
        let mut sig = Signature::new(self.cc);
        sig.params.extend(params.iter().map(|t| AbiParam::new(*t)));
        sig.returns.extend(rets.iter().map(|t| AbiParam::new(*t)));
        let id = self
            .m
            .declare_function(name, Linkage::Import, &sig)
            .unwrap();
        let fr = self.fref(id);
        let c = self.b.ins().call(fr, args);
        self.b.inst_results(c).to_vec()
    }

    fn set(&mut self, op: Ptr<Operation>, vals: Vals) {
        let r = op.deref(self.ctx).get_result(0);
        self.vals.insert(r, vals);
    }

    fn set1(&mut self, op: Ptr<Operation>, v: ir::Value) {
        self.set(op, smallvec![v]);
    }

    fn res_ty(&self, op: Ptr<Operation>) -> TypeHandle {
        op.deref(self.ctx).get_result(0).get_type(self.ctx)
    }

    fn int_width(&self, t: TypeHandle) -> u32 {
        match classify(self.ctx, t) {
            TyK::Int(w) => w,
            TyK::Ptr => 64,
            k => panic!("int width of {k:?}"),
        }
    }

    /// Resize an integer to `t` (zero-extending).
    fn resize(&mut self, x: ir::Value, t: ClType, signed: bool) -> ir::Value {
        let ft = self.b.func.dfg.value_type(x);
        if ft == t {
            x
        } else if ft.bits() > t.bits() {
            self.b.ins().ireduce(t, x)
        } else if signed {
            self.b.ins().sextend(t, x)
        } else {
            self.b.ins().uextend(t, x)
        }
    }

    fn lower_op(&mut self, op: Ptr<Operation>) {
        let ctx = self.ctx;
        let id = Operation::get_opid(op, ctx);
        let (opnds, succs): (Vec<Value>, Vec<Ptr<BasicBlock>>) = {
            let o = op.deref(ctx);
            (o.operands().collect(), o.successors().collect())
        };
        macro_rules! is {
            ($t:ty) => {
                id == <$t>::get_opid_static()
            };
        }
        macro_rules! binop {
            ($m:ident) => {{
                let a = self.get(opnds[0]);
                let b = self.get(opnds[1]);
                let r: Vals = a
                    .iter()
                    .zip(b.iter())
                    .map(|(x, y)| self.b.ins().$m(*x, *y))
                    .collect();
                self.set(op, r);
            }};
        }
        macro_rules! divop {
            ($m:ident, $lib:expr) => {{
                let a = self.get(opnds[0]);
                let b = self.get(opnds[1]);
                let mut r = Vals::new();
                for (x, y) in a.iter().zip(b.iter()) {
                    if self.b.func.dfg.value_type(*x) == clt::I128 {
                        r.push(
                            self.libcall($lib, &[clt::I128, clt::I128], &[clt::I128], &[*x, *y])[0],
                        );
                    } else {
                        r.push(self.b.ins().$m(*x, *y));
                    }
                }
                self.set(op, r);
            }};
        }

        if is!(ReturnOp) {
            let vs = match opnds.first() {
                Some(v) => self.get(*v),
                None => Vals::new(),
            };
            self.b.ins().return_(&vs);
            self.terminated = true;
        } else if is!(UnreachableOp) {
            self.b.ins().trap(TrapCode::unwrap_user(2));
            self.terminated = true;
        } else if is!(BrOp) {
            let d = self.blocks[&succs[0]];
            let a = self.block_args(&opnds);
            self.b.ins().jump(d, &a);
            self.terminated = true;
        } else if is!(SwitchOp) {
            let sw = Operation::get_op::<SwitchOp>(op, ctx).unwrap();
            let x = self.get1(opnds[0]);
            let cases = sw.cases(ctx);
            assert!(
                sw.default_dest_operands(ctx).is_empty()
                    && cases.iter().all(|c| c.dest_opds.is_empty()),
                "switch with block arguments"
            );
            let d = self.blocks[&sw.default_dest(ctx)];
            let vals: Vec<(u128, Block)> = cases
                .iter()
                .map(|c| (c.value.value().to_u128(), self.blocks[&c.dest]))
                .collect();
            if !self.dense_switch(x, &vals, d) {
                let mut s = cranelift_frontend::Switch::new();
                for &(v, b) in &vals {
                    s.set_entry(v, b);
                }
                s.emit(&mut self.b, x, d);
            }
            self.terminated = true;
        } else if is!(CondBrOp) {
            let c = self.get1(opnds[0]);
            let (t, e) = (self.blocks[&succs[0]], self.blocks[&succs[1]]);
            let cb = Operation::get_op::<CondBrOp>(op, ctx).unwrap();
            let ta = self.block_args(&cb.get_true_dest_operands(ctx));
            let ea = self.block_args(&cb.get_false_dest_operands(ctx));
            self.b.ins().brif(c, t, &ta, e, &ea);
            self.terminated = true;
        } else if is!(ICmpOp) {
            use ICmpPredicateAttr as P;
            let pred = Operation::get_op::<ICmpOp>(op, ctx).unwrap().predicate(ctx);
            let cc = match pred {
                P::EQ => IntCC::Equal,
                P::NE => IntCC::NotEqual,
                P::SLT => IntCC::SignedLessThan,
                P::SLE => IntCC::SignedLessThanOrEqual,
                P::SGT => IntCC::SignedGreaterThan,
                P::SGE => IntCC::SignedGreaterThanOrEqual,
                P::ULT => IntCC::UnsignedLessThan,
                P::ULE => IntCC::UnsignedLessThanOrEqual,
                P::UGT => IntCC::UnsignedGreaterThan,
                P::UGE => IntCC::UnsignedGreaterThanOrEqual,
            };
            let a = self.get(opnds[0]);
            let b = self.get(opnds[1]);
            let r: Vals = a
                .iter()
                .zip(b.iter())
                .map(|(x, y)| self.b.ins().icmp(cc, *x, *y))
                .collect();
            self.set(op, r);
        } else if is!(FCmpOp) {
            use FCmpPredicateAttr as P;
            let pred = Operation::get_op::<FCmpOp>(op, ctx).unwrap().predicate(ctx);
            let a = self.get1(opnds[0]);
            let b = self.get1(opnds[1]);
            let cc = match pred {
                P::False | P::True => {
                    let v = self.b.ins().iconst(clt::I8, matches!(pred, P::True) as i64);
                    return self.set1(op, v);
                }
                P::OEQ => FloatCC::Equal,
                P::OGT => FloatCC::GreaterThan,
                P::OGE => FloatCC::GreaterThanOrEqual,
                P::OLT => FloatCC::LessThan,
                P::OLE => FloatCC::LessThanOrEqual,
                P::ONE => FloatCC::OrderedNotEqual,
                P::ORD => FloatCC::Ordered,
                P::UNO => FloatCC::Unordered,
                P::UEQ => FloatCC::UnorderedOrEqual,
                P::UGT => FloatCC::UnorderedOrGreaterThan,
                P::UGE => FloatCC::UnorderedOrGreaterThanOrEqual,
                P::ULT => FloatCC::UnorderedOrLessThan,
                P::ULE => FloatCC::UnorderedOrLessThanOrEqual,
                P::UNE => FloatCC::NotEqual,
            };
            let r = self.b.ins().fcmp(cc, a, b);
            self.set1(op, r);
        } else if is!(AddOp) {
            binop!(iadd)
        } else if is!(SubOp) {
            binop!(isub)
        } else if is!(MulOp) {
            binop!(imul)
        } else if is!(AndOp) {
            binop!(band)
        } else if is!(OrOp) {
            binop!(bor)
        } else if is!(XorOp) {
            binop!(bxor)
        } else if is!(ShlOp) {
            binop!(ishl)
        } else if is!(LShrOp) {
            binop!(ushr)
        } else if is!(AShrOp) {
            binop!(sshr)
        } else if is!(UDivOp) {
            divop!(udiv, "__udivti3")
        } else if is!(SDivOp) {
            divop!(sdiv, "__divti3")
        } else if is!(URemOp) {
            divop!(urem, "__umodti3")
        } else if is!(SRemOp) {
            divop!(srem, "__modti3")
        } else if is!(FAddOp) {
            binop!(fadd)
        } else if is!(FSubOp) {
            binop!(fsub)
        } else if is!(FMulOp) {
            binop!(fmul)
        } else if is!(FDivOp) {
            binop!(fdiv)
        } else if is!(FRemOp) {
            let a = self.get1(opnds[0]);
            let b = self.get1(opnds[1]);
            let t = self.b.func.dfg.value_type(a);
            let name = if t == clt::F32 { "fmodf" } else { "fmod" };
            let r = self.libcall(name, &[t, t], &[t], &[a, b])[0];
            self.set1(op, r);
        } else if is!(FNegOp) {
            let a = self.get(opnds[0]);
            let r = a.iter().map(|x| self.fneg(*x)).collect();
            self.set(op, r);
        } else if is!(TruncOp) || is!(ZExtOp) || is!(SExtOp) || is!(PtrToIntOp) || is!(IntToPtrOp) {
            let src_w = self.int_width(opnds[0].get_type(ctx));
            let dst_w = self.int_width(self.res_ty(op));
            let xs = self.get(opnds[0]);
            let dts = self.ty_leaves(self.res_ty(op));
            let mut r = Vals::new();
            for (x, (_, t)) in xs.into_iter().zip(dts) {
                let v = if is!(TruncOp) && dst_w == 1 && self.bool01.contains(&x) {
                    self.resize(x, clt::I8, false)
                } else if is!(TruncOp) && dst_w == 1 {
                    let x = self.resize(x, clt::I8, false);
                    self.b.ins().band_imm_u(x, 1)
                } else if is!(SExtOp) && src_w == 1 {
                    let x = self.b.ins().ineg(x);
                    self.resize(x, t, true)
                } else {
                    self.resize(x, t, is!(SExtOp))
                };
                r.push(v);
            }
            self.set(op, r);
        } else if is!(FPTruncOp) || is!(FPExtOp) {
            let x = self.get1(opnds[0]);
            let t = self.ty_leaves(self.res_ty(op))[0].1;
            let from = self.b.func.dfg.value_type(x);
            let r = if [from, t].iter().any(|t| *t == clt::F16 || *t == clt::F128) {
                // Cranelift x64 can't convert f16/f128; use compiler-builtins.
                let l = |t: ClType| match t {
                    clt::F16 => "hf",
                    clt::F32 => "sf",
                    clt::F64 => "df",
                    _ => "tf",
                };
                let f = format!(
                    "__{}{}{}2",
                    if is!(FPTruncOp) { "trunc" } else { "extend" },
                    l(from),
                    l(t)
                );
                self.libcall(&f, &[from], &[t], &[x])[0]
            } else if is!(FPTruncOp) {
                self.b.ins().fdemote(t, x)
            } else {
                self.b.ins().fpromote(t, x)
            };
            self.set1(op, r);
        } else if is!(FPToUIOp) || is!(FPToSIOp) {
            let x = self.get1(opnds[0]);
            let t = self.ty_leaves(self.res_ty(op))[0].1;
            let r = self.fcvt_sat(is!(FPToSIOp), t, x);
            self.set1(op, r);
        } else if is!(UIToFPOp) || is!(SIToFPOp) {
            let x = self.get1(opnds[0]);
            let t = self.ty_leaves(self.res_ty(op))[0].1;
            let signed = is!(SIToFPOp);
            let ft = self.b.func.dfg.value_type(x);
            let r = if ft == clt::I128 {
                let name = match (signed, t == clt::F32) {
                    (true, true) => "__floattisf",
                    (true, false) => "__floattidf",
                    (false, true) => "__floatuntisf",
                    (false, false) => "__floatuntidf",
                };
                self.libcall(name, &[clt::I128], &[t], &[x])[0]
            } else {
                let x = if ft.bits() < 32 {
                    self.resize(x, clt::I32, signed)
                } else {
                    x
                };
                if signed {
                    self.b.ins().fcvt_from_sint(t, x)
                } else {
                    self.b.ins().fcvt_from_uint(t, x)
                }
            };
            self.set1(op, r);
        } else if is!(BitcastOp) || is!(AddrSpaceCastOp) || is!(FreezeOp) {
            let xs = self.get(opnds[0]);
            let dts = self.ty_leaves(self.res_ty(op));
            let sts: Vec<ClType> = xs.iter().map(|x| self.b.func.dfg.value_type(*x)).collect();
            let r: Vals = if dts.iter().map(|d| d.1).eq(sts.iter().copied()) {
                xs
            } else if xs.len() == dts.len()
                && sts.iter().zip(&dts).all(|(s, d)| s.bits() == d.1.bits())
            {
                xs.iter()
                    .zip(&dts)
                    .map(|(x, d)| self.bitcast_to(d.1, *x))
                    .collect()
            } else {
                let (sz, _) = size_align(ctx, self.res_ty(op));
                let slot = self.slot(sz.max(16), 16);
                let sl = self.ty_leaves(opnds[0].get_type(ctx));
                for (x, (o, _)) in xs.iter().zip(sl) {
                    self.b
                        .ins()
                        .store(MemFlagsData::trusted(), *x, slot, o as i32);
                }
                dts.iter()
                    .map(|(o, t)| {
                        self.b
                            .ins()
                            .load(*t, MemFlagsData::trusted(), slot, *o as i32)
                    })
                    .collect()
            };
            self.set(op, r);
        } else if is!(AllocaOp) && self.vars.contains_key(&op.deref(ctx).get_result(0)) {
        } else if is!(LoadOp) && self.vars.contains_key(&opnds[0]) {
            let lv = self.ty_leaves(self.res_ty(op));
            let vars = self.vars[&opnds[0]].clone();
            let r = vars
                .into_iter()
                .zip(lv)
                .map(|((var, vt), (_, t))| {
                    let x = self.b.use_var(var);
                    if vt == t { x } else { self.bitcast_to(t, x) }
                })
                .collect();
            self.set(op, r);
        } else if is!(StoreOp) && self.vars.contains_key(&opnds[1]) {
            let vs = self.get(opnds[0]);
            let vars = self.vars[&opnds[1]].clone();
            for (x, (var, vt)) in vs.into_iter().zip(vars) {
                let x = if self.b.func.dfg.value_type(x) == vt {
                    x
                } else {
                    self.bitcast_to(vt, x)
                };
                self.b.def_var(var, x);
            }
        } else if is!(AllocaOp) {
            let res = op.deref(ctx).get_result(0);
            let (size, align) = self.st.allocas[&res];
            let p = self.slot(size, align);
            self.set1(op, p);
        } else if is!(LoadOp) || is!(AtomicLoadOp) {
            let p = self.get1(opnds[0]);
            let atomic = is!(AtomicLoadOp);
            let mf = self.plain_mf(op);
            let r: Vals = self
                .ty_leaves(self.res_ty(op))
                .into_iter()
                .map(|(o, t)| {
                    if atomic {
                        self.b.ins().atomic_load(t, MemFlagsData::trusted(), p)
                    } else {
                        self.b.ins().load(t, mf, p, o as i32)
                    }
                })
                .collect();
            if r.len() == 1 && self.st.nonnull.contains(&op) {
                self.nonnull.insert(r[0]);
            }
            if r.len() == 1 && self.st.bool01.contains(&op) {
                self.bool01.insert(r[0]);
            }
            self.set(op, r);
        } else if is!(StoreOp) || is!(AtomicStoreOp) {
            let vs = self.get(opnds[0]);
            let p = self.get1(opnds[1]);
            let lv = self.ty_leaves(opnds[0].get_type(ctx));
            let mf = self.plain_mf(op);
            for (v, (o, _)) in vs.into_iter().zip(lv) {
                if is!(AtomicStoreOp) {
                    self.b.ins().atomic_store(MemFlagsData::trusted(), v, p);
                } else {
                    self.b.ins().store(mf, v, p, o as i32);
                }
            }
        } else if is!(AtomicRmwOp) {
            use cranelift_codegen::ir::AtomicRmwOp as R;
            let p = self.get1(opnds[0]);
            let v = self.get1(opnds[1]);
            let k = match self.st.rmw[&op] {
                AtomicRmwBinOp::AtomicXchg => R::Xchg,
                AtomicRmwBinOp::AtomicAdd => R::Add,
                AtomicRmwBinOp::AtomicSub => R::Sub,
                AtomicRmwBinOp::AtomicAnd => R::And,
                AtomicRmwBinOp::AtomicNand => R::Nand,
                AtomicRmwBinOp::AtomicOr => R::Or,
                AtomicRmwBinOp::AtomicXor => R::Xor,
                AtomicRmwBinOp::AtomicMax => R::Smax,
                AtomicRmwBinOp::AtomicMin => R::Smin,
                AtomicRmwBinOp::AtomicUMax => R::Umax,
                AtomicRmwBinOp::AtomicUMin => R::Umin,
            };
            let t = self.b.func.dfg.value_type(v);
            let r = self.b.ins().atomic_rmw(t, MemFlagsData::trusted(), k, p, v);
            self.set1(op, r);
        } else if is!(AtomicCmpxchgOp) {
            let p = self.get1(opnds[0]);
            let c = self.get1(opnds[1]);
            let n = self.get1(opnds[2]);
            let old = self.b.ins().atomic_cas(MemFlagsData::trusted(), p, c, n);
            let ok = self.b.ins().icmp(IntCC::Equal, old, c);
            self.set(op, smallvec![old, ok]);
        } else if is!(FenceOp) {
            self.b.ins().fence();
        } else if is!(GetElementPtrOp) {
            self.lower_gep(op, opnds[0]);
        } else if is!(CallOp) {
            self.lower_call(op);
        } else if is!(CallIntrinsicOp) {
            self.lower_intrinsic(op, &opnds);
        } else if is!(SelectOp) {
            let c = self.get(opnds[0]);
            let a = self.get(opnds[1]);
            let b = self.get(opnds[2]);
            let r = a
                .iter()
                .zip(b.iter())
                .enumerate()
                .map(|(i, (x, y))| {
                    let ci = if c.len() == 1 { c[0] } else { c[i] };
                    self.b.ins().select(ci, *x, *y)
                })
                .collect();
            self.set(op, r);
        } else if is!(ExtractValueOp) {
            let idx = Operation::get_op::<ExtractValueOp>(op, ctx)
                .unwrap()
                .indices(ctx);
            let (s, n, _) = leaf_range(ctx, opnds[0].get_type(ctx), &idx);
            let a = self.get(opnds[0]);
            self.set(op, a[s..s + n].into());
        } else if is!(InsertValueOp) {
            let idx = Operation::get_op::<InsertValueOp>(op, ctx)
                .unwrap()
                .indices(ctx);
            let (s, n, _) = leaf_range(ctx, opnds[0].get_type(ctx), &idx);
            let mut a = self.get(opnds[0]);
            let v = self.get(opnds[1]);
            a.drain(s..s + n);
            a.insert_many(s, v);
            self.set(op, a);
        } else if is!(ExtractElementOp) {
            let a = self.get(opnds[0]);
            let vt = self.b.func.dfg.value_type(a[0]);
            let r = if vt.is_vector() {
                match self.const_int(opnds[1]) {
                    Some(i) => {
                        let n = vt.lane_count() as i128;
                        self.b.ins().extractlane(a[(i / n) as usize], (i % n) as u8)
                    }
                    None => {
                        let (p, es) = self.spill_vec(opnds[0], &a);
                        let addr = self.dyn_index(p, opnds[1], es);
                        self.b
                            .ins()
                            .load(vt.lane_type(), MemFlagsData::trusted(), addr, 0)
                    }
                }
            } else {
                match self.const_int(opnds[1]) {
                    Some(i) => a[i as usize],
                    None => {
                        let (p, es) = self.spill_vec(opnds[0], &a);
                        let addr = self.dyn_index(p, opnds[1], es);
                        let t = self.b.func.dfg.value_type(a[0]);
                        self.b.ins().load(t, MemFlagsData::trusted(), addr, 0)
                    }
                }
            };
            self.set1(op, r);
        } else if is!(InsertElementOp) {
            let mut a = self.get(opnds[0]);
            let e = self.get1(opnds[1]);
            let vt = self.b.func.dfg.value_type(a[0]);
            if vt.is_vector() {
                match self.const_int(opnds[2]) {
                    Some(i) => {
                        let n = vt.lane_count() as i128;
                        let j = (i / n) as usize;
                        a[j] = self.b.ins().insertlane(a[j], e, (i % n) as u8);
                    }
                    None => {
                        let (p, es) = self.spill_vec(opnds[0], &a);
                        let addr = self.dyn_index(p, opnds[2], es);
                        self.b.ins().store(MemFlagsData::trusted(), e, addr, 0);
                        for (j, x) in a.iter_mut().enumerate() {
                            let o = (j as u32 * vt.bytes()) as i32;
                            *x = self.b.ins().load(vt, MemFlagsData::trusted(), p, o);
                        }
                    }
                }
                return self.set(op, a);
            }
            match self.const_int(opnds[2]) {
                Some(i) => a[i as usize] = e,
                None => {
                    let (p, es) = self.spill_vec(opnds[0], &a);
                    let addr = self.dyn_index(p, opnds[2], es);
                    self.b.ins().store(MemFlagsData::trusted(), e, addr, 0);
                    let t = self.b.func.dfg.value_type(a[0]);
                    for (i, x) in a.iter_mut().enumerate() {
                        *x = self.b.ins().load(
                            t,
                            MemFlagsData::trusted(),
                            p,
                            (i as u64 * es) as i32,
                        );
                    }
                }
            }
            self.set(op, a);
        } else if is!(UndefOp) || is!(PoisonOp) || is!(ZeroOp) {
            let r = self
                .ty_leaves(self.res_ty(op))
                .into_iter()
                .map(|(_, t)| self.iconst_any(t, 0))
                .collect();
            self.set(op, r);
        } else {
            panic!("pliron->cranelift: unsupported op {id}");
        }
    }

    fn slot(&mut self, size: u64, align: u64) -> ir::Value {
        // Cranelift frames are only 16-byte aligned; over-allocate and round up.
        let align = align.max(1);
        let over = if align > 16 { align } else { 0 };
        let ss = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (size + over) as u32,
            align.min(16).trailing_zeros() as u8,
        ));
        let p = self.b.ins().stack_addr(clt::I64, ss, 0);
        if over == 0 {
            return p;
        }
        let p = self.b.ins().iadd_imm(p, align as i64 - 1);
        self.b.ins().band_imm(p, -(align as i64))
    }

    /// A `pliron.v*` op on vectors split into several native parts.
    fn split_vec_op(&mut self, op: Ptr<Operation>, name: &str, opnds: &[Value], k: usize) {
        let ops: Vec<Vals> = opnds.iter().map(|v| self.get(*v)).collect();
        let part = |j: usize| -> Vec<ir::Value> {
            ops.iter()
                .map(|x| if x.len() == 1 { x[0] } else { x[j] })
                .collect()
        };
        if name == "pliron.vhigh_bits" {
            let rt = self.ty_leaves(self.res_ty(op))[0].1;
            let mut acc: Option<ir::Value> = None;
            let mut sh = 0;
            for j in 0..k {
                let x = part(j)[0];
                let x = self.sign_source(x);
                let m = self.b.ins().vhigh_bits(clt::I32, x);
                let m = if sh > 0 {
                    let s = self.b.ins().iconst(clt::I32, sh);
                    self.b.ins().ishl(m, s)
                } else {
                    m
                };
                sh += self.b.func.dfg.value_type(x).lane_count() as i64;
                acc = Some(acc.map_or(m, |a| self.b.ins().bor(a, m)));
            }
            let r = self.resize(acc.unwrap(), rt, false);
            return self.set1(op, r);
        }
        let rts = self.ty_leaves(self.res_ty(op));
        let r: Vals = (0..k)
            .map(|j| self.vec_op(name, &part(j), rts[j].1))
            .collect();
        self.set(op, r);
    }

    /// `vhigh_bits(icmp slt y, 0)` reads the same sign bits as `vhigh_bits(y)`.
    fn sign_source(&self, x: ir::Value) -> ir::Value {
        let dfg = &self.b.func.dfg;
        let Some(d) = dfg.value_def(x).inst() else {
            return x;
        };
        let ir::InstructionData::IntCompare { cond, args, .. } = dfg.insts[d] else {
            return x;
        };
        if cond != ir::condcodes::IntCC::SignedLessThan
            || dfg.value_type(args[0]).lane_bits() != dfg.value_type(x).lane_bits()
        {
            return x;
        }
        let zero = dfg
            .value_def(args[1])
            .inst()
            .is_some_and(|z| match dfg.insts[z] {
                ir::InstructionData::UnaryConst {
                    opcode: ir::Opcode::Vconst,
                    constant_handle,
                } => dfg
                    .constants
                    .get(constant_handle)
                    .as_slice()
                    .iter()
                    .all(|&b| b == 0),
                _ => false,
            });
        if zero { args[0] } else { x }
    }

    /// One native `pliron.v*` op; `rt` is the (part) result type.
    fn vec_op(&mut self, name: &str, a: &[ir::Value], rt: ClType) -> ir::Value {
        match name {
            "pliron.vsplat" => {
                let t = rt;
                self.b.ins().splat(t, a[0])
            }
            "pliron.vhigh_bits" => {
                let t = rt;
                let x = self.sign_source(a[0]);
                let m = self.b.ins().vhigh_bits(clt::I32, x);
                self.resize(m, t, false)
            }
            "pliron.vbitselect" => {
                let t = self.b.func.dfg.value_type(a[1]);
                let m = if self.b.func.dfg.value_type(a[0]) == t {
                    a[0]
                } else {
                    self.bitcast_to(t, a[0])
                };
                self.b.ins().bitselect(m, a[1], a[2])
            }
            n if n.starts_with("pliron.vcmp.") => {
                let (p, k) = n["pliron.vcmp.".len()..].split_once('.').unwrap();
                if k == "f" {
                    let cc = match p {
                        "eq" => FloatCC::Equal,
                        "ne" => FloatCC::NotEqual,
                        "lt" => FloatCC::LessThan,
                        "le" => FloatCC::LessThanOrEqual,
                        "gt" => FloatCC::GreaterThan,
                        _ => FloatCC::GreaterThanOrEqual,
                    };
                    let m = self.b.ins().fcmp(cc, a[0], a[1]);
                    let t = rt;
                    if self.b.func.dfg.value_type(m) == t {
                        m
                    } else {
                        self.bitcast_to(t, m)
                    }
                } else {
                    let s = k == "s";
                    let cc = match p {
                        "eq" => IntCC::Equal,
                        "ne" => IntCC::NotEqual,
                        "lt" if s => IntCC::SignedLessThan,
                        "le" if s => IntCC::SignedLessThanOrEqual,
                        "gt" if s => IntCC::SignedGreaterThan,
                        "ge" if s => IntCC::SignedGreaterThanOrEqual,
                        "lt" => IntCC::UnsignedLessThan,
                        "le" => IntCC::UnsignedLessThanOrEqual,
                        "gt" => IntCC::UnsignedGreaterThan,
                        _ => IntCC::UnsignedGreaterThanOrEqual,
                    };
                    self.b.ins().icmp(cc, a[0], a[1])
                }
            }
            n => unreachable!("{n}"),
        }
    }

    fn spill_vec(&mut self, v: Value, a: &Vals) -> (ir::Value, u64) {
        let (sz, al) = size_align(self.ctx, v.get_type(self.ctx));
        let p = self.slot(sz, al);
        let es = match classify(self.ctx, v.get_type(self.ctx)) {
            TyK::Vector(e, _) => size_align(self.ctx, e).0,
            _ => sz / a.len() as u64,
        };
        for (x, (o, _)) in a.iter().zip(self.ty_leaves(v.get_type(self.ctx))) {
            self.b.ins().store(MemFlagsData::trusted(), *x, p, o as i32);
        }
        (p, es)
    }

    fn dyn_index(&mut self, base: ir::Value, idx: Value, scale: u64) -> ir::Value {
        let x = self.get1(idx);
        let x = self.resize(x, clt::I64, true);
        let off = self.b.ins().imul_imm_s(x, scale as i64);
        self.b.ins().iadd(base, off)
    }

    fn fcvt_sat(&mut self, signed: bool, t: ClType, x: ir::Value) -> ir::Value {
        if t == clt::I128 {
            // Cranelift x64 can't convert floats to i128; compiler-builtins' helpers saturate.
            let ft = self.b.func.dfg.value_type(x);
            let name = match (signed, ft == clt::F32) {
                (true, true) => "__fixsfti",
                (true, false) => "__fixdfti",
                (false, true) => "__fixunssfti",
                (false, false) => "__fixunsdfti",
            };
            return self.libcall(name, &[ft], &[clt::I128], &[x])[0];
        }
        let it = if t.bits() < 32 { clt::I32 } else { t };
        let r = if signed {
            self.b.ins().fcvt_to_sint_sat(it, x)
        } else {
            self.b.ins().fcvt_to_uint_sat(it, x)
        };
        if it == t {
            return r;
        }
        // Saturate to the narrow range before reducing.
        let (lo, hi) = if signed {
            (-(1i64 << (t.bits() - 1)), (1i64 << (t.bits() - 1)) - 1)
        } else {
            (0, (1i64 << t.bits()) - 1)
        };
        let lo = self.b.ins().iconst(clt::I32, lo);
        let hi = self.b.ins().iconst(clt::I32, hi);
        let r = if signed {
            self.b.ins().smax(r, lo)
        } else {
            self.b.ins().umax(r, lo)
        };
        let r = if signed {
            self.b.ins().smin(r, hi)
        } else {
            self.b.ins().umin(r, hi)
        };
        self.b.ins().ireduce(t, r)
    }

    fn lower_gep(&mut self, op: Ptr<Operation>, base: Value) {
        let ctx = self.ctx;
        let gep = Operation::get_op::<GetElementPtrOp>(op, ctx).unwrap();
        let idxs = gep.indices(ctx);
        let mut cur = gep.src_elem_type(ctx);
        let mut addr = self.get1(base);
        let base_v = addr;
        for (k, idx) in idxs.iter().enumerate() {
            let c = match idx {
                GepIndex::Constant(c) => Some(*c as i128),
                GepIndex::Value(v) => self.const_int(*v),
            };
            let scale;
            if k == 0 {
                scale = size_align(ctx, cur).0;
            } else {
                match classify(ctx, cur) {
                    TyK::Struct(fs, packed) => {
                        let i = c.expect("struct GEP index must be constant") as usize;
                        let offs = struct_offsets(ctx, &fs, packed).0;
                        addr = self.b.ins().iadd_imm_s(addr, offs[i] as i64);
                        cur = fs[i];
                        continue;
                    }
                    TyK::Array(e, _) | TyK::Vector(e, _) => {
                        cur = e;
                        scale = size_align(ctx, e).0;
                    }
                    k => panic!("GEP into {k:?}"),
                }
            }
            match (c, idx) {
                (Some(c), _) => {
                    if c != 0 {
                        addr = self
                            .b
                            .ins()
                            .iadd_imm_s(addr, (c as i64).wrapping_mul(scale as i64));
                    }
                }
                (None, GepIndex::Value(v)) => addr = self.dyn_index(addr, *v, scale),
                _ => unreachable!(),
            }
        }
        if addr != base_v && self.st.inbounds.contains(&op) {
            self.derived.insert(addr, base_v);
        }
        self.set1(op, addr);
    }

    /// An `invoke`: `try_call` whose exception edge stores the exception
    /// pointer in `self.exn` and jumps to the landing block.
    fn try_call(
        &mut self,
        target: Result<FuncRef, (ir::Value, ir::SigRef)>,
        args: &[ir::Value],
        catch: Ptr<BasicBlock>,
        is_catch: bool,
    ) -> Vals {
        use cranelift_codegen::ir::{
            BlockArg, ExceptionTableData, ExceptionTableItem, ExceptionTag,
        };
        let sr = match target {
            Ok(fr) => self.b.func.dfg.ext_funcs[fr].signature,
            Err((_, sr)) => sr,
        };
        let rets: Vec<ClType> = self.b.func.dfg.signatures[sr]
            .returns
            .iter()
            .map(|r| r.value_type)
            .collect();
        let normal = self.b.create_block();
        let nargs: Vec<BlockArg> = (0..rets.len())
            .map(|i| BlockArg::TryCallRet(i as u32))
            .collect();
        let ncall = self.b.func.dfg.block_call(normal, &nargs);
        let pre = self.b.create_block();
        let pcall = self.b.func.dfg.block_call(pre, &[BlockArg::TryCallExn(0)]);
        let tag = if is_catch {
            crate::eh::EXCEPTION_HANDLER_CATCH
        } else {
            crate::eh::EXCEPTION_HANDLER_CLEANUP
        };
        let et = self
            .b
            .func
            .dfg
            .exception_tables
            .push(ExceptionTableData::new(
                sr,
                ncall,
                [ExceptionTableItem::Tag(
                    ExceptionTag::with_number(tag).unwrap(),
                    pcall,
                )],
            ));
        match target {
            Ok(fr) => self.b.ins().try_call(fr, args, et),
            Err((addr, _)) => self.b.ins().try_call_indirect(addr, args, et),
        };
        self.b.switch_to_block(pre);
        self.b.set_cold_block(pre);
        let pt = self.m.target_config().pointer_type();
        let p = self.b.append_block_param(pre, pt);
        self.b.def_var(self.exn.unwrap(), p);
        let lp = self.blocks[&catch];
        self.b.ins().jump(lp, &[]);
        self.b.switch_to_block(normal);
        self.cconst.clear();
        rets.into_iter()
            .map(|t| self.b.append_block_param(normal, t))
            .collect()
    }

    fn lower_call(&mut self, op: Ptr<Operation>) {
        let ctx = self.ctx;
        let call = Operation::get_op::<CallOp>(op, ctx).unwrap();
        let info = &self.st.calls[&op];
        let fn_ty = info.fn_ty;
        let cc = match call.callee(ctx) {
            CallOpCallable::Direct(id)
                if self
                    .internal
                    .contains(&self.st.ident_to_sym[&id.to_string()]) =>
            {
                CallConv::Tail
            }
            _ => self.cc,
        };
        let mut sig = make_sig(ctx, fn_ty, &info.exts, cc);
        let args: Vec<Value> = call.args(ctx);
        let mut cargs = Vec::new();
        for a in args {
            cargs.extend(self.get(a));
        }
        // C variadic: fn_ty only has the fixed params; on SysV the rest are
        // passed like ordinary arguments of their own types.
        let var_arg = matches!(classify(ctx, fn_ty), TyK::Func(_, _, true));
        if var_arg {
            for &v in &cargs[sig.params.len()..] {
                sig.params
                    .push(AbiParam::new(self.b.func.dfg.value_type(v)));
            }
        }
        let target = match call.callee(ctx) {
            CallOpCallable::Direct(ident) => {
                let sym = &self.st.ident_to_sym[&ident.to_string()];
                match self.ids.get(sym).copied() {
                    Some(Sym::F(fid, declty)) if declty == fn_ty && !var_arg => Ok(self.fref(fid)),
                    _ => {
                        let addr = self.sym_addr(&sym.clone());
                        Err((addr, self.b.import_signature(sig)))
                    }
                }
            }
            CallOpCallable::Indirect(v) => {
                let addr = self.get1(v);
                Err((addr, self.b.import_signature(sig)))
            }
        };
        let rs: Vals = if let Some(&(catch, is_catch)) = self.st.invokes.get(&op) {
            self.try_call(target, &cargs, catch, is_catch)
        } else {
            let inst = match target {
                Ok(fr) => self.b.ins().call(fr, &cargs),
                Err((addr, sr)) => self.b.ins().call_indirect(sr, addr, &cargs),
            };
            self.b.inst_results(inst).iter().copied().collect()
        };
        if op.deref(ctx).get_num_results() > 0 {
            self.set(op, rs);
        }
    }

    /// Runtime-size copy: sizes in [k, 2k] (k = 16, or 8 without SIMD) are two
    /// overlapping k-byte moves, loads first so it is also a valid memmove; other
    /// sizes call libc. `PLIRON_MEMFAST=0` always calls libc.
    fn small_copy_or_call(&mut self, memcpy: bool, dst: ir::Value, src: ir::Value, n: ir::Value) {
        let t = if crate::pass_enabled("PLIRON_SIMD") {
            clt::I8X16
        } else {
            clt::I64
        };
        let k = t.bytes() as i64;
        let (fast, slow, done) = (
            self.b.create_block(),
            self.b.create_block(),
            self.b.create_block(),
        );
        let m = self.b.ins().iadd_imm_s(n, -k);
        let c = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, m, k);
        self.b.ins().brif(c, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let (se, de) = (self.b.ins().iadd(src, m), self.b.ins().iadd(dst, m));
        let f = MemFlagsData::new();
        let v0 = self.b.ins().load(t, f, src, 0);
        let v1 = self.b.ins().load(t, f, se, 0);
        self.b.ins().store(f, v0, dst, 0);
        self.b.ins().store(f, v1, de, 0);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(slow);
        let cfg = self.m.target_config();
        if memcpy {
            self.b.call_memcpy(cfg, dst, src, n);
        } else {
            self.b.call_memmove(cfg, dst, src, n);
        }
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
    }

    fn lower_intrinsic(&mut self, op: Ptr<Operation>, opnds: &[Value]) {
        let name = self.st.intrinsics[&op].as_str();
        if name.starts_with("pliron.v") {
            let rl = self.ty_leaves(self.res_ty(op)).len();
            let k = opnds
                .iter()
                .map(|v| self.get(*v).len())
                .fold(rl, usize::max);
            if k > 1 {
                return self.split_vec_op(op, name, opnds, k);
            }
        }
        let a: Vec<ir::Value> = opnds.iter().map(|v| self.get1(*v)).collect();
        let r = match name {
            "pliron.eh.exn" => self.b.use_var(self.exn.unwrap()),
            "llvm.memcpy" | "llvm.memmove" => {
                if let Some(n) = self
                    .const_int(opnds[2])
                    .filter(|n| (0..=SMALL_MEM).contains(n))
                {
                    // Load everything before storing, so this is also a valid memmove.
                    let vals: Vec<_> = mem_chunks(n as u64)
                        .into_iter()
                        .map(|(o, t)| (o, self.b.ins().load(t, MemFlagsData::new(), a[1], o)))
                        .collect();
                    for (o, v) in vals {
                        self.b.ins().store(MemFlagsData::new(), v, a[0], o);
                    }
                    return;
                }
                if crate::pass_enabled("PLIRON_MEMFAST")
                    && !self.st.volatile.contains(&op)
                    && self.b.func.dfg.value_type(a[2]) == clt::I64
                {
                    self.small_copy_or_call(name == "llvm.memcpy", a[0], a[1], a[2]);
                    return;
                }
                let cfg = self.m.target_config();
                if name == "llvm.memcpy" {
                    self.b.call_memcpy(cfg, a[0], a[1], a[2]);
                } else {
                    self.b.call_memmove(cfg, a[0], a[1], a[2]);
                }
                return;
            }
            "llvm.memset" => {
                let n = self
                    .const_int(opnds[2])
                    .filter(|n| (0..=SMALL_MEM).contains(n));
                if let (Some(n), Some(c)) = (n, self.const_int(opnds[1])) {
                    let byte = c as u8 as u64;
                    let mut splat = None;
                    for (o, t) in mem_chunks(n as u64) {
                        let v = if t.is_vector() {
                            *splat.get_or_insert_with(|| {
                                let b = self.b.ins().iconst(clt::I8, byte as i64);
                                self.b.ins().splat(t, b)
                            })
                        } else {
                            let pat = byte.wrapping_mul(0x0101_0101_0101_0101)
                                & (u64::MAX >> (64 - t.bits()));
                            self.b.ins().iconst(t, pat as i64)
                        };
                        self.b.ins().store(MemFlagsData::new(), v, a[0], o);
                    }
                    return;
                }
                let cfg = self.m.target_config();
                self.b.call_memset(cfg, a[0], a[1], a[2]);
                return;
            }
            "llvm.trap" => {
                self.b.ins().trap(TrapCode::unwrap_user(3));
                self.terminated = true;
                return;
            }
            "llvm.ctpop" => self.b.ins().popcnt(a[0]),
            "llvm.ctlz" => self.b.ins().clz(a[0]),
            "llvm.cttz" => self.b.ins().ctz(a[0]),
            "llvm.bswap" => {
                if self.b.func.dfg.value_type(a[0]) == clt::I8 {
                    a[0]
                } else {
                    self.b.ins().bswap(a[0])
                }
            }
            "llvm.bitreverse" => self.b.ins().bitrev(a[0]),
            "llvm.fshl" | "llvm.fshr" => {
                let t = self.b.func.dfg.value_type(a[0]);
                let w = t.bits() as i64;
                let s = self.b.ins().band_imm_u(a[2], w - 1);
                if a[0] == a[1] {
                    if name == "llvm.fshl" {
                        self.b.ins().rotl(a[0], s)
                    } else {
                        self.b.ins().rotr(a[0], s)
                    }
                } else {
                    let wv = self.b.ins().iconst(t, w);
                    let inv = self.b.ins().isub(wv, s);
                    let (hi, lo) = if name == "llvm.fshl" {
                        (self.b.ins().ishl(a[0], s), self.b.ins().ushr(a[1], inv))
                    } else {
                        (self.b.ins().ishl(a[0], inv), self.b.ins().ushr(a[1], s))
                    };
                    let comb = self.b.ins().bor(hi, lo);
                    let z = self.b.ins().icmp_imm_s(IntCC::Equal, s, 0);
                    let keep = if name == "llvm.fshl" { a[0] } else { a[1] };
                    self.b.ins().select(z, keep, comb)
                }
            }
            n if n.ends_with(".with.overflow") => {
                let t = self.b.func.dfg.value_type(a[0]);
                let (r, of) = match (n, t == clt::I128) {
                    ("llvm.smul.with.overflow" | "llvm.umul.with.overflow", true) => {
                        let slot = self.slot(4, 4);
                        let f = if n.starts_with("llvm.s") {
                            "__rust_i128_mulo"
                        } else {
                            "__rust_u128_mulo"
                        };
                        let r = self.libcall(f, &[t, t, clt::I64], &[t], &[a[0], a[1], slot])[0];
                        let o = self
                            .b
                            .ins()
                            .load(clt::I32, MemFlagsData::trusted(), slot, 0);
                        (r, self.b.ins().icmp_imm_s(IntCC::NotEqual, o, 0))
                    }
                    ("llvm.sadd.with.overflow", _) => self.b.ins().sadd_overflow(a[0], a[1]),
                    ("llvm.uadd.with.overflow", _) => self.b.ins().uadd_overflow(a[0], a[1]),
                    ("llvm.ssub.with.overflow", _) => self.b.ins().ssub_overflow(a[0], a[1]),
                    ("llvm.usub.with.overflow", _) => self.b.ins().usub_overflow(a[0], a[1]),
                    ("llvm.smul.with.overflow", _) => self.b.ins().smul_overflow(a[0], a[1]),
                    ("llvm.umul.with.overflow", _) => self.b.ins().umul_overflow(a[0], a[1]),
                    _ => panic!("unknown intrinsic {n}"),
                };
                self.set(op, smallvec![r, of]);
                return;
            }
            "llvm.uadd.sat" | "llvm.usub.sat" | "llvm.sadd.sat" | "llvm.ssub.sat" => {
                let (x, y) = (a[0], a[1]);
                let t = self.b.func.dfg.value_type(x);
                if t.is_vector() {
                    let ins = self.b.ins();
                    match name {
                        "llvm.uadd.sat" => ins.uadd_sat(x, y),
                        "llvm.usub.sat" => ins.usub_sat(x, y),
                        "llvm.sadd.sat" => ins.sadd_sat(x, y),
                        _ => ins.ssub_sat(x, y),
                    }
                } else {
                    let (r, of) = match name {
                        "llvm.uadd.sat" => self.b.ins().uadd_overflow(x, y),
                        "llvm.usub.sat" => self.b.ins().usub_overflow(x, y),
                        "llvm.sadd.sat" => self.b.ins().sadd_overflow(x, y),
                        _ => self.b.ins().ssub_overflow(x, y),
                    };
                    let zero = self.b.ins().iconst(t, 0);
                    let ones = self.b.ins().bnot(zero);
                    let sat = match name {
                        "llvm.uadd.sat" => ones,
                        "llvm.usub.sat" => zero,
                        _ => {
                            // x < 0 saturates to MIN (= !MAX), else to MAX.
                            let max = self.b.ins().ushr_imm(ones, 1);
                            let sign = self.b.ins().sshr_imm(x, t.bits() as i64 - 1);
                            self.b.ins().bxor(sign, max)
                        }
                    };
                    self.b.ins().select(of, sat, r)
                }
            }
            "llvm.fptoui.sat" | "llvm.fptosi.sat" => {
                let t = self.ty_leaves(self.res_ty(op))[0].1;
                self.fcvt_sat(name == "llvm.fptosi.sat", t, a[0])
            }
            n if n.starts_with("pliron.v") => {
                let rt = self.ty_leaves(self.res_ty(op))[0].1;
                self.vec_op(n, &a, rt)
            }
            "llvm.sqrt" => self.b.ins().sqrt(a[0]),
            "llvm.fabs" => self.b.ins().fabs(a[0]),
            "llvm.floor" => self.b.ins().floor(a[0]),
            "llvm.ceil" => self.b.ins().ceil(a[0]),
            "llvm.trunc" => self.b.ins().trunc(a[0]),
            "llvm.roundeven" => self.b.ins().nearest(a[0]),
            "llvm.copysign" => self.b.ins().fcopysign(a[0], a[1]),
            "llvm.fma" => self.b.ins().fma(a[0], a[1], a[2]),
            "llvm.minimum" => self.b.ins().fmin(a[0], a[1]),
            "llvm.maximum" => self.b.ins().fmax(a[0], a[1]),
            "llvm.fmuladd" => self.b.ins().fma(a[0], a[1], a[2]),
            n if matches!(
                n,
                "llvm.round"
                    | "llvm.sin"
                    | "llvm.cos"
                    | "llvm.exp"
                    | "llvm.exp2"
                    | "llvm.log"
                    | "llvm.log2"
                    | "llvm.log10"
                    | "llvm.pow"
            ) =>
            {
                // No Cranelift instruction: call libm, like cg_llvm does on x86.
                let t = self.b.func.dfg.value_type(a[0]);
                let base = &n["llvm.".len()..];
                let f = match t {
                    clt::F32 => format!("{base}f"),
                    clt::F64 => base.to_string(),
                    _ => panic!("pliron->cranelift: unsupported intrinsic {n} on {t}"),
                };
                let ps = vec![t; a.len()];
                self.libcall(&f, &ps, &[t], &a)[0]
            }
            n => panic!("pliron->cranelift: unsupported intrinsic {n}"),
        };
        self.set1(op, r);
    }
}

/// Reverse postorder from the entry block, then any unreachable blocks, so
/// every SSA definition is lowered before its uses.
/// Blocks that end in `unreachable`, call a `#[cold]` function, are the
/// unlikely side of an expect branch, or only lead to such blocks.
fn cold_blocks(
    ctx: &Context,
    st: &State<'_>,
    pblocks: &[Ptr<BasicBlock>],
) -> rustc_data_structures::fx::FxHashSet<Ptr<BasicBlock>> {
    let mut cold = rustc_data_structures::fx::FxHashSet::default();
    let term = |b: Ptr<BasicBlock>| b.deref(ctx).iter(ctx).last().unwrap();
    for &b in pblocks {
        let hit = b.deref(ctx).iter(ctx).any(|op| {
            Operation::is_op::<UnreachableOp>(op, ctx)
                || crate::inline::direct_callee(ctx, st, op)
                    .is_some_and(|s| st.funcs.get(s).is_some_and(|f| f.cold))
        });
        if hit {
            cold.insert(b);
        }
        let t = term(b);
        if let Some(&e) = st.expect.get(&t) {
            cold.insert(t.deref(ctx).get_successor(if e { 1 } else { 0 }));
        }
    }
    loop {
        let mut changed = false;
        for &b in &pblocks[1..] {
            if cold.contains(&b) {
                continue;
            }
            let t = term(b);
            let succ: Vec<_> = t.deref(ctx).successors().collect();
            if !succ.is_empty() && succ.iter().all(|s| cold.contains(s)) {
                cold.insert(b);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    cold.remove(&pblocks[0]);
    cold
}

pub(crate) fn rpo(
    ctx: &Context,
    st: &State<'_>,
    blocks: &[Ptr<BasicBlock>],
) -> Vec<Ptr<BasicBlock>> {
    rpo_split(ctx, st, blocks).0
}

/// Reverse post-order of the reachable blocks, then the unreachable ones;
/// also returns how many are reachable.
fn rpo_split(
    ctx: &Context,
    st: &State<'_>,
    blocks: &[Ptr<BasicBlock>],
) -> (Vec<Ptr<BasicBlock>>, usize) {
    // Invoke landing pads count as successors so values defined before an
    // invoke are lowered before the landing-pad code that uses them.
    let succs = |b: Ptr<BasicBlock>| -> Vec<Ptr<BasicBlock>> {
        b.deref(ctx)
            .iter(ctx)
            .flat_map(|op| {
                let mut v: Vec<_> = op.deref(ctx).successors().collect();
                v.extend(st.invokes.get(&op).map(|&(l, _)| l));
                v
            })
            .collect()
    };
    let mut seen = std::collections::HashSet::new();
    let mut post = Vec::new();
    let mut stack = vec![(blocks[0], succs(blocks[0]), 0usize)];
    seen.insert(blocks[0]);
    while let Some((b, ss, i)) = stack.last_mut() {
        if let Some(&n) = ss.get(*i) {
            *i += 1;
            if seen.insert(n) {
                let ns = succs(n);
                stack.push((n, ns, 0));
            }
        } else {
            post.push(*b);
            stack.pop();
        }
    }
    post.reverse();
    let n = post.len();
    post.extend(blocks.iter().copied().filter(|b| !seen.contains(b)));
    (post, n)
}

/// Constant-size mem{cpy,move,set} up to this many bytes are expanded inline
/// (unaligned scalar loads/stores) instead of calling libc.
/// Constant-size memcpy/memmove/memset up to this many bytes are expanded inline.
const SMALL_MEM: i128 = 128;

fn mem_chunks(n: u64) -> Vec<(i32, ClType)> {
    let mut v = Vec::new();
    let mut o = 0;
    let wide = if crate::pass_enabled("PLIRON_SIMD") {
        16
    } else {
        0
    };
    for (sz, t) in [
        (wide, clt::I8X16),
        (8, clt::I64),
        (4, clt::I32),
        (2, clt::I16),
        (1, clt::I8),
    ] {
        if sz == 0 {
            continue;
        }
        while n - o >= sz {
            v.push((o as i32, t));
            o += sz;
        }
    }
    v
}
