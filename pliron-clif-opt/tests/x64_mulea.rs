//! `PLIRON_X64_IMULEA` (default-on; `=0` keeps `imul`):
//! `imul x, d·2^k` (d ∈ {3,5,9},
//! k ∈ 1..=3) splits into `imul x, d` + `ishl …, k` in `post_sweep`
//! (`mulea_sweep`) — the `ishl` folds into a consuming `iadd`'s x64
//! amode as `base + idx<<k`, while `imul x, d` itself lowers to one
//! `lea (x, x·(d-1))`. `acc·10 + digit` → `lea` + `lea`, LLVM's shape.
//! Only when every use of the product is an `iadd`; otherwise the
//! `ishl` would materialize and the split is a wash.
//!
//! All tests run with the gate on: `pliron_cfg::config()` parses env
//! once per process, so an off-case could not coexist.

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::settings::{self, Configurable};

fn op_count(f: &Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

fn sweep(clif: &str) -> Function {
    unsafe { std::env::set_var("PLIRON_X64_IMULEA", "1") };
    let mut f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    // `iconst128` drives `post_sweep` where `mulea_sweep` lives.
    pliron_clif_opt::clifpeep::iconst128(&mut f, true, true);
    f
}

// Every use is an `iadd` — the `·2` folds into the add's amode.
#[test]
fn split_when_all_iadd_uses() {
    let f = sweep(
        "function %f(i32, i32) -> i32 {
block0(v0: i32, v1: i32):
    v2 = iconst.i32 10
    v3 = imul v0, v2
    v4 = iadd v3, v1
    return v4
}",
    );
    let ishl = op_count(&f, Opcode::Ishl);
    assert_eq!(ishl, 1, "expected one ishl from the split:\n{}", f.display());
    // `imul x, 5` remains — the odd factor.
    let imuls: Vec<_> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Imul)
        .collect();
    assert_eq!(imuls.len(), 1, "expected one residual imul:\n{}", f.display());
    let args = match f.dfg.insts[imuls[0]] {
        cranelift_codegen::ir::InstructionData::Binary { args, .. } => args,
        _ => panic!("imul shape"),
    };
    let k = args
        .iter()
        .find_map(|&a| f.dfg.value_def(a).inst().and_then(|d| {
            match f.dfg.insts[d] {
                cranelift_codegen::ir::InstructionData::UnaryImm { imm, .. } => {
                    Some(imm.bits())
                }
                _ => None,
            }
        }))
        .expect("imul by a constant");
    assert_eq!(k, 5, "residual multiplier is the odd factor:\n{}", f.display());
}

// A non-`iadd` use (`band` for a boolean-mask consumer) keeps `imul`.
#[test]
fn no_split_with_other_uses() {
    let f = sweep(
        "function %f(i32, i32) -> i32 {
block0(v0: i32, v1: i32):
    v2 = iconst.i32 10
    v3 = imul v0, v2
    v4 = iadd v3, v1
    v5 = band v3, v1
    v6 = iadd v4, v5
    return v6
}",
    );
    assert_eq!(op_count(&f, Opcode::Ishl), 0, "{}", f.display());
    assert_eq!(op_count(&f, Opcode::Imul), 1, "{}", f.display());
}

// `d` alone (k = 0) has no `·2^k` tail to split — plain `imul x, 5`
// stays for the x64 lea rule.
#[test]
fn no_split_odd_factor_only() {
    let f = sweep(
        "function %f(i32, i32) -> i32 {
block0(v0: i32, v1: i32):
    v2 = iconst.i32 5
    v3 = imul v0, v2
    v4 = iadd v3, v1
    return v4
}",
    );
    assert_eq!(op_count(&f, Opcode::Ishl), 0, "{}", f.display());
    assert_eq!(op_count(&f, Opcode::Imul), 1, "{}", f.display());
}

fn asm(clif: &str) -> String {
    unsafe { std::env::set_var("PLIRON_X64_IMULEA", "1") };
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

// `x·5` lowers to `lea (x, x, 4)`, not `imull $5`.
#[test]
fn x64_odd_factor_lea() {
    let a = asm(
        "function %f(i32) -> i32 {
block0(v0: i32):
    v2 = iconst.i32 5
    v3 = imul v0, v2
    return v3
}",
    );
    assert!(a.contains("leal"), "expected lea lowering:\n{a}");
    assert!(!a.contains("imull"), "imul should be a lea:\n{a}");
}
