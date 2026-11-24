//! aarch64 `PLIRON_EXTCLEAN=1`: `ldrb`/`ldrh` (and `umov` lane extracts) zero the register
//! above the loaded width, so `ireduce` of a `uload8`/`uload16` needs no
//! `uxtb`/`uxth`. pulldown-cmark's byte dispatch (`ireduce.i8 (uload8.i64
//! p)` feeding a `match`) took one per special byte. Run checks that the
//! narrow value still behaves: every byte value through a `br_table`-style
//! dispatch and an arithmetic use.

#![cfg(all(target_arch = "aarch64", target_os = "macos"))]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};
use std::ffi::c_void;

fn compile(clif: &str) -> (Vec<u8>, String) {
    unsafe { std::env::set_var("PLIRON_EXTCLEAN", "1") };
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

const SRC: &str = "function %f(i64) -> i32 apple_aarch64 {
block0(v0: i64):
    v1 = uload8.i64 notrap v0
    v2 = ireduce.i8 v1
    v3 = iconst.i8 10
    v4 = isub v2, v3
    v5 = uextend.i32 v4
    br_table v5, block9, [block1, block2, block3]
block1:
    v10 = iconst.i32 101
    return v10
block2:
    v11 = iconst.i32 102
    return v11
block3:
    v12 = iconst.i32 103
    return v12
block9:
    v13 = uextend.i32 v2
    v14 = iconst.i32 7
    v15 = imul v13, v14
    return v15
}";

#[test]
fn byte_dispatch_has_no_uxtb() {
    let (_, asm) = compile(SRC);
    assert!(!asm.lines().any(|l| l.split_whitespace().next() == Some("uxtb")), "{asm}");
}

#[test]
fn byte_dispatch_runs() {
    let (code, asm) = compile(SRC);
    let j = Jit::new(&code);
    let f: unsafe extern "C" fn(*const u8) -> i32 = unsafe { std::mem::transmute(j.0) };
    for b in 0..=255u8 {
        // Junk after the byte: only the loaded width may matter.
        let mem = [b, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99];
        let want = match b.wrapping_sub(10) {
            0 => 101,
            1 => 102,
            2 => 103,
            _ => b as i32 * 7,
        };
        assert_eq!(unsafe { f(mem.as_ptr()) }, want, "byte {b}\n{asm}");
    }
}

/// A narrow lane extract is `umov`, which zero-extends: comparing it
/// against a byte needs no `uxtb`.
#[test]
fn lane_extract_has_no_uxtb() {
    let (code, asm) = compile(
        "function %g(i64) -> i32 apple_aarch64 {
block0(v0: i64):
    v1 = load.i8x16 notrap v0
    v2 = extractlane v1, 5
    v3 = iconst.i8 60
    v4 = icmp ult v2, v3
    v5 = uextend.i32 v4
    v6 = uextend.i32 v2
    v7 = iadd v5, v6
    return v7
}",
    );
    assert!(!asm.lines().any(|l| l.split_whitespace().next() == Some("uxtb")), "{asm}");
    let j = Jit::new(&code);
    let f: unsafe extern "C" fn(*const u8) -> i32 = unsafe { std::mem::transmute(j.0) };
    for b in 0..=255u8 {
        let mut mem = [0xa5u8; 16];
        mem[5] = b;
        assert_eq!(unsafe { f(mem.as_ptr()) }, (b < 60) as i32 + b as i32, "byte {b}\n{asm}");
    }
}
