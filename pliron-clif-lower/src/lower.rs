//! pliron LLVM dialect -> Cranelift IR -> object file.
//!
//! Aggregates are kept as SSA "leaf lists" (one Cranelift value per scalar
//! leaf), so extract/insertvalue are free and loads/stores are per-leaf.

use std::sync::Arc;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::{Ieee32, Ieee64};
use cranelift_codegen::ir::{
    self, AbiParam, ArgumentPurpose, Block, FuncRef, GlobalValue, InstBuilder, MemFlagsData,
    Signature, StackSlotData, StackSlotKind, TrapCode, Type as ClType, types as clt,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig, TargetIsa};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module, default_libcall_names};
use cranelift_object::{ObjectBuilder, ObjectModule};
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::{Op, OpId};
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;
use pliron_llvm::attributes::{FCmpPredicateAttr, GepNoWrapFlags, ICmpPredicateAttr};
use pliron_llvm::ops::*;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::{SmallVec, smallvec};

use crate::context::{ArgExt, ConstVal, Exts, State, mask};
use crate::types::{TyK, classify, leaf_range, leaves, size_align, struct_offsets};
use crate::{address_taken, has_body};

type Vals = SmallVec<[ir::Value; 2]>;

#[derive(Clone, Copy)]
enum Sym {
    F(FuncId, TypeHandle),
    D(DataId, bool),
}

/// Set when the target ABI leaves the upper bits of narrow return values
/// unspecified (x86-64 SysV: only `%al`/`%ax` are significant, and neither
/// LLVM nor gcc/clang callers rely on more), so callees needn't extend them
/// (`PLIRON_RET_NOEXT`).
pub static RET_NOEXT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn make_sig(ctx: &Context, fn_ty: TypeHandle, exts: &Exts, cc: CallConv) -> Signature {
    let TyK::Func(ret, args, var_arg) = classify(ctx, fn_ty) else {
        panic!("not a function type")
    };
    let mut sig = Signature::new(cc);
    // Both ends of a tail-CC call are our code and Cranelift never trusts the
    // upper bits of narrow values, so the platform ABI's extension is waste.
    let noext = cc == CallConv::Tail && crate::pass_enabled("PLIRON_TAILCC_NOEXT");
    let apply = |p: AbiParam, e: ArgExt, t: ClType| match e {
        ArgExt::SRet => AbiParam::special(t, ArgumentPurpose::StructReturn),
        // Cranelift requires StructArgument size to be a multiple of the
        // 8-byte stack slot; pad to match SysV stack-arg placement. Above
        // a sane ceiling (huge `repr(align)` types exceed Cranelift's
        // implementation limits) drop the marker and pass the leaves.
        ArgExt::ByVal(n) => {
            if n <= (1 << 20) {
                AbiParam::special(t, ArgumentPurpose::StructArgument(n.next_multiple_of(8)))
            } else {
                p
            }
        }
        ArgExt::Zext | ArgExt::Sext if noext => p,
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
    if var_arg {
        // C-variadic definitions: callers pass the variadic args packed in a
        // buffer whose address arrives as one extra pointer parameter.
        sig.params.push(AbiParam::new(if crate::types::ptr32() {
            clt::I32
        } else {
            clt::I64
        }));
    }
    let lv = leaves(ctx, ret);
    let single = lv.len() == 1;
    let ret_ext = match exts.ret {
        ArgExt::Zext | ArgExt::Sext if RET_NOEXT.load(std::sync::atomic::Ordering::Relaxed) => {
            ArgExt::default()
        }
        e => e,
    };
    for (_, t) in lv {
        let p = AbiParam::new(t);
        sig.returns
            .push(if single { apply(p, ret_ext, t) } else { p });
    }
    sig
}

/// Local, non-variadic functions whose address never escapes and that are
/// never invoked: only called directly from this module, so they may use Cranelift's `tail` ABI (up to
/// 8 int + 8 float return registers instead of SysV's 2+2).
pub fn internal_fns(ctx: &Context, st: &State) -> rustc_hash::FxHashSet<String> {
    use rustc_hash::FxHashSet;
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

/// Profiler hook, shared with the codegen worker threads.
pub trait Prof: Sync {
    /// Guard for phase/step `nm` (optionally per function `arg`).
    fn activity(&self, _nm: &'static str, _arg: Option<&str>) -> Box<dyn Send + '_> {
        Box::new(())
    }
}

/// What lowering needs from its embedder: rustc's `Session`, or a JIT.
pub trait Host {
    fn prof(&self) -> &dyn Prof;
    /// Clean user-facing error (capacity limits), not an ICE.
    fn fatal(&self, msg: String) -> ! {
        panic!("{msg}")
    }
    /// x86-64 `sse4.2` (`pcmpgtq`) is available.
    fn sse42(&self) -> bool {
        false
    }
}

/// Default host: no profiler, panicking `fatal`, baseline x86-64.
pub struct NoHost;
impl Prof for NoHost {}
impl Host for NoHost {
    fn prof(&self) -> &dyn Prof {
        self
    }
}

/// (name, bytes, align, relocs, writable, used, section name, linkage)
type CustomSec = (
    String,
    Vec<u8>,
    u64,
    Vec<(u64, String, i64)>,
    bool,
    bool,
    String,
    Linkage,
);

/// Result of [`lower_module`]: everything defined in the module, plus the
/// pieces only an object file can carry.
pub struct Lowered {
    pub eh: crate::eh::UnwindContext,
    ids: FxHashMap<String, Sym>,
    verbatim_syms: Vec<String>,
    custom_secs: Vec<CustomSec>,
    asm_stubs: rustc_hash::FxHashSet<&'static str>,
    hot_asm: String,
}

impl Lowered {
    /// The module function declared for symbol `n`.
    pub fn func_id(&self, n: &str) -> Option<FuncId> {
        match self.ids.get(n)? {
            Sym::F(id, _) => Some(*id),
            Sym::D(..) => None,
        }
    }

    /// Assembly the module still needs (`asm!`, hot thunks, atomic stubs,
    /// va trampolines): object emitters splice it in, a JIT must provide
    /// these symbols itself or reject the module.
    pub fn pending_asm(&self, st: &State, macho: bool) -> String {
        let va_asm: String = VA_TRAMPS.with(|v| v.borrow_mut().drain().collect());
        let mut asm = format!("{}\n{}\n{va_asm}", st.asm, self.hot_asm);
        for s in &self.asm_stubs {
            asm.push_str(&atomic_stub_asm(s, macho));
        }
        asm
    }
}

pub fn lower_to_object(
    unwind: bool,
    hot: bool,
    ctx: &Context,
    st: &State,
    isa: Arc<dyn TargetIsa>,
    name: &str,
    host: &dyn Host,
) -> Vec<u8> {
    let mut b = ObjectBuilder::new(isa.clone(), name.to_string(), default_libcall_names()).unwrap();
    // Mach-O symbols index sections with a u8 (max 255): per-symbol sections
    // overflow, and Mach-O already strips dead code via subsections-at-symbols.
    let macho = isa.triple().binary_format == target_lexicon::BinaryFormat::Macho;
    b.per_function_section(!macho);
    b.per_data_object_section(!macho);
    let mut m = ObjectModule::new(b);
    let lw = lower_module(&mut m, unwind, hot, ctx, st, &isa, name, host);
    let mut product = m.finish();
    let asm = lw.pending_asm(st, macho);
    let Lowered {
        eh,
        ids,
        verbatim_syms,
        custom_secs,
        ..
    } = lw;
    // `\x01name` is LLVM's no-mangle marker: `obj_sym` already removed it at
    // declaration, so the stored name is `name` or `_name` — drop the
    // platform prefix (e.g. `_`) that add_symbol prepended.
    let prefix = product.object.mangling().global_prefix();
    for n in &verbatim_syms {
        let sid = product
            .object
            .symbol_id(crate::obj_sym(n).as_bytes())
            .unwrap_or_else(|| panic!("verbatim symbol {n:?}"));
        let name = &mut product.object.symbol_mut(sid).name;
        if let Some(p) = prefix
            && name.starts_with(&[p])
        {
            name.remove(0);
        }
    }
    if !custom_secs.is_empty() {
        emit_merged_custom_sections(&mut product.object, custom_secs);
    }
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
    if !asm.trim().is_empty() {
        let x86 = isa.triple().architecture == target_lexicon::Architecture::X86_64;
        crate::objmerge::assemble_into(&mut product.object, &asm, x86);
    }
    product.emit().unwrap()
}

/// Declare and define every function and global of `ctx`'s module in `m`
/// (an `ObjectModule`, a `JITModule`, ...).
#[allow(clippy::too_many_arguments)]
pub fn lower_module(
    m: &mut dyn Module,
    unwind: bool,
    hot: bool,
    ctx: &Context,
    st: &State,
    isa: &Arc<dyn TargetIsa>,
    name: &str,
    host: &dyn Host,
) -> Lowered {
    let macho = isa.triple().binary_format == target_lexicon::BinaryFormat::Macho;
    let cc = isa.default_call_conv();
    let mut internal = internal_fns(ctx, st);
    if matches!(
        isa.triple().architecture,
        target_lexicon::Architecture::Aarch64(_)
    ) {
        // aarch64 has no StructReturn support for the tail CC: sret functions
        // that sret2reg didn't convert keep the platform ABI.
        internal.retain(|n| {
            !st.funcs[n]
                .exts
                .params
                .contains(&crate::context::ArgExt::SRet)
        });
    }
    let cc_of = |n: &str| {
        if internal.contains(n) {
            CallConv::Tail
        } else {
            cc
        }
    };
    let mut ids: FxHashMap<String, Sym> = FxHashMap::default();
    let mut verbatim_syms: Vec<String> = Vec::new();
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
            let thunk = m
                .declare_function(crate::obj_sym(n), Linkage::Import, &sig)
                .unwrap();
            hot_bodies.insert(n.clone(), body);
            hot_asm.push_str(&crate::hot::thunk_asm(n, l));
            ids.insert(n.clone(), Sym::F(thunk, f.ty));
            continue;
        }
        let id = m
            .declare_function(crate::obj_sym(n), l, &sig)
            .unwrap_or_else(|e| panic!("declare {n}: {e}"));
        if n.starts_with('\x01') {
            verbatim_syms.push(n.clone());
        }
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
        // Mach-O custom sections are emitted by emit_merged_custom_sections
        // post-finish, so declare them Import to satisfy finish()'s
        // defined-symbol check.
        let l = if macho && g.section.is_some() && g.init.is_some() {
            Linkage::Import
        } else {
            l
        };
        let id = m
            .declare_data(crate::obj_sym(n), l, g.mutable, g.tls)
            .unwrap_or_else(|e| panic!("declare {n}: {e}"));
        if n.starts_with('\x01') {
            verbatim_syms.push(n.clone());
        }
        ids.insert(n.clone(), Sym::D(id, g.tls));
    }

    // Windows has no landingpad unwinder for us to catch in, so COFF EH is
    // emulated like the wasm backend: a TLS flag word (`__pliron_eh`,
    // defined by the allocator module) checked after every call; a "throw"
    // sets it and unwinds by early-return. Real SEH can still unwind through
    // our frames via `.pdata`/`.xdata` (see eh/mod.rs).
    let eh_data = if unwind && isa.triple().binary_format == target_lexicon::BinaryFormat::Coff {
        match ids.get("__pliron_eh") {
            Some(&Sym::D(id, _)) => Some(id),
            _ => Some(
                m.declare_data("__pliron_eh", Linkage::Import, true, true)
                    .unwrap(),
            ),
        }
    } else {
        None
    };

    let mut eh = crate::eh::UnwindContext::new(m, true, unwind);
    let cfg = m.target_config();
    let mut fbc = FunctionBuilderContext::new();
    let mut clctx = m.make_context();
    let mut asm_stubs = rustc_hash::FxHashSet::default();
    let mut stats = crate::pipeline::Stats::default();
    let fn_flags = crate::pipeline::FnFlags {
        jumpthread: st.jumpthread,
        unreach: st.unreach,
        peep: st.peep,
        tailmerge: st.tailmerge,
        taildup: st.taildup,
        tailrec: st.tailrec,
        loadfwd: st.loadfwd,
        slot_dse: st.slot_dse,
        dse: st.dse,
        indvars: st.indvars,
        licm: st.licm,
        loopidiom: st.loopidiom,
        loopvec: st.loopvec,
        bcheck: st.bcheck,
        slp: st.slp,
    };
    let simd = matches!(
        isa.triple().architecture,
        target_lexicon::Architecture::Aarch64(_) | target_lexicon::Architecture::X86_64
    );
    let x64 = isa.triple().architecture == target_lexicon::Architecture::X86_64;
    // 64-bit vector compares are cheap on aarch64 and with sse4.2
    // (`pcmpgtq`); the SSE2 i64 expansion is ~10 insts per compare.
    let cmp64 = !x64
        || host.sse42();
    // `PLIRON_VERIFY=full` verifies after every pass. Any other truthy
    // value verifies once after the pipeline: on failure the function is
    // restored from a pre-pipeline snapshot and re-run with per-pass
    // verification, so attribution costs a retry only when something
    // actually broke.
    let verify_env = crate::env_set("PLIRON_VERIFY");
    let verify_full_env = crate::env_is("PLIRON_VERIFY", "full");
    /// A function's pliron→CLIF lower output plus everything the CLIF
    /// pipeline and cranelift compile need. Jobs are independent: phase
    /// (B) below runs them on worker threads.
    struct FnJob<'a> {
        name: &'a str,
        id: FuncId,
        func: ir::Function,
        nonnull: FxHashSet<ir::Value>,
        derived: FxHashMap<ir::Value, ir::Value>,
        frozen: FxHashMap<ir::Value, u64>,
        noalias: FxHashSet<ir::Value>,
        deref: FxHashMap<ir::Value, (u64, bool)>,
        nowrite: FxHashMap<FuncRef, bool>,
        call_frozen: FxHashMap<FuncRef, u64>,
        scoped: FxHashMap<ir::Inst, Ptr<BasicBlock>>,
        invariant: FxHashSet<ir::Inst>,
        operand_frozen: FxHashMap<ir::Value, Vec<(u64, Ptr<BasicBlock>)>>,
        block_region: FxHashMap<ir::Block, Ptr<BasicBlock>>,
        // PGO: CLIF block -> executed count (from st.prof_bcount via
        // block_region); CLIF passes read it through Pipe.prof.
        prof: FxHashMap<ir::Block, u64>,
        lifetimes: Vec<(ir::Value, Option<ir::Inst>, ir::Block, bool)>,
        nocap: FxHashSet<(ir::Inst, u32)>,
        stubs: rustc_hash::FxHashSet<&'static str>,
        stats: crate::pipeline::Stats,
        err: Option<cranelift_codegen::CodegenError>,
        code: Option<cranelift_codegen::CompiledCode>,
        dump: bool,
    }
    /// Read-only inputs shared by every job in phase (B).
    struct JobShared<'a> {
        isa: &'a dyn TargetIsa,
        tcfg: TargetFrontendConfig,
        simd: bool,
        x64: bool,
        cmp64: bool,
        has_eh: bool,
        verify_env: bool,
        verify_full_env: bool,
        fn_flags: &'a crate::pipeline::FnFlags,
        region_parent: &'a FxHashMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
        prof: &'a dyn Prof,
    }
    let _slo = tracing::info_span!(target: "pliron::phase", "lowerobj", cgu = name).entered();
    let _pglo = host.prof().activity("pliron_lowerobj", None);
    // Phase (A): pliron → CLIF lowering. Serial — `FnLower` reads the
    // pliron `Context`, which is RefCell-interned and not shareable.
    let _sfl = tracing::info_span!(target: "pliron::phase", "fnlower").entered();
    let _pgfl = host.prof().activity("pliron_fnlower", None);
    let mut jobs: Vec<FnJob> = Vec::new();
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
        let noalias: FxHashSet<cranelift_codegen::ir::Value>;
        let deref: FxHashMap<cranelift_codegen::ir::Value, (u64, bool)>;
        let nowrite: FxHashMap<FuncRef, bool>;
        let call_frozen: FxHashMap<FuncRef, u64>;
        let scoped: FxHashMap<ir::Inst, Ptr<BasicBlock>>;
        let invariant: FxHashSet<ir::Inst>;
        let operand_frozen: FxHashMap<cranelift_codegen::ir::Value, Vec<(u64, Ptr<BasicBlock>)>>;
        let block_region: FxHashMap<ir::Block, Ptr<BasicBlock>>;
        let prof: FxHashMap<ir::Block, u64>;
        let lifetimes: Vec<(ir::Value, Option<ir::Inst>, ir::Block, bool)>;
        let nocap: FxHashSet<(ir::Inst, u32)>;
        let stubs: rustc_hash::FxHashSet<&'static str>;
        {
            let b = FunctionBuilder::new(&mut clctx.func, &mut fbc);
            let mut fl = FnLower {
                ctx,
                st,
                m: &mut *m,
                ids: &ids,
                b,
                vals: FxHashMap::default(),
                cconst: FxHashMap::default(),
                blocks: FxHashMap::default(),
                frefs: FxHashMap::default(),
                cfrefs: FxHashMap::default(),
                gvs: FxHashMap::default(),
                terminated: false,
                cc,
                tcfg: cfg,
                internal: &internal,
                own_tail: internal.contains(n),
                wasm: false,
                exn: None,
                vars: FxHashMap::default(),
                stubs: rustc_hash::FxHashSet::default(),
                nonnull: FxHashSet::default(),
                bool01: FxHashSet::default(),
                derived: FxHashMap::default(),
                frozen: FxHashMap::default(),
                noalias: FxHashSet::default(),
                deref: FxHashMap::default(),
                call_frozen: FxHashMap::default(),
                scoped: FxHashMap::default(),
                invariant: FxHashSet::default(),
                block_region: FxHashMap::default(),
                nowrite: FxHashMap::default(),
                lifetimes: Vec::new(),
                nocap: FxHashSet::default(),
                va_buf: None,
                va_fids: FxHashMap::default(),
                eh_data,
                eh_tls_v: None,
                eject: None,
            };
            fl.lower(f.op);
            fl.b.finalize(cfg);
            asm_stubs.extend(fl.stubs.iter().copied());
            nonnull = std::mem::take(&mut fl.nonnull);
            derived = std::mem::take(&mut fl.derived);
            frozen = std::mem::take(&mut fl.frozen);
            noalias = std::mem::take(&mut fl.noalias);
            deref = std::mem::take(&mut fl.deref);
            nowrite = std::mem::take(&mut fl.nowrite);
            call_frozen = std::mem::take(&mut fl.call_frozen);
            scoped = std::mem::take(&mut fl.scoped);
            invariant = std::mem::take(&mut fl.invariant);
            block_region = std::mem::take(&mut fl.block_region);
            // Resolve each marker's pointer to its CLIF value — safe now
            // that every def has been lowered. Untranslatable pointers
            // (promoted allocas, folded ops) just drop the marker.
            lifetimes = std::mem::take(&mut fl.lifetimes)
                .into_iter()
                .filter_map(|(p, anchor, b, start)| {
                    fl.vals
                        .get(&p)
                        .or_else(|| fl.cconst.get(&p))
                        .and_then(|vs| vs.first().copied())
                        .map(|v| (v, anchor, b, start))
                })
                .collect();
            nocap = std::mem::take(&mut fl.nocap);
            // Caller-side operands of `noalias readonly` clone params, in
            // CLIF values (`State::region_frozen_op`): `*v` is immutable
            // while the binding clone runs — loadfwd's scoped immunity.
            let mut ofz = FxHashMap::default();
            for (&pv, rs) in &fl.st.region_frozen_op {
                if let Some(&v) = fl
                    .vals
                    .get(&pv)
                    .or_else(|| fl.cconst.get(&pv))
                    .and_then(|vs| vs.first())
                {
                    ofz.entry(v).or_insert_with(Vec::new).extend_from_slice(rs);
                }
            }
            operand_frozen = ofz;
            stubs = std::mem::take(&mut fl.stubs);
            prof = block_region
                .iter()
                .filter_map(|(&cb, &rb)| st.prof_bcount.get(&rb).map(|&c| (cb, c)))
                .collect();
        }
        jobs.push(FnJob {
            name: n,
            id,
            func: std::mem::replace(&mut clctx.func, ir::Function::new()),
            nonnull,
            derived,
            frozen,
            noalias,
            deref,
            nowrite,
            call_frozen,
            scoped,
            invariant,
            operand_frozen,
            block_region,
            prof,
            lifetimes,
            nocap,
            stubs,
            stats: crate::pipeline::Stats::default(),
            err: None,
            code: None,
            dump: crate::env_get("PLIRON_CLIF").is_some_and(|f| n.contains(f)),
        });
        m.clear_context(&mut clctx);
    }

    drop(_sfl);
    // Phase (B): CLIF pipeline + cranelift compile per function. Each job
    // is fully self-contained, so workers can run them in any order;
    // results are applied in declaration order in phase (C), keeping the
    // object byte-identical. Serial when `PLIRON_OPT_BISECT` is set (the
    // bisect counter must number pass applications deterministically) or
    // `PLIRON_PAR=0` / `PLIRON_PAR=N` overrides the thread count.
    let shared = JobShared {
        isa: &**isa,
        tcfg: cfg,
        simd,
        x64,
        cmp64,
        has_eh: eh_data.is_some(),
        verify_env,
        verify_full_env,
        fn_flags: &fn_flags,
        region_parent: &st.region_parent,
        prof: host.prof(),
    };
    fn run_job(j: &mut FnJob, clctx: &mut cranelift_codegen::Context, sh: &JobShared) {
        CUR_FN.with(|c| {
            let mut c = c.borrow_mut();
            c.clear();
            c.push_str(j.name);
        });
        clctx.func = std::mem::replace(&mut j.func, ir::Function::new());
        let n = j.name;
        if j.dump {
            eprintln!("==== clif {n} ====\n{}", clctx.func.display());
        }
        let mut verify_full = sh.verify_full_env;
        let snapshot = (sh.verify_env && !verify_full).then(|| clctx.func.clone());
        if crate::env_set("PLIRON_FROZEN_DEBUG") {
            eprintln!("==== {n}: noalias={:?} deref={:?}", j.noalias, j.deref);
        }
        let _sfn = tracing::info_span!(target: "pliron::fn", "fn", name = j.name).entered();
        let tpipe = std::time::Instant::now();
        let _spipe = tracing::info_span!(target: "pliron::pipeline", "pipeline").entered();
        let _pgpipe = sh.prof.activity("pliron_pipeline", Some(n));
        'retry: loop {
            {
                // Per-step self-profile events ride `sess.prof` through
                // this hook — `-Zself-profile` shows each step next to
                // LLVM's `LLVM_passes` activities on the same clock.
                // Scoped to the block so Pipe's borrow set stays per-try.
                let prof_hook = |nm: &'static str| -> Box<dyn Send + '_> {
                    sh.prof.activity(nm, None)
                };
                let mut pipe = crate::pipeline::Pipe {
                    cur_step: "",
                    clctx,
                    isa: sh.isa,
                    fname: n,
                    func_id: j.id,
                    nonnull: &mut j.nonnull,
                    derived: &mut j.derived,
                    frozen: &mut j.frozen,
                    noalias: &mut j.noalias,
                    deref: &mut j.deref,
                    nowrite: &j.nowrite,
                    call_frozen: &j.call_frozen,
                    scoped: &j.scoped,
                    invariant: &j.invariant,
                    operand_frozen: &mut j.operand_frozen,
                    block_region: &j.block_region,
                    prof: &j.prof,
                    region_parent: sh.region_parent,
                    lifetimes: &mut j.lifetimes,
                    nocap: &j.nocap,
                    tcfg: sh.tcfg,
                    simd: sh.simd,
                    x64: sh.x64,
                    cmp64: sh.cmp64,
                    has_eh: sh.has_eh,
                    flags: sh.fn_flags,
                    dump: j.dump,
                    verify_full,
                    stats: &mut j.stats,
                    carry_log: Vec::new(),
                    prof_hook: Some(&prof_hook),
                };
                crate::pipeline::run_pipeline(&mut pipe);
            }
            if crate::env_get("PLIRON_VCODE").is_some_and(|f| n.contains(f)) {
                clctx.set_disasm(true);
            }
            if sh.verify_env
                && let Err(e) = cranelift_codegen::verify_function(&clctx.func, sh.isa.flags())
            {
                if !verify_full && let Some(snap) = &snapshot {
                    // Which pass broke it is unknown until the per-pass
                    // checks run — restore and re-run them.
                    clctx.func = snap.clone();
                    verify_full = true;
                    continue 'retry;
                }
                panic!("pipeline broke `{n}`: {e}\n{}", clctx.func.display());
            }
            break 'retry;
        }
        drop(_spipe);
        // `*pipeline*`/`*compile*` pseudo-steps in `timing` split the
        // per-function total for the tier-1 cache sizing question.
        *j.stats.timing.entry("*pipeline*").or_default() += tpipe.elapsed();
        pliron_cfg::trace("*pipeline*", tpipe.elapsed().as_secs_f64() * 1e3, n);
        if j.dump {
            eprintln!("==== clif {n} final ====\n{}", clctx.func.display());
        }
        // Panics inside Cranelift (verifier, egraph, regalloc asserts)
        // bypass the Err path below — catch and rethrow so the message
        // carries the function name and final CLIF for reproduction.
        // Narrow vectors (i8x8 from core_simd masks): ISLE lowers only
        // 128-bit ops. When emit names an unsupported vector type,
        // scalarize every inst touching it and retry.
        let tcomp = std::time::Instant::now();
        let _scomp = tracing::info_span!(target: "pliron::pipeline", "compile").entered();
        let _pgcomp = sh.prof.activity("pliron_compile", Some(n));
        let mut vec_retry = 0;
        let defr = loop {
            let defr = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                clctx
                    .compile(sh.isa, &mut ControlPlane::default())
                    .map(|_| ())
                    .map_err(|e| e.inner)
            }));
            let msg = match &defr {
                Ok(Err(e)) => format!("{e}"),
                Ok(_) => break defr,
                Err(p) => p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default(),
            };
            // Look for a fixed vector type in the message
            // ("`v3 = iadd.i8x8 v0, v1`", "value of type i8x8").
            let vt = {
                use ir::types::*;
                let re: [(&str, ir::Type); 7] = [
                    ("i8x8", I8X8), ("i16x4", I16X4), ("i32x2", I32X2),
                    ("f32x2", F32X2), ("i8x4", I8X4), ("i16x2", I16X2),
                    ("i8x2", I8X2),
                ];
                re.iter().find(|(t, _)| msg.contains(&format!(".{t}")) || msg.contains(&format!(" {t}"))).map(|(_, ty)| *ty)
            };
            if let Some(t) = vt.filter(|t| t.is_vector() && t.bits() < 128 && sh.x64) {
                if vec_retry >= 4 || crate::veclegalize::run(&mut clctx.func, t) == 0 {
                    break defr;
                }
                vec_retry += 1;
                continue;
            }
            break defr;
        };
        drop(_scomp);
        *j.stats.timing.entry("*compile*").or_default() += tcomp.elapsed();
        pliron_cfg::trace("*compile*", tcomp.elapsed().as_secs_f64() * 1e3, n);
        // Cranelift's own pass profiler: `emit:<pass>` pseudo-steps show
        // where inside `compile` (vcode lower/emit, regalloc) time goes.
        if pliron_cfg::env_set("PLIRON_TIMING") || pliron_cfg::env_get("PLIRON_TRACE_FILE").is_some()
        {
            for (d, t) in cranelift_codegen::timing::take_current().self_times() {
                *j.stats.timing.entry(format!("emit:{d}").leak()).or_default() += t;
                pliron_cfg::trace(&format!("emit:{d}"), t.as_secs_f64() * 1e3, n);
            }
        } else {
            let _ = cranelift_codegen::timing::take_current();
        }
        match defr {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => j.err = Some(e),
            Err(e) => {
                eprintln!("cranelift panicked in `{n}`\n{}", clctx.func.display());
                std::panic::resume_unwind(e);
            }
        }
        j.func = std::mem::replace(&mut clctx.func, ir::Function::new());
        j.code = clctx.take_compiled_code();
        // `optimize` leaves cfg/domtree/loop_analysis valid for this
        // func — drop them like `clear_context` did so the next job's
        // compile doesn't verify against a stale cfg.
        clctx.clear();
    }

    let par = crate::env_get("PLIRON_PAR").and_then(|v| v.parse::<usize>().ok());
    let nthreads = if crate::config::bisect_limit().is_some() || par == Some(0) {
        1
    } else {
        par.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
            .min(jobs.len().max(1))
            .max(1)
    };
    let jobs = if nthreads <= 1 {
        let mut wcl = cranelift_codegen::Context::new();
        for j in &mut jobs {
            run_job(j, &mut wcl, &shared);
        }
        jobs
    } else {
        let jobs: Vec<std::sync::Mutex<FnJob>> =
            jobs.into_iter().map(std::sync::Mutex::new).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let shared = &shared;
        std::thread::scope(|s| {
            for _ in 0..nthreads {
                s.spawn(|| {
                    let mut wcl = cranelift_codegen::Context::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= jobs.len() {
                            break;
                        }
                        let mut j = jobs[i].lock().unwrap_or_else(|e| e.into_inner());
                        run_job(&mut j, &mut wcl, shared);
                    }
                });
            }
        });
        jobs.into_iter().map(|m| m.into_inner().unwrap()).collect()
    };

    // Phase (C): apply compiled code to the object in the original
    // function order — identical to what `define_function` did after
    // compiling each function serially.
    let _spc = tracing::info_span!(target: "pliron::phase", "phasec").entered();
    let _pgpc = host.prof().activity("pliron_phasec", None);
    // x64 without SSSE3 lowers `shuffle`/`swizzle.i8x16` to an `X86Pshufb`
    // libcall; note any compiled function that references it so phase (C+)
    // can define the helper (nothing in the Rust runtime provides it).
    let mut needs_pshufb = false;
    for j in &jobs {
        stats.merge(&j.stats);
        asm_stubs.extend(j.stubs.iter().copied());
        let Some(code) = &j.code else {
            let n = j.name;
            // Known capacity limits are a backend limitation, not a bug:
            // report cleanly instead of dumping a multi-MB function into
            // an ICE.
            if let Some(
                e @ (cranelift_codegen::CodegenError::ImplLimitExceeded
                | cranelift_codegen::CodegenError::CodeTooLarge),
            ) = &j.err
            {
                host.fatal(format!("function `{n}` too large: {e}"));
            }
            panic!(
                "cranelift rejected `{n}`: {:?}\n{}",
                j.err,
                j.func.display()
            );
        };
        if let Some(v) = &code.vcode {
            eprintln!("==== vcode {} ====\n{v}", j.name);
            if let Some(d) = crate::env_get("PLIRON_CODEBIN") {
                let _ = std::fs::write(format!("{d}/{}.bin", j.name), code.buffer.data());
            }
        }
        let relocs: Vec<cranelift_module::ModuleReloc> = code
            .buffer
            .relocs()
            .iter()
            .map(|r| {
                let mr = cranelift_module::ModuleReloc::from_mach_reloc(r, &j.func, j.id);
                needs_pshufb |= matches!(
                    mr.name,
                    cranelift_module::ModuleRelocTarget::LibCall(ir::LibCall::X86Pshufb)
                );
                mr
            })
            .collect();
        m.define_function_bytes(
            j.id,
            u64::from(code.buffer.alignment),
            code.buffer.data(),
            &relocs,
        )
        .unwrap_or_else(|e| panic!("define {}: {e}", j.name));
        eh.add_function(m, j.id, code);
    }

    if needs_pshufb {
        x86_pshufb_helper(&mut *m, &mut clctx, &mut fbc, cc, cfg);
    }

    if crate::env_set("PLIRON_TIMING") {
        let mut t: Vec<_> = stats.timing.iter().collect();
        t.sort_by_key(|(_, d)| std::cmp::Reverse(**d));
        for (n, d) in t {
            eprintln!("timing {name} {n} {:.3}ms", d.as_secs_f64() * 1e3);
        }
    }
    if crate::env_set("PLIRON_STATS") {
        eprintln!("pipeline {name}: {}", crate::config::config().describe());
    }
    if st.loadfwd && crate::env_set("PLIRON_STATS") {
        let (forwarded, peeped, slot_dse) = (stats.forwarded, stats.peeped, stats.slot_dse);
        eprintln!(
            "loadfwd {name}: {forwarded} loads forwarded, {peeped} peepholes, {slot_dse} dead slot stores"
        );
    }
    if st.jumpthread && crate::env_set("PLIRON_STATS") {
        let (threaded, dead_params, dominated_folds, merged, unreached, dupd) = (
            stats.threaded,
            stats.dead_params,
            stats.dominated_folds,
            stats.merged,
            stats.unreached,
            stats.dupd,
        );
        eprintln!(
            "jumpthread {name}: {threaded} edges threaded, {dead_params} dead params removed, {dominated_folds} dominated conditions folded, {merged} exit blocks merged, {unreached} unreachable edges, {dupd} returns duplicated"
        );
    }
    // (name, bytes, align, relocs, writable, used, section name, linkage)
    let mut custom_secs: Vec<CustomSec> = Vec::new();
    for (n, g) in &st.globals {
        let Some(init) = g.init else { continue };
        let Some(Sym::D(id, _)) = ids.get(n).copied() else {
            continue;
        };
        let (size, _) = size_align(ctx, init.get_type(ctx));
        let mut bytes = vec![0u8; size as usize];
        let mut relocs = Vec::new();
        write_const(ctx, st, init, 0, &mut bytes, &mut relocs);
        if macho && g.section.is_some() {
            // cranelift-object calls add_section per data object, and it never
            // merges same-named sections, so N linkme/#[link_section] statics
            // would produce N sections and overflow Mach-O's u8 n_sect field.
            // Emit them into shared sections ourselves after finish().
            custom_secs.push((
                n.clone(),
                bytes,
                g.align.max(1),
                relocs,
                g.mutable,
                g.used,
                g.section.clone().unwrap(),
                g.linkage,
            ));
            continue;
        }
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        // LLVM places constants in 16-byte-capped mergeable/aligned sections;
        // crates that reinterpret `&[u8]` statics rely on that by accident.
        let mut align = g.align.max(1);
        if !g.mutable && g.section.is_none() {
            let pref = if size > 16 {
                16
            } else if size.is_power_of_two() {
                size
            } else {
                1
            };
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
    Lowered {
        eh,
        ids,
        verbatim_syms,
        custom_secs,
        asm_stubs,
        hot_asm,
    }
}

/// Define `__cranelift_x86_pshufb` in `m` as a private function.
///
/// On x64 without SSSE3 the ISLE `lower_pshufb` helper (used by `shuffle`
/// and `swizzle.i8x16`) emits a call to the `X86Pshufb` libcall, which
/// `default_libcall_names` names `__cranelift_x86_pshufb`. No Rust runtime
/// supplies that symbol — upstream embedders (wasmtime) provide it
/// themselves — so the object would carry an unresolvable import. A local
/// definition satisfies it instead: `ObjectModule`'s reloc resolution looks
/// libcall names up in the object's own symbol table before inventing an
/// import.
///
/// pshufb semantics are `r[i] = (m[i] & 0x80) ? 0 : a[m[i] & 15]`: a byte
/// gather with a high-bit kill. Nothing below SSSE3 gathers by byte index,
/// so the body spills both vectors to stack slots and selects each result
/// byte with scalar loads/stores — correct on every ISA.
fn x86_pshufb_helper(
    m: &mut dyn Module,
    clctx: &mut cranelift_codegen::Context,
    fbc: &mut FunctionBuilderContext,
    cc: CallConv,
    tcfg: TargetFrontendConfig,
) {
    let isa = m.isa();
    let pt = isa.pointer_type();
    let sig = ir::LibCall::X86Pshufb.signature(CallConv::for_libcall(isa.flags(), cc), pt);
    let fid = m
        .declare_function("__cranelift_x86_pshufb", Linkage::Local, &sig)
        .unwrap();
    let mut func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, fid.index() as u32),
        sig,
    );
    {
        let mut b = FunctionBuilder::new(&mut func, fbc);
        let block = b.create_block();
        b.append_block_params_for_function_params(block);
        b.switch_to_block(block);
        b.seal_block(block);
        let a = b.block_params(block)[0];
        let mask = b.block_params(block)[1];
        let flags = MemFlagsData::trusted();
        let mk_slot = |b: &mut FunctionBuilder<'_>| {
            let ss = b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                16,
                4,
            ));
            b.ins().stack_addr(pt, ss, 0)
        };
        let pa = mk_slot(&mut b);
        let pm = mk_slot(&mut b);
        let pr = mk_slot(&mut b);
        b.ins().store(flags, a, pa, 0);
        b.ins().store(flags, mask, pm, 0);
        let zero = b.ins().iconst(clt::I32, 0);
        for i in 0..16i32 {
            let mb = b.ins().uload8(clt::I32, flags, pm, i);
            let idx = b.ins().band_imm_u(mb, 0x0f);
            let idxp = if pt == clt::I64 {
                b.ins().uextend(clt::I64, idx)
            } else {
                idx
            };
            let p = b.ins().iadd(pa, idxp);
            let byte = b.ins().uload8(clt::I32, flags, p, 0);
            let hi = b.ins().band_imm_u(mb, 0x80);
            let kill = b.ins().icmp_imm_u(IntCC::NotEqual, hi, 0);
            let byte = b.ins().select(kill, zero, byte);
            b.ins().istore8(flags, byte, pr, i);
        }
        let r = b.ins().load(clt::I8X16, flags, pr, 0);
        b.ins().return_(&[r]);
        b.finalize(tcfg);
    }
    clctx.func = func;
    m.define_function(fid, clctx)
        .unwrap_or_else(|e| panic!("define __cranelift_x86_pshufb: {e}"));
    m.clear_context(clctx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::settings::{self, Configurable};

    /// A function that calls `LibCall::X86Pshufb` (like x64's `lower_pshufb`
    /// emits) must not leave the object with an undefined
    /// `__cranelift_x86_pshufb` import once the helper is defined: the
    /// libcall's symbol-name lookup reuses the local definition. Runnable
    /// on any host — the call only needs a symbol reloc, not x86 codegen.
    #[test]
    fn pshufb_helper_satisfies_libcall_import() {
        let mut fb = settings::builder();
        fb.set("is_pic", "true").unwrap();
        fb.set(
            "enable_verifier",
            if cfg!(debug_assertions) { "true" } else { "false" },
        )
        .unwrap();
        let flags = settings::Flags::new(fb);
        let isa = cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
            .unwrap()
            .finish(flags)
            .unwrap();
        let b = ObjectBuilder::new(isa.clone(), "t".to_string(), default_libcall_names()).unwrap();
        let mut m = ObjectModule::new(b);
        let cc = isa.default_call_conv();
        let cfg = m.target_config();
        let mut clctx = m.make_context();
        let mut fbc = FunctionBuilderContext::new();

        let mut sig = Signature::new(cc);
        sig.params.push(AbiParam::new(clt::I8X16));
        sig.params.push(AbiParam::new(clt::I8X16));
        sig.returns.push(AbiParam::new(clt::I8X16));
        let cid = m
            .declare_function("caller", Linkage::Local, &sig)
            .unwrap();
        let mut f = ir::Function::with_name_signature(
            ir::UserFuncName::user(0, cid.index() as u32),
            sig.clone(),
        );
        {
            let sr = f.import_signature(sig);
            let fr = f.import_function(ir::ExtFuncData {
                name: ir::ExternalName::LibCall(ir::LibCall::X86Pshufb),
                signature: sr,
                colocated: false,
                patchable: false,
            });
            let mut b = FunctionBuilder::new(&mut f, &mut fbc);
            let block = b.create_block();
            b.append_block_params_for_function_params(block);
            b.switch_to_block(block);
            b.seal_block(block);
            let a = b.block_params(block)[0];
            let msk = b.block_params(block)[1];
            let call = b.ins().call(fr, &[a, msk]);
            let r = b.func.dfg.inst_results(call)[0];
            b.ins().return_(&[r]);
            b.finalize(cfg);
        }
        clctx.func = f;
        m.define_function(cid, &mut clctx).unwrap();
        m.clear_context(&mut clctx);

        x86_pshufb_helper(&mut m, &mut clctx, &mut fbc, cc, cfg);

        let product = m.finish();
        // `get_symbol` resolves the libcall's name with this same lookup;
        // it must hit the local definition — an `Undefined` import would
        // mean the libcall isn't satisfied inside the object.
        let sym = product
            .object
            .symbol_id(b"__cranelift_x86_pshufb")
            .expect("__cranelift_x86_pshufb symbol");
        assert!(!product.object.symbol(sym).is_undefined());
    }
}

