//! Large memory offsets on aarch64 (default, `PLIRON_BIGOFF=0` disables; patch 101): an
//! access whose offset the encoder cannot take as an immediate (beyond
//! `ldur`'s -256..255 and the scaled 12-bit form) is split by lcse into
//! `q = base + hi; [q + lo]`, with `hi` a multiple of 4096 shared per
//! `(base, hi)`, and the lowering no longer folds such an `iadd` back
//! into the offset. miniz_oxide's `compress_inner` read three `Box`
//! fields at `d + 0x10018..0x10028` per byte with `mov`+`movk` of the
//! offset at each access; LLVM hoists `d + 0x10000` once.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::clifpeep;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("invalid IR: {e}\n{}", f.display());
    }
}

#[test]
fn lcse_splits_unencodable_offsets_onto_one_shared_base() {
    clifpeep::set_bigoff(true);
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = load.i64 notrap v0+0x0001_0018
    v2 = load.i64 notrap v0+0x0001_0028
    v3 = load.i64 notrap v0+0x0001_0050
    v4 = load.i64 notrap v0+24
    v5 = iadd v1, v2
    v6 = iadd v5, v3
    v7 = iadd v6, v4
    return v7
}";
    let mut f = parse(src);
    assert!(clifpeep::lcse(&mut f) > 0);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        !text.lines().any(|l| l.contains("load") && l.contains("+0x")),
        "offsets still unencodable:\n{text}"
    );
    let iadds = text.lines().filter(|l| l.contains(" = iadd ") && l.contains("v0")).count();
    assert_eq!(iadds, 1, "one shared `v0 + 0x10000` base:\n{text}");
    assert!(text.contains("+24") && text.contains("+40") && text.contains("+80"), "{text}");
    assert_eq!(text.matches("v0+24").count(), 1, "the small offset stays on v0:\n{text}");
}

#[test]
fn lcse_does_not_fold_an_iadd_into_an_unencodable_offset() {
    clifpeep::set_bigoff(true);
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 0x1_0000
    v2 = iadd v0, v1
    v3 = load.i64 notrap v2+24
    v4 = load.i64 notrap v2+40
    v5 = iadd v3, v4
    return v5
}";
    let mut f = parse(src);
    clifpeep::lcse(&mut f);
    verify(&f);
    let text = f.display().to_string();
    assert!(text.contains("v2+24") && text.contains("v2+40"), "folded back onto v0:\n{text}");
}

/// The constant may sit in the base instead of the offset (licm hoisted
/// it; memoff refused the fold): the same split, off the `iadd`'s base.
#[test]
fn lcse_splits_an_iadd_base_with_an_unencodable_constant() {
    clifpeep::set_bigoff(true);
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 0x1_0018
    v2 = iadd v0, v1
    v3 = load.i64 notrap v2
    v4 = iconst.i64 0x1_0028
    v5 = iadd v0, v4
    v6 = load.i64 notrap v5
    v7 = iadd v3, v6
    return v7
}";
    let mut f = parse(src);
    assert!(clifpeep::lcse(&mut f) > 0);
    verify(&f);
    let text = f.display().to_string();
    assert!(text.contains("+24") && text.contains("+40"), "{text}");
    assert_eq!(
        text.lines().filter(|l| l.contains("= iconst.i64 0x0001_0000")).count(),
        1,
        "one shared hi base:\n{text}"
    );
}

#[test]
fn amode_off_fits_matches_the_encoder() {
    use cranelift_codegen::ir::types;
    assert!(clifpeep::amode_off_fits(types::I64, 0));
    assert!(clifpeep::amode_off_fits(types::I64, -256));
    assert!(clifpeep::amode_off_fits(types::I64, 255));
    assert!(clifpeep::amode_off_fits(types::I64, 32760));
    assert!(!clifpeep::amode_off_fits(types::I64, 32768));
    assert!(!clifpeep::amode_off_fits(types::I64, 260)); // not a multiple of 8, past simm9
    assert!(clifpeep::amode_off_fits(types::I8, 4095));
    assert!(!clifpeep::amode_off_fits(types::I8, 4096));
    assert!(!clifpeep::amode_off_fits(types::I64, -257));
}

#[cfg(target_arch = "aarch64")]
mod codegen {
    use cranelift_codegen::Context;
    use cranelift_codegen::control::ControlPlane;
    use cranelift_codegen::ir::Function;
    use cranelift_codegen::settings::{self, Configurable};

    fn asm(clif: &str) -> String {
        unsafe { std::env::set_var("PLIRON_BIGOFF", "1") };
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

    /// The split shape, base hoisted: the loop's three loads address off
    /// the hoisted `q` with immediates and materialize nothing.
    const LOOP: &str = "function %f(i64, i64) -> i64 apple_aarch64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0x1_0000
    v3 = iadd v0, v2
    v4 = iconst.i64 0
    v5 = iconst.i64 -1
    jump block1(v1, v4)
block1(v6: i64, v7: i64):
    v8 = load.i64 notrap v3+24
    v9 = load.i64 notrap v3+32
    v10 = load.i64 notrap v3+40
    v11 = iadd v7, v8
    v12 = iadd v11, v9
    v13 = iadd v12, v10
    v14 = iadd v6, v5
    brif v14, block1(v14, v13), block2
block2:
    return v13
}";

    #[test]
    fn split_base_stays_a_register_in_the_loop() {
        let a = asm(LOOP);
        assert!(!a.lines().any(|l| l.trim_start().starts_with("movk ")), "offset materialized:\n{a}");
        let loads: Vec<&str> = a
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("ldr ") || l.starts_with("ldp ") || l.starts_with("ldur "))
            .collect();
        assert!(
            !loads.is_empty() && loads.iter().all(|l| l.contains(", #") && l.ends_with(']')),
            "loads address the hoisted base with immediates:\n{a}"
        );
        assert_eq!(
            a.lines().filter(|l| l.trim_start().starts_with("add ") && l.contains("#65536")).count(),
            1,
            "one hoisted `base + 0x10000`:\n{a}"
        );
    }
}
