//! Post-sweep / additive-spine peepholes from the wasm kernel work:
//!
//! - `icmpk`: `icmp {eq,ne} (±terms + K), C` → `icmp {eq,ne} Σ+, Σ−`
//!   (`x+k != C` → `x != C-k`, `a-b != 0` → `a != b`, and the
//!   `len - (-ptr) - d - 1 != 0` parse chains → `a + K != b`).
//! - `addnorm`: `(x ± k1) ± k2` → `x ± (k1±k2)`, `a ± (x·-1)` → `a ∓ x`.
//! - `ext_load`: `uextend.i32 (load.i8)`/`band.i8`-hoisted loads fuse
//!   to `uload8.i32` — one wasm `i32.load8_u`, not two ops.
//! - `bitmerge` 2-link: `(x & m1) & m2` → `x & (m1&m2)`.
//! - `iconst128`'s `post_sweep`: dead pure insts, `x<<0`-class
//!   identities, covered `band` masks (`uextend.i32 x8 & 255`).

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Function, InstructionData, Opcode, Value, types};
use pliron_clif_opt::clifpeep;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// `run` + the pipeline's last step (`iconst128`, which carries
/// `post_sweep`'s DCE/identity cleanup) — mirrors the real pipeline so
/// dead leftovers don't count.
fn pipeline(f: &mut Function) -> usize {
    clifpeep::run(f, false) + clifpeep::iconst128(f)
}

fn ops(f: &Function) -> Vec<Opcode> {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .map(|i| f.dfg.insts[i].opcode())
        .collect()
}

fn count(f: &Function, op: Opcode) -> usize {
    ops(f).iter().filter(|&&o| o == op).count()
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("invalid IR: {e}\n{}", f.display());
    }
}

fn equiv(before: &Function, after: &Function) {
    if let Err(m) = check(before, after, &Facts::default(), &Config::default()) {
        panic!("mismatch: {m}\n---- after\n{}", after.display());
    }
}

/// `umul_overflow x, K` → `imul x, K` + `icmp ugt x, MAX/K`: the
/// emitter expands `umulhi` for the flag (a widened multiply + shift
/// on wasm); the dividend-side compare is two ops.
#[test]
fn ovfk_const_rhs() {
    let src = "function %f(i32) -> i32, i8 {
block0(v0: i32):
    v1 = iconst.i32 10
    v2, v3 = umul_overflow.i32 v0, v1
    return v2, v3
}";
    let before = parse(src);
    let mut f = before.clone();
    let _ = pipeline(&mut f);
    assert_eq!(count(&f, Opcode::UmulOverflow), 0, "{}", f.display());
    assert_eq!(count(&f, Opcode::Umulhi), 0, "{}", f.display());
    assert_eq!(count(&f, Opcode::Imul), 1, "{}", f.display());
    assert_eq!(count(&f, Opcode::Icmp), 1, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// Same at i64 — the `umulhi.i64` the emitter would synthesize is ~20
/// wasm ops; `icmp ugt x, 2^64-1/K` is two.
#[test]
fn ovfk_i64() {
    let src = "function %f(i64) -> i64, i8 {
block0(v0: i64):
    v1 = iconst.i64 0x7fff_ffff
    v2, v3 = umul_overflow.i64 v0, v1
    return v2, v3
}";
    let before = parse(src);
    let mut f = before.clone();
    let _ = pipeline(&mut f);
    assert_eq!(count(&f, Opcode::UmulOverflow), 0, "{}", f.display());
    assert_eq!(count(&f, Opcode::Umulhi), 0, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `band.i32 (uextend.i32 x8), K` → `uextend.i32 (band.i8 x, K)` — the
/// extension's zeroed high half is masked regardless, so mask at the
/// source width and widen after (`&255 &1` on call results).
#[test]
fn band_uextend_narrows() {
    let src = "function %f(i8) -> i32 {
block0(v0: i8):
    v1 = uextend.i32 v0
    v2 = iconst.i32 1
    v3 = band.i32 v1, v2
    return v3
}";
    let before = parse(src);
    let mut f = before.clone();
    let _ = pipeline(&mut f);
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if let InstructionData::Binary {
                opcode: Opcode::Band,
                ..
            } = f.dfg.insts[i]
            {
                assert_eq!(f.dfg.value_type(f.dfg.first_result(i)), types::I8);
            }
        }
    }
    verify(&f);
    equiv(&before, &f);
}

