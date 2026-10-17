//! `plirun`: compile a dumped `.pliron` module (e.g. a CubeCL kernel from
//! `CUBECL_DEBUG_PLIRON=<dir>`) through the same scan -> ir-opt -> CLIF ->
//! object path the CubeCL `CUBECL_PLIRON_CLIF=1` JIT uses, without needing
//! cubek/cubecl built. Writes a native object file.
//!
//!     cargo run -p pliron-clif-lower --example plirun -- kernel.pliron out.o
//!
//! `PLIRON_*` knobs work as usual: `PLIRON_CORPUS=<dir>` dumps each fn's
//! pre-pipeline CLIF, `PLIRON_TRACE=pliron::loopvec=debug` shows pass
//! decisions, `PLIRON_PIPELINE=a,b`/`PLIRON_<PASS>=0` gate steps.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use pliron::combine::Parser;
use pliron::context::{Context, Ptr};
use pliron::location::Source;
use pliron::operation::Operation;
use pliron::parsable::{State, state_stream_from_iterator};

use pliron_clif_lower::{iropt, lower, scan, types};

fn clif_isa() -> Arc<dyn cranelift_codegen::isa::TargetIsa> {
    use cranelift_codegen::settings::{self, Configurable};
    let mut fb = settings::builder();
    fb.set("is_pic", "true").unwrap();
    fb.set("enable_verifier", "false").unwrap();
    fb.set("preserve_frame_pointers", "false").unwrap();
    fb.set("opt_level", "speed_and_size").unwrap();
    fb.set("enable_llvm_abi_extensions", "true").unwrap();
    fb.enable("enable_multi_ret_implicit_sret").unwrap();
    let flags = settings::Flags::new(fb);
    cranelift_native::builder()
        .expect("native ISA builder")
        .finish(flags)
        .expect("native ISA finish")
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PLIRON_TRACE")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .without_time()
        .with_target(false)
        .with_level(false)
        .with_writer(std::io::stderr)
        .init();

    let mut args = std::env::args().skip(1);
    let input = args.next().expect("usage: plirun <in.pliron> <out.o>");
    let output = args.next().expect("usage: plirun <in.pliron> <out.o>");
    let text = std::fs::read_to_string(&input).expect("read input");

    let mut ctx = Context::new();
    let module: Ptr<Operation> = {
        let source = Source::new_from_file(&ctx, input.clone());
        let ss = state_stream_from_iterator(text.chars(), State::new(&mut ctx, source));
        Operation::top_level_parser()
            .parse(ss)
            .unwrap_or_else(|e| panic!("parse {input}: {e:?}"))
            .0
    };

    let isa = clif_isa();

    // Host SIMD knobs, mirroring cubecl-llvm's `compile_clif`.
    {
        let flag = |name: &str| {
            isa.isa_flags()
                .iter()
                .find(|v| v.name == name)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        types::WIDE_SIMD.store(true, Relaxed);
        types::NATIVE_F16.store(flag("has_fp16") || flag("has_avx512fp16"), Relaxed);
        types::F16C.store(flag("has_fp16") || flag("has_f16c"), Relaxed);
        types::NATIVE_BF16.store(flag("has_avx512bf16"), Relaxed);
        let wide = if flag("has_avx512f") {
            512
        } else if flag("has_avx2") {
            256
        } else {
            0
        };
        types::WIDE_F32_BITS.store(wide, Relaxed);
    }

    let name = "kernel";

    let mut st = scan::scan(&mut ctx, module);
    let mut adj = None;
    iropt::inline::run(&mut ctx, &mut st, false, iropt::inline::SMALL_LIMIT, 0, None, false, None, &mut adj);
    iropt::instcombine::run(&mut ctx, &mut st);
    iropt::memcpyopt::run(&mut ctx, &mut st);
    iropt::phisimp::run(&mut ctx, &mut st);
    iropt::sroa::run(&mut ctx, &mut st);
    iropt::constload::run(&mut ctx, &mut st);

    let object = lower::lower_to_object(false, false, &ctx, &st, isa, &name, &lower::NoHost);
    std::fs::write(&output, object).expect("write output");
    eprintln!("wrote {output}");
}
