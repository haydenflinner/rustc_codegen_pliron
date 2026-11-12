//! x64 flag-recompute elision (`FLAGDUP`, default-on,
//! `PLIRON_FLAGDUP=0` disables): a compare/test whose identical flag
//! state is still live — same operands, only flag-preserving insts
//! between, no operand-preg rewrite — is not re-emitted. The classic
//! shape is one `icmp` feeding several consumers (`brif` + `select`
//! clamps); each consumer used to re-emit `cmp`.

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str, gate: &str) -> String {
    unsafe { std::env::set_var("PLIRON_FLAGDUP", gate) };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("x86_64")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

// FLAGX (`PLIRON_FLAGX=1`, opt-in): propagate the tracked compare and
// the flag bits a conditional predecessor's edges prove across a
// single-predecessor block boundary; a `cmov` whose condition the
// proven bits decide (false) is elided.
fn asm_fx(clif: &str, fx: &str) -> String {
    unsafe {
        std::env::set_var("PLIRON_FLAGDUP", "1");
        std::env::set_var("PLIRON_FLAGX", fx)
    };
    asm(clif, "1")
}

fn count(asm: &str, mnem: &str) -> usize {
    asm.lines()
        .filter(|l| l.split_whitespace().next() == Some(mnem))
        .count()
}

// One `icmp` feeding two `select` clamps (consumed by same-block
// loads, so the selects can't sink to successors) plus a `brif` on the
// same block's compare: three consumers used to emit three compares;
// flags stay valid across the cmovs/movs between them.
#[test]
fn icmp_multi_use() {
    let clif = "
function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v8 = icmp eq v1, v2
    v4 = select v8, v3, v1
    v9 = iadd v0, v4
    v10 = load.i64 v9
    v5 = select v8, v3, v2
    v11 = iadd v0, v5
    v12 = load.i64 v11
    v6 = iadd v10, v12
    brif v8, block1, block2
block1:
    return v6
block2:
    return v6
}";
    let on = asm(clif, "1");
    assert_eq!(count(&on, "cmpq"), 1, "{on}");
}

// An operand rewrite between the compares keeps the second one: the
// flags from the first `cmp` describe the *old* `v1`.
#[test]
fn operand_rewrite_keeps() {
    let clif = "
function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v8 = icmp eq v0, v1
    v4 = select v8, v2, v3
    v9 = iconst.i64 1
    v5 = iadd v1, v9
    v10 = icmp eq v0, v5
    v6 = select v10, v2, v3
    brif v10, block1, block2
block1:
    return v6
block2:
    return v4
}";
    let a = asm(clif, "1");
    // Both logical compares must remain — the second is not a recompute
    // of the first (v5 != v1).
    assert!(count(&a, "cmpq") >= 2, "{a}");
}

// A `lea` that shrinks to `add` (`emit_lea`, when the destination is
// also the base register) writes flags even though the mnemonic looks
// flag-preserving — the cmp3 miscompile. Whether regalloc produces the
// `lea dst,(dst)` shape isn't CLIF-controllable, so the regression is
// pinned end-to-end by the `tests/cmp3` stanza (sort ordering), which
// fails without the `lea_encodes_as_add` check in `flag_kind`.
//
// A flag-writing instruction between identical compares keeps the
// second: `isub` clobbers the flags the first `cmp` established.
#[test]
fn flag_writer_keeps() {
    let clif = "
function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v8 = icmp eq v0, v1
    v4 = select v8, v2, v3
    v5 = isub v0, v1
    v10 = icmp eq v0, v1
    v6 = select v10, v2, v3
    v7 = iadd v6, v5
    brif v10, block1, block2
block1:
    return v7
block2:
    return v4
}";
    let a = asm(clif, "1");
    assert!(count(&a, "cmpq") >= 2, "{a}");
}

// FLAGX (`PLIRON_FLAGX=1`): the compare feeding a `brif` propagates
// across each single-predecessor edge together with the flag bits the
// edge proves. On the `jz` not-taken edge ZF is known 0, so a `cmove`
// there is provably dead (and the re-issued compare dedups against the
// propagated state); on the taken edge ZF is known 1 and the `cmove`
// is provably live — it stays.
#[test]
fn edge_cc_folds_cmov() {
    let clif = "
function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v8 = icmp eq v0, v1
    brif v8, block1, block2
block1:
    v4 = select v8, v2, v3
    return v4
block2:
    v5 = select v8, v3, v2
    return v5
}";
    let off = asm_fx(clif, "0");
    let on = asm_fx(clif, "1");
    assert_eq!(count(&off, "cmpq"), 3, "{off}");
    assert_eq!(count(&off, "cmoveq"), 2, "{off}");
    assert_eq!(count(&on, "cmpq"), 1, "{on}");
    assert_eq!(count(&on, "cmoveq"), 1, "{on}");
}

// With two predecessors the flag state at a block entry is not pinned
// by either edge — the propagated compare and bits may only be used
// for single-pred successors. block3 is reached both from the `brif`'s
// taken edge and from block4, so its select keeps both the compare and
// the cmov; block5 is single-pred and still folds.
#[test]
fn multi_pred_keeps_cmov() {
    let clif = "
function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v9 = icmp ne v2, v3
    brif v9, block1, block4
block1:
    v8 = icmp eq v0, v1
    brif v8, block3, block5
block4:
    jump block3
block3:
    v11 = icmp eq v0, v1
    v5 = select v11, v3, v2
    return v5
block5:
    v12 = icmp eq v0, v1
    v6 = select v12, v2, v3
    return v6
}";
    let off = asm_fx(clif, "0");
    let on = asm_fx(clif, "1");
    // block3 (two preds) keeps its compare and cmov; block5
    // (single-pred, ZF=0 edge) dedups the compare and folds the
    // provably-false `cmove`.
    assert_eq!(count(&off, "cmpq"), 4, "{off}");
    assert_eq!(count(&off, "cmoveq"), 2, "{off}");
    assert_eq!(count(&on, "cmpq"), 3, "{on}");
    assert_eq!(count(&on, "cmoveq"), 1, "{on}");
}