/// `icmpk`'s `a + k != b` collapse re-materializes `base + len - 1`
/// inside the residual loop — `PLIRON_LIH` pulls the invariant spine
/// back to the entry edge.
#[test]
fn lih_invariant_sum() {
    let src = "function %f(i32, i32) -> i32 {
block0(v0: i32, v1: i32):
    v2 = iconst.i32 0
    jump block1(v0, v2)
block1(v3: i32, v4: i32):
    v5 = iadd.i32 v0, v1
    v6 = iconst.i32 -1
    v7 = iadd.i32 v5, v6
    v8 = icmp ne v3, v7
    brif v8, block2, block3
block2:
    v9 = iconst.i32 1
    v10 = iadd.i32 v3, v9
    v11 = iadd.i32 v4, v9
    jump block1(v10, v11)
block3:
    return v4
}";
    let before = parse(src);
    let mut f = before.clone();
    let _ = pipeline(&mut f);
    // `base + len` (and the `-1` tail) must now live outside the loop.
    let (v0, v1) = (Value::new(0), Value::new(1));
    let entry = f.layout.entry_block().unwrap();
    let mut found = false;
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if let InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            } = f.dfg.insts[i]
            {
                let (a0, a1) = (
                    f.dfg.resolve_aliases(args[0]),
                    f.dfg.resolve_aliases(args[1]),
                );
                if (a0 == v0 && a1 == v1) || (a0 == v1 && a1 == v0) {
                    found = true;
                    assert_eq!(
                        f.layout.inst_block(i),
                        Some(entry),
                        "invariant iadd stayed in-loop:\n{}",
                        f.display()
                    );
                }
            }
        }
    }
    assert!(found, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `icmp ne ((len - (-ptr)) - d - 1), 0` — the parse/decode loop bound
/// — becomes `icmp ne (len + ptr - 1), d`: the mul-negations and the
/// additive chain collapse to a two-sided compare.
#[test]
fn icmpk_linear_chain() {
    let src = "function %f(i32, i32, i32) -> i8 {
block0(v0: i32, v1: i32, v2: i32):
    v3 = iconst.i32 -1
    v4 = imul.i32 v1, v3
    v5 = isub.i32 v0, v4
    v6 = imul.i32 v2, v3
    v7 = iadd.i32 v5, v6
    v8 = iadd.i32 v7, v3
    v9 = iconst.i32 0
    v10 = icmp ne v8, v9
    return v10
}";
    let before = parse(src);
    let mut f = before.clone();
    let n = pipeline(&mut f);
    assert!(n > 0, "no folds: {}", f.display());
    assert_eq!(count(&f, Opcode::Imul), 0, "{}", f.display());
    assert_eq!(count(&f, Opcode::Isub), 0, "{}", f.display());
    let icmps: Vec<_> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i], InstructionData::IntCompare { .. }))
        .collect();
    assert_eq!(icmps.len(), 1, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `x + 8 == 0` → `x == -8`.
#[test]
fn icmpk_add_const() {
    let src = "function %f(i32) -> i8 {
block0(v0: i32):
    v1 = iconst.i32 8
    v2 = iadd.i32 v0, v1
    v3 = iconst.i32 0
    v4 = icmp eq v2, v3
    return v4
}";
    let before = parse(src);
    let mut f = before.clone();
    pipeline(&mut f);
    let i = f
        .layout
        .block_insts(f.layout.blocks().next().unwrap())
        .find(|&i| matches!(f.dfg.insts[i], InstructionData::IntCompare { .. }))
        .unwrap();
    let InstructionData::IntCompare { cond, args, .. } = f.dfg.insts[i] else {
        unreachable!()
    };
    assert_eq!(cond, IntCC::Equal);
    // `v0 == -8` — the compare reads the param and a folded const.
    let k = f
        .dfg
        .resolved_value_def(args[1])
        .inst()
        .and_then(|d| match f.dfg.insts[d] {
            InstructionData::UnaryImm { imm, .. } => Some(imm.bits()),
            _ => None,
        });
    assert_eq!(k.map(|k| k as u32 as i32), Some(-8), "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `a - b != 0` → `a != b`.
#[test]
fn icmpk_sub_zero() {
    let src = "function %f(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = isub.i64 v0, v1
    v3 = iconst.i64 0
    v4 = icmp ne v2, v3
    return v4
}";
    let before = parse(src);
    let mut f = before.clone();
    pipeline(&mut f);
    assert_eq!(count(&f, Opcode::Isub), 0, "{}", f.display());
    let icmp = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| matches!(f.dfg.insts[i], InstructionData::IntCompare { .. }));
    let InstructionData::IntCompare { args, .. } = f.dfg.insts[icmp.unwrap()] else {
        unreachable!()
    };
    let r = |v: cranelift_codegen::ir::Value| f.dfg.resolve_aliases(v);
    let (p0, p1) = (
        f.dfg.block_params(f.layout.blocks().next().unwrap())[0],
        f.dfg.block_params(f.layout.blocks().next().unwrap())[1],
    );
    assert!((r(args[0]), r(args[1])) == (p0, p1) || (r(args[0]), r(args[1])) == (p1, p0));
    verify(&f);
    equiv(&before, &f);
}

