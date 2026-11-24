//! aarch64 `PLIRON_CMN_NARROW=1`: an 8/16-bit signed `icmp` against a
//! constant compares the sign-extended operand with the sign-extended
//! constant as an immediate (`cmp w, #k` / `cmn w, #-k`), and a single-use
//! narrow load becomes `ldrsb`/`ldrsh`. Every operand value of `i8` (and a
//! sweep of `i16`) is compiled, run and checked against Rust's comparison,
//! for each signed condition and a set of constants that cover `0`, small
//! positives, small negatives, the extremes and values past the imm12 range.
//! UTF-8's `is_char_boundary` (`(b as i8) >= -0x40`) is the motivating case.

#![cfg(all(target_arch = "aarch64", target_os = "macos"))]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};
use std::ffi::c_void;

fn compile(clif: &str) -> (Vec<u8>, String) {
    unsafe { std::env::set_var("PLIRON_CMN_NARROW", "1") };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    let cc = ctx.compiled_code().unwrap();
    (cc.code_buffer().to_vec(), cc.vcode.clone().unwrap())
}

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

struct Jit(*mut c_void, usize);
impl Jit {
    fn new(code: &[u8]) -> Jit {
        const PROT_RWX: i32 = 1 | 2 | 4;
        const MAP_PRIVATE_ANON_JIT: i32 = 0x0002 | 0x1000 | 0x0800;
        let len = code.len().next_multiple_of(16384);
        unsafe {
            let p = mmap(std::ptr::null_mut(), len, PROT_RWX, MAP_PRIVATE_ANON_JIT, -1, 0);
            assert!(!p.is_null() && p as isize != -1, "mmap MAP_JIT failed");
            pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(code.as_ptr(), p as *mut u8, code.len());
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(p, code.len());
            Jit(p, len)
        }
    }
}
impl Drop for Jit {
    fn drop(&mut self) {
        unsafe {
            munmap(self.0, self.1);
        }
    }
}

const CONDS: [(&str, fn(i64, i64) -> bool); 4] = [
    ("slt", |a, b| a < b),
    ("sle", |a, b| a <= b),
    ("sgt", |a, b| a > b),
    ("sge", |a, b| a >= b),
];

/// `icmp cc (load.ty p), k` with the byte at `p`, and the same on a
/// register operand: both shapes the rules take.
fn check(ty: &str, bits: u32, k: i64, values: &mut dyn Iterator<Item = i64>) {
    let values: Vec<i64> = values.collect();
    for (cc, f) in CONDS {
        let (lcode, lasm) = compile(&format!(
            "function %f(i64) -> i8 apple_aarch64 {{
block0(v0: i64):
    v1 = load.{ty} notrap aligned v0
    v2 = iconst.{ty} {k}
    v3 = icmp {cc} v1, v2
    return v3
}}"
        ));
        let (rcode, _) = compile(&format!(
            "function %g(i64) -> i8 apple_aarch64 {{
block0(v0: i64):
    v1 = ireduce.{ty} v0
    v2 = iconst.{ty} {k}
    v3 = icmp {cc} v1, v2
    return v3
}}"
        ));
        let (lj, rj) = (Jit::new(&lcode), Jit::new(&rcode));
        let lf: unsafe extern "C" fn(*const u8) -> u8 = unsafe { std::mem::transmute(lj.0) };
        let rf: unsafe extern "C" fn(u64) -> u8 = unsafe { std::mem::transmute(rj.0) };
        let kk = ((k << (64 - bits)) as i64) >> (64 - bits);
        for &x in &values {
            let want = f(x, kk) as u8;
            let mem = (x as u64).to_le_bytes();
            // Junk above the narrow value in the register: the lowering
            // must extend, not trust the upper bits.
            let reg = (x as u64 & ((1u64 << bits) - 1)) | 0xdead_beef_0000_0000 | (0x5a5a_u64 << bits);
            assert_eq!(unsafe { lf(mem.as_ptr()) } & 1, want, "load {ty} {x} {cc} {k}\n{lasm}");
            assert_eq!(unsafe { rf(reg) } & 1, want, "reg {ty} {x} {cc} {k}");
        }
    }
}

#[test]
fn i8_all_values() {
    for k in [0, 1, 5, 100, 127, -1, -2, -64, -65, -100, -128] {
        check("i8", 8, k, &mut (-128i64..=127));
    }
}

#[test]
fn i16_sweep() {
    let vals = || (-32768i64..=32767).step_by(97).chain([-32768, -32767, -4096, -4095, -1, 0, 1, 4095, 4096, 32766, 32767]);
    for k in [0, 1, 4095, 4096, 32767, -1, -4095, -4096, -4097, -32768] {
        check("i16", 16, k, &mut vals());
    }
}

/// The motivating shape: one `ldrsb` and an immediate compare, no
/// materialized constant.
#[test]
fn char_boundary_shape() {
    let (_, asm) = compile(
        "function %f(i64) -> i8 apple_aarch64 {
block0(v0: i64):
    v1 = load.i8 notrap aligned v0
    v2 = iconst.i8 -64
    v3 = icmp sge v1, v2
    return v3
}",
    );
    assert!(asm.contains("ldrsb"), "{asm}");
    assert!(asm.lines().any(|l| l.contains("cmn") && l.contains("#64")), "{asm}");
    assert!(!asm.contains("sxtb"), "{asm}");
}
