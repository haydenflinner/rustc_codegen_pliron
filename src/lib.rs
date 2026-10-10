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
mod bcheck;
mod builder;
mod celim;
mod clifpeep;
mod constload;
mod consts;
mod context;
mod domcheck;
mod dse;
mod edgefwd;
mod eh;
mod hot;
mod ifconv;
mod indvars;
mod inline;
mod instcombine;
mod intrinsic;
mod ivrefold;
mod jumpthread;
mod licm;
mod loadfwd;
mod loopdel;
mod loopidiom;
mod looprot;
mod loopvec;
mod unroll;
mod lower;
mod memcpyopt;
mod nounwind;
mod nowrite;
mod objmerge;
mod phisimp;
mod punroll;
mod revnorm;
mod simd;
mod slp;
mod spec;
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

fn build_isa(sess: &Session, tail_calls: bool) -> Arc<dyn TargetIsa> {
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
    // Cranelift's x64 `return_call` emission requires frame pointers.
    let keep_fp = fp != rustc_target::spec::FramePointer::MayOmit
        || !pass_enabled("PLIRON_OMIT_FP")
        || tail_calls;
    fb.set(
        "preserve_frame_pointers",
        if keep_fp { "true" } else { "false" },
    )
    .unwrap();
    let tls_model = match sess.target.options.binary_format {
        rustc_target::spec::BinaryFormat::Elf => "elf_gd",
        rustc_target::spec::BinaryFormat::MachO => "macho",
        rustc_target::spec::BinaryFormat::Coff => "coff",
        _ => "none",
    };
    // `-Ztls-model=initial-exec` (bootstrap passes it for rustc itself): no
    // `__tls_get_addr` call per access. `PLIRON_TLS_IE=0` keeps general-dynamic.
    let ie = sess.target.arch == rustc_target::spec::Arch::X86_64
        && sess.target.options.binary_format == rustc_target::spec::BinaryFormat::Elf
        && matches!(
            sess.tls_model(),
            rustc_target::spec::TlsModel::InitialExec | rustc_target::spec::TlsModel::LocalExec
        )
        && pass_enabled("PLIRON_TLS_IE");
    crate::lower::RET_NOEXT.store(
        sess.target.arch == rustc_target::spec::Arch::X86_64 && pass_enabled("PLIRON_RET_NOEXT"),
        std::sync::atomic::Ordering::Relaxed,
    );
    fb.set("tls_model", if ie { "elf_ie" } else { tls_model })
        .unwrap();
    fb.set("enable_llvm_abi_extensions", "true").unwrap();
    fb.enable("enable_multi_ret_implicit_sret").unwrap();
    fb.set(
        "opt_level",
        if sess.opts.optimize == OptLevel::No
            || !crate::pass_enabled("PLIRON_COMPILE_EGRAPH")
        {
            "none"
        } else {
            "speed_and_size"
        },
    )
    .unwrap();
    // Inline stack probes keep large frames from jumping the guard page.
    // `inline` needs no `__cranelift_probestack` helper (only emitted on
    // x86_64/aarch64/riscv64 anyway).
    let triple = target_lexicon::Triple::from_str(&sess.target.llvm_target)
        .unwrap_or_else(|e| sess.dcx().fatal(format!("unsupported target: {e}")));
    if let target_lexicon::Architecture::Aarch64(_)
    | target_lexicon::Architecture::Riscv64(_)
    | target_lexicon::Architecture::X86_64 = triple.architecture
    {
        fb.enable("enable_probestack").unwrap();
        fb.set("probestack_strategy", "inline").unwrap();
    }
    let flags = settings::Flags::new(fb);
    let mut isa = cranelift_codegen::isa::lookup(triple.clone())
        .unwrap_or_else(|e| sess.dcx().fatal(format!("cranelift: {e}")));
    // Target features → ISA flags. `sess.internal_target_features` holds the
    // effective set (spec baseline + `-Ctarget-cpu` + `-Ctarget-feature`).
    let tf = |rust: &str| sess.internal_target_features.contains(&Symbol::intern(rust));
    let isa_feats: &[(&str, &str)] = match triple.architecture {
        target_lexicon::Architecture::Aarch64(_) => &[
            ("lse", "has_lse"),
            ("dotprod", "has_dotprod"),
            ("i8mm", "has_i8mm"),
            ("fp16", "has_fp16"),
            ("bti", "use_bti"),
        ],
        // The VEX families (avx/avx2/fma/avx512*) are deliberately not
        // mapped: this Cranelift has no ymm register class, so the flags
        // only buy VEX-128 encodings — which translate ~2x slower than
        // legacy SSE under Rosetta, and on real hardware offer only the
        // 3-operand form. `PLIRON_X64_VEX=1` opts back in for real-HW runs.
        // BMI1/BMI2 stay: GPR VEX (mulx/shlx/…) measures fast under Rosetta.
        target_lexicon::Architecture::X86_64 => &[
            ("sse3", "has_sse3"),
            ("ssse3", "has_ssse3"),
            ("sse4.1", "has_sse41"),
            ("sse4.2", "has_sse42"),
            ("popcnt", "has_popcnt"),
            ("bmi1", "has_bmi1"),
            ("bmi2", "has_bmi2"),
            ("lzcnt", "has_lzcnt"),
            ("cmpxchg16b", "has_cmpxchg16b"),
        ],
        _ => &[],
    };
    for &(rust, flag) in isa_feats {
        if tf(rust) {
            isa.enable(flag)
                .unwrap_or_else(|e| sess.dcx().fatal(format!("cranelift flag {flag}: {e}")));
        }
    }
    // Opt-in VEX encodings on x64 (see the comment on the flag table above).
    if triple.architecture == target_lexicon::Architecture::X86_64
        && std::env::var("PLIRON_X64_VEX").is_ok_and(|v| v == "1")
    {
        for &(rust, flag) in &[
            ("avx", "has_avx"),
            ("avx2", "has_avx2"),
            ("fma", "has_fma"),
            ("avx512f", "has_avx512f"),
            ("avx512vl", "has_avx512vl"),
            ("avx512dq", "has_avx512dq"),
            ("avx512bitalg", "has_avx512bitalg"),
            ("avx512vbmi", "has_avx512vbmi"),
            ("avx512vnni", "has_avx512vnni"),
            ("avxvnni", "has_avx_vnni"),
        ] {
            if tf(rust) {
                isa.enable(flag)
                    .unwrap_or_else(|e| sess.dcx().fatal(format!("cranelift flag {flag}: {e}")));
            }
        }
    }
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

/// `PLIRON_OPT_BISECT=N`: allow only the first N optimization-pass
/// applications (across all passes and functions), like LLVM's
/// `-opt-bisect-limit`. Each gate call consumes one unit; with no env var
/// the budget is unlimited. `PLIRON_OPT_BISECT_DEBUG=1` logs each gate.
pub(crate) fn bisect(name: &str) -> bool {
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    static LIMIT: LazyLock<Option<u64>> = LazyLock::new(|| {
        std::env::var("PLIRON_OPT_BISECT").ok().and_then(|v| v.parse().ok())
    });
    static DEBUG: LazyLock<bool> =
        LazyLock::new(|| std::env::var("PLIRON_OPT_BISECT_DEBUG").is_ok_and(|v| v == "1"));
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let Some(limit) = *LIMIT else { return true };
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let ok = n < limit;
    if *DEBUG {
        eprintln!("bisect {} {name} {}", n + 1, if ok { "run" } else { "skip" });
    }
    ok
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
        if pass_enabled("PLIRON_NOALIAS_FWD") {
            st.noalias = context::noalias_values(ctx, st)
                .into_iter()
                .map(|(v, d, w)| (v, (d, w)))
                .collect();
        }
        inline::run(ctx, st, small, None, false);
        domcheck::run(ctx, st, "inline");
        if pass_enabled("PLIRON_SPEC") {
            spec::run(ctx, st);
            domcheck::run(ctx, st, "spec");
        }
        // Opt-in: unsound on wasm emulated EH, which unwinds through ordinary
        // calls (llvm.wasm.throw / __pliron_eh) the body scan can't see.
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
        if pass_enabled("PLIRON_MEMCPYOPT") {
            memcpyopt::run(ctx, st);
            domcheck::run(ctx, st, "memcpyopt");
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
        if pass_enabled("PLIRON_NOWRITE") {
            nowrite::run(ctx, st);
        }
        st.notrap = pass_enabled("PLIRON_NOTRAP");
        st.jumpthread = pass_enabled("PLIRON_JUMPTHREAD");
        st.loadfwd = pass_enabled("PLIRON_LOADFWD");
        st.slot_dse = pass_enabled("PLIRON_SLOT_DSE");
        st.dse = pass_enabled("PLIRON_DSE");
        st.peep = pass_enabled("PLIRON_PEEP");
        st.tailmerge = pass_enabled("PLIRON_TAILMERGE");
        st.unreach = pass_enabled("PLIRON_UNREACH");
        st.taildup = pass_enabled("PLIRON_TAILDUP");
        st.licm = pass_enabled("PLIRON_LICM");
        st.indvars = pass_enabled("PLIRON_INDUCT");
        st.loopidiom = pass_enabled("PLIRON_IDIOM");
        st.loopvec = pass_enabled("PLIRON_VEC");
        st.slp = pass_enabled("PLIRON_SLP");
        st.bcheck = pass_enabled("PLIRON_BCHECK");
        if pass_enabled("PLIRON_DEADFN") {
            inline::dead_fns(ctx, st);
        }
    }
    if std::env::var_os("PLIRON_DUMP_OPT").is_some() {
        eprintln!("==== {name} (optimized) ====\n{}", cx.print_ir());
    }
    if cx.tcx.sess.target.arch == rustc_target::spec::Arch::Wasm32 {
        let obj = wasm::lower_to_wasm(
            &cx.pctx.borrow(),
            &cx.st.borrow(),
            name,
            &wasm::target_features(&cx.tcx.sess.target, &cx.tcx.sess.opts),
            cx.tcx.sess.panic_strategy() == rustc_target::spec::PanicStrategy::Unwind,
        );
        return PlironModule {
            obj,
            ir,
            asm: String::new(),
        };
    }
    let isa = build_isa(
        cx.tcx.sess,
        cx.tcx.features().enabled(rustc_span::sym::explicit_tail_calls)
            || std::env::var("PLIRON_TAILCALL").is_ok(),
    );
    let hot = std::env::var("PLIRON_HOT")
        .is_ok_and(|c| c == cx.tcx.crate_name(rustc_span::def_id::LOCAL_CRATE).as_str());
    let obj = lower::lower_to_object(
        cx.tcx.sess.panic_strategy() == rustc_target::spec::PanicStrategy::Unwind,
        hot,
        &cx.pctx.borrow(),
        &cx.st.borrow(),
        isa,
        name,
        cx.tcx.sess,
    );
    let asm = std::mem::take(&mut cx.st.borrow_mut().asm);
    PlironModule { obj, ir, asm }
}

/// Whether a rustc target feature is enabled in the *base* target machine:
/// the arch baseline, the spec's `features` string, or the spec/`-C` cpu.
fn base_has_feature(sess: &rustc_session::EarlySession, feature: &str) -> bool {
    // The spec `features` string is a comma-separated "+x"/"-y" list; a later
    // entry wins. An explicit `-feat` suppresses even a cpu-implied feature.
    let mut spec = None;
    for f in sess.target.features.split(',') {
        let n = f.strip_prefix('+').or_else(|| f.strip_prefix('-'));
        if n == Some(feature) {
            spec = Some(f.starts_with('+'));
        }
    }
    if let Some(v) = spec {
        return v;
    }
    let baseline: &[&str] = match sess.target.arch {
        // x86_64 mandates SSE2; rustc requires fxsr/x87 on top.
        rustc_target::spec::Arch::X86_64 => &["fxsr", "sse", "sse2", "x87"],
        // Every aarch64 platform chip has NEON.
        rustc_target::spec::Arch::AArch64 => &["neon"],
        _ => &[],
    };
    if baseline.contains(&feature) {
        return true;
    }
    let cpu = sess.opts.cg.target_cpu.as_deref().unwrap_or(&sess.target.cpu);
    if cpu == "native" {
        return native_has_feature(sess, feature);
    }
    cpu_features(cpu).contains(&feature)
}
/// `-Ctarget-cpu=native`: LLVM would expand the host cpu name; we probe the
/// host directly. Only meaningful when compiling for the host arch.
fn native_has_feature(sess: &rustc_session::EarlySession, feature: &str) -> bool {
    use rustc_target::spec::Arch;
    match sess.target.arch {
        #[cfg(target_arch = "aarch64")]
        Arch::AArch64 => match feature {
            "aes" => std::arch::is_aarch64_feature_detected!("aes"),
            "dotprod" => std::arch::is_aarch64_feature_detected!("dotprod"),
            "fcma" => std::arch::is_aarch64_feature_detected!("fcma"),
            "fp16" => std::arch::is_aarch64_feature_detected!("fp16"),
            "i8mm" => std::arch::is_aarch64_feature_detected!("i8mm"),
            "lse" => std::arch::is_aarch64_feature_detected!("lse"),
            "neon" => true,
            "rcpc" => std::arch::is_aarch64_feature_detected!("rcpc"),
            "sha2" => std::arch::is_aarch64_feature_detected!("sha2"),
            "sha3" => std::arch::is_aarch64_feature_detected!("sha3"),
            _ => false,
        },
        #[cfg(target_arch = "x86_64")]
        Arch::X86_64 => match feature {
            "sse3" => std::arch::is_x86_feature_detected!("sse3"),
            "ssse3" => std::arch::is_x86_feature_detected!("ssse3"),
            "sse4.1" => std::arch::is_x86_feature_detected!("sse4.1"),
            "sse4.2" => std::arch::is_x86_feature_detected!("sse4.2"),
            "avx" => std::arch::is_x86_feature_detected!("avx"),
            "avx2" => std::arch::is_x86_feature_detected!("avx2"),
            "fma" => std::arch::is_x86_feature_detected!("fma"),
            "popcnt" => std::arch::is_x86_feature_detected!("popcnt"),
            "bmi1" => std::arch::is_x86_feature_detected!("bmi1"),
            "bmi2" => std::arch::is_x86_feature_detected!("bmi2"),
            "lzcnt" => std::arch::is_x86_feature_detected!("lzcnt"),
            "cmpxchg16b" => std::arch::is_x86_feature_detected!("cmpxchg16b"),
            _ => false,
        },
        _ => false,
    }
}
/// rustc `-Ctarget-cpu` names → feature sets, for the cpus whose properties
/// feed cranelift lowering. Only entries we can state confidently go here;
/// anything else falls back to the spec baseline.
fn cpu_features(cpu: &str) -> &'static [&'static str] {
    // x86-64 psABI levels: each arm lists entry-point features only —
    // `internal_target_features` expands the implied closure
    // (avx2 → avx → sse4.2 → … → sse2), so the v2 non-SIMD extras are the
    // only ones repeated per tier.
    const X64_V2: &[&str] = &["cmpxchg16b", "lahfsahf", "popcnt", "sse4.2"];
    const X64_AVX: &[&str] = &["avx", "cmpxchg16b", "lahfsahf", "popcnt"];
    const X64_V3: &[&str] = &[
        "avx2", "bmi1", "bmi2", "f16c", "fma", "lzcnt", "movbe", "cmpxchg16b", "lahfsahf", "popcnt",
    ];
    const X64_V4: &[&str] = &[
        "avx2", "bmi1", "bmi2", "f16c", "fma", "lzcnt", "movbe", "cmpxchg16b", "lahfsahf", "popcnt",
        "avx512bw", "avx512cd", "avx512dq", "avx512vl",
    ];
    const X64_V4_EXT: &[&str] = &[
        "avx2", "bmi1", "bmi2", "f16c", "fma", "lzcnt", "movbe", "cmpxchg16b", "lahfsahf", "popcnt",
        "avx512bw", "avx512cd", "avx512dq", "avx512vl", "avx512bitalg", "avx512vbmi", "avx512vnni",
    ];
    match cpu {
        // Apple A12-generation and later: v8.3/v8.4+ — dotprod, lse, fp16.
        "apple-a12" | "apple-s4" | "apple-s5" => {
            &["aes", "sha2", "dotprod", "lse", "fp16"]
        }
        // FEAT_SHA3 arrived with A13; all Apple Silicon Macs (M1+) have it.
        "apple-a13" | "apple-a14" | "apple-s6" | "apple-s7" | "apple-s8" | "apple-m1" => {
            &["aes", "sha2", "sha3", "dotprod", "lse", "fp16"]
        }
        // FEAT_I8MM arrived with the A15/M2 generation.
        "apple-a15" | "apple-a16" | "apple-a17" | "apple-a18" | "apple-s9" | "apple-s10"
        | "apple-s11" | "apple-m2" | "apple-m3" | "apple-m4" | "apple-latest" => {
            &["aes", "sha2", "sha3", "dotprod", "lse", "fp16", "i8mm"]
        }
        // SSSE3-era (Core 2, original Atom): no SSE4.
        "core2" | "bonnell" | "saltwell" => &["cmpxchg16b", "lahfsahf", "ssse3"],
        // SSE4.1-era (Penryn; also the x86_64-apple-darwin spec default).
        "penryn" => &["cmpxchg16b", "lahfsahf", "sse4.1"],
        // SSE4.2-era Intel (v2): Nehalem/Westmere and friends.
        "x86-64-v2" | "nehalem" | "corei7" | "westmere" | "silvermont" | "slm"
        | "goldmont" | "goldmont-plus" | "tremont" => X64_V2,
        // AVX without AVX2: Sandy Bridge/Ivy Bridge and the matching AMD.
        "sandybridge" | "corei7-avx" | "ivybridge" | "core-avx-i" | "bdver1" | "bdver2"
        | "btver2" => X64_AVX,
        // AVX2-class (v3): Haswell onward, Zen 1–3, recent E-cores.
        "x86-64-v3" | "haswell" | "core-avx2" | "broadwell" | "skylake" | "kabylake"
        | "coffeelake" | "cometlake" | "whiskeylake" | "amberlake" | "alderlake"
        | "raptorlake" | "meteorlake" | "arrowlake" | "arrowlake-s" | "lunarlake"
        | "pantherlake" | "sierraforest" | "grandridge" | "clearwaterforest" | "gracemont"
        | "bdver4" | "znver1" | "znver2" | "znver3" => X64_V3,
        // AVX-512 F/BW/CD/DQ/VL (v4): Skylake-SP through Cascade/Cooper Lake.
        "x86-64-v4" | "skylake-avx512" | "cascadelake" | "cooperlake" => X64_V4,
        // v4 plus the VNNI/VBMI/BITALG extensions Cranelift has flags for
        // (Ice Lake onward, Zen 4/5).
        "icelake-client" | "icelake-server" | "tigerlake" | "rocketlake" | "sapphirerapids"
        | "emeraldrapids" | "graniterapids" | "graniterapids-d" | "diamondrapids" | "znver4"
        | "znver5" => X64_V4_EXT,
        _ => &[],
    }
}
impl CodegenBackend for PlironCodegenBackend {
    fn target_config(&self, sess: &rustc_session::EarlySession) -> TargetConfig {
        types::PTR32.store(
            sess.target.pointer_width == 32,
            std::sync::atomic::Ordering::Relaxed,
        );
        use rustc_target::spec::{Arch, Os};
        // Base-target features: the arch baseline, plus whatever the spec's
        // `features` string / `cpu` name imply, plus `-Ctarget-feature` (parsed
        // and validated by the helper against rustc's feature names).
        let feats = match sess.target.arch {
            Arch::X86_64 | Arch::AArch64 if sess.target.os != Os::None => {
                rustc_codegen_ssa::target_features::internal_target_features::<0>(
                    sess,
                    |_| Default::default(),
                    |feature| base_has_feature(sess, feature),
                )
            }
            Arch::Wasm32 => wasm::target_features(&sess.target, &sess.opts)
                .iter()
                .map(|f| Symbol::intern(f))
                .collect(),
            _ => Default::default(),
        };
        TargetConfig {
            internal_target_features: feats,
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
        sess: &Session,
        _cgcx: &CodegenContext,
        _shared_emitter: &SharedEmitter,
        _tm_factory: TargetMachineFactoryFn<Self>,
        _exported_symbols_for_lto: &[String],
        _each_linked_rlib_for_lto: &[PathBuf],
        _modules: Vec<FatLtoInput<Self>>,
    ) -> CompiledModule {
        sess.dcx().fatal("LTO is not supported by the pliron backend")
    }

    fn run_thin_lto(
        _cgcx: &CodegenContext,
        _prof: &SelfProfilerRef,
        dcx: DiagCtxtHandle<'_>,
        _exported_symbols_for_lto: &[String],
        _each_linked_rlib_for_lto: &[PathBuf],
        _modules: Vec<ThinLtoInput<Self>>,
    ) -> (Vec<ThinModule<Self>>, Vec<WorkProduct>) {
        dcx.fatal("LTO is not supported by the pliron backend")
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