/// Result of running the wasm module through the CLIF pipeline: per-function
/// optimized CLIF plus symbol tables for resolving FuncRef/GlobalValue.
pub struct WasmClif {
    /// (symbol name, optimized CLIF) — one per `st.funcs` entry with a body.
    pub funcs: Vec<(String, ir::Function)>,
    /// FuncId::index() -> declared symbol name.
    pub fn_names: Vec<String>,
    /// DataId::index() -> declared symbol name.
    pub data_names: Vec<String>,
}

/// pliron -> CLIF -> pliron-clif-opt pipeline for the wasm backend
/// (`wasm::lower_to_wasm_clif`). Serial and single-context: wasm objects are
/// small enough that the worker pool buys little, and this reuses the same
/// FnLower + run_pipeline code path as `lower_to_object` without it.
///
/// The `ObjectModule` is pure bookkeeping (it is never finished into an
/// object): FnLower needs it for `declare_*_in_func`. It stays on the host
/// ISA — wasm32 is not a Cranelift target — but `FnLower.tcfg` overrides
/// the pointer width to 32 bits so `func_addr`/`symbol_value`/`stack_addr`
/// produce i32 values matching the wasm32 leaf types. The CLIF is then
/// internally consistent (all pointer values i32); the ISA-aware verifier
/// checks (`verify_is_address`, `global_type`) don't apply, so
/// `PLIRON_VERIFY` verifies with flags only, same as the pipeline's
/// per-step checks.
pub fn lower_clif_wasm(
    ctx: &Context,
    st: &State,
    host: &dyn Host,
    cgu: &str,
    unwind: bool,
) -> WasmClif {
    // wasm32 is not a Cranelift target; the bookkeeping ObjectModule and
    // FnLower run on the *host* ISA (see the doc comment). The ISA is never
    // used to emit machine code, so flags/features do not matter beyond
    // `enable_verifier` under debug builds.
    use cranelift_codegen::settings::Configurable as _;
    let mut fb = cranelift_codegen::settings::builder();
    fb.set("is_pic", "true").unwrap();
    fb.set(
        "enable_verifier",
        if cfg!(debug_assertions) { "true" } else { "false" },
    )
    .unwrap();
    let flags = cranelift_codegen::settings::Flags::new(fb);
    let isa = cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
        .expect("cranelift host isa")
        .finish(flags)
        .expect("cranelift host isa flags");
    let b = ObjectBuilder::new(isa.clone(), cgu.to_string(), default_libcall_names()).unwrap();
    let mut m = ObjectModule::new(b);
    // Emitted CLIF uses i32 pointers and the neutral SysV default callconv
    // (not the host's AppleAarch64, which would pull in the Darwin variadic
    // padding path).
    let tcfg = TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: target_lexicon::PointerWidth::U32,
        page_size_align_log2: isa.page_size_align_log2(),
    };
    let cc = CallConv::SystemV;
    let internal = internal_fns(ctx, st);
    let cc_of = |n: &str| {
        if internal.contains(n) {
            CallConv::Tail
        } else {
            cc
        }
    };
    let mut ids: FxHashMap<String, Sym> = FxHashMap::default();
    for (n, f) in &st.funcs {
        if st.dead_fns.contains(n) {
            continue;
        }
        let sig = make_sig(ctx, f.ty, &f.exts, cc_of(n));
        let mut l = f.linkage;
        if l == Linkage::Import && has_body(ctx, f.op) {
            l = Linkage::Export;
        }
        let id = m
            .declare_function(crate::obj_sym(n), l, &sig)
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
            .declare_data(crate::obj_sym(n), l, g.mutable, g.tls)
            .unwrap_or_else(|e| panic!("declare {n}: {e}"));
        ids.insert(n.clone(), Sym::D(id, g.tls));
    }
    // panic=unwind: emulated EH driven by `__pliron_eh`, an 8-byte
    // {flag, exn} data word the wasm linker synthesizes (see FL::eh_addr).
    let eh_data = if unwind {
        Some(
            m.declare_data("__pliron_eh", Linkage::Import, true, false)
                .unwrap(),
        )
    } else {
        None
    };
    let cfg = tcfg;
    let mut fbc = FunctionBuilderContext::new();
    let mut clctx = m.make_context();
    let fn_flags = crate::pipeline::FnFlags {
        jumpthread: st.jumpthread,
        unreach: st.unreach,
        peep: st.peep,
        tailmerge: st.tailmerge,
        taildup: st.taildup,
        tailrec: st.tailrec,
        loadfwd: st.loadfwd,
        slot_dse: st.slot_dse,
        dse: st.dse,
        indvars: st.indvars,
        licm: st.licm,
        loopidiom: st.loopidiom,
        loopvec: st.loopvec,
        bcheck: st.bcheck,
        slp: st.slp,
    };
    let mut out = Vec::new();
    for (n, f) in &st.funcs {
        if !has_body(ctx, f.op) || st.dead_fns.contains(n) {
            continue;
        }
        CUR_FN.with(|c| c.borrow_mut().clone_from(n));
        let Sym::F(id, _) = ids[n] else {
            unreachable!()
        };
        clctx.func.signature = make_sig(ctx, f.ty, &f.exts, cc_of(n));
        let (
            nonnull,
            derived,
            frozen,
            noalias,
            deref,
            nowrite,
            call_frozen,
            scoped,
            invariant,
            operand_frozen,
            block_region,
            prof,
            lifetimes,
            nocap,
        ) = {
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
                cfrefs: FxHashMap::default(),
                gvs: FxHashMap::default(),
                terminated: false,
                cc,
                tcfg,
                internal: &internal,
                own_tail: internal.contains(n),
                wasm: true,
                exn: None,
                vars: FxHashMap::default(),
                stubs: rustc_hash::FxHashSet::default(),
                nonnull: FxHashSet::default(),
                bool01: FxHashSet::default(),
                derived: FxHashMap::default(),
                frozen: FxHashMap::default(),
                noalias: FxHashSet::default(),
                deref: FxHashMap::default(),
                call_frozen: FxHashMap::default(),
                scoped: FxHashMap::default(),
                invariant: FxHashSet::default(),
                block_region: FxHashMap::default(),
                nowrite: FxHashMap::default(),
                lifetimes: Vec::new(),
                nocap: FxHashSet::default(),
                va_buf: None,
                va_fids: FxHashMap::default(),
                eh_data,
                eh_tls_v: None,
                eject: None,
            };
            fl.lower(f.op);
            fl.b.finalize(cfg);
            let lifetimes = fl
                .lifetimes
                .into_iter()
                .filter_map(|(p, anchor, b, start)| {
                    fl.vals
                        .get(&p)
                        .or_else(|| fl.cconst.get(&p))
                        .and_then(|vs| vs.first().copied())
                        .map(|v| (v, anchor, b, start))
                })
                .collect();
            let mut operand_frozen = FxHashMap::default();
            for (&pv, rs) in &fl.st.region_frozen_op {
                if let Some(&v) = fl
                    .vals
                    .get(&pv)
                    .or_else(|| fl.cconst.get(&pv))
                    .and_then(|vs| vs.first())
                {
                    operand_frozen
                        .entry(v)
                        .or_insert_with(Vec::new)
                        .extend_from_slice(rs);
                }
            }
            let prof = fl
                .block_region
                .iter()
                .filter_map(|(&cb, &rb)| st.prof_bcount.get(&rb).map(|&c| (cb, c)))
                .collect();
            (
                fl.nonnull,
                fl.derived,
                fl.frozen,
                fl.noalias,
                fl.deref,
                fl.nowrite,
                fl.call_frozen,
                fl.scoped,
                fl.invariant,
                operand_frozen,
                fl.block_region,
                prof,
                lifetimes,
                fl.nocap,
            )
        };
        let dump = crate::env_get("PLIRON_CLIF").is_some_and(|s| n.contains(&s));
        if dump {
            eprintln!("==== clif {n} ====\n{}", clctx.func.display());
        }
        // Flags-only verify: the ISA-aware checks (pointer width, global
        // value types) consult `isa.pointer_type()` — i64 on the host —
        // while this CLIF deliberately uses i32 pointers (see `tcfg`).
        if crate::env_set("PLIRON_VERIFY")
            && let Err(e) =
                cranelift_codegen::verifier::verify_function(&clctx.func, isa.flags())
        {
            panic!("bad clif {n}: {e}");
        }
        let prof_hook = |nm: &'static str| -> Box<dyn Send + '_> {
            host.prof().activity(nm, None)
        };
        let mut stats = crate::pipeline::Stats::default();
        let mut nonnull = nonnull;
        let mut derived = derived;
        let mut frozen = frozen;
        let mut noalias = noalias;
        let mut deref = deref;
        let mut operand_frozen = operand_frozen;
        let mut lifetimes = lifetimes;
        let mut pipe = crate::pipeline::Pipe {
            cur_step: "",
            clctx: &mut clctx,
            isa: &*isa,
            fname: n,
            func_id: id,
            nonnull: &mut nonnull,
            derived: &mut derived,
            frozen: &mut frozen,
            noalias: &mut noalias,
            deref: &mut deref,
            nowrite: &nowrite,
            call_frozen: &call_frozen,
            scoped: &scoped,
            invariant: &invariant,
            operand_frozen: &mut operand_frozen,
            block_region: &block_region,
            prof: &prof,
            region_parent: &st.region_parent,
            lifetimes: &mut lifetimes,
            nocap: &nocap,
            tcfg: cfg,
            simd: false,
            x64: false,
            cmp64: false,
            has_eh: false,
            flags: &fn_flags,
            dump,
            verify_full: crate::env_is("PLIRON_VERIFY", "full"),
            stats: &mut stats,
            carry_log: Vec::new(),
            prof_hook: Some(&prof_hook),
        };
        // Debug bisection: PLIRON_PIPEFN=<substr> runs the pipeline only on
        // matching functions; PLIRON_PIPEFNX=<substr> skips matches.
        let run_pipe = crate::env_get("PLIRON_PIPEFN").is_none_or(|f| n.contains(&f))
            && crate::env_get("PLIRON_PIPEFNX").is_none_or(|f| !n.contains(&f));
        if run_pipe {
            crate::pipeline::run_pipeline(&mut pipe);
        }
        drop(pipe);
        if dump {
            eprintln!("==== clif {n} final ====\n{}", clctx.func.display());
        }
        out.push((n.clone(), std::mem::replace(&mut clctx.func, ir::Function::new())));
        m.clear_context(&mut clctx);
    }
    CUR_FN.with(|c| c.borrow_mut().clear());
    // FnLower can declare more symbols mid-lower (libcalls, va_fids, lazy
    // data refs) — build the id->name tables last.
    let decls = m.declarations();
    let mut fn_names: Vec<String> = Vec::new();
    for (id, d) in decls.get_functions() {
        if fn_names.len() <= id.index() {
            fn_names.resize(id.index() + 1, String::new());
        }
        fn_names[id.index()] = d.name.clone().unwrap_or_default();
    }
    let mut data_names: Vec<String> = Vec::new();
    for (id, d) in decls.get_data_objects() {
        if data_names.len() <= id.index() {
            data_names.resize(id.index() + 1, String::new());
        }
        data_names[id.index()] = d.name.clone().unwrap_or_default();
    }
    WasmClif {
        funcs: out,
        fn_names,
        data_names,
    }
}

