//! rustc codegen backend: rustc_codegen_ssa -> pliron LLVM dialect -> Cranelift -> object.
//! No LLVM is linked; pliron-llvm is built without its `llvm-sys` features.

#![feature(rustc_private)]
#![allow(clippy::too_many_arguments)]

extern crate rustc_abi;
extern crate rustc_apfloat;
extern crate rustc_ast;
extern crate rustc_attr_ir;
extern crate rustc_codegen_ssa;
extern crate rustc_const_eval;
extern crate rustc_data_structures;
#[cfg(not(target_family = "wasm"))]
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_symbol_mangling;
extern crate rustc_target;

mod abi;
mod asm;
mod builder;
mod clifpeep;
mod constload;
mod consts;
mod context;
mod domcheck;
mod eh;
mod hot;
mod inline;
mod instcombine;
mod intrinsic;
mod jumpthread;
mod loadfwd;
mod looprot;
mod lower;
mod memcpyopt;
mod nounwind;
mod objmerge;
mod phisimp;
mod simd;
mod sroa;
mod switchmap;
mod taildup;
mod tailmerge;
mod type_of;
mod types;
mod unreach;
mod wasm;
mod xcgu;

use std::any::Any;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_module::Linkage;
use rustc_ast::expand::allocator::{
    AllocatorMethod, AllocatorTy, NO_ALLOC_SHIM_IS_UNSTABLE, default_fn_name, global_fn_name,
};
use rustc_codegen_ssa::back::lto::ThinModule;
use rustc_codegen_ssa::back::write::{
    CodegenContext, EmitObj, FatLtoInput, ModuleConfig, SharedEmitter, TargetMachineFactoryFn,
    ThinLtoInput,
};
use rustc_codegen_ssa::base::{codegen_crate, maybe_create_entry_wrapper};
use rustc_codegen_ssa::mono_item::MonoItemExt;
use rustc_codegen_ssa::traits::*;
use rustc_codegen_ssa::{CompiledModule, CompiledModules, CrateInfo, ModuleCodegen, TargetConfig};
use rustc_data_structures::profiling::SelfProfilerRef;
use rustc_errors::DiagCtxtHandle;
use rustc_middle::dep_graph::{WorkProduct, WorkProductMap};
use rustc_middle::ty::TyCtxt;
use rustc_session::config::{OptLevel, OutputFilenames, OutputType};
use rustc_session::{IncrCompSession, Session};
use rustc_span::Symbol;
use rustc_symbol_mangling::mangle_internal_symbol;

pub use builder::Builder;
pub use context::CodegenCx;
use context::Exts;

#[derive(Clone)]
pub struct PlironCodegenBackend;

pub struct PlironModule {
    pub obj: Vec<u8>,
    pub ir: String,
    pub asm: String,
}

pub struct PlironBuffer(Vec<u8>);

impl ModuleBufferMethods for PlironBuffer {
    fn data(&self) -> &[u8] {
        &self.0
    }
}

fn build_isa(sess: &Session) -> Arc<dyn TargetIsa> {
    let mut fb = settings::builder();
    fb.set("is_pic", "true").unwrap();
    if std::env::var("PLIRON_RA_CHECK").is_ok_and(|v| v == "1") {
        fb.set("regalloc_checker", "true").unwrap();
    }
    fb.set(
        "enable_verifier",
        if cfg!(debug_assertions) {
            "true"
        } else {
            "false"
        },
    )
    .unwrap();
    // Frame pointers follow the target default and `-Cforce-frame-pointers`, as in
    // cg_clif: unwinding uses .eh_frame, so rbp is free for the register allocator.
    // `PLIRON_OMIT_FP=0` keeps them unconditionally.
    let fp = { sess.target.options.frame_pointer }.ratchet(sess.opts.cg.force_frame_pointers);
    let keep_fp =
        fp != rustc_target::spec::FramePointer::MayOmit || !pass_enabled("PLIRON_OMIT_FP");
    fb.set(
        "preserve_frame_pointers",
        if keep_fp { "true" } else { "false" },
    )
    .unwrap();
    // `-Ztls-model=initial-exec` (bootstrap passes it for rustc itself): no
    // `__tls_get_addr` call per access. `PLIRON_TLS_IE=0` keeps general-dynamic.
    let ie = sess.target.arch == rustc_target::spec::Arch::X86_64
        && sess.target.binary_format == rustc_target::spec::BinaryFormat::Elf
        && matches!(
            sess.tls_model(),
            rustc_target::spec::TlsModel::InitialExec | rustc_target::spec::TlsModel::LocalExec
        )
        && pass_enabled("PLIRON_TLS_IE");
    fb.set("tls_model", if ie { "elf_ie" } else { "elf_gd" })
        .unwrap();
    fb.set("enable_llvm_abi_extensions", "true").unwrap();
    fb.enable("enable_multi_ret_implicit_sret").unwrap();
    fb.set(
        "opt_level",
        if sess.opts.optimize == OptLevel::No {
            "none"
        } else {
            "speed_and_size"
        },
    )
    .unwrap();
    let triple = target_lexicon::Triple::from_str(&sess.target.llvm_target)
        .unwrap_or_else(|e| sess.dcx().fatal(format!("unsupported target: {e}")));
    let flags = settings::Flags::new(fb);
    let isa = cranelift_codegen::isa::lookup(triple)
        .unwrap_or_else(|e| sess.dcx().fatal(format!("cranelift: {e}")));
    isa.finish(flags)
        .unwrap_or_else(|e| sess.dcx().fatal(format!("cranelift: {e}")))
}