/// `a - (x·-1)` → `a + x` and `(x+1)+1` → `x+2`.
#[test]
fn addnorm_neg_and_reassoc() {
    let src = "function %f(i32, i32) -> i32 {
block0(v0: i32, v1: i32):
    v2 = iconst.i32 -1
    v3 = imul.i32 v1, v2
    v4 = isub.i32 v0, v3
    v5 = iconst.i32 1
    v6 = iadd.i32 v4, v5
    v7 = iadd.i32 v6, v5
    return v7
}";
    let before = parse(src);
    let mut f = before.clone();
    pipeline(&mut f);
    // `v0 - (-v1)` → `v0 + v1`, then `+1 +1` → `+2`: `iadd; iadd`.
    assert_eq!(count(&f, Opcode::Imul), 0, "{}", f.display());
    assert_eq!(count(&f, Opcode::Isub), 0, "{}", f.display());
    assert_eq!(count(&f, Opcode::Iadd), 2, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `uextend.i32 (band.i8 (load.i8 p), 63)` — the UTF-8 continuation
/// mask — fuses the load to `uload8.i32` instead of widening in two ops.
#[test]
fn widen_fuses_load() {
    let src = "function %f(i32) -> i32 {
block0(v0: i32):
    v1 = load.i8 notrap v0
    v2 = iconst.i8 63
    v3 = band.i8 v1, v2
    v4 = uextend.i32 v3
    return v4
}";
    let before = parse(src);
    let mut f = before.clone();
    pipeline(&mut f);
    assert!(
        ops(&f).contains(&Opcode::Uload8),
        "no fused load: {}",
        f.display()
    );
    // The surviving band reads the i32 widening load.
    let band = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| {
            matches!(f.dfg.insts[i], InstructionData::Binary { opcode: Opcode::Band, .. })
                && f.dfg.value_type(f.dfg.first_result(i)) == types::I32
        });
    let InstructionData::Binary { args, .. } = f.dfg.insts[band.unwrap()] else {
        unreachable!()
    };
    let ld = f.dfg.resolved_value_def(args[0]).inst().unwrap();
    assert_eq!(f.dfg.insts[ld].opcode(), Opcode::Uload8, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `(x & 31) & 7` → `x & 7` — the 4-byte decode head's stacked masks.
#[test]
fn bitmerge_two_link() {
    let src = "function %f(i32) -> i32 {
block0(v0: i32):
    v1 = iconst.i32 31
    v2 = band.i32 v0, v1
    v3 = iconst.i32 7
    v4 = band.i32 v2, v3
    return v4
}";
    let before = parse(src);
    let mut f = before.clone();
    pipeline(&mut f);
    assert_eq!(count(&f, Opcode::Band), 1, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `post_sweep` reaps what late passes strand: dead icmps, `x<<0`,
/// `x+0`, and `uextend.i32 x8 & 255` (cover-all-bits mask).
#[test]
fn post_sweep_reaps_residue() {
    let src = "function %f(i32) -> i32 {
block0(v0: i32):
    v1 = iconst.i32 0
    v2 = ishl.i32 v0, v1
    v3 = icmp eq v2, v1
    v4 = iconst.i32 255
    v5 = ireduce.i8 v0
    v6 = uextend.i32 v5
    v7 = band.i32 v6, v4
    v8 = iadd.i32 v7, v1
    return v8
}";
    let before = parse(src);
    let mut f = before.clone();
    let n = clifpeep::iconst128(&mut f);
    assert!(n > 0, "nothing swept: {}", f.display());
    assert_eq!(count(&f, Opcode::Ishl), 0, "{}", f.display());
    assert_eq!(count(&f, Opcode::Band), 0, "{}", f.display());
    assert_eq!(
        f.layout
            .blocks()
            .flat_map(|b| f.layout.block_insts(b))
            .filter(|&i| matches!(f.dfg.insts[i], InstructionData::IntCompare { .. }))
            .count(),
        0,
        "dead icmp survived: {}",
        f.display()
    );
    verify(&f);
    equiv(&before, &f);
}

/// `icmp uge.i8 (ireduce.i8 (uload8.i32 p)), -16` →
/// `icmp uge.i32 (uload8.i32 p), 240` — the continuation-byte head
/// checks compare on the wide load, dropping the `&255` norms and the
/// `ireduce`.
#[test]
fn icmpred_widens_ireduce_cmp() {
    let src = "function %f(i32) -> i8 {
block0(v0: i32):
    v1 = uload8.i32 notrap v0
    v2 = ireduce.i8 v1
    v3 = iconst.i8 -16
    v4 = icmp uge v2, v3
    return v4
}";
    let before = parse(src);
    let mut f = before.clone();
    pipeline(&mut f);
    let i = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| matches!(f.dfg.insts[i], InstructionData::IntCompare { .. }));
    let i = i.unwrap();
    let InstructionData::IntCompare { args, .. } = f.dfg.insts[i] else {
        unreachable!()
    };
    assert_eq!(f.dfg.value_type(args[0]), types::I32, "{}", f.display());
    assert_eq!(count(&f, Opcode::Ireduce), 0, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `brif (select c, 0, 1)` ≡ `brif !c` — the `Option`-is-some test —
/// swaps the targets so the `select` folds away.
#[test]
fn brif_select_01_swaps() {
    let before = parse("function %f(i32) -> i32 {
block0(v0: i32):
    v1 = iconst.i32 0
    v2 = icmp eq v0, v1
    v5 = iconst.i32 1
    v3 = select v2, v1, v5
    brif v3, block1, block2
block1:
    v6 = iconst.i32 7
    return v6
block2:
    v7 = iconst.i32 9
    return v7
}");
    let mut f = before.clone();
    let n = clifpeep::iconst128(&mut f);
    assert!(n > 0, "nothing swept: {}", f.display());
    // The brif now tests the icmp directly with swapped targets.
    let brif = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| matches!(f.dfg.insts[i], InstructionData::Brif { .. }));
    let InstructionData::Brif { arg, blocks, .. } = f.dfg.insts[brif.unwrap()] else {
        unreachable!()
    };
    let c = f.dfg.resolved_value_def(arg).inst().unwrap();
    assert!(matches!(f.dfg.insts[c], InstructionData::IntCompare { .. }));
    // `select p==0,0,1` != 0 iff p != 0 → the `9` arm moved first.
    let t0 = blocks[0].block(&f.dfg.value_lists);
    let ret0 = f.layout.block_insts(t0).next_back().unwrap();
    assert_eq!(f.dfg.insts[ret0].opcode(), Opcode::Return);
    let blocks_ = f.layout.block_insts(t0).collect::<Vec<_>>();
    let k = blocks_
        .iter()
        .filter_map(|&i| match f.dfg.insts[i] {
            InstructionData::UnaryImm { imm, .. } => Some(imm.bits()),
            _ => None,
        })
        .next();
    assert_eq!(k, Some(9), "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}

/// `icmp ne (iadd x,-1), 3` → `icmp ne x, 4` — the parse loop's
/// unrolled digit-count checks.
#[test]
fn icmpk_add_const_rhs() {
    let src = "function %f(i32) -> i8 {
block0(v0: i32):
    v1 = iconst.i32 -1
    v2 = iadd.i32 v0, v1
    v3 = iconst.i32 3
    v4 = icmp ne v2, v3
    return v4
}";
    let before = parse(src);
    let mut f = before.clone();
    pipeline(&mut f);
    assert_eq!(count(&f, Opcode::Iadd), 0, "{}", f.display());
    verify(&f);
    equiv(&before, &f);
}