/// Mach-O caps section ordinals at 255 (n_sect is a u8), so all globals that
/// share a `#[link_section]`/`linkme` name must land in one section. The
/// module already declared their symbols (undefined); define them here in a
/// single section per name.
fn emit_merged_custom_sections(
    obj: &mut object::write::Object<'_>,
    globals: Vec<(
        String,
        Vec<u8>,
        u64,
        Vec<(u64, String, i64)>,
        bool,
        bool,
        String,
        Linkage,
    )>,
) {
    use object::write::{Relocation, Symbol as WSymbol, SymbolSection};
    use object::{
        RelocationEncoding, RelocationFlags, RelocationKind, SectionKind, SymbolFlags, SymbolKind,
        SymbolScope, macho,
    };
    // Group by parsed (segment, section) name; the merged kind must satisfy
    // every member.
    let mut order: Vec<String> = Vec::new();
    let mut groups: FxHashMap<String, (bool, bool, u32)> = FxHashMap::default();
    for (_, _, _, relocs, writable, _, secname, _) in &globals {
        let (seg, sec, flags) = parse_macho_section(secname);
        let key = format!("{seg},{sec}");
        let e = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            (false, false, 0)
        });
        e.0 |= *writable;
        e.1 |= !relocs.is_empty();
        e.2 |= flags;
    }
    let mut sections: FxHashMap<String, object::write::SectionId> = FxHashMap::default();
    for key in order {
        let (writable, has_relocs, flags) = groups[&key];
        let kind = if writable {
            SectionKind::Data
        } else if has_relocs {
            SectionKind::ReadOnlyDataWithRel
        } else {
            SectionKind::ReadOnlyData
        };
        let (seg, sec) = key.split_once(',').unwrap();
        let sid = obj.add_section(seg.as_bytes().to_vec(), sec.as_bytes().to_vec(), kind);
        obj.section_mut(sid).flags = object::SectionFlags::MachO { flags };
        sections.insert(key.clone(), sid);
    }
    for (name, bytes, align, relocs, _, used, secname, linkage) in globals {
        let (seg, sec, _) = parse_macho_section(&secname);
        let sid = sections[&format!("{seg},{sec}")];
        let off = obj.append_section_data(sid, &bytes, align);
        let sym = match obj.symbol_id(name.as_bytes()) {
            Some(s) => s,
            None => obj.add_symbol(WSymbol {
                name: name.clone().into_bytes(),
                value: 0,
                size: 0,
                kind: SymbolKind::Data,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Undefined,
                flags: SymbolFlags::None,
            }),
        };
        obj.symbol_mut(sym).scope = match linkage {
            Linkage::Export | Linkage::Preemptible => SymbolScope::Dynamic,
            Linkage::Hidden => SymbolScope::Linkage,
            _ => SymbolScope::Compilation,
        };
        obj.set_symbol_data(sym, sid, off, bytes.len() as u64);
        if used {
            if let SymbolFlags::MachO { n_desc } = obj.symbol_flags_mut(sym) {
                *n_desc |= macho::N_NO_DEAD_STRIP;
            }
        }
        for (o, s, a) in relocs {
            let target = obj
                .symbol_id(s.as_bytes())
                .unwrap_or_else(|| panic!("reloc to undeclared symbol {s} in {name}"));
            obj.add_relocation(
                sid,
                Relocation {
                    offset: off + o,
                    symbol: target,
                    addend: a,
                    flags: RelocationFlags::Generic {
                        kind: RelocationKind::Absolute,
                        encoding: RelocationEncoding::Generic,
                        size: 64,
                    },
                },
            )
            .unwrap();
        }
    }
}

/// `#[link_section]` values carry the full `seg,sec[,type[,attrs]]` Mach-O
/// form; split the name out and translate the flags (same table as
/// cranelift-object's parse_section).
fn parse_macho_section(section: &str) -> (&str, &str, u32) {
    use object::macho::*;
    const TYPES: &[(&str, u32)] = &[
        ("regular", S_REGULAR),
        ("zerofill", S_ZEROFILL),
        ("cstring_literals", S_CSTRING_LITERALS),
        ("4byte_literals", S_4BYTE_LITERALS),
        ("8byte_literals", S_8BYTE_LITERALS),
        ("literal_pointers", S_LITERAL_POINTERS),
        ("non_lazy_symbol_pointers", S_NON_LAZY_SYMBOL_POINTERS),
        ("lazy_symbol_pointers", S_LAZY_SYMBOL_POINTERS),
        ("mod_init_funcs", S_MOD_INIT_FUNC_POINTERS),
        ("mod_term_funcs", S_MOD_TERM_FUNC_POINTERS),
        ("coalesced", S_COALESCED),
        ("interposing", S_INTERPOSING),
        ("16byte_literals", S_16BYTE_LITERALS),
        ("thread_local_regular", S_THREAD_LOCAL_REGULAR),
        ("thread_local_zerofill", S_THREAD_LOCAL_ZEROFILL),
        ("thread_local_variables", S_THREAD_LOCAL_VARIABLES),
        (
            "thread_local_variable_pointers",
            S_THREAD_LOCAL_VARIABLE_POINTERS,
        ),
        (
            "thread_local_init_function_pointers",
            S_THREAD_LOCAL_INIT_FUNCTION_POINTERS,
        ),
    ];
    const ATTRS: &[(&str, u32)] = &[
        ("pure_instructions", S_ATTR_PURE_INSTRUCTIONS),
        ("no_toc", S_ATTR_NO_TOC),
        ("strip_static_syms", S_ATTR_STRIP_STATIC_SYMS),
        ("no_dead_strip", S_ATTR_NO_DEAD_STRIP),
        ("live_support", S_ATTR_LIVE_SUPPORT),
        ("self_modifying_code", S_ATTR_SELF_MODIFYING_CODE),
        ("debug", S_ATTR_DEBUG),
    ];
    let mut parts = section.split(',');
    let seg = parts.next().unwrap();
    let Some(sec) = parts.next() else {
        return ("", section, 0);
    };
    let ty = parts.next().unwrap_or("regular");
    let mut flags = TYPES
        .iter()
        .find(|(n, _)| *n == ty)
        .unwrap_or_else(|| panic!("unsupported Mach-O section type `{ty}` in `{section}`"))
        .1;
    if let Some(attrs) = parts.next() {
        for a in attrs.split('+') {
            flags |= ATTRS
                .iter()
                .find(|(n, _)| *n == a)
                .unwrap_or_else(|| {
                    panic!("unsupported Mach-O section attribute `{a}` in `{section}`")
                })
                .1;
        }
    }
    (seg, sec, flags)
}

/// 16-byte atomic helpers for aarch64: Cranelift can't lower i128 atomic ops
/// there, so loads go through `ldaxp`/`stlxp` and RMW/CAS through `ldxp`/`stlxp`
/// loops. All three take pointers so the CAS retry loop can stay in CLIF:
///   __pliron_a64_atomic_ld_16(x0=ptr, x1=out16)
///   __pliron_a64_atomic_st_16(x0=ptr, x1=&val16)
///   __pliron_a64_atomic_cas_16(x0=ptr, x1=&expected in/out16, x2=&desired16) -> w0
fn atomic_stub_asm(name: &str, macho: bool) -> String {
    // rsasm knows no exclusive loads/stores, so they're emitted as .word.
    // Local labels get per-stub names to stay unique when combined.
    let body = match name {
        // ldaxp x2,x3,[x0]; stlxp w8,x2,x3,[x0]; retry; stp x2,x3,[x1]
        "__pliron_a64_atomic_ld_16" => concat!(
            "Lald_1: .word 0xc87f8c02\n.word 0xc8288c02\ncbnz w8, Lald_1\n",
            "stp x2, x3, [x1]\nret\n"
        ),
        // ldp x2,x3,[x1]; ldxp x6,x7,[x0]; stlxp w8,x2,x3,[x0]; retry
        "__pliron_a64_atomic_st_16" => concat!(
            "ldp x2, x3, [x1]\nLast_1: .word 0xc87f1c06\n.word 0xc8288c02\n",
            "cbnz w8, Last_1\nret\n"
        ),
        // x4:x5 = expected, x9:x10 = desired; CAS loop, fail writes back current
        "__pliron_a64_atomic_cas_16" => concat!(
            ".word 0xc87f1424\nldp x9, x10, [x2]\n",
            "Lacas_1: .word 0xc87f1c06\ncmp x6, x4\nb.ne Lacas_2\ncmp x7, x5\nb.ne Lacas_2\n",
            ".word 0xc828a809\ncbnz w8, Lacas_1\nmov w0, #1\nret\n",
            "Lacas_2: .word 0xd5033f5f\nstp x6, x7, [x1]\nmov w0, #0\nret\n"
        ),
        _ => unreachable!("unknown atomic stub {name}"),
    };
    if macho {
        format!(
            ".section __TEXT,__text,regular,pure_instructions\n.private_extern {name}\n{name}:\n{body}"
        )
    } else {
        format!(
            ".section .text.{name},\"ax\",@progbits\n.globl {name}\n.hidden {name}\n.type {name},@function\n{name}:\n{body}.size {name}, .-{name}\n.text\n"
        )
    }
}

pub fn write_const(
    ctx: &Context,
    st: &State,
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
    /// `%al=8` trampoline asm texts to assemble into the object (see `va_sym`
    /// and the variadic indirect-call path in `lower_call`).
    static VA_TRAMPS: std::cell::RefCell<FxHashSet<String>> =
        std::cell::RefCell::new(FxHashSet::default());
}

/// `%al=8` + tail-`jmp` stub so foreign variadic callees dump all xmm regs.
/// Every CGU that calls `sym` emits its own copy, so the stubs are weak:
/// two CGUs of one crate in the same archive would otherwise collide.
fn va_tramp_asm(sym: &str, macho: bool) -> String {
    let sym = crate::obj_sym(sym);
    let t = format!("__pliron_va8.{sym}");
    if macho {
        format!(
            ".att_syntax\n.section __TEXT,__text,regular,pure_instructions\n.globl {t}\n.weak_definition {t}\n.private_extern {t}\n{t}:\nmov $8, %eax\njmp {sym}\n.text\n"
        )
    } else {
        format!(
            ".att_syntax\n.section .text.{t},\"ax\",@progbits\n.weak {t}\n.hidden {t}\n.type {t},@function\n{t}:\nmov $8, %eax\njmp {sym}\n.size {t}, .-{t}\n.text\n"
        )
    }
}

/// `%al=8` + `jmp *<reg>` stub for calls through variadic function pointers:
/// the fp is appended as a hidden last argument, landing in `reg`.
fn va_tramp_ind_asm(reg: &str, macho: bool) -> String {
    let t = format!("__pliron_va8_ind_{reg}");
    if macho {
        format!(
            ".att_syntax\n.section __TEXT,__text,regular,pure_instructions\n.globl {t}\n.weak_definition {t}\n.private_extern {t}\n{t}:\nmov $8, %eax\njmp *%{reg}\n.text\n"
        )
    } else {
        format!(
            ".att_syntax\n.section .text.{t},\"ax\",@progbits\n.weak {t}\n.hidden {t}\n.type {t},@function\n{t}:\nmov $8, %eax\njmp *%{reg}\n.size {t}, .-{t}\n.text\n"
        )
    }
}

struct FnLower<'a, 'b> {
    ctx: &'a Context,
    st: &'a State,
    m: &'a mut dyn Module,
    ids: &'a FxHashMap<String, Sym>,
    b: FunctionBuilder<'b>,
    vals: FxHashMap<Value, Vals>,
    cconst: FxHashMap<Value, Vals>,
    blocks: FxHashMap<Ptr<BasicBlock>, Block>,
    frefs: FxHashMap<FuncId, FuncRef>,
    /// Call-only refs marked colocated: direct `call` (PLT32 under PIC) instead of
    /// loading the callee from the GOT and calling through a register.
    cfrefs: FxHashMap<FuncId, FuncRef>,
    gvs: FxHashMap<DataId, GlobalValue>,
    terminated: bool,
    cc: CallConv,
    /// Frontend target config: pointer width and callconv for emitted CLIF.
    /// The wasm path sets a 32-bit-pointer config so `func_addr`/
    /// `symbol_value`/`stack_addr`/call sigs produce i32 values matching
    /// the i32 leaf types (the bookkeeping `ObjectModule`'s ISA stays the
    /// host's).
    tcfg: TargetFrontendConfig,
    /// Local functions using our internal ABI (`CallConv::Tail`).
    internal: &'a rustc_hash::FxHashSet<String>,
    /// This function's own convention is `CallConv::Tail` (it is internal), so
    /// `return_call`/`return_call_indirect` are legal in its body.
    own_tail: bool,
    /// Lowering for the wasm backend (i32 pointers, wasm32 C ABI): 128-bit
    /// libcall results go through a hidden sret pointer arg.
    wasm: bool,
    exn: Option<cranelift_frontend::Variable>,
    /// Promoted allocas (sroa.rs): one variable per scalar leaf.
    vars: FxHashMap<Value, Vec<(cranelift_frontend::Variable, ClType)>>,
    /// 16-byte atomic helper stubs this function needs (aarch64 only).
    stubs: rustc_hash::FxHashSet<&'static str>,
    /// Results of `!nonnull` memory loads.
    nonnull: FxHashSet<cranelift_codegen::ir::Value>,
    /// Results of `bool` loads known to be 0 or 1.
    bool01: FxHashSet<cranelift_codegen::ir::Value>,
    /// Inbounds GEP result → base (null only if the base is).
    derived: FxHashMap<cranelift_codegen::ir::Value, cranelift_codegen::ir::Value>,
    /// Entry params pointing at frozen memory → dereferenceable bytes.
    frozen: FxHashMap<cranelift_codegen::ir::Value, u64>,
    /// Entry params rustc marks `noalias`.
    noalias: FxHashSet<cranelift_codegen::ir::Value>,
    /// `noalias` params' (dereferenceable bytes, writable) — 0 bytes = unknown.
    deref: FxHashMap<cranelift_codegen::ir::Value, (u64, bool)>,
    /// FuncRefs of calls to write-free functions (nowrite.rs).
    nowrite: FxHashMap<FuncRef, bool>,
    /// FuncRefs of calls → bitmask of arg positions bound to a callee param
    /// rustc marks `noalias readonly`: the callee can't write the pointee.
    call_frozen: FxHashMap<FuncRef, u64>,
    /// CLIF load inst → inlined-clone region id (`State::frozen_loads`):
    /// the load reads a `noalias readonly` pointee for the clone's
    /// duration, so LICM may hoist it to an in-region preheader without
    /// proving every in-loop writer can't clobber it.
    scoped: FxHashMap<ir::Inst, Ptr<BasicBlock>>,
    /// CLIF load insts from `!invariant.load` (`State::invariant`): the
    /// pointee never changes while the load's scope is live, so their
    /// locations forward across any store.
    invariant: FxHashSet<ir::Inst>,
    /// CLIF block → inlined-clone region id (`State::region_blocks`), the
    /// same id space `scoped` uses: pliron clone-entry block ptr.
    block_region: FxHashMap<ir::Block, Ptr<BasicBlock>>,
    /// `llvm.lifetime.{start,end}` markers, as (pointer operand, anchor
    /// instruction, block, is_start); the pointer is a pliron value — its
    /// CLIF form is resolved after lowering since an inlined alloca may
    /// lower after its marker. slotmerge turns these into use positions
    /// for the slot's live interval (MIR StorageLive/Dead).
    lifetimes: Vec<(Value, Option<ir::Inst>, ir::Block, bool)>,
    /// (call inst, arg index) pairs whose callee param rustc deduced
    /// `captures(none)` for — the pointer can't outlive the call, so
    /// slotmerge bounds the use instead of escaping the slot to
    /// function end.
    nocap: FxHashSet<(ir::Inst, u32)>,
    /// Hidden buffer-pointer param of a C-variadic function (`pliron.va.buf`).
    va_buf: Option<ir::Value>,
    /// `__pliron_va8.<sym>` trampoline imports declared for this module.
    va_fids: FxHashMap<String, FuncId>,
    /// Emulated EH on COFF: DataId of the `__pliron_eh` TLS slot.
    eh_data: Option<DataId>,
    /// Entry-block-hoisted `&__pliron_eh` address; all EH helpers read this.
    eh_tls_v: Option<ir::Value>,
    /// Shared early-return block used to propagate a pending "exception".
    eject: Option<Block>,
}

