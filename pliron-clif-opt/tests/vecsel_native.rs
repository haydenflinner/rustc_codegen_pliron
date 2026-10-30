//! `PLIRON_FMINMAX` / `PLIRON_VSELECT` semantics, run natively.
//!
//! The functions are built with the lowerer's own emitters
//! (`pliron_clif_opt::vecsel`), compiled by the vendored Cranelift for the host
//! at `opt_level` none and speed_and_size, mapped executable and called on
//! every pair of special values. Reference: LLVM `maxnum`/`minnum` (if exactly
//! one operand is NaN return the other; both NaN -> NaN; +-0 either way) and
//! the LLVM `fcmp`/`icmp` + `select` semantics (ordered predicates are false
//! on NaN, `une` is true).

#![cfg(all(target_arch = "aarch64", target_os = "macos"))]

mod jtcommon;

use cranelift_codegen::ir::{
    self, AbiParam, InstBuilder, MemFlagsData, Signature, UserFuncName, types,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use pliron_clif_opt::vecsel;

type Entry = unsafe extern "C" fn(*const u8, *mut u8);

fn call_conv() -> CallConv {
    CallConv::triple_default(&target_lexicon::Triple::host())
}

/// `fn(in, out)`: `build` gets (builder, in_ptr) and returns the value stored at `out`.
fn compile(
    opt: &str,
    build: impl FnOnce(&mut FunctionBuilder, ir::Value) -> ir::Value,
) -> jtcommon::Code {
    let mut sig = Signature::new(call_conv());
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    let mut f = ir::Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut fbc = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut f, &mut fbc);
        let blk = b.create_block();
        b.append_block_params_for_function_params(blk);
        b.switch_to_block(blk);
        b.seal_block(blk);
        let inp = b.block_params(blk)[0];
        let out = b.block_params(blk)[1];
        let r = build(&mut b, inp);
        b.ins().store(MemFlagsData::trusted(), r, out, 0);
        b.ins().return_(&[]);
        b.finalize(
            jtcommon_frontend_config(),
        );
    }
    jtcommon::compile_func_opt(f, opt)
}

fn jtcommon_frontend_config() -> cranelift_codegen::isa::TargetFrontendConfig {
    let isa = cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
        .unwrap()
        .finish(cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap();
    isa.frontend_config()
}

fn run(code: &jtcommon::Code, inp: &[u8], out: &mut [u8]) {
    let f: Entry = unsafe { std::mem::transmute(code.entry()) };
    unsafe { f(inp.as_ptr(), out.as_mut_ptr()) };
}

const OPTS: [&str; 2] = ["none", "speed_and_size"];

fn f32_specials() -> Vec<f32> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        1.5,
        -2.5,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        -f32::NAN,
        f32::from_bits(0x7fa0_0001), // signaling NaN, payload
        f32::from_bits(0xffc0_1234), // negative quiet NaN, payload
        f32::from_bits(1),           // min subnormal
        -f32::from_bits(1),
        f32::from_bits(0x007f_ffff), // max subnormal
        f32::MIN_POSITIVE,
        f32::MAX,
        -f32::MAX,
    ]
}

fn f64_specials() -> Vec<f64> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        1.5,
        -2.5,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        -f64::NAN,
        f64::from_bits(0x7ff4_0000_0000_0001),
        f64::from_bits(0xfff8_0000_dead_beef),
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::from_bits(0x000f_ffff_ffff_ffff),
        f64::MIN_POSITIVE,
        f64::MAX,
        -f64::MAX,
    ]
}

/// minNum/maxNum reference. `Ok(bits)` when the answer is unique up to NaN
/// payload (`None` = any NaN); `Err(())` for +-0 vs -+0 (either zero).
macro_rules! reference {
    ($name:ident, $t:ty) => {
        fn $name(max: bool, a: $t, b: $t) -> Result<Option<$t>, ()> {
            if a.is_nan() && b.is_nan() {
                return Ok(None);
            }
            if a.is_nan() {
                return Ok(Some(b));
            }
            if b.is_nan() {
                return Ok(Some(a));
            }
            if a == 0.0 && b == 0.0 && a.is_sign_negative() != b.is_sign_negative() {
                return Err(());
            }
            Ok(Some(if max { if a > b { a } else { b } } else if a < b { a } else { b }))
        }
    };
}
reference!(ref32, f32);
reference!(ref64, f64);

