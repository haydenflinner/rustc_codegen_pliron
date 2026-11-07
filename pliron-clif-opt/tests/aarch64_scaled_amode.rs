//! aarch64 scaled index amodes after egraph strength reduction
//! (patch 105): `iadd y, y` and `imul y, C` produce the same scaled
//! index `ishl y, log2 C` does, so `base + index*sizeof` still lowers
//! to a single `lsl #N` register amode instead of a standalone `add`
//! plus reg+reg. Surfaced on miniz_oxide's `compress_inner`, whose
//! halfword-table indexing lost the `uxtw`/`lsl #1` folded form.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    b.set("is_pic", "true").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

/// `t[i]` where `t` is a `u16` table and the `*2` arrives as `iadd i, i`.
const DOUBLED: &str = "function %f(i64, i32) -> i32 {
block0(v0: i64, v1: i32):
    v2 = uextend.i64 v1
    v3 = iadd v2, v2
    v4 = iadd v0, v3
    v5 = load.i16 notrap v4
    v6 = uextend.i32 v5
    return v6
}";

/// Same, with the multiply left as `imul i, 2`.
const MUL2: &str = "function %f(i64, i32) -> i32 {
block0(v0: i64, v1: i32):
    v2 = uextend.i64 v1
    v7 = iconst.i64 2
    v3 = imul v2, v7
    v4 = iadd v0, v3
    v5 = load.i16 notrap v4
    v6 = uextend.i32 v5
    return v6
}";

#[test]
fn doubled_index_folds_into_scaled_amode() {
    for (name, src) in [("iadd", DOUBLED), ("imul_imm", MUL2)] {
        let a = asm(src);
        assert!(
            a.lines().any(|l| {
                l.contains("ldrh") && (l.contains("lsl #1") || l.to_lowercase().contains("uxtw #1"))
            }),
            "{name}: want `ldrh` with a scaled index amode:\n{a}"
        );
        assert!(
            !a.lines().any(|l| {
                let l = l.trim_start();
                l.starts_with("add ") && !l.contains("#0x")
            }),
            "{name}: standalone doubling `add` left:\n{a}"
        );
    }
}

/// `nodes[i'].f = v` four times over: `iadd(imul(i, 20), base)` with four
/// amode uses. The share gate's fanout cap (3) used to decompose this,
/// paying a `mul` + reg+reg amode per store; an `iadd` over an `imul` is
/// always worth sharing — `madd` once is never worse than `mul` per use.
/// (`BrotliCreateHqZopfliBackwardReferences` node walk.)
const NODE_STORES: &str = "function %f(i64, i64, i32) {
block0(v0: i64, v1: i64, v2: i32):
    v10 = iconst.i64 20
    v3 = imul v1, v10
    v4 = iadd v0, v3
    store.i32 notrap v2, v4
    store.i32 notrap v2, v4+4
    store.i32 notrap v2, v4+8
    store.i32 notrap v2, v4+12
    return
}";

#[test]
fn imul_address_is_shared_across_stores() {
    let a = asm(NODE_STORES);
    let madds = a
        .lines()
        .filter(|l| l.trim_start().starts_with("madd "))
        .count();
    let muls = a
        .lines()
        .filter(|l| {
            let l = l.trim_start();
            l.starts_with("mul ") || l.starts_with("umulh ")
        })
        .count();
    assert_eq!(
        madds, 1,
        "want one `madd` materializing the shared address:\n{a}"
    );
    assert_eq!(muls, 0, "no standalone `mul` per store:\n{a}");
    assert_eq!(
        a.lines()
            .filter(|l| {
                let l = l.trim_start();
                l.starts_with("str w") || l.starts_with("stur w")
            })
            .count(),
        4,
        "all four stores present:\n{a}"
    );
}