impl<'a, 'b> FnLower<'a, 'b> {
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
            // Unmapped blocks belong to the outermost region — the function
            // body — anchored at its entry so licm's ancestry walk bottoms
            // out here.
            let r = self.st.region_blocks.get(pb).copied().unwrap_or(pblocks[0]);
            self.block_region.insert(cb, r);
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
                // A cloned arg bound to a `noalias readonly` callee param
                // (`inline::frozen_args`): the freeze holds only inside the
                // clone, so seed `noalias`/`deref` — licm's hoists stay
                // inside the region — rather than `frozen`'s function-wide
                // `readonly can_move` load flags.
                if let Some(rs) = self.st.region_frozen.get(&a) {
                    for &(sub, bytes, _) in rs {
                        let Some(&v) = vs.get(sub as usize) else {
                            continue;
                        };
                        if crate::env_set("PLIRON_FROZEN_DEBUG") {
                            eprintln!("region_frozen seed {v} leaf {sub} bytes {bytes} blk {cb:?}");
                        }
                        self.noalias.insert(v);
                        self.deref.insert(v, (bytes, false));
                    }
                }
                // Any `noalias` callee param — the pivot/slice scope split
                // described on `State::region_noalias`.
                if let Some(subs) = self.st.region_noalias.get(&a) {
                    for &sub in subs {
                        let Some(&v) = vs.get(sub as usize) else {
                            continue;
                        };
                        if crate::env_set("PLIRON_FROZEN_DEBUG") {
                            eprintln!(
                                "region_noalias seed {v} leaf {sub} blk {cb:?} pb={:?} a={a:?} nargs={}",
                                *pb,
                                pb.deref(ctx).arguments().count()
                            );
                        }
                        self.noalias.insert(v);
                    }
                }
                self.vals.insert(a, vs);
            }
        }
        let pt = self.tcfg.pointer_type();
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
            // `noalias` applies to the pointee; for wide refs (`&mut [T]`,
            // `&mut dyn`) the first leaf is the data pointer.
            if n >= 1
                && let Some(&(d, w)) = self.st.noalias.get(&arg)
            {
                self.noalias.insert(params[i]);
                self.deref.insert(params[i], (d, w));
            }
            // `nonnull` arg leaves (`&str`/`&T` data ptrs, `Box`, `NonNull`):
            // seed them like `!nonnull` load results so jumpthread's
            // lattice folds the `Option<ptr>` niche selects iterator
            // adapters emit (`Chars`, `slice::Iter`).
            if let Some(&mask) = self.st.nnargs.get(&arg) {
                for (k, &p) in params[i..i + n].iter().enumerate() {
                    if mask & (1 << k) != 0 {
                        self.nonnull.insert(p);
                    }
                }
            }
            self.vals.insert(arg, params[i..i + n].into());
            i += n;
        }
        if i < params.len() {
            // Variadic functions carry one extra hidden param (see make_sig).
            self.va_buf = Some(params[i]);
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
                if self.eh_data.is_some() {
                    // Resolve the TLS slot once at entry: `tls_value` isn't
                    // CSE-able across calls, and every post-call flag check
                    // would otherwise redo the `_tls_index`/`gs:[0x58]` chain.
                    let v = self.eh_tls_addr();
                    self.eh_tls_v = Some(v);
                }
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
        if let Some(eject) = self.eject {
            self.b.switch_to_block(eject);
            self.b.set_cold_block(eject);
            let tys: Vec<ClType> = self
                .b
                .func
                .signature
                .returns
                .iter()
                .map(|r| r.value_type)
                .collect();
            let rets: Vec<ir::Value> = tys.into_iter().map(|t| self.iconst_any(t, 0)).collect();
            self.b.ins().return_(&rets);
        }
        self.b.seal_all_blocks();
    }

    /// Address of `__pliron_eh`, the TLS slot driving emulated EH on COFF:
    /// `{ flag: i32, _pad: i32, exn: ptr, buf: [u8; 32] }` (defined by the
    /// allocator module; see `codegen_allocator` in lib.rs). On wasm the
    /// slot is a plain data symbol the linker synthesizes —
    /// `{ flag: i32, exn: ptr }` — reached via `symbol_value` instead.
    /// Entry-block-only — see `eh_tls_v`; all post-entry users read the
    /// hoisted address.
    fn eh_tls_addr(&mut self) -> ir::Value {
        let id = self.eh_data.expect("emulated EH without __pliron_eh");
        let gv = *self
            .gvs
            .entry(id)
            .or_insert_with(|| self.m.declare_data_in_func(id, self.b.func));
        if self.wasm {
            self.b.ins().symbol_value(self.tcfg.pointer_type(), gv)
        } else {
            self.b.ins().tls_value(self.tcfg.pointer_type(), gv)
        }
    }

    fn eh_tls(&mut self) -> ir::Value {
        self.eh_tls_v.expect("eh_tls used outside entry block")
    }

    /// After a possibly-throwing call, `__pliron_eh.flag != 0` means a
    /// "throw" is in flight: branch to the `catch` funclet block for an
    /// invoke, or to the shared early-return block to keep propagating.
    fn eh_check(&mut self, catch: Option<Ptr<BasicBlock>>) {
        let tls = self.eh_tls();
        let f = self.b.ins().load(clt::I32, MemFlagsData::new(), tls, 0);
        let cont = self.b.create_block();
        let bad = match catch {
            Some(pb) => self.blocks[&pb],
            None => *self.eject.get_or_insert_with(|| self.b.create_block()),
        };
        self.b.ins().brif(f, bad, &[], cont, &[]);
        self.b.switch_to_block(cont);
        self.cconst.clear();
    }

    /// Emulated throw: set `__pliron_eh.flag` and unwind by early-return.
    /// Callers that change the exception in flight store to `tls + 8` first.
    fn eh_raise(&mut self) {
        let tls = self.eh_tls();
        let one = self.b.ins().iconst(clt::I32, 1);
        self.b.ins().store(MemFlagsData::new(), one, tls, 0);
        let eject = *self.eject.get_or_insert_with(|| self.b.create_block());
        self.b.ins().jump(eject, &[]);
        self.terminated = true;
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

    /// f16/f128 have no native `fabs`; clear the top bit of the bit pattern.
    fn fabs_wide(&mut self, x: ir::Value) -> ir::Value {
        let t = self.b.func.dfg.value_type(x);
        let mf = MemFlagsData::new();
        if t == clt::F16 {
            let i = self.b.ins().bitcast(clt::I16, mf, x);
            let r = self.b.ins().band_imm_u(i, 0x7fff);
            self.b.ins().bitcast(t, mf, r)
        } else {
            let i = self.b.ins().bitcast(clt::I128, mf, x);
            let (lo, hi) = self.b.ins().isplit(i);
            let hi = self.b.ins().band_imm_s(hi, i64::MAX);
            let r = self.b.ins().iconcat(lo, hi);
            self.b.ins().bitcast(t, mf, r)
        }
    }

    /// f16/f128 have no native `fcopysign`; move `b`'s sign bit onto `a`.
    fn fcopysign_wide(&mut self, a: ir::Value, b: ir::Value) -> ir::Value {
        let t = self.b.func.dfg.value_type(a);
        let mf = MemFlagsData::new();
        if t == clt::F16 {
            let i = self.b.ins().bitcast(clt::I16, mf, a);
            let j = self.b.ins().bitcast(clt::I16, mf, b);
            let v = self.b.ins().band_imm_u(i, 0x7fff);
            let s = self.b.ins().band_imm_u(j, 0x8000);
            let r = self.b.ins().bor(v, s);
            self.b.ins().bitcast(t, mf, r)
        } else {
            let i = self.b.ins().bitcast(clt::I128, mf, a);
            let (lo, hi) = self.b.ins().isplit(i);
            let hi = self.b.ins().band_imm_s(hi, i64::MAX);
            let j = self.b.ins().bitcast(clt::I128, mf, b);
            let (_, jhi) = self.b.ins().isplit(j);
            let jhi = self.b.ins().band_imm_s(jhi, i64::MIN);
            let hi = self.b.ins().bor(hi, jhi);
            let r = self.b.ins().iconcat(lo, hi);
            self.b.ins().bitcast(t, mf, r)
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
    fn dense_switch(
        &mut self,
        x: ir::Value,
        vals: &[(u128, Block)],
        d: Block,
        dead_default: bool,
    ) -> bool {
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
        // A default that can't happen (enum discriminants): pad the table to a
        // power of two and mask the index, so Cranelift needs no bounds clamp.
        static MASK: std::sync::LazyLock<bool> =
            std::sync::LazyLock::new(|| crate::pass_enabled("PLIRON_SWITCH_MASK"));
        if dead_default && *MASK {
            let p = span.next_power_of_two();
            let mut idx = if lo == 0 {
                x
            } else {
                self.b.ins().iadd_imm_s(x, (lo as i64).wrapping_neg())
            };
            if ty.bits() > 32 {
                idx = self.b.ins().ireduce(clt::I32, idx);
            } else if ty.bits() < 32 {
                idx = self.b.ins().uextend(clt::I32, idx);
            }
            let idx = self.b.ins().band_imm_u(idx, (p - 1) as i64);
            let mut table = vec![d; p as usize];
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
            return true;
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
        let (b, w) = self.const_bits(v, 0)?;
        Some(if w < 128 && (b >> (w - 1)) & 1 == 1 {
            (b | (!0u128 << w)) as i128
        } else {
            b as i128
        })
    }

    /// The constant bit pattern and width of `v`, evaluating a bounded
    /// chain of pure ops (`icmp`/`select`/casts/int binops) whose operands
    /// are themselves constant — `7u128.max(1)` arrives as
    /// `select(icmp ugt 7, 1, 7, 1)`, not a literal.
    fn const_bits(&self, v: Value, depth: u32) -> Option<(u128, u32)> {
        let width = |ty: TypeHandle| match classify(self.ctx, ty) {
            TyK::Int(w) => w.min(128),
            _ => 64,
        };
        let int_ty = |v: Value| width(v.get_type(self.ctx));
        if let Some(cv) = self.st.consts.get(&v) {
            let w = int_ty(v);
            return match cv {
                ConstVal::Bits(b) => Some((mask(*b, w), w)),
                ConstVal::Zero | ConstVal::Undef => Some((0, w)),
                _ => None,
            };
        }
        if depth >= 8 {
            return None;
        }
        let op = v.defining_op()?;
        let ctx = self.ctx;
        let id = Operation::get_opid(op, ctx);
        if crate::env_set("PLIRON_CONSTDBG") {
            eprintln!("const_bits {}: {}", depth, id);
        }
        let ops: Vec<Value> = op.deref(ctx).operands().collect();
        let bit = |v| self.const_bits(v, depth + 1);
        let sext = |(b, w): (u128, u32)| -> i128 {
            if w < 128 && (b >> (w - 1)) & 1 == 1 {
                (b | (!0u128 << w)) as i128
            } else {
                b as i128
            }
        };
        let r = if id == SelectOp::get_opid_static() {
            let (c, _) = bit(ops[0])?;
            bit(if c & 1 == 1 { ops[1] } else { ops[2] })?.0
        } else if id == ICmpOp::get_opid_static() {
            use ICmpPredicateAttr as P;
            let pred = Operation::get_op::<ICmpOp>(op, ctx).unwrap().predicate(ctx);
            let (a, wa) = bit(ops[0])?;
            let (b, _) = bit(ops[1])?;
            let (sa, sb) = (sext((a, wa)), sext((b, wa)));
            let r = match pred {
                P::EQ => a == b,
                P::NE => a != b,
                P::SLT => sa < sb,
                P::SLE => sa <= sb,
                P::SGT => sa > sb,
                P::SGE => sa >= sb,
                P::ULT => a < b,
                P::ULE => a <= b,
                P::UGT => a > b,
                P::UGE => a >= b,
            };
            return Some((u128::from(r), 1));
        } else if id == LoadOp::get_opid_static() && !self.st.volatile.contains(&op) {
            // `const 7i128` operands codegen_ssa spills to an alloca come
            // back as loads. Whenever the load executes before any store it
            // reads uninitialized memory (UB), so the value is determined by
            // whichever constant stores can run before it.
            if !self.st.allocas.contains_key(&ops[0]) {
                return None;
            }
            let lb = op.deref(ctx).get_parent_block()?;
            let mut stores = Vec::new();
            for u in ops[0].uses(ctx) {
                let uop = u.user_op();
                let uid = Operation::get_opid(uop, ctx);
                // Loads read whatever the matching stores wrote; lifetime
                // markers don't touch memory.
                if uid == LoadOp::get_opid_static()
                    || self
                        .st
                        .intrinsics
                        .get(&uop)
                        .is_some_and(|n| n.starts_with("llvm.lifetime"))
                {
                    continue;
                }
                if uid != StoreOp::get_opid_static()
                    || u.find_index(ctx) != 1
                    || self.st.volatile.contains(&uop)
                {
                    return None;
                }
                stores.push((uop, bit(uop.deref(ctx).get_operand(0))?));
            }
            let (pre, rest): (Vec<_>, Vec<_>) = stores
                .iter()
                .partition(|(uop, _)| uop.deref(ctx).get_parent_block() == Some(lb));
            // A store in the load's own block: the latest one before the load
            // wins outright — nothing can write between them.
            let lops: Vec<Ptr<Operation>> = lb.deref(ctx).iter(ctx).collect();
            let lpos = lops.iter().position(|o| *o == op)?;
            let local = pre
                .iter()
                .filter_map(|(uop, s)| {
                    lops.iter()
                        .position(|o| o == uop)
                        .filter(|p| *p < lpos)
                        .map(|p| (p, s))
                })
                .max_by_key(|(p, _)| *p);
            if let Some((_, s)) = local {
                return Some(*s);
            }
            // Stores in other blocks: paths reaching the load either ran one
            // of these or read uninitialized (UB).
            if rest.is_empty() {
                return None;
            }
            if rest.iter().all(|(_, s)| *s == rest[0].1) {
                rest[0].1.0
            } else if rest.len() == 2 {
                let b1 = rest[0].0.deref(ctx).get_parent_block()?;
                let b2 = rest[1].0.deref(ctx).get_parent_block()?;
                if b1 == b2 {
                    // Sequential stores in one block: the later wins.
                    let bl: Vec<Ptr<Operation>> = b1.deref(ctx).iter(ctx).collect();
                    let p1 = bl.iter().position(|o| *o == rest[0].0)?;
                    let p2 = bl.iter().position(|o| *o == rest[1].0)?;
                    if p1 > p2 { rest[0].1.0 } else { rest[1].1.0 }
                } else {
                    // `if c { v_t } else { v_f }` arms: each arm must be
                    // single-entry from one shared `condbr`.
                    let arm = |b: Ptr<BasicBlock>| {
                        let us = b.uses(ctx);
                        (us.len() == 1).then(|| (us[0].user_op(), us[0].find_index(ctx)))
                    };
                    let (t1, i1) = arm(b1)?;
                    let (t2, i2) = arm(b2)?;
                    if t1 != t2
                        || i1 == i2
                        || Operation::get_opid(t1, ctx) != CondBrOp::get_opid_static()
                    {
                        return None;
                    }
                    let (c, _) = bit(t1.deref(ctx).get_operand(0))?;
                    if (i1 == 0) == (c & 1 == 1) {
                        rest[0].1.0
                    } else {
                        rest[1].1.0
                    }
                }
            } else {
                return None;
            }
        } else {
            let Some(&o0) = ops.first() else {
                return None;
            };
            let (a, wa) = bit(o0)?;
            let (b, _) = if ops.len() > 1 { bit(ops[1])? } else { (0, wa) };
            let (sa, sb) = (sext((a, wa)), sext((b, wa)));
            let v = if id == AddOp::get_opid_static() {
                a.wrapping_add(b)
            } else if id == SubOp::get_opid_static() {
                a.wrapping_sub(b)
            } else if id == MulOp::get_opid_static() {
                a.wrapping_mul(b)
            } else if id == AndOp::get_opid_static() {
                a & b
            } else if id == OrOp::get_opid_static() {
                a | b
            } else if id == XorOp::get_opid_static() {
                a ^ b
            } else if id == ShlOp::get_opid_static() && b < wa as u128 {
                a.wrapping_shl(b as u32)
            } else if id == LShrOp::get_opid_static() && b < wa as u128 {
                a.wrapping_shr(b as u32)
            } else if id == AShrOp::get_opid_static() && b < wa as u128 {
                sa.wrapping_shr(b as u32) as u128
            } else if id == UDivOp::get_opid_static() && b != 0 {
                a / b
            } else if id == URemOp::get_opid_static() && b != 0 {
                a % b
            } else if id == SDivOp::get_opid_static() && b != 0 && !(sa == i128::MIN && sb == -1) {
                (sa / sb) as u128
            } else if id == SRemOp::get_opid_static() && b != 0 && !(sa == i128::MIN && sb == -1) {
                (sa % sb) as u128
            } else {
                return None;
            };
            mask(v, wa)
        };
        Some((r, int_ty(v)))
    }

    fn iconst_any(&mut self, t: ClType, bits: u128) -> ir::Value {
        if t.is_vector() {
            let mut bytes = bits.to_le_bytes().to_vec();
            debug_assert!(t.bytes() <= 16 || bits == 0, "wide non-zero iconst {t}");
            bytes.resize(t.bytes() as usize, 0);
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
                let x = self.as_lane(x, t.lane_type());
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
                if lv.len() == 1 {
                    smallvec![self.iconst_any(lv[0].1, bits)]
                } else {
                    // Packed vector constant: `bits` holds the leaves
                    // back-to-back.
                    lv.iter()
                        .map(|&(lo, lt)| {
                            let w = lt.bits() as u64;
                            let m = if w >= 128 {
                                u128::MAX
                            } else {
                                (1u128 << w) - 1
                            };
                            self.iconst_any(lt, bits >> (lo * 8) & m)
                        })
                        .collect()
                }
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

    fn call_fref(&mut self, id: FuncId) -> FuncRef {
        if !crate::pass_enabled("PLIRON_PLTCALL") {
            return self.fref(id);
        }
        if let Some(f) = self.cfrefs.get(&id) {
            return *f;
        }
        let f = self.m.declare_func_in_func(id, self.b.func);
        self.b.func.dfg.ext_funcs[f].colocated = true;
        self.cfrefs.insert(id, f);
        f
    }

    /// Foreign variadic callees can't be given `%al` (the xmm vararg count) by
    /// Cranelift, so calls to them go through a `__pliron_va8.<sym>` stub that
    /// sets it to 8 and tail-jumps (emitted into `st.asm` at emit time).
    /// SysV x86-64 only: on aarch64 there is no register count to fake (Darwin
    /// puts every variadic arg on the stack), so no redirect is needed.
    fn va_sym(&mut self, sym: &str) -> String {
        let foreign_variadic = self.m.isa().triple().architecture
            == target_lexicon::Architecture::X86_64
            && self.st.funcs.get(sym).is_some_and(|f| {
                !has_body(self.ctx, f.op)
                    && matches!(classify(self.ctx, f.ty), TyK::Func(_, _, true))
            });
        if !foreign_variadic {
            return sym.to_string();
        }
        let t = format!("__pliron_va8.{}", crate::obj_sym(sym));
        if !self.va_fids.contains_key(&t) {
            let sig = Signature::new(self.cc);
            let id = self.m.declare_function(&t, Linkage::Import, &sig).unwrap();
            self.va_fids.insert(t.clone(), id);
        }
        let macho = self.m.isa().triple().binary_format == target_lexicon::BinaryFormat::Macho;
        VA_TRAMPS.with(|v| v.borrow_mut().insert(va_tramp_asm(sym, macho)));
        t
    }

    fn sym_addr(&mut self, sym: &str) -> ir::Value {
        let sym = self.va_sym(sym);
        let pt = self.tcfg.pointer_type();
        if let Some(&id) = self.va_fids.get(&sym) {
            let fr = self.fref(id);
            return self.b.ins().func_addr(pt, fr);
        }
        match self
            .ids
            .get(&sym)
            .copied()
            .unwrap_or_else(|| panic!("unknown symbol {sym}"))
        {
            Sym::F(id, _) => {
                let fr = self.fref(id);
                self.b.ins().func_addr(pt, fr)
            }
            Sym::D(id, tls) => {
                let gv = *self
                    .gvs
                    .entry(id)
                    .or_insert_with(|| self.m.declare_data_in_func(id, self.b.func));
                if tls && !self.wasm {
                    self.b.ins().tls_value(pt, gv)
                } else {
                    self.b.ins().symbol_value(pt, gv)
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
        // MSVC has no i128/f128 ABI: LLVM passes them byval through pointers
        // and returns them in xmm0. Cranelift's ABI doesn't model that, so
        // spill wide args to stack slots and pass the pointers ourselves, and
        // read the result back as a vector.
        let coff = self.m.isa().triple().binary_format == target_lexicon::BinaryFormat::Coff;
        let wideof = |t: ClType| t == clt::I128 || t == clt::F128;
        let wide = coff && (params.iter().any(|t| wideof(*t)) || rets.iter().any(|t| wideof(*t)));
        let mut sig = Signature::new(self.cc);
        let mut call_args: Vec<ir::Value> = Vec::new();
        for (t, a) in params.iter().zip(args.iter()) {
            if wide && wideof(*t) {
                let s = self.slot(16, 16);
                self.b.ins().store(MemFlagsData::trusted(), *a, s, 0);
                sig.params.push(AbiParam::new(self.tcfg.pointer_type()));
                call_args.push(s);
            } else {
                sig.params.push(AbiParam::new(*t));
                call_args.push(*a);
            }
        }
        sig.returns.extend(
            rets.iter()
                .map(|t| AbiParam::new(if wide && wideof(*t) { clt::I8X16 } else { *t })),
        );
        // wasm32's C ABI returns 128-bit scalars through a hidden sret
        // pointer, so a declared `(i128,i128)->i128` sig would clash with the
        // real compiler-rt definition. Pass a slot and load the result back.
        let wsret = self.wasm && rets.iter().any(|t| *t == clt::I128 || *t == clt::F128);
        let sret_slot = wsret.then(|| {
            let s = self.slot(16 * rets.len() as u64, 16);
            sig.params.insert(
                0,
                AbiParam::special(self.tcfg.pointer_type(), ArgumentPurpose::StructReturn),
            );
            call_args.insert(0, s);
            sig.returns.clear();
            s
        });
        let id = self
            .m
            .declare_function(crate::obj_sym(name), Linkage::Import, &sig)
            .unwrap();
        let fr = self.call_fref(id);
        let c = self.b.ins().call(fr, &call_args);
        if let Some(s) = sret_slot {
            return rets
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    self.b
                        .ins()
                        .load(*t, MemFlagsData::trusted(), s, i as i32 * 16)
                })
                .collect();
        }
        let results: Vec<ir::Value> = self.b.inst_results(c).to_vec();
        results
            .iter()
            .zip(rets)
            .map(|(v, t)| {
                if wide && wideof(*t) {
                    // Lane-count-changing bitcasts need a byte order; a
                    // store+load through a slot is simpler.
                    let s = self.slot(16, 16);
                    self.b.ins().store(MemFlagsData::trusted(), *v, s, 0);
                    self.b.ins().load(*t, MemFlagsData::trusted(), s, 0)
                } else {
                    *v
                }
            })
            .collect()
    }

    /// Integer `div`/`rem` elementwise, with an i128 fast path: constant
    /// unsigned divisors become limb magic-multiply sequences instead of
    /// `__udivti3`/`__umodti3` libcalls (~15 insts vs ~100+ cycles).
    fn int_divrem(
        &mut self,
        op: Ptr<Operation>,
        opnds: Vec<Value>,
        lib: &str,
        unsigned: bool,
        rem: bool,
    ) {
        let a = self.get(opnds[0]);
        let b = self.get(opnds[1]);
        let mut r = Vals::new();
        for (x, y) in a.iter().zip(b.iter()) {
            let v = if self.b.func.dfg.value_type(*x) == clt::I128 {
                let d = unsigned
                    .then(|| self.const_int(opnds[1]).map(|d| d as u128))
                    .flatten();
                d.and_then(|d| self.div_u128_const(*x, d, rem))
                    .unwrap_or_else(|| {
                        self.libcall(lib, &[clt::I128, clt::I128], &[clt::I128], &[*x, *y])[0]
                    })
            } else {
                match (unsigned, rem) {
                    (true, false) => self.b.ins().udiv(*x, *y),
                    (true, true) => self.b.ins().urem(*x, *y),
                    (false, false) => self.b.ins().sdiv(*x, *y),
                    (false, true) => self.b.ins().srem(*x, *y),
                }
            };
            r.push(v);
        }
        self.set(op, r);
    }

    /// Unsigned `x / d` or `x % d` for i128 `x` and constant `d`, expanded to
    /// 64-bit limb multiplies (LLVM's `BuildUDIV`): powers of two shift,
    /// otherwise `q = mulhi(x, M) >> s` with the `(x - q)/2 + q` fixup when
    /// the magic needs a 129th bit. `None` asks the caller to libcall.
    ///
    /// Small odd divisors take a cheaper route first (LLVM's own choice on
    /// aarch64): fold `x`'s width-`w` limbs into a u64 congruent mod `d`
    /// (`2^w ≡ 1 (mod d)`), one `urem.i64` finishes the remainder, and the
    /// quotient is `(x - r)·d⁻¹ mod 2^128` — a plain `imul` — instead of the
    /// four-`umulhi` carry chain a full 128×128 magic multiply costs.
    fn div_u128_const(&mut self, x: ir::Value, d: u128, rem: bool) -> Option<ir::Value> {
        if d == 0 {
            return None;
        }
        // Even divisors: pull the shared power of two out of x first.
        let pre = d.trailing_zeros();
        let d_odd = d >> pre;
        let xs = if pre > 0 { self.ushr128_imm(x, pre) } else { x };
        if d_odd > 1 && fold_width(d_odd).is_some() {
            let r = self.rem_u128_fold(xs, d_odd);
            if rem && pre == 0 {
                return Some(self.b.ins().uextend(clt::I128, r));
            }
            // `xs - r` is exactly `q·d`, so one wrap-multiply by the
            // inverse mod 2^128 yields the quotient.
            let r128 = self.b.ins().uextend(clt::I128, r);
            let xr = self.b.ins().isub(xs, r128);
            let inv = self.iconst_any(clt::I128, modinv_u128(d_odd));
            let q = self.b.ins().imul(xr, inv);
            if !rem {
                return Some(q);
            }
            let dc = self.iconst_any(clt::I128, d);
            let qd = self.b.ins().imul(q, dc);
            return Some(self.b.ins().isub(x, qd));
        }
        let mut q = xs;
        if d_odd > 1 {
            let (m, s, add) = magicu128(d_odd);
            let (xl, xh) = self.b.ins().isplit(xs);
            let ml = self.b.ins().iconst(clt::I64, m as u64 as i64);
            let mh = self.b.ins().iconst(clt::I64, (m >> 64) as u64 as i64);
            q = self.mulhi128(xl, xh, ml, mh);
            if add {
                // True magic is 2^128 + M; recover the missing term as
                // q = ((x - mulhi(x, M)) >> 1) + mulhi(x, M).
                let diff = self.b.ins().isub(xs, q);
                let half = self.ushr128_imm(diff, 1);
                q = self.b.ins().iadd(half, q);
            }
            if s > 0 {
                q = self.ushr128_imm(q, s);
            }
        }
        if !rem {
            return Some(q);
        }
        let dc = self.iconst_any(clt::I128, d);
        let qd = self.b.ins().imul(q, dc);
        Some(self.b.ins().isub(x, qd))
    }

    /// `x mod d` as a u64, for odd `d` with `fold_width(d) = Some(w)`:
    /// `x ≡ Σ` limbs of `x` at width `w` `(mod d)` because `2^w ≡ 1`.
    /// The sum is below `2^64` for every legal `w`; `urem.i64` shrinks it
    /// to `[0, d)` unless `d ≥ 2^64` already bounds it.
    fn rem_u128_fold(&mut self, x: ir::Value, d: u128) -> ir::Value {
        let w = fold_width(d).expect("fold_width checked by caller");
        let (lo, hi) = self.b.ins().isplit(x);
        let n = 128usize.div_ceil(w as usize);
        let mut acc = self.limb64(lo, hi, 0, w);
        for i in 1..n as u32 {
            let l = self.limb64(lo, hi, i * w, w);
            acc = self.b.ins().iadd(acc, l);
        }
        if d > u64::MAX as u128 {
            acc
        } else {
            let dc = self.b.ins().iconst(clt::I64, d as u64 as i64);
            self.b.ins().urem(acc, dc)
        }
    }

    /// Bits `[j, j+w)` of the i128 `(lo, hi)` pair as a u64 — an `extr`
    /// pair for limbs straddling the limb boundary, a masked shift for
    /// the rest. `w ≤ 62` keeps the mask and the shifted-out top legal.
    fn limb64(&mut self, lo: ir::Value, hi: ir::Value, j: u32, w: u32) -> ir::Value {
        let mask = (1u64 << w) - 1;
        let v = if j >= 64 {
            self.b.ins().ushr_imm_u(hi, (j - 64) as i64)
        } else if j + w <= 64 {
            let l = self.b.ins().ushr_imm_u(lo, j as i64);
            if j + w == 64 {
                l
            } else {
                let m = self.b.ins().iconst(clt::I64, mask as i64);
                self.b.ins().band(l, m)
            }
        } else {
            let l = self.b.ins().ushr_imm_u(lo, j as i64);
            let h = self.b.ins().ishl_imm_u(hi, (64 - j) as i64);
            let v = self.b.ins().bor(l, h);
            let m = self.b.ins().iconst(clt::I64, mask as i64);
            self.b.ins().band(v, m)
        };
        if j >= 64 && j + w < 128 {
            let m = self.b.ins().iconst(clt::I64, mask as i64);
            self.b.ins().band(v, m)
        } else {
            v
        }
    }

    /// High 128 bits of the 256-bit product of two u128s given as (lo, hi)
    /// u64 limbs: `p3hi : (p1h + p2h + p3l + carries)` where carries bubble
    /// up out of the bits-[127:64] partial sums.
    fn mulhi128(
        &mut self,
        xl: ir::Value,
        xh: ir::Value,
        ml: ir::Value,
        mh: ir::Value,
    ) -> ir::Value {
        let t0 = self.b.ins().umulhi(xl, ml);
        let p1l = self.b.ins().imul(xl, mh);
        let p1h = self.b.ins().umulhi(xl, mh);
        let p2l = self.b.ins().imul(xh, ml);
        let p2h = self.b.ins().umulhi(xh, ml);
        let p3l = self.b.ins().imul(xh, mh);
        let p3h = self.b.ins().umulhi(xh, mh);
        // Bits [191:128] take carry from the [127:64] column.
        let (s1, c1) = self.add_carry64(t0, p1l);
        let (_, c2) = self.add_carry64(s1, p2l);
        let cs = self.b.ins().iadd(c1, c2);
        let (h1, c3) = self.add_carry64(p1h, p2h);
        let (h2, c4) = self.add_carry64(h1, p3l);
        let (h, c5) = self.add_carry64(h2, cs);
        let c45 = self.b.ins().iadd(c4, c5);
        let ch = self.b.ins().iadd(c3, c45);
        let hi = self.b.ins().iadd(p3h, ch);
        self.b.ins().iconcat(h, hi)
    }

    /// `(sum, carry)` for two u64s: `a + b` wraps iff `sum < a`.
    fn add_carry64(&mut self, a: ir::Value, b: ir::Value) -> (ir::Value, ir::Value) {
        let s = self.b.ins().iadd(a, b);
        let c = self.b.ins().icmp(IntCC::UnsignedLessThan, s, a);
        (s, self.b.ins().uextend(clt::I64, c))
    }

    /// `x >> imm` on i128 via its 64-bit limbs: the generic i128 shift
    /// legalization treats constant amounts as variable and emits a
    /// `tst`/`csel` dance, while limb shifts are 2-3 real insts.
    fn ushr128_imm(&mut self, x: ir::Value, imm: u32) -> ir::Value {
        debug_assert!(imm > 0 && imm < 128);
        let (lo, hi) = self.b.ins().isplit(x);
        let (lo, hi) = if imm >= 64 {
            let lo = self.b.ins().ushr_imm_u(hi, (imm - 64) as i64);
            (lo, self.b.ins().iconst(clt::I64, 0))
        } else {
            let l = self.b.ins().ushr_imm_u(lo, imm as i64);
            let c = self.b.ins().ishl_imm_u(hi, (64 - imm) as i64);
            let lo = self.b.ins().bor(l, c);
            (lo, self.b.ins().ushr_imm_u(hi, imm as i64))
        };
        self.b.ins().iconcat(lo, hi)
    }

    /// Cranelift can't lower 16-byte atomic ops on aarch64: route them through
    /// the `ldxp`/`stlxp` assembly stubs emitted by `lower_to_object`.
    fn needs_atomic_stub(&self, t: ClType) -> bool {
        t == clt::I128
            && matches!(
                self.m.isa().triple().architecture,
                target_lexicon::Architecture::Aarch64(_)
            )
    }

    fn atomic_load_stub(&mut self, p: ir::Value, o: u64) -> ir::Value {
        let tmp = self.slot(16, 16);
        let p = if o > 0 {
            self.b.ins().iadd_imm_s(p, o as i64)
        } else {
            p
        };
        self.stubs.insert("__pliron_a64_atomic_ld_16");
        self.libcall(
            "__pliron_a64_atomic_ld_16",
            &[clt::I64, clt::I64],
            &[],
            &[p, tmp],
        );
        self.b
            .ins()
            .load(clt::I128, MemFlagsData::trusted(), tmp, 0)
    }

    fn atomic_store_stub(&mut self, p: ir::Value, o: u64, v: ir::Value) {
        let tmp = self.slot(16, 16);
        self.b.ins().store(MemFlagsData::trusted(), v, tmp, 0);
        let p = if o > 0 {
            self.b.ins().iadd_imm_s(p, o as i64)
        } else {
            p
        };
        self.stubs.insert("__pliron_a64_atomic_st_16");
        self.libcall(
            "__pliron_a64_atomic_st_16",
            &[clt::I64, clt::I64],
            &[],
            &[p, tmp],
        );
    }

    fn atomic_cas_stub(&mut self, op: Ptr<Operation>, p: ir::Value, c: ir::Value, n: ir::Value) {
        let mf = MemFlagsData::trusted();
        let exp = self.slot(16, 16);
        let des = self.slot(16, 16);
        self.b.ins().store(mf, c, exp, 0);
        self.b.ins().store(mf, n, des, 0);
        self.stubs.insert("__pliron_a64_atomic_cas_16");
        let ok = self.libcall(
            "__pliron_a64_atomic_cas_16",
            &[clt::I64, clt::I64, clt::I64],
            &[clt::I8],
            &[p, exp, des],
        )[0];
        let old = self.b.ins().load(clt::I128, mf, exp, 0);
        self.set(op, smallvec![old, ok]);
    }

    fn atomic_rmw_stub(
        &mut self,
        op: Ptr<Operation>,
        p: ir::Value,
        v: ir::Value,
        k: cranelift_codegen::ir::AtomicRmwOp,
    ) {
        use cranelift_codegen::ir::AtomicRmwOp as R;
        let mf = MemFlagsData::trusted();
        let exp = self.slot(16, 16);
        let des = self.slot(16, 16);
        self.stubs.insert("__pliron_a64_atomic_ld_16");
        self.stubs.insert("__pliron_a64_atomic_cas_16");
        self.libcall(
            "__pliron_a64_atomic_ld_16",
            &[clt::I64, clt::I64],
            &[],
            &[p, exp],
        );
        let retry = self.b.create_block();
        let done = self.b.create_block();
        self.b.ins().jump(retry, &[]);
        self.b.switch_to_block(retry);
        let old = self.b.ins().load(clt::I128, mf, exp, 0);
        let new = match k {
            R::Xchg => v,
            R::Add => self.b.ins().iadd(old, v),
            R::Sub => self.b.ins().isub(old, v),
            R::And => self.b.ins().band(old, v),
            R::Nand => {
                let a = self.b.ins().band(old, v);
                self.b.ins().bnot(a)
            }
            R::Or => self.b.ins().bor(old, v),
            R::Xor => self.b.ins().bxor(old, v),
            // aarch64 lowers no i128 select/minmax: compare hi, then lo on tie.
            R::Smax => self.minmax128(IntCC::SignedGreaterThan, old, v),
            R::Smin => self.minmax128(IntCC::SignedLessThan, old, v),
            R::Umax => self.minmax128(IntCC::UnsignedGreaterThan, old, v),
            R::Umin => self.minmax128(IntCC::UnsignedLessThan, old, v),
        };
        self.b.ins().store(mf, new, des, 0);
        let ok = self.libcall(
            "__pliron_a64_atomic_cas_16",
            &[clt::I64, clt::I64, clt::I64],
            &[clt::I8],
            &[p, exp, des],
        )[0];
        self.b.ins().brif(ok, done, &[], retry, &[]);
        self.b.switch_to_block(done);
        let r = self.b.ins().load(clt::I128, mf, exp, 0);
        self.set1(op, r);
    }

    /// `a cc b ? a : b` for i128 via i64 halves; `cc` selects hi-word ordering,
    /// the lo word always compares unsigned.
    fn minmax128(&mut self, cc: IntCC, a: ir::Value, b: ir::Value) -> ir::Value {
        let (a_lo, a_hi) = self.b.ins().isplit(a);
        let (b_lo, b_hi) = self.b.ins().isplit(b);
        let hi = self.b.ins().icmp(cc, a_hi, b_hi);
        let eq = self.b.ins().icmp(IntCC::Equal, a_hi, b_hi);
        let lo_cc = match cc {
            IntCC::SignedGreaterThan | IntCC::UnsignedGreaterThan => IntCC::UnsignedGreaterThan,
            _ => IntCC::UnsignedLessThan,
        };
        let lo = self.b.ins().icmp(lo_cc, a_lo, b_lo);
        let c = self.b.ins().select(eq, lo, hi);
        let lo = self.b.ins().select(c, a_lo, b_lo);
        let hi = self.b.ins().select(c, a_hi, b_hi);
        self.b.ins().iconcat(lo, hi)
    }

    fn set(&mut self, op: Ptr<Operation>, vals: Vals) {
        let r = op.deref(self.ctx).get_result(0);
        self.vals.insert(r, vals);
    }

    fn set1(&mut self, op: Ptr<Operation>, v: ir::Value) {
        self.set(op, smallvec![v]);
    }

    /// `op` is a pure int-producing op in `const_bits`' evaluable set (the
    /// loads const_bits resolves are operands, not top-level results).
    fn op_int_constable(&self, op: Ptr<Operation>, id: &OpId) -> bool {
        let eval = [
            ICmpOp::get_opid_static(),
            SelectOp::get_opid_static(),
            AddOp::get_opid_static(),
            SubOp::get_opid_static(),
            MulOp::get_opid_static(),
            AndOp::get_opid_static(),
            OrOp::get_opid_static(),
            XorOp::get_opid_static(),
            ShlOp::get_opid_static(),
            LShrOp::get_opid_static(),
            AShrOp::get_opid_static(),
            UDivOp::get_opid_static(),
            URemOp::get_opid_static(),
            SDivOp::get_opid_static(),
            SRemOp::get_opid_static(),
        ];
        if !eval.contains(id) {
            return false;
        }
        matches!(classify(self.ctx, self.res_ty(op)), TyK::Int(_))
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
        macro_rules! float_binop {
            ($m:ident, $tf:expr) => {{
                let a = self.get(opnds[0]);
                let b = self.get(opnds[1]);
                let mut r = Vals::new();
                for (x, y) in a.iter().zip(b.iter()) {
                    let t = self.b.func.dfg.value_type(*x);
                    // Cranelift can't do f16/f128 arithmetic, and the
                    // compiler-builtins f16 set is incomplete (`__divhf3` is
                    // missing), so f16 promotes to f32 — exact in, single
                    // rounding out — while f128 uses the quad helpers.
                    let v = if t == clt::F16 {
                        let x = self.h2f(*x);
                        let y = self.h2f(*y);
                        let v = self.b.ins().$m(x, y);
                        self.f2h(v)
                    } else if t == clt::F128 {
                        self.libcall($tf, &[t, t], &[t], &[*x, *y])[0]
                    } else {
                        self.b.ins().$m(*x, *y)
                    };
                    r.push(v);
                }
                self.set(op, r);
            }};
        }

        // Lower-time constant fold: evaluable pure ops (select/icmp/int
        // binops over constants, including alloca-loaded i128 consts) emit
        // iconst directly. rustc's mir-opts leave some constant expressions
        // unfolded (`7u128.max(1)` lowers to `select(icmp ..)`); folding
        // here also collapses their dependent Assert(cond) checks.
        if crate::pass_enabled("PLIRON_LOWERFOLD") && self.op_int_constable(op, &id) {
            let r = op.deref(ctx).get_result(0);
            let t = self.ty_leaves(r.get_type(ctx));
            if t.len() == 1 {
                if let Some((b, _)) = self.const_bits(r, 0) {
                    let v = self.iconst_any(t[0].1, b);
                    return self.set1(op, v);
                }
            }
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
            let dead_default = sw
                .default_dest(ctx)
                .deref(ctx)
                .iter(ctx)
                .next()
                .is_some_and(|o| Operation::is_op::<UnreachableOp>(o, ctx));
            if !self.dense_switch(x, &vals, d, dead_default) {
                if self.b.func.dfg.value_type(x) == clt::I128 {
                    // `Switch::emit` subtracts a case cluster's minimum via
                    // `iadd_imm_s(val, (min as i64).wrapping_neg())`, which is
                    // only valid mod 2^64 — wrong for i128 indices. Split into
                    // a switch on the high half whose cases each switch on the
                    // low half.
                    let (lo, hi) = self.b.ins().isplit(x);
                    let mut by_hi: std::collections::BTreeMap<u64, Vec<(u64, ir::Block)>> =
                        Default::default();
                    for &(v, b) in &vals {
                        by_hi
                            .entry((v >> 64) as u64)
                            .or_default()
                            .push((v as u64, b));
                    }
                    let mut hi_switch = cranelift_frontend::Switch::new();
                    let groups: Vec<(ir::Block, Vec<(u64, ir::Block)>)> = by_hi
                        .values()
                        .map(|g| {
                            let blk = self.b.create_block();
                            (blk, g.clone())
                        })
                        .collect();
                    for (&h, &(blk, _)) in by_hi.keys().zip(groups.iter()) {
                        hi_switch.set_entry(h as u128, blk);
                    }
                    hi_switch.emit(&mut self.b, hi, d);
                    for (blk, group) in groups {
                        self.b.switch_to_block(blk);
                        let mut lo_switch = cranelift_frontend::Switch::new();
                        for (v, b) in group {
                            lo_switch.set_entry(v as u128, b);
                        }
                        lo_switch.emit(&mut self.b, lo, d);
                        self.b.seal_block(blk);
                    }
                } else {
                    let mut s = cranelift_frontend::Switch::new();
                    for &(v, b) in &vals {
                        s.set_entry(v, b);
                    }
                    s.emit(&mut self.b, x, d);
                }
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
            if self.b.func.dfg.value_type(a) == clt::F128 {
                // x64 Cranelift cannot compare f128: lower via compiler-rt
                // `__*tf2` comparison helpers, as LLVM does.
                let lc = |this: &mut Self, s: &str| {
                    this.libcall(s, &[clt::F128, clt::F128], &[clt::I32], &[a, b])[0]
                };
                let z = |this: &mut Self| this.b.ins().iconst(clt::I32, 0);
                let eq = |this: &mut Self, r: ir::Value| {
                    let z = this.b.ins().iconst(clt::I32, 0);
                    this.b.ins().icmp(IntCC::Equal, r, z)
                };
                let cmp = match pred {
                    P::False | P::True => {
                        let v = self.b.ins().iconst(clt::I8, matches!(pred, P::True) as i64);
                        return self.set1(op, v);
                    }
                    P::OEQ => {
                        let r = lc(self, "__eqtf2");
                        eq(self, r)
                    }
                    P::OGT => {
                        let r = lc(self, "__gttf2");
                        let z = z(self);
                        self.b.ins().icmp(IntCC::SignedGreaterThan, r, z)
                    }
                    P::OGE => {
                        let r = lc(self, "__getf2");
                        let z = z(self);
                        self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, r, z)
                    }
                    P::OLT => {
                        let r = lc(self, "__lttf2");
                        let z = z(self);
                        self.b.ins().icmp(IntCC::SignedLessThan, r, z)
                    }
                    P::OLE => {
                        let r = lc(self, "__letf2");
                        let z = z(self);
                        self.b.ins().icmp(IntCC::SignedLessThanOrEqual, r, z)
                    }
                    P::UNE => {
                        let r = lc(self, "__netf2");
                        let z = z(self);
                        self.b.ins().icmp(IntCC::NotEqual, r, z)
                    }
                    P::ORD => {
                        let r = lc(self, "__unordtf2");
                        let z = z(self);
                        self.b.ins().icmp(IntCC::Equal, r, z)
                    }
                    P::UNO => {
                        let r = lc(self, "__unordtf2");
                        let z = z(self);
                        self.b.ins().icmp(IntCC::NotEqual, r, z)
                    }
                    P::ONE => {
                        let u = {
                            let r = lc(self, "__unordtf2");
                            let z = z(self);
                            self.b.ins().icmp(IntCC::Equal, r, z)
                        };
                        let n = {
                            let r = lc(self, "__netf2");
                            let z = z(self);
                            self.b.ins().icmp(IntCC::NotEqual, r, z)
                        };
                        self.b.ins().band(u, n)
                    }
                    P::UEQ | P::UGT | P::UGE | P::ULT | P::ULE => {
                        let u = {
                            let r = lc(self, "__unordtf2");
                            let z = z(self);
                            self.b.ins().icmp(IntCC::NotEqual, r, z)
                        };
                        let o = match pred {
                            P::UEQ => {
                                let r = lc(self, "__eqtf2");
                                eq(self, r)
                            }
                            P::UGT => {
                                let r = lc(self, "__gttf2");
                                let z = z(self);
                                self.b.ins().icmp(IntCC::SignedGreaterThan, r, z)
                            }
                            P::UGE => {
                                let r = lc(self, "__getf2");
                                let z = z(self);
                                self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, r, z)
                            }
                            P::ULT => {
                                let r = lc(self, "__lttf2");
                                let z = z(self);
                                self.b.ins().icmp(IntCC::SignedLessThan, r, z)
                            }
                            _ => {
                                let r = lc(self, "__letf2");
                                let z = z(self);
                                self.b.ins().icmp(IntCC::SignedLessThanOrEqual, r, z)
                            }
                        };
                        self.b.ins().bor(u, o)
                    }
                };
                return self.set1(op, cmp);
            }
            if self.b.func.dfg.value_type(a) == clt::F16 {
                // Cranelift has no f16 compare either; promote both sides to
                // f32 (lossless, preserves every predicate incl. NaN cases)
                // via compiler-builtins and compare there.
                let a = self.h2f(a);
                let b = self.h2f(b);
                let r = self.b.ins().fcmp(cc, a, b);
                return self.set1(op, r);
            }
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
            self.int_divrem(op, opnds, "__udivti3", true, false)
        } else if is!(SDivOp) {
            self.int_divrem(op, opnds, "__divti3", false, false)
        } else if is!(URemOp) {
            self.int_divrem(op, opnds, "__umodti3", true, true)
        } else if is!(SRemOp) {
            self.int_divrem(op, opnds, "__modti3", false, true)
        } else if is!(FAddOp) {
            float_binop!(fadd, "__addtf3")
        } else if is!(FSubOp) {
            float_binop!(fsub, "__subtf3")
        } else if is!(FMulOp) {
            float_binop!(fmul, "__multf3")
        } else if is!(FDivOp) {
            float_binop!(fdiv, "__divtf3")
        } else if is!(FRemOp) {
            let a = self.get1(opnds[0]);
            let b = self.get1(opnds[1]);
            let t = self.b.func.dfg.value_type(a);
            let r = if t == clt::F16 {
                let a = self.h2f(a);
                let b = self.h2f(b);
                let m = self.libcall("fmodf", &[clt::F32, clt::F32], &[clt::F32], &[a, b])[0];
                self.f2h(m)
            } else if t == clt::F128 {
                // compiler-builtins' libm port exports `fmodf128`.
                self.libcall("fmodf128", &[t, t], &[t], &[a, b])[0]
            } else {
                let name = if t == clt::F32 { "fmodf" } else { "fmod" };
                self.libcall(name, &[t, t], &[t], &[a, b])[0]
            };
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
        } else if (is!(FPTruncOp) || is!(FPExtOp))
            && self.ty_leaves(opnds[0].get_type(self.ctx))[0].1.is_vector()
        {
            // Only bf16/packed-f16 <-> f32 vectors reach here unsplit (scalarize).
            let xs = self.get(opnds[0]);
            let rts: Vec<ClType> = self.ty_leaves(self.res_ty(op)).iter().map(|l| l.1).collect();
            let half = |t: ClType| t.lane_type() == clt::F16;
            // Wide bf16 leaves (NATIVE_BF16) are f16-bit containers too.
            let bfv = |ty: TypeHandle| {
                ty.deref(self.ctx)
                    .downcast_ref::<pliron_llvm::types::VectorType>()
                    .is_some_and(|v| v.elem_type().deref(self.ctx).is::<pliron::builtin::types::BF16Type>())
            };
            let bf = bfv(if is!(FPExtOp) { opnds[0].get_type(self.ctx) } else { self.res_ty(op) });
            let r = if half(rts[0]) || half(self.b.func.dfg.value_type(xs[0])) {
                let mut r = Vals::new();
                if xs.len() == rts.len() {
                    for (&x, &t) in xs.iter().zip(&rts) {
                        r.push(match (is!(FPExtOp), bf) {
                            (true, false) => self.b.ins().fvpromote_f16(t, x),
                            (false, false) => self.b.ins().fvdemote_f16(t, x),
                            (true, true) => self.b.ins().fvpromote_bf16(t, x),
                            (false, true) => self.b.ins().fvdemote_bf16(t, x),
                        });
                    }
                } else if is!(FPExtOp) {
                    // f16x32 leaves (native FP16) <-> pairs of f32x16.
                    for &x in xs.iter() {
                        r.push(self.b.ins().fvpromote_f16_lo(rts[0], x));
                        r.push(self.b.ins().fvpromote_f16_hi(rts[0], x));
                    }
                } else {
                    for p in xs.chunks(2) {
                        r.push(self.b.ins().fvdemote_f16_pair(p[0], p[1]));
                    }
                }
                r
            } else {
                self.vcast_bf16(is!(FPExtOp), &xs)
            };
            self.set(op, r);
        } else if is!(FPTruncOp) || is!(FPExtOp) {
            let x = self.get1(opnds[0]);
            let t = self.ty_leaves(self.res_ty(op))[0].1;
            let from = self.b.func.dfg.value_type(x);
            let bf = |ty: TypeHandle| ty.deref(self.ctx).is::<pliron::builtin::types::BF16Type>();
            let r = if is!(FPExtOp) && bf(opnds[0].get_type(self.ctx)) {
                let f = self.bf16_to_f32(x);
                if t == clt::F32 { f } else { self.b.ins().fpromote(t, f) }
            } else if is!(FPTruncOp) && bf(self.res_ty(op)) {
                let f = if from == clt::F32 { x } else { self.b.ins().fdemote(clt::F32, x) };
                self.f32_to_bf16(f)
            } else if from == clt::F16 && t == clt::F32 {
                self.h2f(x)
            } else if from == clt::F32 && t == clt::F16 {
                self.f2h(x)
            } else if [from, t].iter().any(|t| *t == clt::F16 || *t == clt::F128) {
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
            let x = if ft.bits() < 32 {
                self.resize(x, clt::I32, signed)
            } else {
                x
            };
            let it = self.b.func.dfg.value_type(x);
            let r = if t == clt::F16 {
                // compiler-builtins doesn't ship `__float*hf`; go via f32.
                let f32v = if it == clt::I128 {
                    let name = if signed {
                        "__floattisf"
                    } else {
                        "__floatuntisf"
                    };
                    self.libcall(name, &[clt::I128], &[clt::F32], &[x])[0]
                } else if signed {
                    self.b.ins().fcvt_from_sint(clt::F32, x)
                } else {
                    self.b.ins().fcvt_from_uint(clt::F32, x)
                };
                self.f2h(f32v)
            } else if t == clt::F128 {
                // No cranelift int->f128 conversion either; compiler-builtins.
                let isx = match it.bits() {
                    128 => "t",
                    64 => "d",
                    _ => "s",
                };
                let name = format!("__float{}{}itf", if signed { "" } else { "un" }, isx);
                self.libcall(&name, &[it], &[t], &[x])[0]
            } else if ft == clt::I128 {
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
            // Volatile accesses stay plain loads/stores bracketed by fences:
            // cranelift MemFlags has no volatile bit, and without a barrier
            // its alias analysis can fold a volatile load to an earlier
            // value or dead-store-eliminate a volatile store. The fences put
            // each volatile access behind an unkillable memory version.
            let vol = !atomic && self.st.volatile.contains(&op);
            let mf = self.plain_mf(op);
            if vol {
                self.b.ins().fence();
            }
            let r: Vals = self
                .ty_leaves(self.res_ty(op))
                .into_iter()
                .map(|(o, t)| {
                    if atomic && self.needs_atomic_stub(t) {
                        self.atomic_load_stub(p, o)
                    } else if atomic && t.is_float() {
                        let it = ClType::int(t.bits() as u16).unwrap();
                        let x = self.b.ins().atomic_load(it, MemFlagsData::trusted(), p);
                        self.b.ins().bitcast(t, MemFlagsData::new(), x)
                    } else if atomic {
                        self.b.ins().atomic_load(t, MemFlagsData::trusted(), p)
                    } else {
                        self.b.ins().load(t, mf, p, o as i32)
                    }
                })
                .collect();
            if vol {
                self.b.ins().fence();
            }
            if r.len() == 1 && self.st.nonnull.contains(&op) {
                if std::env::var_os("PLIRON_NNDBG").is_some() {
                    eprintln!("nndbg seed {} <- op {:?}", r[0], op);
                }
                self.nonnull.insert(r[0]);
            }
            if r.len() == 1 && self.st.bool01.contains(&op) {
                self.bool01.insert(r[0]);
            }
            if let Some(&reg) = self.st.frozen_loads.get(&op) {
                for &v in &r {
                    if let Some(i) = self.b.func.dfg.value_def(v).inst() {
                        self.scoped.insert(i, reg);
                    }
                }
            }
            if self.st.invariant.contains(&op) {
                for &v in &r {
                    if let Some(i) = self.b.func.dfg.value_def(v).inst() {
                        self.invariant.insert(i);
                    }
                }
            }
            self.set(op, r);
        } else if is!(StoreOp) || is!(AtomicStoreOp) {
            let vs = self.get(opnds[0]);
            let p = self.get1(opnds[1]);
            let lv = self.ty_leaves(opnds[0].get_type(ctx));
            let mf = self.plain_mf(op);
            // Volatile store: bracket with fences so cranelift's alias
            // analysis can never dead-store-eliminate or reorder it (see
            // the LoadOp branch above).
            let vol = !is!(AtomicStoreOp) && self.st.volatile.contains(&op);
            if vol {
                self.b.ins().fence();
            }
            for (v, (o, t)) in vs.into_iter().zip(lv) {
                if is!(AtomicStoreOp) && self.needs_atomic_stub(t) {
                    self.atomic_store_stub(p, o, v);
                } else if is!(AtomicStoreOp) {
                    let v = if t.is_float() {
                        let it = ClType::int(t.bits() as u16).unwrap();
                        self.b.ins().bitcast(it, MemFlagsData::new(), v)
                    } else {
                        v
                    };
                    self.b.ins().atomic_store(MemFlagsData::trusted(), v, p);
                } else {
                    self.b.ins().store(mf, v, p, o as i32);
                }
            }
            if vol {
                self.b.ins().fence();
            }
        } else if is!(AtomicRmwOp) {
            use cranelift_codegen::ir::AtomicRmwOp as R;
            let p = self.get1(opnds[0]);
            let v = self.get1(opnds[1]);
            use pliron_llvm::attributes::AtomicRmwKindAttr as KA;
            let kind = &self.st.rmw[&op];
            if self.b.func.dfg.value_type(v).is_float()
                || matches!(kind, KA::FAdd | KA::FSub | KA::FMax | KA::FMin)
            {
                let r = self.float_rmw(kind.clone(), p, v);
                return self.set1(op, r);
            }
            let k = match self.st.rmw[&op] {
                KA::Xchg => R::Xchg,
                KA::Add => R::Add,
                KA::Sub => R::Sub,
                KA::And => R::And,
                KA::Nand => R::Nand,
                KA::Or => R::Or,
                KA::Xor => R::Xor,
                KA::Max => R::Smax,
                KA::Min => R::Smin,
                KA::UMax => R::Umax,
                KA::UMin => R::Umin,
                _ => unreachable!(),
            };
            let t = self.b.func.dfg.value_type(v);
            if self.needs_atomic_stub(t) {
                self.atomic_rmw_stub(op, p, v, k);
            } else {
                let r = self.b.ins().atomic_rmw(t, MemFlagsData::trusted(), k, p, v);
                self.set1(op, r);
            }
        } else if is!(AtomicCmpxchgOp) {
            let p = self.get1(opnds[0]);
            let c = self.get1(opnds[1]);
            let n = self.get1(opnds[2]);
            if self.needs_atomic_stub(self.b.func.dfg.value_type(c)) {
                self.atomic_cas_stub(op, p, c, n);
            } else {
                let old = self.b.ins().atomic_cas(MemFlagsData::trusted(), p, c, n);
                let ok = self.b.ins().icmp(IntCC::Equal, old, c);
                self.set(op, smallvec![old, ok]);
            }
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
                let et = self.ty_leaves(self.res_ty(op))[0].1;
                match self.const_int(opnds[1]) {
                    Some(i) => {
                        let n = vt.lane_count() as i128;
                        let x = self.b.ins().extractlane(a[(i / n) as usize], (i % n) as u8);
                        self.as_lane(x, et)
                    }
                    None => {
                        let (p, es) = self.spill_vec(opnds[0], &a);
                        let addr = self.dyn_index(p, opnds[1], es);
                        self.b.ins().load(et, MemFlagsData::trusted(), addr, 0)
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
                        let e = self.as_lane(e, vt.lane_type());
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
        } else if is!(ConstantOp) || is!(AddressOfOp) {
            // Values come from `st.consts` (filled by `scan::scan`) at each use.
        } else if let Some(a) = Operation::get_op::<InlineAsmOp>(op, self.ctx) {
            // Operand-free spin hints (cubecl's `pause`/`yield`) have no
            // observable effect; anything else must go through rsasm.
            let t = String::from(a.get_attr_llvm_inline_asm_template(self.ctx).unwrap().clone());
            assert!(
                opnds.is_empty() && matches!(t.trim(), "" | "pause" | "yield" | "isb"),
                "pliron->cranelift: unsupported inline asm {t:?}"
            );
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
        let p = self.b.ins().stack_addr(self.tcfg.pointer_type(), ss, 0);
        if over == 0 {
            return p;
        }
        let p = self.b.ins().iadd_imm_u(p, align as i64 - 1);
        self.b.ins().band_imm_s(p, -(align as i64))
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
                let m = self.vhigh_bits(x);
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

    /// aarch64's `vhigh_bits` ISLE covers only 128-bit vectors — for a
    /// 64-bit input, SWAR the sign bits out of a scalar bitcast instead:
    /// `(x & lane_msbs) * magic >> (64 - lanes)` packs each lane's top
    /// bit into the low bits, the same movemask multiply NEON targets
    /// use for `u8x8`.
    fn vhigh_bits(&mut self, x: ir::Value) -> ir::Value {
        let ty = self.b.func.dfg.value_type(x);
        if !ty.is_vector() {
            // 1-lane vectors flatten to their scalar lane type.
            let s = self.b.ins().ushr_imm_u(x, ty.bits() as i64 - 1);
            return self.resize(s, clt::I32, false);
        }
        if ty.bits() != 64 {
            return self.b.ins().vhigh_bits(clt::I32, x);
        }
        let s = self.bitcast_to(clt::I64, x);
        let l = ty.lane_bits() as u64;
        let m = if l == 64 {
            self.b.ins().ushr_imm_u(s, 63)
        } else {
            let msbs = (u64::MAX / ((1u64 << l) - 1)) << (l - 1);
            let n = 64 / l;
            // Magic bit j places lane i's sign bit at (64-n)+i.
            let magic: u64 = (0..n)
                .map(|i| 1u64 << (65 - l - n - (l - 1) * i))
                .fold(0, |a, b| a | b);
            let m = self.b.ins().band_imm_u(s, msbs as i64);
            let m = self.b.ins().imul_imm_u(m, magic as i64);
            self.b.ins().ushr_imm_u(m, (64 - n) as i64)
        };
        self.b.ins().ireduce(clt::I32, m)
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
                // 1-lane vectors flatten to their scalar lane type.
                if t.is_vector() {
                    self.b.ins().splat(t, a[0])
                } else {
                    self.resize(a[0], t, false)
                }
            }
            "pliron.vhigh_bits" => {
                let t = rt;
                let x = self.sign_source(a[0]);
                let m = self.vhigh_bits(x);
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
        // Index arithmetic happens at pointer width: `base + idx*scale`
        // needs both operands at the pointer type.
        let x = self.resize(x, self.tcfg.pointer_type(), true);
        let off = self.b.ins().imul_imm_s(x, scale as i64);
        self.b.ins().iadd(base, off)
    }

    fn fcvt_sat(&mut self, signed: bool, t: ClType, x: ir::Value) -> ir::Value {
        let ft = self.b.func.dfg.value_type(x);
        if ft == clt::F16 {
            // compiler-builtins doesn't ship `__fix*hf*`; f16->f32 is exact,
            // so extending first preserves the saturating result.
            let x = self.h2f(x);
            return self.fcvt_sat(signed, t, x);
        }
        if t == clt::I128 || ft == clt::F128 {
            // Cranelift can't convert floats to i128 nor anything to/from
            // f128; compiler-builtins' helpers saturate.
            let sfx = match ft {
                clt::F32 => "sf",
                clt::F64 => "df",
                _ => "tf",
            };
            let (isx, it) = if t == clt::I128 {
                ("ti", clt::I128)
            } else if t.bits() > 32 {
                ("di", clt::I64)
            } else {
                ("si", clt::I32)
            };
            let name = format!("__fix{}{}{}", if signed { "" } else { "uns" }, sfx, isx);
            let r = self.libcall(&name, &[ft], &[it], &[x])[0];
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
            return self.b.ins().ireduce(t, r);
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
        // `getelementptr inbounds` from rustc (`ptr.add` cursor bumps,
        // `slice::Iter` advances) carries the no-wrap flag on the op itself;
        // `st.inbounds` covers our own `inbounds_gep` emissions. Either one
        // keeps the derived base non-null so jumpthread's null-guard folds
        // can collapse the `Option<NonNull>` safe-load selects iterators
        // emit. The rustc-flag path is off by default: folding the extra
        // guards reshapes the `split_whitespace` dispatch badly enough to
        // cost ~8% on tokenize while the chars loop it was aimed at gains
        // nothing (see `chars_alpha` in coordination-ledger.md).
        static IBNN: std::sync::LazyLock<bool> =
            std::sync::LazyLock::new(|| crate::env_is("PLIRON_INBOUNDSNN", "1"));
        if addr != base_v
            && (self.st.inbounds.contains(&op)
                || (*IBNN && gep.no_wrap_flags(ctx).contains(GepNoWrapFlags::INBOUNDS)))
        {
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
    ) -> (ir::Inst, Vals) {
        if self.eh_data.is_some() {
            // Emulated EH: call normally, then branch to the funclet block
            // when `__pliron_eh.flag` is set (the pad reads `pliron.eh.exn`).
            let inst = match target {
                Ok(fref) => self.b.ins().call(fref, args),
                Err((addr, sigref)) => self.b.ins().call_indirect(sigref, addr, args),
            };
            let rs: Vals = self.b.inst_results(inst).iter().copied().collect();
            self.eh_check(Some(catch));
            return (inst, rs);
        }
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
        let inst = match target {
            Ok(fr) => self.b.ins().try_call(fr, args, et),
            Err((addr, _)) => self.b.ins().try_call_indirect(addr, args, et),
        };
        self.b.switch_to_block(pre);
        self.b.set_cold_block(pre);
        let pt = self.tcfg.pointer_type();
        let p = self.b.append_block_param(pre, pt);
        self.b.def_var(self.exn.unwrap(), p);
        let lp = self.blocks[&catch];
        self.b.ins().jump(lp, &[]);
        self.b.switch_to_block(normal);
        self.cconst.clear();
        let rs = rets
            .into_iter()
            .map(|t| self.b.append_block_param(normal, t))
            .collect();
        (inst, rs)
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
        // The caller recorded an explicit ABI override (e.g. `extern "win64"`
        // called on a SysV host).
        let cc = info.cc.unwrap_or(cc);
        let mut sig = make_sig(ctx, fn_ty, &info.exts, cc);
        let args: Vec<Value> = call.args(ctx);
        let mut cargs = Vec::new();
        // Which pliron operand each CLIF arg expands from (for `nocap`
        // lookup); usize::MAX marks entries with no source operand
        // (variadic buffer pointer, trampoline padding).
        let mut csrc = Vec::new();
        for (ai, a) in args.iter().enumerate() {
            let vs = self.get(*a);
            csrc.extend(std::iter::repeat_n(ai, vs.len()));
            cargs.extend(vs);
        }
        // `black_box` barriers arrive as calls to never-defined
        // `__pliron_bb*`/`__pliron_bbm` symbols (flagged in CallInfo by
        // `call_bb`); emit the zero-instruction CLIF `black_box` op
        // instead of a real call. One op per leaf covers scalars, pairs
        // and multi-leaf vectors; `__pliron_bbm`'s pointer arg carries
        // the `~{memory}` clobber.
        if info.bb {
            let mut rs = Vals::new();
            for &a in &cargs {
                rs.push(self.b.ins().black_box(a));
            }
            if op.deref(ctx).get_num_results() > 0 {
                self.set(op, rs);
            }
            return;
        }
        // C variadic: fn_ty only has the fixed params. Foreign callees get the
        // real SysV ABI (extra args passed like ordinary arguments); callees
        // compiled by this backend get the args packed in a buffer whose
        // address is the extra param make_sig appends.
        let var_arg = matches!(classify(ctx, fn_ty), TyK::Func(_, _, true));
        if var_arg {
            // Indirect calls could target foreign variadic callees, so they
            // always use the real SysV convention (fn pointers to our own
            // variadic defs aren't distinguishable and would need a shim).
            // Direct calls whose sym is a foreign variadic get the real
            // convention too: sym_addr redirects them to the %al trampoline.
            // Wasm has no foreign ABIs: every callee is linked into the same
            // module and its variadic def expects the packed-buffer param.
            let foreign = !self.wasm
                && (info.foreign
                    || match call.callee(ctx) {
                        CallOpCallable::Indirect(_) => true,
                        CallOpCallable::Direct(ident) => {
                            let sym = &self.st.ident_to_sym[&ident.to_string()];
                            self.st.funcs.get(sym).is_some_and(|f| {
                                !has_body(self.ctx, f.op)
                                    && matches!(classify(self.ctx, f.ty), TyK::Func(_, _, true))
                            })
                        }
                    });
            if foreign {
                sig.params.pop();
                let fixed = sig.params.len();
                for i in fixed..cargs.len() {
                    // Darwin variadic stack slots are 8 bytes minimum; a
                    // narrower param would pack at its natural size and
                    // misalign every later va_arg. Promote like C does:
                    // sub-64-bit ints zero-extend (the callee reads the
                    // low bits anyway) and f32 promotes to f64.
                    let v = cargs[i];
                    let t = self.b.func.dfg.value_type(v);
                    let (v, t) = if t.is_int() && t.bits() < 64 {
                        (self.b.ins().uextend(clt::I64, v), clt::I64)
                    } else if t == clt::F32 {
                        (self.b.ins().fpromote(clt::F64, v), clt::F64)
                    } else {
                        (v, t)
                    };
                    cargs[i] = v;
                    sig.params.push(AbiParam::new(t));
                }
                if cc == CallConv::AppleAarch64 && sig.params.len() > fixed {
                    // Darwin puts every variadic argument on the stack, but
                    // Cranelift does not distinguish variadic params: pad the
                    // argument list with dummies so the real variadic
                    // arguments land past x7/v7. AAPCS64 allocates the
                    // integer and FP banks independently, so both are
                    // filled; variadic scalars beyond that are unsupported
                    // (e.g. vector or f128 varargs).
                    assert!(
                        sig.params[fixed..].iter().all(|p| p.value_type.is_int()
                            || matches!(p.value_type, clt::F32 | clt::F64)),
                        "unsupported argument type in C-variadic call on aarch64-apple-darwin"
                    );
                    let sret = usize::from(matches!(
                        sig.params[0].purpose,
                        ArgumentPurpose::StructReturn
                    ));
                    let (mut xi, mut vi) = (0usize, 0usize);
                    for p in &sig.params[sret..fixed] {
                        if p.value_type.is_int() {
                            xi += if p.value_type.bits() == 128 { 2 } else { 1 };
                        } else {
                            vi += 1;
                        }
                    }
                    for _ in xi..8 {
                        sig.params.insert(fixed, AbiParam::new(clt::I64));
                        cargs.insert(fixed, self.b.ins().iconst(clt::I64, 0));
                        csrc.insert(fixed, usize::MAX);
                    }
                    for _ in vi..8 {
                        sig.params.insert(fixed, AbiParam::new(clt::F64));
                        cargs.insert(fixed, self.b.ins().f64const(Ieee64::with_bits(0)));
                        csrc.insert(fixed, usize::MAX);
                    }
                }
            } else {
                // SysV overflow-area layout: each vararg in an 8-byte slot
                // aligned to its alignment (capped at 16), leaves at their
                // struct offsets. A VaList forwarding this buffer to a foreign
                // callee then decodes it identically to a real register-save
                // area's overflow region.
                let fixed = sig.params.len() - 1;
                let mut seen = 0usize;
                let mut first = args.len();
                for (i, &a) in args.iter().enumerate() {
                    if seen >= fixed {
                        first = i;
                        break;
                    }
                    seen += self.get(a).len();
                }
                cargs.drain(fixed..);
                csrc.drain(fixed..);
                let mut off = 0u64;
                let mut plan = Vec::new();
                for &a in &args[first..] {
                    let ty = a.get_type(ctx);
                    let (size, align) = size_align(ctx, ty);
                    off = off.next_multiple_of(align.clamp(8, 16));
                    let slot = off;
                    off += size.max(8).next_multiple_of(8);
                    plan.push((a, ty, slot));
                }
                let buf = self.slot(off.max(1), 16);
                for (a, ty, slot) in plan {
                    for ((lo, _), v) in leaves(ctx, ty).into_iter().zip(self.get(a)) {
                        self.b
                            .ins()
                            .store(MemFlagsData::new(), v, buf, (slot + lo) as i32);
                    }
                }
                cargs.push(buf);
                csrc.push(usize::MAX);
            }
        }
        // Emulated EH: a post-call flag check is only needed when the callee
        // might "throw". Leaf asm stubs/wrappers can never throw; vararg
        // trampolines tail-jump to the callee and can.
        let mut maythrow = true;
        let mut callee_sym = None;
        let target = match call.callee(ctx) {
            CallOpCallable::Direct(ident) => {
                let sym = &self.st.ident_to_sym[&ident.to_string()].clone();
                callee_sym = Some(sym.clone());
                maythrow = !self.st.funcs.get(sym).is_some_and(|f| f.nounwind)
                    && !(sym.starts_with("__pliron_asm_")
                        || sym.starts_with("__pliron_llvm_")
                        || sym.starts_with("__pliron_a64_"));
                match self.ids.get(sym).copied() {
                    Some(Sym::F(fid, declty)) if declty == fn_ty && !var_arg => {
                        let nw = self.st.nowrite.get(sym).copied();
                        let fr = self.call_fref(fid);
                        if let Some(strict) = nw {
                            self.nowrite.insert(fr, strict);
                        }
                        if let Some(f) = self.st.funcs.get(sym) {
                            let m = f
                                .frozen
                                .iter()
                                .fold(0u64, |m, &(i, _)| if i < 64 { m | 1 << i } else { m });
                            if m != 0 {
                                self.call_frozen.insert(fr, m);
                            }
                        }
                        Ok(fr)
                    }
                    _ => {
                        let addr = self.sym_addr(&sym.clone());
                        Err((addr, self.b.import_signature(sig)))
                    }
                }
            }
            CallOpCallable::Indirect(v) => {
                let addr = self.get1(v);
                if var_arg
                    && self.m.isa().triple().architecture == target_lexicon::Architecture::X86_64
                {
                    // fp appended as a hidden last argument; a per-register
                    // stub sets %al=8 then tail-jumps to it (fp's SysV
                    // register is computed positionally).
                    cargs.push(addr);
                    csrc.push(usize::MAX);
                    sig.params.push(AbiParam::new(self.tcfg.pointer_type()));
                    const INT_REGS: [&str; 6] = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
                    let n_int = sig.params[..sig.params.len() - 1]
                        .iter()
                        .filter(|p| {
                            p.value_type.is_int()
                                && p.purpose == cranelift_codegen::ir::ArgumentPurpose::Normal
                        })
                        .count();
                    match INT_REGS.get(n_int) {
                        Some(reg) => {
                            let t = format!("__pliron_va8_ind_{reg}");
                            if !self.va_fids.contains_key(&t) {
                                let id =
                                    self.m.declare_function(&t, Linkage::Import, &sig).unwrap();
                                self.va_fids.insert(t.clone(), id);
                            }
                            let macho = self.m.isa().triple().binary_format
                                == target_lexicon::BinaryFormat::Macho;
                            VA_TRAMPS.with(|v| v.borrow_mut().insert(va_tramp_ind_asm(reg, macho)));
                            Ok(self.call_fref(self.va_fids[&t]))
                        }
                        // fp lands on the stack (rare): call it directly; %al
                        // stays wrong for float varargs there.
                        None => Err((addr, self.b.import_signature(sig))),
                    }
                } else {
                    Err((addr, self.b.import_signature(sig)))
                }
            }
        };
        if self.eh_data.is_some()
            && let CallOpCallable::Direct(ident) = call.callee(ctx)
        {
            // Emulated EH: a "throw" records the exception in `__pliron_eh`
            // and unwinds by early-return instead of raising SEH.
            let sym = self.st.ident_to_sym[&ident.to_string()].as_str();
            if self.wasm {
                // The wasm emulated-EH intrinsics: __pliron_eh is the
                // {flag@0, exn@4} word the linker synthesizes (FL::call).
                let tls = self.eh_tls();
                let mf = MemFlagsData::new();
                if sym == "llvm.wasm.throw" || sym == "__pliron_llvm_wasm_throw" {
                    self.b.ins().store(mf, cargs[1], tls, 4);
                    self.eh_raise();
                    return;
                }
                if sym == "llvm.wasm.rethrow" || sym == "__pliron_llvm_wasm_rethrow" {
                    self.eh_raise();
                    return;
                }
                if sym == "llvm.wasm.get_exception"
                    || sym == "__pliron_llvm_wasm_get_exception"
                {
                    let v = self
                        .b
                        .ins()
                        .load(self.tcfg.pointer_type(), mf, tls, 4);
                    self.set(op, smallvec![v]);
                    return;
                }
                if sym == "llvm.wasm.get_ehselector"
                    || sym == "llvm.wasm.landingpad_index"
                    || sym == "__pliron_llvm_wasm_get_ehselector"
                    || sym == "__pliron_llvm_wasm_landingpad_index"
                {
                    let z = self.b.ins().iconst(clt::I32, 0);
                    self.set(op, smallvec![z]);
                    return;
                }
            }
            if !self.wasm && sym == "_CxxThrowException" {
                // `panic_unwind::seh::Exception` = { canary, payload } = 24
                // bytes; copy it into the TLS buffer so it outlives the
                // unwound frame.
                let tls = self.eh_tls();
                let mf = MemFlagsData::new();
                let vs: Vec<ir::Value> = (0..3)
                    .map(|i| {
                        self.b
                            .ins()
                            .load(self.tcfg.pointer_type(), mf, cargs[0], i * 8)
                    })
                    .collect();
                for (i, v) in vs.into_iter().enumerate() {
                    self.b.ins().store(mf, v, tls, 16 + i as i32 * 8);
                }
                let bufp = self.b.ins().iadd_imm_s(tls, 16);
                self.b.ins().store(mf, bufp, tls, 8);
                self.eh_raise();
                return;
            }
            if !self.wasm && sym == "_Unwind_Resume" {
                let tls = self.eh_tls();
                self.b.ins().store(MemFlagsData::new(), cargs[0], tls, 8);
                self.eh_raise();
                return;
            }
        }
        if info.tail && self.own_tail {
            // `return_call` produces no results, but the dead Return op after
            // it still reads the call's result operand — bind dummies.
            let mut dummies = Vals::new();
            for p in self.b.func.signature.returns.clone() {
                dummies.push(self.iconst_any(p.value_type, 0));
            }
            if !dummies.is_empty() {
                self.set(op, dummies);
            }
            match target {
                Ok(fr) => self.b.ins().return_call(fr, &cargs),
                Err((addr, sr)) => self.b.ins().return_call_indirect(sr, addr, &cargs),
            };
            self.terminated = true;
            return;
        }
        let (inst, rs): (ir::Inst, Vals) =
            if let Some(&(catch, is_catch)) = self.st.invokes.get(&op) {
                self.try_call(target, &cargs, catch, is_catch)
            } else {
                let inst = match target {
                    Ok(fr) => self.b.ins().call(fr, &cargs),
                    Err((addr, sr)) => self.b.ins().call_indirect(sr, addr, &cargs),
                };
                let rs: Vals = self.b.inst_results(inst).iter().copied().collect();
                if self.eh_data.is_some() && maythrow {
                    self.eh_check(None);
                }
                (inst, rs)
            };
        // `st.nnret` inference: a nonnull-returning callee's result is a
        // known-non-null pointer for jumpthread's null-test folds.
        if rs.len() == 1
            && callee_sym
                .as_deref()
                .is_some_and(|s| self.st.nnret.contains(s))
        {
            self.nonnull.insert(rs[0]);
        }
        // `info.nocap` carries rustc's `captures(none)` (Indirect params);
        // `st.nocapture` is our body-level inference, which also covers
        // `&`/`&mut` (Direct) params. `noalias` is NOT enough: a
        // `&'a mut self -> Entry<'a>`-style reborrow legally hands a
        // pointer into the slot back through the return value or an
        // out-param — a later call writing through it would bypass the
        // slot's writeback/reload sync. Only true no-capture flags the
        // CLIF arg a bounded use: no usable alias outlives the call.
        let own = callee_sym.and_then(|s| self.st.nocapture.get(&s));
        if info.nocap.is_some() || own.is_some() {
            for (k, &s) in csrc.iter().enumerate() {
                let flagged = s != usize::MAX
                    && (info
                        .nocap
                        .as_ref()
                        .is_some_and(|nc| nc.get(s) == Some(&true))
                        || own.is_some_and(|o| o.contains(&(s as u32))));
                if flagged {
                    self.nocap.insert((inst, k as u32));
                }
            }
        }
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
        // Same UB-on-invalid rule as `plain_mf`: a copy to/from invalid
        // memory is UB, so at -O the expanded accesses need no trap record.
        let f = if self.st.notrap {
            MemFlagsData::new().with_notrap()
        } else {
            MemFlagsData::new()
        };
        let v0 = self.b.ins().load(t, f, src, 0);
        let v1 = self.b.ins().load(t, f, se, 0);
        self.b.ins().store(f, v0, dst, 0);
        self.b.ins().store(f, v1, de, 0);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(slow);
        let cfg = self.tcfg;
        if memcpy {
            self.b.call_memcpy(cfg, dst, src, n);
        } else {
            self.b.call_memmove(cfg, dst, src, n);
        }
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
    }

    /// bf16 (carried as f16 bits) is the top half of an f32.
    fn bf16_to_f32(&mut self, x: ir::Value) -> ir::Value {
        let mf = MemFlagsData::new();
        let h = self.b.ins().bitcast(clt::I16, mf, x);
        let w = self.b.ins().uextend(clt::I32, h);
        let w = self.b.ins().ishl_imm_u(w, 16);
        self.b.ins().bitcast(clt::F32, mf, w)
    }

    /// `i16x8` bf16 leaves <-> `f32x4` leaves (two per bf16 leaf, low lanes first).
    fn vcast_bf16(&mut self, ext: bool, xs: &[ir::Value]) -> Vals {
        let mf = MemFlagsData::new().with_endianness(ir::Endianness::Little);
        let mut out = Vals::new();
        if ext {
            for &x in xs {
                for hi in [false, true] {
                    let w = if hi { self.b.ins().uwiden_high(x) } else { self.b.ins().uwiden_low(x) };
                    let w = self.b.ins().ishl_imm_u(w, 16);
                    out.push(self.b.ins().bitcast(clt::F32X4, mf, w));
                }
            }
        } else {
            for p in xs.chunks(2) {
                let lo = self.f32x4_to_bf16_bits(p[0]);
                let hi = self.f32x4_to_bf16_bits(p[1]);
                out.push(self.b.ins().unarrow(lo, hi));
            }
        }
        out
    }

    /// `f32_to_bf16` on four lanes, leaving each result in the low half of an i32.
    fn f32x4_to_bf16_bits(&mut self, f: ir::Value) -> ir::Value {
        let mf = MemFlagsData::new().with_endianness(ir::Endianness::Little);
        let k = |b: &mut Self, c: i64| {
            let c = b.b.ins().iconst(clt::I32, c);
            b.b.ins().splat(clt::I32X4, c)
        };
        let (one, half, quiet) = (k(self, 1), k(self, 0x7fff), k(self, 0x40));
        let w = self.b.ins().bitcast(clt::I32X4, mf, f);
        let hi = self.b.ins().ushr_imm_u(w, 16);
        let odd = self.b.ins().band(hi, one);
        let r = self.b.ins().iadd(w, half);
        let r = self.b.ins().iadd(r, odd);
        let r = self.b.ins().ushr_imm_u(r, 16);
        let nan = self.b.ins().fcmp(FloatCC::Unordered, f, f);
        let q = self.b.ins().bor(hi, quiet);
        self.b.ins().bitselect(nan, q, r)
    }

    /// Reinterpret a lane between same-width int/float types (bf16 lanes of
    /// `i16x8` leaves are F16 scalars).
    fn as_lane(&mut self, x: ir::Value, want: ClType) -> ir::Value {
        let t = self.b.func.dfg.value_type(x);
        if t != want && t.bits() == want.bits() { self.bitcast_to(want, x) } else { x }
    }

    /// f32 -> bf16, round-to-nearest-even; NaN stays a quiet NaN.
    fn f32_to_bf16(&mut self, f: ir::Value) -> ir::Value {
        let mf = MemFlagsData::new();
        let w = self.b.ins().bitcast(clt::I32, mf, f);
        let hi = self.b.ins().ushr_imm_u(w, 16);
        let odd = self.b.ins().band_imm_u(hi, 1);
        let r = self.b.ins().iadd_imm_u(w, 0x7fff);
        let r = self.b.ins().iadd(r, odd);
        let r = self.b.ins().ushr_imm_u(r, 16);
        let nan = self.b.ins().fcmp(FloatCC::Unordered, f, f);
        let q = self.b.ins().bor_imm_u(hi, 0x40);
        let r = self.b.ins().select(nan, q, r);
        let h = self.b.ins().ireduce(clt::I16, r);
        self.b.ins().bitcast(clt::F16, mf, h)
    }

    /// f16 -> f32: F16C `vcvtph2ps` when the host enables it, else compiler-builtins.
    fn h2f(&mut self, x: ir::Value) -> ir::Value {
        if crate::types::F16C.load(std::sync::atomic::Ordering::Relaxed) {
            self.b.ins().fpromote(clt::F32, x)
        } else {
            self.libcall("__extendhfsf2", &[clt::F16], &[clt::F32], &[x])[0]
        }
    }

    /// f32 -> f16 (round to nearest even), F16C `vcvtps2ph` when enabled.
    fn f2h(&mut self, x: ir::Value) -> ir::Value {
        if crate::types::F16C.load(std::sync::atomic::Ordering::Relaxed) {
            self.b.ins().fdemote(clt::F16, x)
        } else {
            self.libcall("__truncsfhf2", &[clt::F32], &[clt::F16], &[x])[0]
        }
    }

    /// Float binop by name; f16 widens to f32 like `float_binop!`.
    fn fbin(&mut self, k: &str, x: ir::Value, y: ir::Value) -> ir::Value {
        let t = self.b.func.dfg.value_type(x);
        let h = t == clt::F16;
        let (x, y) = if h {
            let x = self.h2f(x);
            let y = self.h2f(y);
            (x, y)
        } else {
            (x, y)
        };
        let i = self.b.ins();
        let r = match k {
            "fadd" => i.fadd(x, y),
            "fsub" => i.fsub(x, y),
            "fmul" => i.fmul(x, y),
            "fmax" => i.fmax(x, y),
            _ => i.fmin(x, y),
        };
        if h {
            self.f2h(r)
        } else {
            r
        }
    }

    /// Every scalar lane of `v`, across its leaves.
    fn lanes_of(&mut self, v: Value) -> Vec<ir::Value> {
        let mut out = Vec::new();
        let et = match classify(self.ctx, v.get_type(self.ctx)) {
            TyK::Vector(e, _) => self.ty_leaves(e).first().map(|x| x.1),
            _ => None,
        };
        for x in self.get(v) {
            let t = self.b.func.dfg.value_type(x);
            if t.is_vector() {
                for i in 0..t.lane_count() {
                    let l = self.b.ins().extractlane(x, i as u8);
                    out.push(et.map_or(l, |et| self.as_lane(l, et)));
                }
            } else {
                out.push(x);
            }
        }
        out
    }

    /// `llvm.vector.reduce.<k>`: sequential fold (the ordered form for fadd/fmul).
    fn vec_reduce(&mut self, op: Ptr<Operation>, k: &str, opnds: &[Value]) {
        let ordered = matches!(k, "fadd" | "fmul");
        let vt = opnds[ordered as usize].get_type(self.ctx);
        let bf = vt
            .deref(self.ctx)
            .downcast_ref::<pliron_llvm::types::VectorType>()
            .is_some_and(|v| v.elem_type().deref(self.ctx).is::<pliron::builtin::types::BF16Type>());
        let mut acc = ordered.then(|| self.get1(opnds[0]));
        let mut lanes = self.lanes_of(opnds[ordered as usize]);
        if bf {
            // Reduce in f32, round once.
            acc = acc.map(|a| self.bf16_to_f32(a));
            lanes = lanes.into_iter().map(|x| self.bf16_to_f32(x)).collect();
        }
        for x in lanes {
            let Some(a) = acc else {
                acc = Some(x);
                continue;
            };
            let i = self.b.ins();
            acc = Some(match k {
                "add" => i.iadd(a, x),
                "mul" => i.imul(a, x),
                "and" => i.band(a, x),
                "or" => i.bor(a, x),
                "xor" => i.bxor(a, x),
                "smax" => i.smax(a, x),
                "smin" => i.smin(a, x),
                "umax" => i.umax(a, x),
                "umin" => i.umin(a, x),
                "fadd" | "fmul" | "fmax" | "fmin" => {
                    acc = Some(self.fbin(k, a, x));
                    continue;
                }
                _ => panic!("pliron->cranelift: unsupported intrinsic llvm.vector.reduce.{k}"),
            });
        }
        let r = acc.expect("llvm.vector.reduce of an empty vector");
        let r = if bf { self.f32_to_bf16(r) } else { r };
        self.set1(op, r);
    }

    /// `llvm.is.fpclass(x, mask)` on the IEEE bit pattern; mask bits are
    /// snan, qnan, -inf, -normal, -subnormal, -0, +0, +subnormal, +normal, +inf.
    fn is_fpclass(&mut self, x: ir::Value, m: u32) -> ir::Value {
        let t = self.b.func.dfg.value_type(x);
        let mb: u32 = match t {
            clt::F16 => 10,
            clt::F32 => 23,
            clt::F64 => 52,
            _ => panic!("pliron->cranelift: llvm.is.fpclass on {t}"),
        };
        let it = ClType::int(t.bits() as u16).unwrap();
        let w = t.bits();
        let bits = self.b.ins().bitcast(it, MemFlagsData::new(), x);
        let absm = (1u128 << (w - 1)) - 1;
        let inf = ((1u128 << (w - 1 - mb)) - 1) << mb;
        let quiet = 1u128 << (mb - 1);
        let minnorm = 1u128 << mb;
        let c = |s: &mut Self, v: u128| s.iconst_any(it, v);
        let am = c(self, absm);
        let abs = self.b.ins().band(bits, am);
        let (ci, cq, cm, z) = (c(self, inf), c(self, inf | quiet), c(self, minnorm), c(self, 0));
        let ins = |s: &mut Self, cc, a, b| s.b.ins().icmp(cc, a, b);
        let nan = ins(self, IntCC::UnsignedGreaterThan, abs, ci);
        let notq = ins(self, IntCC::UnsignedLessThan, abs, cq);
        let snan = self.b.ins().band(nan, notq);
        let qnan = ins(self, IntCC::UnsignedGreaterThanOrEqual, abs, cq);
        let isinf = ins(self, IntCC::Equal, abs, ci);
        let zero = ins(self, IntCC::Equal, abs, z);
        let nz = ins(self, IntCC::NotEqual, abs, z);
        let lt_min = ins(self, IntCC::UnsignedLessThan, abs, cm);
        let sub = self.b.ins().band(nz, lt_min);
        let ge_min = ins(self, IntCC::UnsignedGreaterThanOrEqual, abs, cm);
        let lt_inf = ins(self, IntCC::UnsignedLessThan, abs, ci);
        let norm = self.b.ins().band(ge_min, lt_inf);
        let neg = ins(self, IntCC::SignedLessThan, bits, z);
        let pos = ins(self, IntCC::SignedGreaterThanOrEqual, bits, z);
        let classes = [
            (snan, None),
            (qnan, None),
            (isinf, Some(neg)),
            (norm, Some(neg)),
            (sub, Some(neg)),
            (zero, Some(neg)),
            (zero, Some(pos)),
            (sub, Some(pos)),
            (norm, Some(pos)),
            (isinf, Some(pos)),
        ];
        let mut r = self.b.ins().iconst(clt::I8, 0);
        for (bit, (cls, sign)) in classes.into_iter().enumerate() {
            if m & (1 << bit) == 0 {
                continue;
            }
            let v = match sign {
                Some(sg) => self.b.ins().band(cls, sg),
                None => cls,
            };
            r = self.b.ins().bor(r, v);
        }
        r
    }

    /// Float `atomicrmw` via the same-width integer: xchg directly, the
    /// arithmetic kinds as a load + compare-and-swap retry loop.
    fn float_rmw(
        &mut self,
        k: pliron_llvm::attributes::AtomicRmwKindAttr,
        p: ir::Value,
        v: ir::Value,
    ) -> ir::Value {
        use pliron_llvm::attributes::AtomicRmwKindAttr as KA;
        let t = self.b.func.dfg.value_type(v);
        let it = ClType::int(t.bits() as u16).unwrap();
        let (mf, tr) = (MemFlagsData::new(), MemFlagsData::trusted());
        if matches!(k, KA::Xchg) {
            let vi = self.b.ins().bitcast(it, mf, v);
            let r = self.b.ins().atomic_rmw(it, tr, ir::AtomicRmwOp::Xchg, p, vi);
            return self.b.ins().bitcast(t, mf, r);
        }
        let retry = self.b.create_block();
        let done = self.b.create_block();
        self.b.ins().jump(retry, &[]);
        self.b.switch_to_block(retry);
        let old = self.b.ins().atomic_load(it, tr, p);
        let of = self.b.ins().bitcast(t, mf, old);
        let nf = match k {
            KA::FAdd => self.fbin("fadd", of, v),
            KA::FSub => self.fbin("fsub", of, v),
            KA::FMax => self.fbin("fmax", of, v),
            KA::FMin => self.fbin("fmin", of, v),
            k => panic!("pliron->cranelift: atomicrmw {k:?} on {t}"),
        };
        let ni = self.b.ins().bitcast(it, mf, nf);
        let got = self.b.ins().atomic_cas(tr, p, old, ni);
        let ok = self.b.ins().icmp(IntCC::Equal, got, old);
        self.b.ins().brif(ok, done, &[], retry, &[]);
        self.b.switch_to_block(done);
        of
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
        if let Some(k) = name.strip_prefix("llvm.vector.reduce.") {
            return self.vec_reduce(op, k, opnds);
        }
        if name == "llvm.is.fpclass" {
            let x = self.get1(opnds[0]);
            let m = self.const_int(opnds[1]).expect("llvm.is.fpclass: non-constant mask");
            let r = self.is_fpclass(x, m as u32);
            return self.set1(op, r);
        }
        if matches!(name, "llvm.lifetime.start" | "llvm.lifetime.end") {
            // Not code — a storage-liveness marker. Record where it lands:
            // the instruction just emitted before it is the anchor (the
            // marker sits between it and whatever comes next), and the
            // block is the fallback if that instruction doesn't survive
            // the pass pipeline. The pointer stays a pliron value: an
            // inlined alloca may not be lowered yet, and materializing it
            // here would panic.
            if let Some(b) = self.b.current_block() {
                let anchor = self.b.func.layout.block_insts(b).last();
                self.lifetimes
                    .push((opnds[1], anchor, b, name == "llvm.lifetime.start"));
            }
            return;
        }
        if crate::scalarize::LEAFWISE_INTRINSICS.contains(&name) {
            let ls: Vec<Vals> = opnds.iter().map(|v| self.get(*v)).collect();
            if ls[0].iter().all(|x| self.b.func.dfg.value_type(*x).bits() >= 128) {
                let r: Vals = (0..ls[0].len())
                    .map(|j| {
                        let x: Vec<_> = ls.iter().map(|l| l[j]).collect();
                        let i = self.b.ins();
                        match name {
                            "llvm.fma" | "llvm.fmuladd" => i.fma(x[0], x[1], x[2]),
                            "llvm.sqrt" => i.sqrt(x[0]),
                            "llvm.fabs" => i.fabs(x[0]),
                            "llvm.floor" => i.floor(x[0]),
                            "llvm.ceil" => i.ceil(x[0]),
                            "llvm.trunc" => i.trunc(x[0]),
                            "llvm.roundeven" => i.nearest(x[0]),
                            "llvm.minimum" => i.fmin(x[0], x[1]),
                            _ => i.fmax(x[0], x[1]),
                        }
                    })
                    .collect();
                return self.set(op, r);
            }
        }
        let a: Vec<ir::Value> = opnds.iter().map(|v| self.get1(*v)).collect();
        let r = match name {
            "pliron.eh.exn" => {
                if self.eh_data.is_some() {
                    // Emulated EH: deliver the in-flight exception and clear
                    // the flag. COFF's TLS word stores exn at +8; wasm's
                    // linker-synthesized {flag, exn} word stores it at +4.
                    let tls = self.eh_tls();
                    let exn_off = if self.wasm { 4 } else { 8 };
                    let exn = self.b.ins().load(
                        self.tcfg.pointer_type(),
                        MemFlagsData::new(),
                        tls,
                        exn_off,
                    );
                    let z = self.b.ins().iconst(clt::I32, 0);
                    self.b.ins().store(MemFlagsData::new(), z, tls, 0);
                    exn
                } else {
                    self.b.use_var(self.exn.unwrap())
                }
            }
            "pliron.va.buf" => self.va_buf.unwrap_or_else(|| {
                panic!(
                    "pliron.va.buf in a non-variadic function {}",
                    CUR_FN.with(|c| c.borrow().clone())
                )
            }),
            "llvm.memcpy" | "llvm.memmove" => {
                if let Some(n) = self
                    .const_int(opnds[2])
                    .filter(|n| (0..=SMALL_MEM).contains(n))
                {
                    // Load everything before storing, so this is also a valid memmove.
                    let mf = self.plain_mf(op);
                    let vals: Vec<_> = mem_chunks(n as u64)
                        .into_iter()
                        .map(|(o, t)| (o, self.b.ins().load(t, mf, a[1], o)))
                        .collect();
                    for (o, v) in vals {
                        self.b.ins().store(mf, v, a[0], o);
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
                let cfg = self.tcfg;
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
                    let mf = self.plain_mf(op);
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
                        self.b.ins().store(mf, v, a[0], o);
                    }
                    return;
                }
                let cfg = self.tcfg;
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
            "llvm.cttz_nz" => {
                let ty = self.b.func.dfg.value_type(a[0]);
                if self.m.isa().triple().architecture == target_lexicon::Architecture::X86_64
                    && ty.bits() <= 64
                {
                    self.b.ins().ctz_nonzero(a[0])
                } else {
                    self.b.ins().ctz(a[0])
                }
            }
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
                        let r = self.libcall(
                            f,
                            &[t, t, self.tcfg.pointer_type()],
                            &[t],
                            &[a[0], a[1], slot],
                        )[0];
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
                            let max = self.b.ins().ushr_imm_u(ones, 1);
                            let sign = self.b.ins().sshr_imm_u(x, t.bits() as i64 - 1);
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
            "llvm.fabs" => {
                let t = self.b.func.dfg.value_type(a[0]);
                if t == clt::F16 || t == clt::F128 {
                    self.fabs_wide(a[0])
                } else {
                    self.b.ins().fabs(a[0])
                }
            }
            "llvm.copysign" => {
                let t = self.b.func.dfg.value_type(a[0]);
                if t == clt::F16 || t == clt::F128 {
                    self.fcopysign_wide(a[0], a[1])
                } else {
                    self.b.ins().fcopysign(a[0], a[1])
                }
            }
            n @ ("llvm.sqrt" | "llvm.floor" | "llvm.ceil" | "llvm.trunc" | "llvm.roundeven"
            | "llvm.fma" | "llvm.fmuladd" | "llvm.minimum" | "llvm.maximum") => {
                let t = self.b.func.dfg.value_type(a[0]);
                let h = t == clt::F16;
                let a: Vec<_> = if h {
                    // f16 widens to f32: exact for these (sqrt/rounding are
                    // innocuous under double rounding at 11 -> 24 bits).
                    a.iter()
                        .map(|&x| self.h2f(x))
                        .collect()
                } else {
                    a.to_vec()
                };
                let r = if t == clt::F128 {
                    // No Cranelift lowering; compiler-builtins' libm port
                    // exports e.g. fmaf128.
                    let base = match n {
                        "llvm.sqrt" => "sqrt",
                        "llvm.floor" => "floor",
                        "llvm.ceil" => "ceil",
                        "llvm.trunc" => "trunc",
                        "llvm.roundeven" => "roundeven",
                        "llvm.minimum" => "fminimum",
                        "llvm.maximum" => "fmaximum",
                        _ => "fma",
                    };
                    let f = format!("{base}f128");
                    let ps = vec![t; a.len()];
                    self.libcall(&f, &ps, &[t], &a)[0]
                } else {
                    match n {
                        "llvm.sqrt" => self.b.ins().sqrt(a[0]),
                        "llvm.floor" => self.b.ins().floor(a[0]),
                        "llvm.ceil" => self.b.ins().ceil(a[0]),
                        "llvm.trunc" => self.b.ins().trunc(a[0]),
                        "llvm.roundeven" => self.b.ins().nearest(a[0]),
                        "llvm.minimum" => self.b.ins().fmin(a[0], a[1]),
                        "llvm.maximum" => self.b.ins().fmax(a[0], a[1]),
                        _ => self.b.ins().fma(a[0], a[1], a[2]),
                    }
                };
                if h {
                    self.f2h(r)
                } else {
                    r
                }
            }
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
                    | "llvm.tan"
                    | "llvm.sinh"
                    | "llvm.cosh"
                    | "llvm.tanh"
                    | "llvm.asin"
                    | "llvm.acos"
                    | "llvm.atan"
                    | "llvm.atan2"
                    | "llvm.minnum"
                    | "llvm.maxnum"
            ) =>
            {
                // No Cranelift instruction: call libm, like cg_llvm does on x86.
                let t = self.b.func.dfg.value_type(a[0]);
                let base = match &n["llvm.".len()..] {
                    "minnum" => "fmin",
                    "maxnum" => "fmax",
                    b => b,
                };
                if t == clt::F16 && n != "llvm.round" {
                    // No f16 libm on most hosts: widen, call the f32 form, narrow.
                    let w: Vec<_> = a
                        .iter()
                        .map(|&x| self.h2f(x))
                        .collect();
                    let ps = vec![clt::F32; w.len()];
                    let m = self.libcall(&format!("{base}f"), &ps, &[clt::F32], &w)[0];
                    let r = self.f2h(m);
                    return self.set1(op, r);
                }
                let f = match t {
                    clt::F32 => format!("{base}f"),
                    clt::F64 => base.to_string(),
                    clt::F16 | clt::F128 if n == "llvm.round" => {
                        format!("{base}f{}", if t == clt::F16 { "16" } else { "128" })
                    }
                    _ => panic!("pliron->cranelift: unsupported intrinsic {n} on {t}"),
                };
                let ps = vec![t; a.len()];
                self.libcall(&f, &ps, &[t], &a)[0]
            }
            "llvm.abs" => {
                let x = self.get1(opnds[0]);
                self.b.ins().iabs(x)
            }
            "llvm.umax" | "llvm.umin" | "llvm.smax" | "llvm.smin" => {
                let (x, y) = (self.get1(opnds[0]), self.get1(opnds[1]));
                match name {
                    "llvm.umax" => self.b.ins().umax(x, y),
                    "llvm.umin" => self.b.ins().umin(x, y),
                    "llvm.smax" => self.b.ins().smax(x, y),
                    _ => self.b.ins().smin(x, y),
                }
            }
            "pliron.eh.rethrow" if self.eh_data.is_some() => {
                // Emulated rethrow (`cleanup_ret` with no unwind dest):
                // the exception pointer is still in `__pliron_eh`; set the
                // flag again and keep unwinding by early-return.
                let pt = self.tcfg.pointer_type();
                let z = self.iconst_any(pt, 0);
                self.set1(op, z);
                self.eh_raise();
                return;
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
    st: &State,
    pblocks: &[Ptr<BasicBlock>],
) -> rustc_hash::FxHashSet<Ptr<BasicBlock>> {
    let mut cold = rustc_hash::FxHashSet::default();
    let term = |b: Ptr<BasicBlock>| b.deref(ctx).iter(ctx).last().unwrap();
    for &b in pblocks {
        let hit = b.deref(ctx).iter(ctx).any(|op| {
            Operation::is_op::<UnreachableOp>(op, ctx)
                || crate::inline::direct_callee(ctx, st, op)
                    .is_some_and(|s| st.funcs.get(s).is_some_and(|f| f.cold))
        }) || crate::pgo::block_count(st, b).is_some_and(|c| c == 0);
        if hit {
            cold.insert(b);
        }
        let t = term(b);
        if let Some(&e) = st.expect.get(&t) {
            let s = t.deref(ctx).get_successor(if e { 1 } else { 0 });
            // An expect hint that marks a loop arm cold (hashbrown's
            // `likely(match_empty)` marks the probe-advance latch
            // "unlikely") banishes a back-edge that runs once per
            // iteration — as hot as the loop itself. Skip the mark when
            // `s` can reach `b` back: the edge is part of a cycle.
            if !reaches(ctx, s, b) {
                cold.insert(s);
            }
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

/// Can `from` reach `to` through terminator successors? Used to keep
/// expect-hinted cold marks off cycle edges.
fn reaches(ctx: &Context, from: Ptr<BasicBlock>, to: Ptr<BasicBlock>) -> bool {
    let mut seen = rustc_hash::FxHashSet::default();
    let mut stack = vec![from];
    while let Some(b) = stack.pop() {
        if b == to {
            return true;
        }
        if !seen.insert(b) {
            continue;
        }
        for op in b.deref(ctx).iter(ctx) {
            stack.extend(op.deref(ctx).successors());
        }
    }
    false
}

pub fn rpo(ctx: &Context, st: &State, blocks: &[Ptr<BasicBlock>]) -> Vec<Ptr<BasicBlock>> {
    rpo_split(ctx, st, blocks).0
}

/// Reverse post-order of the reachable blocks, then the unreachable ones;
/// also returns how many are reachable.
fn rpo_split(
    ctx: &Context,
    st: &State,
    blocks: &[Ptr<BasicBlock>],
) -> (Vec<Ptr<BasicBlock>>, usize) {
    // Invoke landing pads count as successors so values defined before an
    // invoke are lowered before the landing-pad code that uses them.
    let succs = |b: Ptr<BasicBlock>| -> Vec<Ptr<BasicBlock>> {
        b.deref(ctx)
            .iter(ctx)
            .flat_map(|op| {
                let mut v: Vec<_> = op.deref(ctx).successors().collect();
                // Profile-weighted order: hottest successor first, so RPO
                // places hot blocks earlier in the function's layout.
                if let Some(cs) = st.prof_edges.get(&op) {
                    let mut ord: Vec<usize> = (0..v.len()).collect();
                    ord.sort_by_key(|&i| std::cmp::Reverse(cs.get(i).copied().unwrap_or(0)));
                    v = ord.into_iter().map(|i| v[i]).collect();
                }
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

/// `UnsignedDivisionByConstantInfo` for u128 (Hacker's Delight ch. 10,
/// `magicu`): returns `(magic, post_shift, is_add)` where
/// `x / d == mulhi128(x, magic) >> post_shift` for all `x < 2^128`, or with
/// `is_add`, `q = (mulhi128(x, magic) + ((x - mulhi128(x, magic)) >> 1))`
/// then `>> post_shift`. All arithmetic wraps mod 2^128 like a W-bit APInt.
/// Fold width for `rem_u128_fold`: the largest `w ≤ 62` with
/// `2^w ≡ 1 (mod d)` — a multiple of `ord_d(2)` that leaves ≥ 32 bits per
/// limb so the limb sum always fits u64. `None` when the order exceeds 62
/// (e.g. `u64::MAX`-scale divisors), which falls back to magic multiply.
fn fold_width(d: u128) -> Option<u32> {
    debug_assert!(d > 1 && d % 2 == 1);
    let mut p = 1u128;
    for ord in 1..=62u32 {
        // `2p mod d` without the `2p` overflow when `d > 2^127`.
        p = if p >= d - p { p - (d - p) } else { p + p };
        if p == 1 {
            return Some(ord * (62 / ord));
        }
    }
    None
}

/// `d⁻¹ mod 2^128` for odd `d` via Newton iteration — `inv = d` is exact
/// to 3 bits and each step doubles them (3→6→12→24→48→96→192).
fn modinv_u128(d: u128) -> u128 {
    debug_assert!(d % 2 == 1);
    let mut inv = d;
    for _ in 0..6 {
        inv = inv.wrapping_mul(2u128.wrapping_sub(d.wrapping_mul(inv)));
    }
    debug_assert_eq!(d.wrapping_mul(inv), 1);
    inv
}

fn magicu128(d: u128) -> (u128, u32, bool) {
    debug_assert!(d > 1);
    let all_ones = u128::MAX;
    let signed_min = 1u128 << 127;
    let signed_max = signed_min - 1;
    // NC: largest dividend with NC % d == d - 1.
    let nc = all_ones - all_ones.wrapping_add(1).wrapping_sub(d) % d;
    let mut p = 127u32;
    let (mut q1, mut r1) = (signed_min / nc, signed_min % nc);
    let (mut q2, mut r2) = (signed_max / d, signed_max % d);
    let mut add = false;
    loop {
        p += 1;
        if r1 >= nc - r1 {
            q1 = (q1 << 1) | 1;
            r1 = (r1 << 1).wrapping_sub(nc);
        } else {
            q1 <<= 1;
            r1 <<= 1;
        }
        if r2 + 1 >= d - r2 {
            add |= q2 >= signed_max;
            q2 = (q2 << 1) | 1;
            r2 = (r2 << 1).wrapping_add(1).wrapping_sub(d);
        } else {
            add |= q2 >= signed_min;
            q2 <<= 1;
            r2 = (r2 << 1).wrapping_add(1);
        }
        let delta = d - 1 - r2;
        if !(p < 256 && (q1 < delta || (q1 == delta && r1 == 0))) {
            break;
        }
    }
    (q2.wrapping_add(1), p - 128 - u32::from(add), add)
}
