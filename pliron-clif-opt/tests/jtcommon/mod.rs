//! Shared by the `jt_*` tests: compile CLIF text for the host (aarch64 or
//! x86-64), map it executable, and decode deferred jump-table dispatches
//! (`PLIRON_JT_NARROW`). On an Apple-silicon Mac the x86-64 leg runs under
//! Rosetta: `cargo test --target x86_64-apple-darwin -p pliron-clif-opt
//! --test jt_narrow`.
#![allow(dead_code)]

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, isa};

/// Table slots past the `n` arms: x86-64 keeps the default as a last
/// entry (lowering clamps the index to it); aarch64 branches around.
pub const DEFAULT_SLOTS: usize = if cfg!(target_arch = "x86_64") { 1 } else { 0 };
/// Entry unit in bytes (instruction alignment).
pub const SCALE: u32 = if cfg!(target_arch = "x86_64") { 1 } else { 4 };

pub fn narrow_on() {
    // SAFETY: every test in the binary sets the same value before compiling.
    unsafe { std::env::set_var("PLIRON_JT_NARROW", "1") };
}

/// Executable copy of one function's machine code.
pub struct Code {
    ptr: *mut u8,
    len: usize,
    pub bytes: Vec<u8>,
}

impl Code {
    pub fn entry(&self) -> *const u8 {
        self.ptr
    }
    pub fn word(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.bytes[off..off + 4].try_into().unwrap())
    }
}

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
unsafe extern "C" {
    fn pthread_jit_write_protect_np(enabled: libc::c_int);
    fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
}

fn map_exec(bytes: &[u8]) -> Code {
    let len = bytes.len().next_multiple_of(1 << 14).max(1 << 14);
    unsafe {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let ptr = {
            let p = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            );
            assert_ne!(p, libc::MAP_FAILED);
            pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast(), bytes.len());
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(p, len);
            p
        };
        // A fresh never-executed mapping: no stale icache lines to flush
        // (x86 keeps them coherent; Linux/aarch64 maps fresh pages clean).
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        let ptr = {
            let p = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            assert_ne!(p, libc::MAP_FAILED);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast(), bytes.len());
            assert_eq!(libc::mprotect(p, len, libc::PROT_READ | libc::PROT_EXEC), 0);
            p
        };
        Code {
            ptr: ptr.cast(),
            len,
            bytes: bytes.to_vec(),
        }
    }
}

pub fn parse(src: &str) -> cranelift_codegen::ir::Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

pub fn compile(src: &str) -> Code {
    compile_func(parse(src))
}

pub fn compile_func(func: cranelift_codegen::ir::Function) -> Code {
    let mut fb = settings::builder();
    fb.set("opt_level", "none").unwrap();
    let isa = isa::lookup(target_lexicon::Triple::host())
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap();
    let mut ctx = Context::for_function(func);
    let code = ctx
        .compile(&*isa, &mut ControlPlane::default())
        .map_err(|e| format!("{e:?}"))
        .unwrap();
    assert!(code.buffer.relocs().is_empty(), "test functions must not relocate");
    map_exec(code.code_buffer())
}

/// A decoded deferred dispatch: table address, entry load, base address,
/// add, indirect branch.
#[derive(Debug)]
pub struct Dispatch {
    /// Offset of the dispatch sequence.
    pub dispatch: usize,
    pub width: usize,
    pub table: usize,
    pub base: usize,
    pub entries: Vec<u32>,
}

impl Dispatch {
    pub fn max_entry(&self) -> u32 {
        self.entries.iter().copied().max().unwrap()
    }
    pub fn targets(&self) -> Vec<usize> {
        self.entries.iter().map(|&e| self.base + (SCALE * e) as usize).collect()
    }
}