/// Per-pass ablation toggle: `PLIRON_<PASS>=0` turns a pass off.
/// LLVM's `\x01` prefix (bindgen's `link_name = "\u{1}sym"`) means "emit the name verbatim".
pub(crate) fn obj_sym(n: &str) -> &str {
    n.strip_prefix('\u{1}').unwrap_or(n)
}

pub(crate) fn pass_enabled(var: &str) -> bool {
    std::env::var(var).map_or(true, |v| v != "0")
}

fn finish_module(cx: &CodegenCx<'_>, name: &str) -> PlironModule {
    let emit_ir = cx
        .tcx
        .sess
        .opts
        .output_types
        .contains_key(&OutputType::LlvmAssembly)
        || std::env::var_os("PLIRON_DUMP").is_some();
    let ir = if emit_ir {
        cx.print_ir()
    } else {
        String::new()
    };
    if std::env::var_os("PLIRON_DUMP").is_some() {
        eprintln!("==== {name} ====\n{ir}");
    }
    if cx.tcx.sess.opts.optimize == OptLevel::No && pass_enabled("PLIRON_ALWAYS_INLINE") {
        let (ctx, st) = (&mut *cx.pctx.borrow_mut(), &mut *cx.st.borrow_mut());
        inline::run(ctx, st, true, None, true);
        domcheck::run(ctx, st, "always-inline");
        inline::dead_fns(ctx, st);
    }
    if cx.tcx.sess.opts.optimize != OptLevel::No {
        let (ctx, st) = (&mut *cx.pctx.borrow_mut(), &mut *cx.st.borrow_mut());
        let small = cx.tcx.sess.target.arch == rustc_target::spec::Arch::Wasm32
            || matches!(
                cx.tcx.sess.opts.optimize,
                OptLevel::Size | OptLevel::SizeMin
            );
        if pass_enabled("PLIRON_FROZEN") {
            st.frozen = context::frozen_values(ctx, st).into_iter().collect();
        }
        inline::run(ctx, st, small, None, false);
        domcheck::run(ctx, st, "inline");
        if std::env::var("PLIRON_NOUNWIND").is_ok_and(|v| v == "1") {
            nounwind::run(ctx, st);
        }
        if pass_enabled("PLIRON_SRET2REG") {
            abi::run(ctx, st);
            domcheck::run(ctx, st, "abi");
        }
        if std::env::var("PLIRON_DEADARG").is_ok_and(|v| v == "1") {
            abi::dead_args(ctx, st);
        }
        if pass_enabled("PLIRON_INSTCOMBINE") {
            instcombine::run(ctx, st);
            domcheck::run(ctx, st, "instcombine");
        }
        if std::env::var("PLIRON_MEMCPYOPT").is_ok_and(|v| v == "1") {
            memcpyopt::run(ctx, st);
        }
        if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_SROA_FWD") {
            sroa::forward(ctx, st);
            domcheck::run(ctx, st, "sroa-fwd");
        }
        if pass_enabled("PLIRON_PHISIMP") {
            phisimp::run(ctx, st);
            domcheck::run(ctx, st, "phisimp");
        }
        if pass_enabled("PLIRON_SROA") {
            sroa::run(ctx, st);
            domcheck::run(ctx, st, "sroa");
        }
        if pass_enabled("PLIRON_CONSTLOAD") {
            constload::run(ctx, st);
            domcheck::run(ctx, st, "constload");
            // Inlining + SROA make vtable pointers constant: call those slots
            // directly, inline them and clean up again (`PLIRON_DEVIRT=0` disables).
            if pass_enabled("PLIRON_DEVIRT") {
                let sites = inline::devirt(ctx, st);
                domcheck::run(ctx, st, "devirt");
                if !sites.is_empty() && pass_enabled("PLIRON_DEVIRT_INLINE") {
                    inline::run(ctx, st, small, Some(&sites), false);
                    domcheck::run(ctx, st, "devirt-inline");
                    if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_SROA_FWD") {
                        sroa::forward(ctx, st);
                    }
                    if pass_enabled("PLIRON_PHISIMP") && pass_enabled("PLIRON_DEVIRT_PHI") {
                        phisimp::run(ctx, st);
                    }
                    if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_DEVIRT_SROA") {
                        sroa::run(ctx, st);
                    }
                    if pass_enabled("PLIRON_DEVIRT_CL") {
                        constload::run(ctx, st);
                    }
                }
            }
        }
        st.notrap = pass_enabled("PLIRON_NOTRAP");
        st.jumpthread = pass_enabled("PLIRON_JUMPTHREAD");
        st.loadfwd = pass_enabled("PLIRON_LOADFWD");
        st.peep = pass_enabled("PLIRON_PEEP");
        st.tailmerge = pass_enabled("PLIRON_TAILMERGE");
        st.unreach = pass_enabled("PLIRON_UNREACH");
        st.taildup = pass_enabled("PLIRON_TAILDUP");
        if pass_enabled("PLIRON_DEADFN") {
            inline::dead_fns(ctx, st);
        }
    }
    if std::env::var_os("PLIRON_DUMP_OPT").is_some() {
        eprintln!("==== {name} (optimized) ====\n{}", cx.print_ir());
    }
    if cx.tcx.sess.target.arch == rustc_target::spec::Arch::Wasm32 {
        let obj = wasm::lower_to_wasm(&cx.pctx.borrow(), &cx.st.borrow(), name, &wasm::target_features(&cx.tcx.sess.target, &cx.tcx.sess.opts));
        return PlironModule {
            obj,
            ir,
            asm: String::new(),
        };
    }
    let isa = build_isa(cx.tcx.sess);
    let hot = std::env::var("PLIRON_HOT")
        .is_ok_and(|c| c == cx.tcx.crate_name(rustc_span::def_id::LOCAL_CRATE).as_str());
    let obj = lower::lower_to_object(
        cx.tcx.sess.panic_strategy() == rustc_target::spec::PanicStrategy::Unwind,
        hot,
        &cx.pctx.borrow(),
        &cx.st.borrow(),
        isa,
        name,
    );
    let asm = std::mem::take(&mut cx.st.borrow_mut().asm);
    PlironModule { obj, ir, asm }
}