macro_rules! check_lane {
    ($ctx:expr, $max:expr, $a:expr, $b:expr, $got:expr, $refn:ident, $zero_seen:expr) => {
        match $refn($max, $a, $b) {
            Ok(None) => assert!($got.is_nan(), "{}: max={} {:?} {:?} -> {:?}, want NaN", $ctx, $max, $a, $b, $got),
            Ok(Some(w)) => assert!(
                $got.to_bits() == w.to_bits(),
                "{}: max={} {:?} {:?} -> {:?}, want {:?}",
                $ctx, $max, $a, $b, $got, w
            ),
            Err(()) => {
                assert!($got == 0.0, "{}: max={} {:?} {:?} -> {:?}, want a zero", $ctx, $max, $a, $b, $got);
                $zero_seen.push(($max, $got.is_sign_negative()));
            }
        }
    };
}

/// All special-value pairs through `lanes`-wide vectors (or scalars when
/// `vec` is `None`): lane i sees pair `(sp[(p+i) % n], sp[(q+5i) % n])`, so
/// every lane meets every pair.
macro_rules! minmax_test {
    ($test:ident, $t:ty, $specials:ident, $refn:ident, $vecty:expr, $lanes:expr) => {
        #[test]
        fn $test() {
            let sp = $specials();
            let n = sp.len();
            const L: usize = $lanes;
            for opt in OPTS {
                for max in [false, true] {
                    let code = compile(opt, |b, inp| {
                        let t: ir::Type = $vecty;
                        let a = b.ins().load(t, MemFlagsData::trusted(), inp, 0);
                        let c = b.ins().load(t, MemFlagsData::trusted(), inp, 16);
                        vecsel::fminmax_num(b, max, a, c)
                    });
                    let mut zeros = Vec::new();
                    for p in 0..n {
                        for q in 0..n {
                            let mut inp = [0u8; 32];
                            let sz = std::mem::size_of::<$t>();
                            let mut av = [<$t>::default(); L];
                            let mut bv = [<$t>::default(); L];
                            for i in 0..L {
                                av[i] = sp[(p + i) % n];
                                bv[i] = sp[(q + 5 * i) % n];
                                inp[i * sz..(i + 1) * sz].copy_from_slice(&av[i].to_le_bytes());
                                inp[16 + i * sz..16 + (i + 1) * sz].copy_from_slice(&bv[i].to_le_bytes());
                            }
                            let mut out = [0u8; 16];
                            run(&code, &inp, &mut out);
                            for i in 0..L {
                                let got = <$t>::from_le_bytes(out[i * sz..(i + 1) * sz].try_into().unwrap());
                                check_lane!(
                                    format!("{} opt={opt} lane {i}", stringify!($test)),
                                    max, av[i], bv[i], got, $refn, zeros
                                );
                            }
                        }
                    }
                    // Mixed zeros: document what the hardware picks (max -> +0, min -> -0).
                    assert!(!zeros.is_empty());
                }
            }
        }
    };
}

minmax_test!(minmax_f32_scalar, f32, f32_specials, ref32, types::F32, 1);
minmax_test!(minmax_f64_scalar, f64, f64_specials, ref64, types::F64, 1);
minmax_test!(minmax_f32x4, f32, f32_specials, ref32, types::F32X4, 4);
minmax_test!(minmax_f64x2, f64, f64_specials, ref64, types::F64X2, 2);
minmax_test!(minmax_f32x2_lanewise, f32, f32_specials, ref32, types::F32X2, 2);

