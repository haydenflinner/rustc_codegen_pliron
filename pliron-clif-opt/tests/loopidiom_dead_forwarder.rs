//! `plan_dead` walks trivial forwarders on the exit path and binds their
//! params in `fwd`, then emits a fast edge straight to the final dest —
//! past the binding block. A forwarder param read under that dest (std's
//! `PathBuf::__push` read `block20`'s param from the merge two blocks
//! down, wasm verifier ICE) escapes just like a loop value: `exit_dest`
//! gains a param, old edges pass the original, the fast edge passes the
//! closed form.

use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;
use target_lexicon::PointerWidth;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn tcfg() -> TargetFrontendConfig {
    TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: 12,
    }
}

fn run_idiom(f: &mut Function) -> usize {
    pliron_clif_opt::loopidiom::run(
        f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        tcfg(),
        "f",
    )
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("{e}\n{}", f.display());
    }
}

/// Dead counting loop `for _ in 0..v0 {}`; the exit edge feeds an
/// invariant param to a lone-jump forwarder `block2`, whose param `v7`
/// is read by the merge (`block4`) and a call two blocks down. The fast
/// edge `brif ok, block4, block1` skips `block2`, so `v7` needs an
/// escape param.
fn fwd_src() -> &'static str {
    "function %f(i32, i32) -> i32 {
        sig0 = (i32, i32) -> i32 system_v
        fn0 = %g sig0

    block0(v0: i32, v1: i32):
        v2 = iconst.i32 0
        jump block1(v2, v1)

    block1(v3: i32, v4: i32):
        v5 = icmp uge v3, v0
        brif v5, block2(v4), block3

    block3:
        v12 = iconst.i32 1
        v6 = iadd v3, v12
        jump block1(v6, v4)

    block2(v7: i32):
        jump block4

    block4:
        v8 = isub v1, v7
        v9 = icmp ult v8, v0
        brif v9, block5, block6(v7)

    block5:
        v10 = call fn0(v0, v7)
        return v10

    block6(v11: i32):
        return v11
    }"
}

/// The old escape scan only tracked loop-internal values: `v7` (the
/// forwarder `block2`'s param) was left dangling on the new fast edge.
#[test]
fn dead_loop_forwarder_param_escapes() {
    let before = parse(fwd_src());
    let mut after = before.clone();
    assert!(run_idiom(&mut after) > 0, "{}", after.display());
    verify(&after);
    check(&before, &after, &Facts::default(), &Config::default()).unwrap();
}

/// When the forwarder param is read by `dest` itself the same escape
/// applies — the minimal form of the crash.
#[test]
fn dead_loop_forwarder_param_at_dest() {
    let mut f = parse(
        "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v2 = iconst.i32 0
        jump block1(v2, v1)

    block1(v3: i32, v4: i32):
        v5 = icmp uge v3, v0
        brif v5, block2(v4), block3

    block3:
        v9 = iconst.i32 1
        v6 = iadd v3, v9
        jump block1(v6, v4)

    block2(v7: i32):
        jump block4

    block4:
        v8 = isub v1, v7
        return v8
    }",
    );
    assert!(run_idiom(&mut f) > 0, "{}", f.display());
    verify(&f);
}

/// The merge block gains a param bound to the forwarder param on the
/// old edges and to its closed form on the fast edge — so semantics
/// hold on both paths.
#[test]
fn dead_loop_forwarder_transforms() {
    let before = parse(fwd_src());
    let mut after = before.clone();
    assert!(run_idiom(&mut after) > 0, "{}", after.display());
    verify(&after);
    check(&before, &after, &Facts::default(), &Config::default()).unwrap();
    // The merge (the block holding the `isub`) gained the escape param.
    let merge = after
        .layout
        .blocks()
        .find(|&b| {
            after
                .layout
                .block_insts(b)
                .any(|i| after.dfg.insts[i].opcode() == Opcode::Isub)
        })
        .unwrap();
    assert_eq!(after.dfg.block_params(merge).len(), 1, "{}", after.display());
}