impl CodegenBackend for PlironCodegenBackend {
    fn target_config(&self, sess: &rustc_session::EarlySession) -> TargetConfig {
        types::PTR32.store(
            sess.target.pointer_width == 32,
            std::sync::atomic::Ordering::Relaxed,
        );
        use rustc_target::spec::{Arch, Os};
        let feats: Vec<Symbol> = match sess.target.arch {
            Arch::X86_64 if sess.target.os != Os::None => ["fxsr", "sse", "sse2", "x87"]
                .iter()
                .map(|f| Symbol::intern(f))
                .collect(),
            Arch::AArch64 if sess.target.os != Os::None => vec![rustc_span::sym::neon],
            Arch::Wasm32 => wasm::target_features(&sess.target, &sess.opts)
                .iter()
                .map(|f| Symbol::intern(f))
                .collect(),
            _ => vec![],
        };
        TargetConfig {
            internal_target_features: rustc_data_structures::unord::UnordSet::from_iter(feats),
            has_reliable_f16: false,
            has_reliable_f16_math: false,
            has_reliable_f16b: false,
            has_reliable_f128: false,
            has_reliable_f128_math: false,
        }
    }

    fn name(&self) -> &'static str {
        "pliron"
    }

    fn init(&mut self, _sess: &rustc_session::EarlySession) -> rustc_session::CodegenBackendInit {
        // No LTO: keep rustc from requesting thin-local LTO at opt-level > 0.
        rustc_session::CodegenBackendInit {
            thin_lto_supported: false,
            #[cfg(rustc_in_tree)]
            fat_lto_supported: false,
            ..Default::default()
        }
    }

    fn target_cpu(&self, _sess: &Session) -> String {
        "generic".to_string()
    }

    fn codegen_crate(&self, tcx: TyCtxt<'_>) -> Box<dyn Any> {
        Box::new(codegen_crate(self.clone(), tcx))
    }

    fn join_codegen(
        &self,
        ongoing_codegen: Box<dyn Any>,
        sess: &Session,
        incr_comp_session: Option<&IncrCompSession>,
        _outputs: &OutputFilenames,
        crate_info: &CrateInfo,
    ) -> (CompiledModules, WorkProductMap) {
        ongoing_codegen
            .downcast::<rustc_codegen_ssa::back::write::OngoingCodegen<PlironCodegenBackend>>()
            .expect("expected PlironCodegenBackend's OngoingCodegen")
            .join(sess, incr_comp_session, crate_info)
    }
}

