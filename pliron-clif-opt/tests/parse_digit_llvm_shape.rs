//! LLVM's `u32::from_str` / parse_only shape (wasm of `parse_only::run`):
//!
//! 1. `br_table` on length for the short / odd cases.
//! 2. **One** shared straight-line unroll of up to 8 digits:
//!    `load8(p+k); d=c-'0'; if d>9 fail; acc=acc*10+d; if len==k+1 done`.
//! 3. ~7 `imul` of 10 total — not one copy per length arm.
//!
//! Peel is supposed to emit (2). This file pins that shape so we can
//! iterate under `harness/dev.py test parse_digit` / relax without a
//! full wasmbench rebuild.

use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::verifier::verify_function;
use cranelift_codegen::settings::{self, Configurable};
use pliron_clif_opt::pipeline::Analysis;

/// Counted digit loop: `for i in 0..len { acc = acc*10 + (load8(p+i)-'0') }`
/// with a dominating `len <= 8` (the `can_not_overflow` path for u32/radix10).
fn digit_loop_src() -> &'static str {
    // Dominating `len <= 8`, then `for i in 0..len { acc = acc*10 + (p[i]-'0') }`.
    // Block names must be `blockN` — cranelift-reader rejects underscores.
    r#"
function u0:0(i64, i64) -> i32, i8 fast {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 8
    v3 = icmp ugt v1, v2
    brif v3, block5, block2

block2:
    v4 = iconst.i32 0
    v5 = iconst.i64 0
    jump block1(v5, v4)

block1(v10: i64, v11: i32):
    v12 = icmp ult v10, v1
    brif v12, block3, block4

block3:
    v13 = iadd v0, v10
    v14 = uload8.i32 v13
    v15 = iconst.i32 48
    v16 = isub v14, v15
    v17 = iconst.i32 9
    v18 = icmp ugt v16, v17
    brif v18, block5, block6

block6:
    v19 = iconst.i32 10
    v20 = imul v11, v19
    v21 = iadd v20, v16
    v22 = iconst.i64 1
    v23 = iadd v10, v22
    jump block1(v23, v21)

block4:
    v30 = iconst.i8 1
    return v11, v30

block5:
    v31 = iconst.i32 0
    v32 = iconst.i8 0
    return v31, v32
}
"#
}

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn flags() -> settings::Flags {
    let mut b = settings::builder();
    b.set("is_pic", "true").unwrap();
    b.enable("enable_verifier").unwrap();
    settings::Flags::new(b)
}

fn imuls(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Imul)
        .count()
}

fn br_tables(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i], cranelift_codegen::ir::InstructionData::BranchTable { .. }))
        .count()
}

fn back_edges(f: &Function) -> usize {
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(f);
    let dt = cranelift_codegen::dominator_tree::DominatorTree::with_function(f, &cfg);
    let mut la = cranelift_codegen::loop_analysis::LoopAnalysis::new();
    la.compute(f, &cfg, &dt);
    la.loops().count()
}

/// Early `len` exits interleaved with the unroll (LLVM: `if len == k done`
/// after each digit; we emit remaining-count `icmp` against `v1`).
fn len_exit_icmps(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| {
            matches!(
                f.dfg.insts[i],
                cranelift_codegen::ir::InstructionData::IntCompare {
                    opcode: Opcode::Icmp,
                    ..
                }
            )
        })
        .filter(|&i| {
            // Compare involving the length param v1 (block0's second arg).
            let args = f.dfg.inst_args(i);
            let len = f.dfg.block_params(f.layout.entry_block().unwrap())[1];
            args.iter().any(|&a| f.dfg.resolve_aliases(a) == len)
        })
        .count()
}

fn rotate_then_peel(f: &mut Function) {
    let mut an = Analysis::default();
    let mut derived = pliron_clif_opt::vmap::VMap::default();
    // Peel wants the do-while / latch-at-bottom shape looprot emits.
    pliron_clif_opt::looprot::run_with(
        f,
        &mut an,
        &mut derived,
        false,
        &rustc_hash::FxHashMap::default(),
    );
    pliron_clif_opt::punroll::run(
        f,
        &mut an,
        "parse_digit",
        &Default::default(),
        /* wasm */ true,
    );
    // Full unroll leaves the residual header unreachable.
    pliron_clif_opt::jumpthread::remove_unreachable_blocks(f);
    // Fold the leading `0*10+d → d` (LLVM's first digit is just the load):
    // `cparm` pushes iconst entry args through block params, then peep
    // kills `imul 0, 10`.
    let _ = pliron_clif_opt::clifpeep::cparm(f);
    pliron_clif_opt::clifpeep::run(f, /* x64 */ false);
    pliron_clif_opt::jumpthread::remove_unreachable_blocks(f);
}