fn read_entries(code: &Code, table: usize, width: usize, n: usize) -> Vec<u32> {
    (0..n)
        .map(|k| {
            let o = table + k * width;
            match width {
                1 => code.bytes[o] as u32,
                2 => u16::from_le_bytes([code.bytes[o], code.bytes[o + 1]]) as u32,
                _ => code.word(o),
            }
        })
        .collect()
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use super::*;

    fn adr_target(w: u32, off: usize) -> Option<usize> {
        if w & 0x9f00_0000 != 0x1000_0000 {
            return None;
        }
        let imm = (((w >> 5) & 0x7ffff) << 2) | ((w >> 29) & 3);
        let imm = ((imm << 11) as i32) >> 11;
        Some((off as i64 + imm as i64) as usize)
    }

    fn is_br(w: u32) -> bool {
        w & 0xffff_fc1f == 0xd61f_0000
    }

    /// `cmp wN, #n; b.hs; csel; adr; ldr{b,h,}; adr; add; br`; the table
    /// length is the `cmp` immediate.
    pub fn decode_all(code: &Code) -> Vec<Dispatch> {
        let mut out = Vec::new();
        for i in 7..code.bytes.len() / 4 {
            let at = |k: usize| code.word(4 * (i - k));
            if !is_br(at(0)) {
                continue;
            }
            let (Some(base), Some(table)) =
                (adr_target(at(2), 4 * (i - 2)), adr_target(at(4), 4 * (i - 4)))
            else {
                continue;
            };
            let width = match at(3) >> 24 {
                0x38 => 1,
                0x78 => 2,
                0xb8 => 4,
                _ => continue,
            };
            let cmp = at(7);
            assert_eq!(cmp & 0xff80_001f, 0x7100_001f, "expected cmp w, #imm");
            let n = ((cmp >> 10) & 0xfff) as usize;
            let entries = read_entries(code, table, width, n);
            out.push(Dispatch {
                dispatch: 4 * (i - 4),
                width,
                table,
                base,
                entries,
            });
        }
        out
    }

    /// Cranelift's stock inline form: `adr; ldrsw [.., uxtw #2]; add; br`.
    pub fn has_inline_dispatch(code: &Code) -> bool {
        (3..code.bytes.len() / 4).any(|i| {
            is_br(code.word(4 * i))
                && code.word(4 * (i - 2)) >> 22 == 0b1011100010
                && adr_target(code.word(4 * (i - 3)), 4 * (i - 3)).is_some()
        })
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::*;

    /// `lea r, [rip + disp32]` at `i`: its end and target.
    fn lea_rip(b: &[u8], i: usize) -> Option<(usize, usize)> {
        if i + 7 > b.len() || b[i] & 0xf8 != 0x48 || b[i + 1] != 0x8d || b[i + 2] & 0xc7 != 0x05 {
            return None;
        }
        let d = i32::from_le_bytes(b[i + 3..i + 7].try_into().unwrap());
        Some((i + 7, (i as i64 + 7 + d as i64) as usize))
    }

    /// The entry load after the first `lea`: (width, length).
    fn load_at(b: &[u8], i: usize) -> Option<(usize, usize)> {
        let rex = (b[i] & 0xf0 == 0x40) as usize;
        let j = i + rex;
        let (w, op) = match b[j..j + 2] {
            [0x0f, 0xb6] => (1, 2),
            [0x0f, 0xb7] => (2, 2),
            [0x8b, _] => (4, 1),
            _ => return None,
        };
        // modrm (mod 00, rm 100 = SIB) + SIB.
        (b[j + op] & 0xc7 == 0x04).then_some((w, rex + op + 2))
    }

    /// `lea; mov{zbl,zwl,l} (NOP-padded); lea; add; jmp *`; a table runs
    /// to the next table or the end of the code.
    pub fn decode_all(code: &Code) -> Vec<Dispatch> {
        let b = &code.bytes;
        let mut found = Vec::new();
        for i in 0..b.len().saturating_sub(16) {
            let Some((e1, table)) = lea_rip(b, i) else {
                continue;
            };
            let Some((width, len)) = load_at(b, e1) else {
                continue;
            };
            let mut k = e1 + len;
            while b[k] == 0x90 {
                k += 1;
            }
            let Some((_, base)) = lea_rip(b, k) else {
                continue;
            };
            found.push((i, width, table, base));
        }
        let mut starts: Vec<usize> = found.iter().map(|f| f.2).collect();
        starts.push(b.len());
        starts.sort_unstable();
        found
            .into_iter()
            .map(|(dispatch, width, table, base)| {
                let end = *starts.iter().find(|&&s| s > table).unwrap();
                let n = (end - table) / width;
                Dispatch {
                    dispatch,
                    width,
                    table,
                    base,
                    entries: read_entries(code, table, width, n),
                }
            })
            .collect()
    }

    /// Cranelift's stock inline form: `lea r, [rip+d]; movslq (r, i, 4)`.
    pub fn has_inline_dispatch(code: &Code) -> bool {
        let b = &code.bytes;
        (0..b.len().saturating_sub(16)).any(|i| {
            lea_rip(b, i).is_some_and(|(e, _)| b[e] & 0xf8 == 0x48 && b[e + 1] == 0x63)
        })
    }
}

pub use arch::{decode_all, has_inline_dispatch};

pub fn decode_dispatch(code: &Code) -> Option<Dispatch> {
    decode_all(code).into_iter().next()
}