impl ExtraBackendMethods for PlironCodegenBackend {
    type Module = PlironModule;

    fn codegen_allocator<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        module_name: &str,
        methods: &[AllocatorMethod],
    ) -> PlironModule {
        let cx = CodegenCx::new(tcx, module_name);
        let usize = cx.type_isize();
        let ptr = cx.type_ptr();
        for method in methods {
            let mut types = Vec::new();
            for input in method.inputs.iter() {
                match input.ty {
                    AllocatorTy::Layout => {
                        types.push(usize);
                        types.push(usize);
                    }
                    AllocatorTy::Ptr => types.push(ptr),
                    AllocatorTy::Usize => types.push(usize),
                    AllocatorTy::Never | AllocatorTy::ResultPtr | AllocatorTy::Unit => {
                        panic!("invalid allocator arg")
                    }
                }
            }
            let output = match method.output {
                AllocatorTy::ResultPtr => Some(ptr),
                AllocatorTy::Never | AllocatorTy::Unit => None,
                _ => panic!("invalid allocator output"),
            };
            let from = mangle_internal_symbol(tcx, &global_fn_name(method.name));
            let to = mangle_internal_symbol(tcx, &default_fn_name(method.name));
            wrapper(&cx, &from, Some(&to), &types, output);
        }
        wrapper(
            &cx,
            &mangle_internal_symbol(tcx, NO_ALLOC_SHIM_IS_UNSTABLE),
            None,
            &[],
            None,
        );
        finish_module(&cx, module_name)
    }

    fn compile_codegen_unit(
        &self,
        tcx: TyCtxt<'_>,
        cgu_name: Symbol,
        _bitcode_needed: bool,
    ) -> (ModuleCodegen<PlironModule>, u64) {
        let start = std::time::Instant::now();
        let dep_node = tcx.codegen_unit(cgu_name).codegen_dep_node(tcx);
        let (module, _) = tcx.dep_graph.with_task(
            dep_node,
            tcx,
            || module_codegen(tcx, cgu_name),
            Some(rustc_middle::dep_graph::hash_result),
        );
        (module, start.elapsed().as_nanos() as u64)
    }
}

fn module_codegen(tcx: TyCtxt<'_>, cgu_name: Symbol) -> ModuleCodegen<PlironModule> {
    let cgu = tcx.codegen_unit(cgu_name);
    let mut cx = CodegenCx::new(tcx, cgu_name.as_str());
    cx.st.borrow_mut().cgu = cgu_name.to_string();
    let mono_items = cgu.items_in_deterministic_order(tcx);
    let extra = if tcx.sess.opts.optimize != OptLevel::No
        && pass_enabled("PLIRON_XCGU")
        && !tcx.sess.target.is_like_wasm
        && std::env::var_os("PLIRON_HOT").is_none()
    {
        xcgu::copies(tcx, cgu)
    } else {
        Vec::new()
    };
    if std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!("xcgu {cgu_name}: {} local copies", extra.len());
    }
    for &inst in &extra {
        rustc_middle::mono::MonoItem::Fn(inst).predefine::<Builder<'_, '_>>(
            &mut cx,
            cgu_name.as_str(),
            rustc_attr_ir::Linkage::Internal,
            rustc_middle::mono::Visibility::Default,
        );
    }
    for &(mono_item, data) in &mono_items {
        mono_item.predefine::<Builder<'_, '_>>(
            &mut cx,
            cgu_name.as_str(),
            data.linkage,
            data.visibility,
        );
    }
    for &(mono_item, data) in &mono_items {
        mono_item.define::<Builder<'_, '_>>(&mut cx, cgu_name.as_str(), data);
    }
    for &inst in &extra {
        let data = rustc_middle::mono::MonoItemData {
            inlined: true,
            linkage: rustc_attr_ir::Linkage::Internal,
            visibility: rustc_middle::mono::Visibility::Default,
            size_estimate: 0,
        };
        rustc_middle::mono::MonoItem::Fn(inst).define::<Builder<'_, '_>>(
            &mut cx,
            cgu_name.as_str(),
            data,
        );
    }
    maybe_create_entry_wrapper::<Builder<'_, '_>>(&cx, cgu);
    let m = finish_module(&cx, cgu_name.as_str());
    ModuleCodegen::new_regular(cgu_name.to_string(), m)
}