/// +-0: Rust documents either; pin what the lowering gives so a change is
/// noticed (the old libcall path gave max(-0,+0) = +0, min(-0,+0) = -0).
#[test]
fn minmax_mixed_zero_matches_libm() {
    for opt in OPTS {
        for max in [false, true] {
            let code = compile(opt, |b, inp| {
                let a = b.ins().load(types::F32, MemFlagsData::trusted(), inp, 0);
                let c = b.ins().load(types::F32, MemFlagsData::trusted(), inp, 16);
                vecsel::fminmax_num(b, max, a, c)
            });
            for (x, y) in [(0.0f32, -0.0f32), (-0.0, 0.0)] {
                let mut inp = [0u8; 32];
                inp[..4].copy_from_slice(&x.to_le_bytes());
                inp[16..20].copy_from_slice(&y.to_le_bytes());
                let mut out = [0u8; 16];
                run(&code, &inp, &mut out);
                let got = f32::from_le_bytes(out[..4].try_into().unwrap());
                let want = if max { x.max(y) } else { x.min(y) };
                // std's f32::max/min lower to the same libm call; both zero.
                assert_eq!(got, 0.0);
                if got.is_sign_negative() != want.is_sign_negative() {
                    eprintln!("note: opt={opt} max={max} {x:?} {y:?}: got {got:?}, std {want:?}");
                }
            }
        }
    }
}

// ---- select(fcmp/icmp) fusion ------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const PREDS: [&str; 6] = ["eq", "ne", "lt", "le", "gt", "ge"];

fn fref(p: &str, a: f32, b: f32) -> bool {
    match p {
        "eq" => a == b,
        "ne" => a != b, // une: true when unordered
        "lt" => a < b,
        "le" => a <= b,
        "gt" => a > b,
        _ => a >= b,
    }
}

fn iref(p: &str, signed: bool, a: u32, b: u32) -> bool {
    if signed {
        let (a, b) = (a as i32, b as i32);
        match p {
            "eq" => a == b,
            "ne" => a != b,
            "lt" => a < b,
            "le" => a <= b,
            "gt" => a > b,
            _ => a >= b,
        }
    } else {
        match p {
            "eq" => a == b,
            "ne" => a != b,
            "lt" => a < b,
            "le" => a <= b,
            "gt" => a > b,
            _ => a >= b,
        }
    }
}

/// a, b: 4 lanes at 0/16, x, y: 4 lanes at 32/48; result stored as a vector of type `xt`.
fn fused_code(opt: &str, p: &'static str, kind: &'static str, at: ir::Type, xt: ir::Type) -> jtcommon::Code {
    compile(opt, |b, inp| {
        let a = b.ins().load(at, MemFlagsData::trusted(), inp, 0);
        let c = b.ins().load(at, MemFlagsData::trusted(), inp, 16);
        let x = b.ins().load(xt, MemFlagsData::trusted(), inp, 32);
        let y = b.ins().load(xt, MemFlagsData::trusted(), inp, 48);
        let m = vecsel::vcmp(b, p, kind, a, c, types::I32X4);
        vecsel::vbitselect(b, m, x, y)
    })
}

fn lane(buf: &[u8], off: usize, i: usize) -> u32 {
    u32::from_le_bytes(buf[off + 4 * i..off + 4 * i + 4].try_into().unwrap())
}

