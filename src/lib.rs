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
mod consts;
mod context;
mod eh;
mod hot;
mod intrinsic;
mod lower;
mod objmerge;
mod simd;
mod type_of;
mod types;
mod wasm;
mod xcgu;

// CLIF-pass modules live in the stable-Rust `pliron-clif-opt` crate; the
// glob re-export keeps `crate::<pass>` paths working unchanged.
pub use pliron_clif_opt::*;
pub use pliron_ir_opt::{
    address_taken, callgvn, constload, domcheck, has_body, inline, instcombine, memcpyopt, nnret,
    nocapture, nounwind, nowrite, pgo, phisimp, retarg, spec, sroa,
};

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

/// `base_has_feature` for the codegen `Session` (its twin takes
/// `EarlySession`), plus `-Ctarget-feature` `+/-` overrides on top.
fn sess_has_feature(sess: &Session, feature: &str) -> bool {
    let baseline: &[&str] = match sess.target.arch {
        rustc_target::spec::Arch::X86_64 => &["fxsr", "sse", "sse2", "x87"],
        rustc_target::spec::Arch::AArch64 => &["neon"],
        _ => &[],
    };
    let cpu = sess
        .opts
        .cg
        .target_cpu
        .as_deref()
        .unwrap_or(&sess.target.cpu);
    let mut on = baseline.contains(&feature)
        || if cpu == "native" {
            native_feat(sess, feature)
        } else {
            cpu_features(cpu).contains(&feature)
        };
    for t in sess.opts.cg.target_feature.split(',') {
        let t = t.trim();
        if t == feature || t == format!("+{feature}") {
            on = true;
        } else if t == format!("-{feature}") {
            on = false;
        }
    }
    on
}
#[cfg(target_arch = "x86_64")]
fn native_feat(sess: &Session, feature: &str) -> bool {
    let _ = sess;
    match feature {
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
        "avx512f" => std::arch::is_x86_feature_detected!("avx512f"),
        "avx512dq" => std::arch::is_x86_feature_detected!("avx512dq"),
        "avx512vl" => std::arch::is_x86_feature_detected!("avx512vl"),
        "avx512bw" => std::arch::is_x86_feature_detected!("avx512bw"),
        "avx512cd" => std::arch::is_x86_feature_detected!("avx512cd"),
        "avx512vbmi" => std::arch::is_x86_feature_detected!("avx512vbmi"),
        "avx512bitalg" => std::arch::is_x86_feature_detected!("avx512bitalg"),
        "avx512vnni" => std::arch::is_x86_feature_detected!("avx512vnni"),
        "avxvnni" => std::arch::is_x86_feature_detected!("avxvnni"),
        _ => false,
    }
}
#[cfg(not(target_arch = "x86_64"))]
fn native_feat(_sess: &Session, _feature: &str) -> bool {
    false
}
fn build_isa(sess: &Session, tail_calls: bool) -> Arc<dyn TargetIsa> {
    let mut fb = settings::builder();
    // COFF has no GOT: is_pic would emit sym@GOTPCREL loads that read the
    // symbol's bytes rather than its address.
    let coff = sess.target.options.binary_format == rustc_target::spec::BinaryFormat::Coff;
    fb.set("is_pic", if coff { "false" } else { "true" })
        .unwrap();
    if crate::env_is("PLIRON_RA_CHECK", "1") {
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
    // cg_clif: unwinding uses .eh_frame/.xdata, so rbp is free for the register
    // allocator. `PLIRON_OMIT_FP=0` keeps them unconditionally.
    let fp = { sess.target.options.frame_pointer }.ratchet(sess.opts.cg.force_frame_pointers);
    // Cranelift's x64 `return_call` emission requires frame pointers, and an
    // explicit `-Cforce-frame-pointers` wins outright. Otherwise omit fp even
    // where the spec defaults to `Always` (windows-msvc): unwind is
    // metadata-driven and LLVM drops it for ordinary functions the same way.
    let forced = sess.opts.cg.force_frame_pointers != rustc_target::spec::FramePointer::MayOmit;
    let keep_fp = tail_calls || forced || !pass_enabled("PLIRON_OMIT_FP");
    if crate::env_set("PLIRON_DEBUG_FP") {
        eprintln!(
            "fp debug: spec={:?} cgflag={:?} ratchet={:?} tail_calls={} forced={} omit_fp={} keep={}",
            sess.target.options.frame_pointer,
            sess.opts.cg.force_frame_pointers,
            fp,
            tail_calls,
            forced,
            pass_enabled("PLIRON_OMIT_FP"),
            keep_fp
        );
    }
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
    // ISA extension flags from rustc's effective target features.
    // Without this everything compiles to the arch baseline (SSE2 on
    // x86-64) no matter what `-Ctarget-cpu`/`-Ctarget-feature` asks.
    for (feat, flag) in [
        ("sse3", "has_sse3"),
        ("ssse3", "has_ssse3"),
        ("sse4.1", "has_sse41"),
        ("sse4.2", "has_sse42"),
        ("popcnt", "has_popcnt"),
        ("avx", "has_avx"),
        ("avx2", "has_avx2"),
        ("fma", "has_fma"),
        ("bmi1", "has_bmi1"),
        ("bmi2", "has_bmi2"),
        ("lzcnt", "has_lzcnt"),
        ("cmpxchg16b", "has_cmpxchg16b"),
        ("avx512f", "has_avx512f"),
        ("avx512dq", "has_avx512dq"),
        ("avx512vl", "has_avx512vl"),
        ("avx512vbmi", "has_avx512vbmi"),
        ("avx512bitalg", "has_avx512bitalg"),
        ("avx512vnni", "has_avx512vnni"),
        ("avxvnni", "has_avx_vnni"),
    ] {
        if sess_has_feature(sess, feat) {
            let _ = fb.enable(flag);
        }
    }
    fb.set(
        "opt_level",
        if sess.opts.optimize == OptLevel::No || !crate::pass_enabled("PLIRON_COMPILE_EGRAPH") {
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
    let tf = |rust: &str| {
        sess.internal_target_features
            .contains(&Symbol::intern(rust))
    };
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
        && crate::env_is("PLIRON_X64_VEX", "1")
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

/// Install the tracing subscriber when `PLIRON_TRACE` is set. The env var
/// is an [`EnvFilter`](tracing_subscriber::EnvFilter) over our `pliron::*`
/// span targets — `pliron` for everything, `pliron::phase` for per-phase
/// times, `pliron::iropt` for each ir-opt pass, `pliron::step` for every
/// pipeline step (span name = step name), `pliron::fn` per function. The
/// fmt layer emits each span's `time.busy` on close. Narrow further with
/// grep — EnvFilter's `[{field=x}]` syntax only gates events *inside* a
/// matching span, not the span's own close line. Unset means no subscriber
/// is installed and span sites cost a branch or a hash lookup.
fn tracing_init() {
    if std::env::var_os("PLIRON_TRACE").is_none() {
        return;
    }
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_env_var("PLIRON_TRACE")
        .from_env_lossy();
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
                .with_ansi(false)
                .compact(),
        )
        .with(filter)
        .try_init();
}

/// Wrap an ir-opt pass call in a tracing span: `PLIRON_TRACE=pliron::iropt`
/// prints each pass's `time.busy` on span close (see `tracing_init`).
macro_rules! pt {
    ($cx:expr, $n:literal, $e:expr) => {{
        let _s = tracing::info_span!(target: "pliron::iropt", $n).entered();
        // Same interval in rustc's self-profile: `-Zself-profile` shows
        // `pliron_ir_<pass>` next to LLVM's own `LLVM_*` activities.
        let _pg = $cx
            .tcx
            .sess
            .prof
            .generic_activity(concat!("pliron_ir_", $n));
        $e
    }};
}

fn finish_module(cx: &CodegenCx<'_>, name: &str) -> PlironModule {
    let emit_ir = cx
        .tcx
        .sess
        .opts
        .output_types
        .contains_key(&OutputType::LlvmAssembly)
        || crate::env_set("PLIRON_DUMP");
    let ir = if emit_ir {
        cx.print_ir()
    } else {
        String::new()
    };
    if crate::env_set("PLIRON_DUMP") {
        eprintln!("==== {name} ====\n{ir}");
    }
    if cx.tcx.sess.opts.optimize == OptLevel::No && pass_enabled("PLIRON_ALWAYS_INLINE") {
        let (ctx, st) = (&mut *cx.pctx.borrow_mut(), &mut *cx.st.borrow_mut());
        inline::run(ctx, st, true, None, true, None, &mut None);
        domcheck::run(ctx, st, "always-inline");
        inline::dead_fns(ctx, st);
    }
    // PGO: `PLIRON_PGO=<dir>` instruments every body (source-level
    // attribution — the inliner clones counters, keeping counts on the
    // callee), `PLIRON_PGO_USE=<file>` loads a profile before the
    // pipeline. Both run after the O0 always-inline round so
    // fingerprints are computed at the same CFG stage in gen and use
    // builds.
    if crate::env_set("PLIRON_PGO") || crate::env_set("PLIRON_PGO_USE") {
        let (ctx, st) = (&mut *cx.pctx.borrow_mut(), &mut *cx.st.borrow_mut());
        let wasm = cx.tcx.sess.target.arch == rustc_target::spec::Arch::Wasm32;
        if crate::env_set("PLIRON_PGO") {
            let (hdr, ctor, _) = pgo::instrument(ctx, st, name, wasm);
            if !wasm
                && let Some(a) = pgo::ctor_asm(&hdr, &ctor, matches!(
                    cx.tcx.sess.target.arch,
                    rustc_target::spec::Arch::AArch64
                ))
            {
                st.asm.push_str(&a);
            }
        }
        if let Some(f) = crate::env_get("PLIRON_PGO_USE") {
            pgo::load(ctx, st, f);
        }
    }
    let _siro = tracing::info_span!(target: "pliron::phase", "iropt").entered();
    let _pgiro = cx.tcx.sess.prof.generic_activity("pliron_iropt");
    if cx.tcx.sess.opts.optimize != OptLevel::No {
        let (ctx, st) = (&mut *cx.pctx.borrow_mut(), &mut *cx.st.borrow_mut());
        let small = cx.tcx.sess.target.arch == rustc_target::spec::Arch::Wasm32
            || matches!(
                cx.tcx.sess.opts.optimize,
                OptLevel::Size | OptLevel::SizeMin
            );
        if pass_enabled("PLIRON_FROZEN") {
            pt!(cx, "frozen", st.frozen = context::frozen_values(ctx, st).into_iter().collect());
        }
        if pass_enabled("PLIRON_NOALIAS_FWD") {
            pt!(cx, "noalias", st.noalias = context::noalias_values(ctx, st)
                .into_iter()
                .map(|(v, d, w)| (v, (d, w)))
                .collect());
        }
        if pass_enabled("PLIRON_NNARGS") {
            pt!(cx, "nnargs", st.nnargs = context::nonnull_values(ctx, st).into_iter().collect());
        }
        let mut adj = None;
        let d1 = pt!(cx, 
            "inline",
            inline::run(ctx, st, small, None, false, None, &mut adj)
        );
        // Second round: callsites that just moved into a hot caller (e.g.
        // `get`'s `hash_one` landing inside `main`'s probe loop) never got
        // scored in their new context — re-run so the loop bonus applies.
        // Incremental: rescore only the callers round 1 inlined into, plus
        // callers of callees that lost a callsite (a now-single callee can
        // newly inline free there).
        if pass_enabled("PLIRON_INLINE_ITER") && !d1.callers.is_empty() {
            let mut touched = d1.callers;
            if let Some(a) = &adj {
                for c in &d1.callees {
                    if let Some(v) = a.callers_of.get(c) {
                        touched.extend(v.iter().cloned());
                    }
                }
            }
            pt!(cx, 
                "inline2",
                inline::run(ctx, st, small, None, false, Some(&touched), &mut adj)
            );
        }
        pt!(cx, "domcheck:inline", domcheck::run(ctx, st, "inline"));
        if pass_enabled("PLIRON_SPEC") {
            pt!(cx, "spec", spec::run(ctx, st));
            pt!(cx, "domcheck:spec", domcheck::run(ctx, st, "spec"));
        }
        // Opt-in: unsound on wasm emulated EH, which unwinds through ordinary
        // calls (llvm.wasm.throw / __pliron_eh) the body scan can't see. On
        // COFF the emulated-EH "throw" is a flag+early-return through plain
        // calls, so inference is sound — and lets lowering skip the flag
        // check after calls that can't throw.
        let coff_eh = cx.tcx.sess.target.options.binary_format
            == rustc_target::spec::BinaryFormat::Coff
            && cx.tcx.sess.panic_strategy() == rustc_target::spec::PanicStrategy::Unwind;
        if pass_enabled("PLIRON_NOUNWIND") || coff_eh {
            pt!(cx, "nounwind", nounwind::run(ctx, st));
        }
        if pass_enabled("PLIRON_SRET2REG") {
            pt!(cx, "sret2reg", abi::run(ctx, st));
            pt!(cx, "domcheck:abi", domcheck::run(ctx, st, "abi"));
        }
        if crate::env_is("PLIRON_DEADARG", "1") {
            pt!(cx, "deadarg", abi::dead_args(ctx, st));
        }
        if pass_enabled("PLIRON_INSTCOMBINE") {
            pt!(cx, "instcombine", instcombine::run(ctx, st));
            pt!(cx, "domcheck:ic", domcheck::run(ctx, st, "instcombine"));
        }
        if pass_enabled("PLIRON_MEMCPYOPT") {
            pt!(cx, "memcpyopt", memcpyopt::run(ctx, st));
            pt!(cx, "domcheck:mc", domcheck::run(ctx, st, "memcpyopt"));
        }
        if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_SROA_FWD") {
            pt!(cx, "sroa-fwd", sroa::forward(ctx, st));
            pt!(cx, "domcheck:sf", domcheck::run(ctx, st, "sroa-fwd"));
        }
        if pass_enabled("PLIRON_PHISIMP") {
            pt!(cx, "phisimp", phisimp::run(ctx, st));
            pt!(cx, "domcheck:ps", domcheck::run(ctx, st, "phisimp"));
        }
        // Run ahead of sroa: its `copyable_arg` check consults `st.nowrite`
        // and `st.nocapture` to decide which call args can take a fresh copy.
        if pass_enabled("PLIRON_NOWRITE") {
            pt!(cx, "nowrite", nowrite::run(ctx, st));
        }
        if pass_enabled("PLIRON_NOCAP") {
            pt!(cx, "nocap", nocapture::run(ctx, st));
        }
        if pass_enabled("PLIRON_SROA") {
            pt!(cx, "sroa", sroa::run(ctx, st));
            pt!(cx, "domcheck:sroa", domcheck::run(ctx, st, "sroa"));
        }
        if pass_enabled("PLIRON_CONSTLOAD") {
            pt!(cx, "constload", constload::run(ctx, st));
            pt!(cx, "domcheck:cl", domcheck::run(ctx, st, "constload"));
            // Inlining + SROA make vtable pointers constant: call those slots
            // directly, inline them and clean up again (`PLIRON_DEVIRT=0` disables).
            if pass_enabled("PLIRON_DEVIRT") {
                let (sites, owners) = pt!(cx, "devirt", inline::devirt(ctx, st));
                pt!(cx, "domcheck:dv", domcheck::run(ctx, st, "devirt"));
                if !sites.is_empty() && pass_enabled("PLIRON_DEVIRT_INLINE") {
                    // Passes since the inline rounds may have moved calls —
                    // rebuild the adjacency fresh.
                    adj = None;
                    pt!(cx, 
                        "devirt-inline",
                        inline::run(
                            ctx,
                            st,
                            small,
                            Some(&sites),
                            false,
                            Some(&owners),
                            &mut adj
                        )
                    );
                    pt!(cx, "domcheck:di", domcheck::run(ctx, st, "devirt-inline"));
                    if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_SROA_FWD") {
                        pt!(cx, "sroa-fwd2", sroa::forward(ctx, st));
                    }
                    if pass_enabled("PLIRON_PHISIMP") && pass_enabled("PLIRON_DEVIRT_PHI") {
                        pt!(cx, "phisimp2", phisimp::run(ctx, st));
                    }
                    if pass_enabled("PLIRON_SROA") && pass_enabled("PLIRON_DEVIRT_SROA") {
                        pt!(cx, "sroa2", sroa::run(ctx, st));
                    }
                    if pass_enabled("PLIRON_DEVIRT_CL") {
                        pt!(cx, "constload2", constload::run(ctx, st));
                    }
                }
            }
        }
        // Re-run capture inference now that devirt/constload have made
        // callee sets final: indirect calls through fn-ptr slots resolve to
        // `arg` edges, and params those callees don't keep go non-capturing.
        // The earlier run stays for sroa's `copyable_arg` check.
        if pass_enabled("PLIRON_NOCAP") {
            pt!(cx, "nocap2", nocapture::run(ctx, st));
        }
        // IPO return-value attributes, after inlining/devirt so remaining
        // direct calls are the final set. `nnret` is consumed at lowering
        // (nonnull call results feed jumpthread); `retarg` folds call
        // results to the returned argument in place.
        if pass_enabled("PLIRON_NNRET") {
            pt!(cx, "nnret", nnret::run(ctx, st));
        }
        if pass_enabled("PLIRON_RETARG") {
            pt!(cx, "retarg", retarg::run(ctx, st));
            pt!(cx, "domcheck:ra", domcheck::run(ctx, st, "retarg"));
        }
        // Dead-call elim + readonly call dedup + loop-call hoist, on the
        // final post-inline call set.
        if pass_enabled("PLIRON_CALLGVN") {
            pt!(cx, "callgvn", callgvn::run(ctx, st));
            pt!(cx, "domcheck:cg", domcheck::run(ctx, st, "callgvn"));
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
        st.tailrec = pass_enabled("PLIRON_TAILREC");
        if pass_enabled("PLIRON_DEADFN") {
            inline::dead_fns(ctx, st);
        }
    }
    drop(_siro);
    if crate::env_set("PLIRON_DUMP_OPT") {
        eprintln!("==== {name} (optimized) ====\n{}", cx.print_ir());
    }
    if cx.tcx.sess.target.arch == rustc_target::spec::Arch::Wasm32 {
        let obj = if crate::env_set("PLIRON_WASM_CLIF") {
            wasm::lower_to_wasm_clif(
                &cx.pctx.borrow(),
                &cx.st.borrow(),
                cx.tcx.sess,
                name,
                &wasm::target_features(&cx.tcx.sess.target, &cx.tcx.sess.opts),
                cx.tcx.sess.panic_strategy() == rustc_target::spec::PanicStrategy::Unwind,
            )
        } else {
            wasm::lower_to_wasm(
                &cx.pctx.borrow(),
                &cx.st.borrow(),
                name,
                &wasm::target_features(&cx.tcx.sess.target, &cx.tcx.sess.opts),
                cx.tcx.sess.panic_strategy() == rustc_target::spec::PanicStrategy::Unwind,
            )
        };
        return PlironModule {
            obj,
            ir,
            asm: String::new(),
        };
    }
    let isa = build_isa(
        cx.tcx.sess,
        cx.tcx
            .features()
            .enabled(rustc_span::sym::explicit_tail_calls)
            || crate::env_set("PLIRON_TAILCALL"),
    );
    let hot = crate::env_get("PLIRON_HOT")
        .is_some_and(|c| c == cx.tcx.crate_name(rustc_span::def_id::LOCAL_CRATE).as_str());
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
    let cpu = sess
        .opts
        .cg
        .target_cpu
        .as_deref()
        .unwrap_or(&sess.target.cpu);
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
        "avx2",
        "bmi1",
        "bmi2",
        "f16c",
        "fma",
        "lzcnt",
        "movbe",
        "cmpxchg16b",
        "lahfsahf",
        "popcnt",
    ];
    const X64_V4: &[&str] = &[
        "avx2",
        "bmi1",
        "bmi2",
        "f16c",
        "fma",
        "lzcnt",
        "movbe",
        "cmpxchg16b",
        "lahfsahf",
        "popcnt",
        "avx512bw",
        "avx512cd",
        "avx512dq",
        "avx512vl",
    ];
    const X64_V4_EXT: &[&str] = &[
        "avx2",
        "bmi1",
        "bmi2",
        "f16c",
        "fma",
        "lzcnt",
        "movbe",
        "cmpxchg16b",
        "lahfsahf",
        "popcnt",
        "avx512bw",
        "avx512cd",
        "avx512dq",
        "avx512vl",
        "avx512bitalg",
        "avx512vbmi",
        "avx512vnni",
    ];
    match cpu {
        // Apple A12-generation and later: v8.3/v8.4+ — dotprod, lse, fp16.
        "apple-a12" | "apple-s4" | "apple-s5" => &["aes", "sha2", "dotprod", "lse", "fp16"],
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
        "x86-64-v2" | "nehalem" | "corei7" | "westmere" | "silvermont" | "slm" | "goldmont"
        | "goldmont-plus" | "tremont" => X64_V2,
        // AVX without AVX2: Sandy Bridge/Ivy Bridge and the matching AMD.
        "sandybridge" | "corei7-avx" | "ivybridge" | "core-avx-i" | "bdver1" | "bdver2"
        | "btver2" => X64_AVX,
        // AVX2-class (v3): Haswell onward, Zen 1–3, recent E-cores.
        "x86-64-v3" | "haswell" | "core-avx2" | "broadwell" | "skylake" | "kabylake"
        | "coffeelake" | "cometlake" | "whiskeylake" | "amberlake" | "alderlake" | "raptorlake"
        | "meteorlake" | "arrowlake" | "arrowlake-s" | "lunarlake" | "pantherlake"
        | "sierraforest" | "grandridge" | "clearwaterforest" | "gracemont" | "bdver4"
        | "znver1" | "znver2" | "znver3" => X64_V3,
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
        tracing_init();
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
        if tcx.sess.panic_strategy() == rustc_target::spec::PanicStrategy::Unwind
            && tcx.sess.target.options.binary_format == rustc_target::spec::BinaryFormat::Coff
        {
            // TLS slot driving emulated EH on COFF: `{ flag: i32, _pad: i32,
            // exn: ptr, buf: [u8; 32] }` — the buffer holds a copy of
            // `panic_unwind::seh::Exception` made at the throw site so it
            // outlives the unwound frame. See `eh_tls`/`eh_check` in lower.rs.
            let ty = cx.type_array(cx.type_i8(), 48);
            let init = cx.new_value(ty, context::ConstVal::Bytes(vec![0; 48]));
            cx.declare_global(
                "__pliron_eh",
                context::GlobalInfo {
                    ty,
                    init: Some(init),
                    align: 8,
                    mutable: true,
                    tls: true,
                    linkage: Linkage::Export,
                    used: false,
                    section: None,
                },
            );
        }
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
    let _smod = tracing::info_span!(target: "pliron::module", "module", cgu = %cgu_name).entered();
    let _pgmod = tcx
        .sess
        .prof
        .generic_activity_with_arg("pliron_module", cgu_name.as_str());
    let cgu = tcx.codegen_unit(cgu_name);
    let mut cx = CodegenCx::new(tcx, cgu_name.as_str());
    cx.st.borrow_mut().cgu = cgu_name.to_string();
    let mono_items = cgu.items_in_deterministic_order(tcx);
    let extra = if tcx.sess.opts.optimize != OptLevel::No
        && pass_enabled("PLIRON_XCGU")
        && !tcx.sess.target.is_like_wasm
        && !crate::env_set("PLIRON_HOT")
    {
        xcgu::copies(tcx, cgu)
    } else {
        Vec::new()
    };
    if crate::env_set("PLIRON_STATS") {
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
    let _sdef = tracing::info_span!(target: "pliron::phase", "define").entered();
    let _pgdef = tcx.sess.prof.generic_activity("pliron_define");
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
    drop(_sdef);
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
        sess.dcx()
            .fatal("LTO is not supported by the pliron backend")
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
