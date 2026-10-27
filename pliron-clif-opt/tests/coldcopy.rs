//! `PLIRON_COLDCOPY`: `coldargs(f, true)` rebinds each hot int arg of a
//! cold call as `iadd x, (umulhi 0, 1)`, which the vendored aarch64 backend
//! lowers to a register copy (dropped when regalloc reuses the register).
//! These tests compile small CLIF functions for aarch64, check that the
//! code has no `umulh` (and that the default form still has one), then run
//! the code in a MAP_JIT page. The cold call goes to a Rust recorder,
//! which must see the same argument values for both forms, for every int
//! width, boundary values, repeated args, a folded zero arg and stack args.

#![cfg(all(target_arch = "aarch64", target_os = "macos"))]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use pliron_clif_opt::clifpeep;
use std::cell::RefCell;
use std::ffi::c_void;

fn isa() -> std::sync::Arc<dyn TargetIsa> {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(b);
    cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .expect("aarch64 ISA")
        .finish(flags)
        .unwrap()
}

fn parse(clif: &str) -> Function {
    cranelift_reader::parse_functions(clif).unwrap().into_iter().next().unwrap()
}

/// Run `coldargs` (copy form or default) and compile. Returns the code and
/// how many args were rebound.
fn build(clif: &str, copy: bool) -> (Vec<u8>, usize) {
    let mut f = parse(clif);
    let n = clifpeep::coldargs(&mut f, copy);
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder()))
        .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    let isa = isa();
    let mut ctx = Context::for_function(f);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    let cc = ctx.compiled_code().unwrap();
    assert!(cc.buffer.relocs().is_empty(), "test code must be self-contained");
    (cc.code_buffer().to_vec(), n)
}

fn words(code: &[u8]) -> impl Iterator<Item = u32> + '_ {
    code.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap()))
}

/// `umulh xd, xn, xm`.
fn umulh_count(code: &[u8]) -> usize {
    words(code).filter(|w| w & 0xffe0_fc00 == 0x9bc0_7c00).count()
}

/// `orr xd, xzr, xd` (64-bit self-move), which emission must drop.
fn self_moves(code: &[u8]) -> usize {
    words(code)
        .filter(|w| w & 0xffe0_ffe0 == 0xaa00_03e0 && (w >> 16) & 31 == w & 31)
        .count()
}

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

/// Executable copy of `code`; unmapped on drop.
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

thread_local! {
    static SEEN: RefCell<Vec<Vec<u64>>> = const { RefCell::new(Vec::new()) };
}

#[allow(clippy::too_many_arguments)]
extern "C" fn rec(a: u64, b: u32, c: u16, d: u8, e: u64, f: u64, g: u64, h: u64, i: u64, j: u32, k: i8, l: i16) {
    SEEN.with(|s| {
        s.borrow_mut().push(vec![a, b as u64, c as u64, d as u64, e, f, g, h, i, j as u64, k as u64, l as u64])
    });
}

/// `v1..v5` are hot params of every width, `v6`/`v7` hot results. The cold
/// block passes them (some twice), a cold-block zero, a hot zero (`v8`,
/// whose rebind a later fold could turn into the bare `umulhi 0, 1`) and
/// narrow signed args; 12 args, so 4 go on the stack.
const CLIF: &str = "
function %f(i64, i64, i32, i16, i8, i64, i8) -> i64 apple_aarch64 {
    sig0 = (i64, i32 uext, i16 uext, i8 uext, i64, i64, i64, i64, i64, i32 uext, i8 sext, i16 sext) apple_aarch64
block0(v0: i64, v1: i64, v2: i32, v3: i16, v4: i8, v5: i64, v9: i8):
    v6 = iadd v1, v5
    v7 = imul v1, v5
    v8 = iconst.i64 0
    v20 = ireduce.i16 v7
    brif v9, block1, block2

block2:
    v10 = iadd v6, v7
    v11 = iadd v10, v8
    return v11

block1 cold:
    v12 = iconst.i64 7
    call_indirect sig0, v0(v1, v2, v3, v4, v6, v7, v8, v1, v12, v2, v4, v20)
    v13 = iadd v6, v12
    return v13
}
";