/// After peel of a clean ≤8 digit loop: LLVM-like shared unroll.
///
/// Target (wasm parse_only::run): 7 i32.mul, 0 residual digit loop,
/// early `len == k` (or remaining-count) exits.
#[test]
fn peel_clean_digit_loop_matches_llvm_shared_unroll() {
    let mut f = parse(digit_loop_src());
    assert_eq!(imuls(&f), 1, "input must be one mul per iter:\n{}", f.display());
    rotate_then_peel(&mut f);
    verify_function(&f, &flags()).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));

    let n = imuls(&f);
    let loops = back_edges(&f);
    let exits = len_exit_icmps(&f);
    let s = f.display().to_string();
    if std::env::var_os("PEEL_DUMP").is_some() {
        eprintln!("PEEL_DUMP imuls={n} loops={loops} len_exits={exits}\n{s}");
    }

    // LLVM has 7 i32.mul (first digit is just the load). After folding
    // `0*10`, a full peel of ≤8 leaves 7..=8.
    assert!(
        (7..=8).contains(&n),
        "want 7..=8 imuls (LLVM shared unroll), got {n}; loops={loops}\n{s}"
    );
    assert_eq!(
        loops, 0,
        "want the digit loop fully peeled (0 loops left), got {loops}\n{s}"
    );
    // One early-exit test per peeled digit (LLVM's `len == k`).
    assert!(
        exits >= 6,
        "want ≥6 early len exits interleaved with the unroll, got {exits}\n{s}"
    );
}

/// Length dispatch: LLVM opens with `br_table` on the `&str` length.
/// A complete match also puts a table (or a small compare tree that
/// lowers to one) in front of the shared unroll. Soft for now: when
/// present, it must not explode imul count.
///
/// Real `k_parse_only` wasm: after peel the shared Horner is ~10–15
/// imuls; late `egraph2` remats it into every `len==k` arm → 45
/// static muls (skipping egraph2 is slower under V8). This unit pins
/// the peel-only shared-unroll budget LLVM's shape needs.
#[test]
fn llvm_shape_budget_imul_not_per_length() {
    let mut f = parse(digit_loop_src());
    rotate_then_peel(&mut f);
    let n = imuls(&f);
    // Absolute ceiling: 8 lengths × 8 digits = 64; we must stay near one
    // unroll. 16 is already twice LLVM — fail well before 45.
    assert!(
        n <= 16,
        "imul budget for one shared unroll blown: {n}\n{}",
        f.display()
    );
    let _ = br_tables(&f); // observed; assertion lands once we emit one
}

/// Diamond whose true arm does `imul 10`: ifconv must keep the branch
/// (LLVM keeps digit muls behind control flow). Speculating it clones
/// the mul onto every path — the parse_only 7→45 blow-up.
#[test]
fn ifconv_refuses_speculating_digit_imul() {
    let src = r#"
function u0:0(i32, i8) -> i32 fast {
block0(v0: i32, v1: i8):
    brif v1, block1, block2

block1:
    v2 = iconst.i32 10
    v3 = imul v0, v2
    jump block3(v3)

block2:
    jump block3(v0)

block3(v4: i32):
    return v4
}
"#;
    let mut f = parse(src);
    let before = imuls(&f);
    assert_eq!(before, 1);
    pliron_clif_opt::ifconv::run(&mut f, &mut pliron_clif_opt::vmap::VMap::default(), None, true);
    verify_function(&f, &flags()).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    // Still exactly one imul, and the header must remain a brif (not a
    // select-merged straight line that always executes the mul).
    assert_eq!(imuls(&f), 1, "speculated mul?\n{}", f.display());
    let entry = f.layout.entry_block().unwrap();
    let term = f.layout.last_inst(entry).unwrap();
    assert!(
        matches!(f.dfg.insts[term], cranelift_codegen::ir::InstructionData::Brif { .. }),
        "digit-mul diamond was if-converted:\n{}",
        f.display()
    );
}

/// Same ≤8 digit loop, with enough pure ops that the ordinary peel cap
/// (12 non-terminator insts → 4 copies) leaves a residual loop.
/// `PLIRON_PEEL_DIGIT=1` peels the proved trip instead.
fn fat_digit_loop_src() -> String {
    let mut s = digit_loop_src().to_string();
    let extra = "\
    v40 = iadd v16, v16
    v41 = iadd v40, v16
    v42 = iadd v41, v16
    v43 = iadd v42, v16
";
    s = s.replacen(
        "    v19 = iconst.i32 10\n",
        &format!("{extra}    v19 = iconst.i32 10\n"),
        1,
    );
    s
}

#[test]
fn peel_digit_knob_unrolls_a_fat_bounded_loop() {
    let src = fat_digit_loop_src();
    unsafe { std::env::remove_var("PLIRON_PEEL_DIGIT") };
    let mut off = parse(&src);
    rotate_then_peel(&mut off);
    let n_off = imuls(&off);
    assert!(
        n_off <= 5,
        "ordinary cap should stop near 4 copies, got {n_off}\n{}",
        off.display()
    );

    unsafe { std::env::set_var("PLIRON_PEEL_DIGIT", "1") };
    let mut on = parse(&src);
    rotate_then_peel(&mut on);
    unsafe { std::env::remove_var("PLIRON_PEEL_DIGIT") };
    verify_function(&on, &flags()).unwrap_or_else(|e| panic!("{e}\n{}", on.display()));
    let n = imuls(&on);
    assert!(
        (7..=9).contains(&n),
        "PEEL_DIGIT should peel 8 copies, got {n} imuls\n{}",
        on.display()
    );
}