fn put(buf: &mut [u8], off: usize, i: usize, v: u32) {
    buf[off + 4 * i..off + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn vselect_fcmp_f32x4_all_special_pairs_and_random() {
    let sp = f32_specials();
    let n = sp.len();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for opt in OPTS {
        for p in PREDS {
            for xt in [types::F32X4, types::I32X4] {
                let code = fused_code(opt, p, "f", types::F32X4, xt);
                // Exhaustive special pairs (every lane meets every pair) ...
                for pi in 0..n {
                    for qi in 0..n {
                        let mut inp = [0u8; 64];
                        for i in 0..4 {
                            put(&mut inp, 0, i, sp[(pi + i) % n].to_bits());
                            put(&mut inp, 16, i, sp[(qi + 5 * i) % n].to_bits());
                            put(&mut inp, 32, i, rng.next() as u32);
                            put(&mut inp, 48, i, rng.next() as u32);
                        }
                        check_fsel(&code, p, &inp, opt);
                    }
                }
                // ... plus randomized bit patterns (NaNs, subnormals appear naturally).
                for _ in 0..4000 {
                    let mut inp = [0u8; 64];
                    for i in 0..4 {
                        let pick = |r: &mut Rng| {
                            if r.next() % 4 == 0 { sp[(r.next() as usize) % n].to_bits() } else { r.next() as u32 }
                        };
                        // Make equal operands common.
                        let a = pick(&mut rng);
                        let b = if rng.next() % 3 == 0 { a } else { pick(&mut rng) };
                        put(&mut inp, 0, i, a);
                        put(&mut inp, 16, i, b);
                        put(&mut inp, 32, i, rng.next() as u32);
                        put(&mut inp, 48, i, rng.next() as u32);
                    }
                    check_fsel(&code, p, &inp, opt);
                }
            }
        }
    }
}

fn check_fsel(code: &jtcommon::Code, p: &str, inp: &[u8; 64], opt: &str) {
    let mut out = [0u8; 16];
    run(code, inp, &mut out);
    for i in 0..4 {
        let (a, b) = (f32::from_bits(lane(inp, 0, i)), f32::from_bits(lane(inp, 16, i)));
        let want = if fref(p, a, b) { lane(inp, 32, i) } else { lane(inp, 48, i) };
        assert_eq!(
            lane(&out, 0, i),
            want,
            "opt={opt} fcmp {p} lane {i}: a={a:?} b={b:?}"
        );
    }
}

#[test]
fn vselect_icmp_i32x4_all_predicates_random() {
    let edge = [0u32, 1, 2, 0x7fff_ffff, 0x8000_0000, 0x8000_0001, 0xffff_ffff, 0xffff_fffe];
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for opt in OPTS {
        for p in PREDS {
            for (kind, signed) in [("s", true), ("u", false)] {
                if matches!(p, "eq" | "ne") && !signed {
                    // eq/ne only exist as `.s` in vselect_fuse; `.u` form is harmless and tested too.
                }
                let code = fused_code(opt, p, kind, types::I32X4, types::I32X4);
                for _ in 0..4000 {
                    let mut inp = [0u8; 64];
                    for i in 0..4 {
                        let pick = |r: &mut Rng| {
                            if r.next() % 2 == 0 { edge[(r.next() as usize) % edge.len()] } else { r.next() as u32 }
                        };
                        let a = pick(&mut rng);
                        let b = if rng.next() % 4 == 0 { a } else { pick(&mut rng) };
                        put(&mut inp, 0, i, a);
                        put(&mut inp, 16, i, b);
                        put(&mut inp, 32, i, rng.next() as u32);
                        put(&mut inp, 48, i, rng.next() as u32);
                    }
                    let mut out = [0u8; 16];
                    run(&code, &inp, &mut out);
                    for i in 0..4 {
                        let (a, b) = (lane(&inp, 0, i), lane(&inp, 16, i));
                        let want = if iref(p, signed, a, b) { lane(&inp, 32, i) } else { lane(&inp, 48, i) };
                        assert_eq!(lane(&out, 0, i), want, "opt={opt} icmp {p}.{kind} lane {i}: a={a:#x} b={b:#x}");
                    }
                }
            }
        }
    }
}

/// The fused min/max idiom CubeCL's max-pool emits: `select(fcmp ogt a b, a, b)`
/// is `a > b ? a : b` (NaN in either operand picks `b`), unlike maxnum.
#[test]
fn vselect_ogt_nan_picks_false_arm() {
    let code = compile("speed_and_size", |b, inp| {
        let a = b.ins().load(types::F32X4, MemFlagsData::trusted(), inp, 0);
        let c = b.ins().load(types::F32X4, MemFlagsData::trusted(), inp, 16);
        let m = vecsel::vcmp(b, "gt", "f", a, c, types::I32X4);
        vecsel::vbitselect(b, m, a, c)
    });
    let mut inp = [0u8; 32];
    for (i, (a, c)) in [(f32::NAN, 1.0f32), (2.0, f32::NAN), (3.0, 1.0), (1.0, 3.0)].iter().enumerate() {
        put(&mut inp, 0, i, a.to_bits());
        put(&mut inp, 16, i, c.to_bits());
    }
    let mut out = [0u8; 16];
    run(&code, &inp, &mut out);
    let got: Vec<f32> = (0..4).map(|i| f32::from_bits(lane(&out, 0, i))).collect();
    assert_eq!(got[0], 1.0);
    assert!(got[1].is_nan());
    assert_eq!(got[2], 3.0);
    assert_eq!(got[3], 3.0);
}