fn wrapper(
    cx: &CodegenCx<'_>,
    from: &str,
    to: Option<&str>,
    types: &[pliron::r#type::TypeHandle],
    output: Option<pliron::r#type::TypeHandle>,
) {
    let ret = output.unwrap_or_else(|| cx.type_void());
    let fty = cx.type_func(types, ret);
    let f = cx.declare_fn_sym(from, fty, Linkage::Export, Exts::default());
    let bb = Builder::append_block(cx, f, "entry");
    let mut bx = Builder::build(cx, bb);
    if let Some(to) = to {
        cx.declare_fn_sym(to, fty, Linkage::Import, Exts::default());
        let args: Vec<_> = (0..types.len()).map(|i| bx.get_param(i)).collect();
        let callee = cx.sym_addr(to);
        let r = bx.call_raw(fty, callee, &args, Exts::default());
        if output.is_some() {
            bx.ret(r)
        } else {
            bx.ret_void()
        }
    } else {
        bx.ret_void();
    }
}

impl WriteBackendMethods for PlironCodegenBackend {
    type Module = PlironModule;
    type TargetMachine = ();
    type ModuleBuffer = PlironBuffer;
    type ThinData = ();

    fn supports_parallel(&self) -> bool {
        false
    }

    fn target_machine_factory(
        &self,
        _sess: &Session,
        _opt_level: OptLevel,
    ) -> TargetMachineFactoryFn<Self> {
        Arc::new(|_, _| ())
    }

    fn optimize_and_codegen_fat_lto(
        _sess: &Session,
        _cgcx: &CodegenContext,
        _shared_emitter: &SharedEmitter,
        _tm_factory: TargetMachineFactoryFn<Self>,
        _exported_symbols_for_lto: &[String],
        _each_linked_rlib_for_lto: &[PathBuf],
        _modules: Vec<FatLtoInput<Self>>,
    ) -> CompiledModule {
        unimplemented!("LTO is not supported by the pliron backend")
    }

    fn run_thin_lto(
        _cgcx: &CodegenContext,
        _prof: &SelfProfilerRef,
        _dcx: DiagCtxtHandle<'_>,
        _exported_symbols_for_lto: &[String],
        _each_linked_rlib_for_lto: &[PathBuf],
        _modules: Vec<ThinLtoInput<Self>>,
    ) -> (Vec<ThinModule<Self>>, Vec<WorkProduct>) {
        unimplemented!("LTO is not supported by the pliron backend")
    }

    fn optimize(
        _cgcx: &CodegenContext,
        _prof: &SelfProfilerRef,
        _shared_emitter: &SharedEmitter,
        _module: &mut ModuleCodegen<PlironModule>,
        _config: &ModuleConfig,
    ) {
    }

    fn optimize_and_codegen_thin(
        _cgcx: &CodegenContext,
        _prof: &SelfProfilerRef,
        _shared_emitter: &SharedEmitter,
        _tm_factory: TargetMachineFactoryFn<Self>,
        _thin: ThinModule<Self>,
    ) -> CompiledModule {
        unimplemented!("LTO is not supported by the pliron backend")
    }

    fn codegen(
        cgcx: &CodegenContext,
        _prof: &SelfProfilerRef,
        _shared_emitter: &SharedEmitter,
        module: ModuleCodegen<PlironModule>,
        config: &ModuleConfig,
    ) -> CompiledModule {
        let outs = &cgcx.output_filenames;
        let emit_obj = config.emit_obj != EmitObj::None;
        if emit_obj {
            let path = outs.temp_path_for_cgu(OutputType::Object, &module.name);
            let m = &module.module_llvm;
            std::fs::write(&path, &m.obj).expect("write object");
        }
        if config.emit_ir {
            let path = outs.temp_path_for_cgu(OutputType::LlvmAssembly, &module.name);
            std::fs::write(&path, &module.module_llvm.ir).expect("write ir");
        }
        module.into_compiled_module(emit_obj, false, false, false, config.emit_ir, outs)
    }

    fn serialize_module(module: PlironModule, _is_thin: bool) -> PlironBuffer {
        PlironBuffer(module.obj)
    }
}

/// Entry point loaded by `-Zcodegen-backend=path/to/librustc_codegen_pliron.so`.
#[unsafe(no_mangle)]
pub fn __rustc_codegen_backend() -> Box<dyn CodegenBackend> {
    Box::new(PlironCodegenBackend)
}