type F = unsafe extern "C" fn(u64, u64, u32, u16, u8, u64, u8) -> u64;

#[test]
fn copy_form_has_no_umulh() {
    let (on, n_on) = build(CLIF, true);
    let (off, n_off) = build(CLIF, false);
    assert!(n_on >= 8 && n_on == n_off, "rebinds: copy {n_on}, default {n_off}");
    assert_eq!(umulh_count(&on), 0, "copy form left a umulh");
    assert!(umulh_count(&off) >= 1, "default form should still use umulh (test is vacuous)");
    assert_eq!(self_moves(&on), 0, "a 64-bit self-move reached the code");
    assert!(on.len() < off.len(), "copy form not smaller: {} vs {}", on.len(), off.len());
}

#[test]
fn bare_zero_is_movz() {
    // The rebind folded away: the `umulhi 0, 1` value is used directly.
    let clif = "
function %z(i64) -> i64 apple_aarch64 {
block0(v0: i64):
    v1 = iconst.i64 0
    v2 = iconst.i64 1
    v3 = umulhi v1, v2
    v4 = bxor v0, v3
    return v4
}
";
    let isa = isa();
    let mut ctx = Context::for_function(parse(clif));
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    let code = ctx.compiled_code().unwrap().code_buffer().to_vec();
    assert_eq!(umulh_count(&code), 0);
    let j = Jit::new(&code);
    let f: unsafe extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(j.0) };
    for x in [0u64, 1, u64::MAX, 0x8000_0000_0000_0000, 0xdead_beef] {
        assert_eq!(unsafe { f(x) }, x);
    }
}

fn run(code: &[u8], args: (u64, u32, u16, u8, u64, u8)) -> (u64, Vec<Vec<u64>>) {
    SEEN.with(|s| s.borrow_mut().clear());
    let j = Jit::new(code);
    let f: F = unsafe { std::mem::transmute(j.0) };
    let r = unsafe { f(rec as usize as u64, args.0, args.1, args.2, args.3, args.4, args.5) };
    (r, SEEN.with(|s| s.borrow().clone()))
}

#[test]
fn copy_form_runs_like_default() {
    let (on, _) = build(CLIF, true);
    let (off, _) = build(CLIF, false);
    let v64 = [0u64, 1, 2, 7, u64::MAX, u64::MAX - 1, 1 << 63, (1 << 63) - 1, 0x1234_5678_9abc_def0];
    let v32 = [0u32, 1, u32::MAX, 1 << 31, 0x7fff_ffff, 0xdead_beef];
    let v16 = [0u16, 1, u16::MAX, 0x8000, 0x7fff];
    let v8 = [0u8, 1, u8::MAX, 0x80, 0x7f];
    let mut runs = 0;
    for (i, &a) in v64.iter().enumerate() {
        for (k, &e) in v64.iter().enumerate() {
            let b = v32[(i + k) % v32.len()];
            let c = v16[(i * 3 + k) % v16.len()];
            let d = v8[(i + 2 * k) % v8.len()];
            for cold in [0u8, 1] {
                let args = (a, b, c, d, e, cold);
                let got = run(&on, args);
                let want = run(&off, args);
                assert_eq!(got, want, "args {args:?}");
                if cold == 1 {
                    // The recorder sees exactly what the CLIF passes.
                    let s = &got.1[0];
                    let (h6, h7) = (a.wrapping_add(e), a.wrapping_mul(e));
                    assert_eq!(
                        s,
                        &vec![a, b as u64, c as u64, d as u64, h6, h7, 0, a, 7, b as u64, d as i8 as u64, h7 as i16 as u64],
                        "args {args:?}"
                    );
                    assert_eq!(got.0, h6.wrapping_add(7));
                } else {
                    assert!(got.1.is_empty());
                    assert_eq!(got.0, a.wrapping_add(e).wrapping_add(a.wrapping_mul(e)));
                }
                runs += 1;
            }
        }
    }
    assert_eq!(runs, 2 * v64.len() * v64.len());
}
