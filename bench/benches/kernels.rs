use std::hint::black_box;

use benchfmt::{CountingAlloc, Suite};

mod arrayvec;
mod chars_alpha;
mod chars_ascii;
mod chars_mixed;
mod contains_rng;
mod eqbug;
mod fib;
mod hashmap;
mod hmget;
mod iterw;
mod matmul;
mod nbody;
mod parse_only;
mod sieve;
mod sortw;
mod strwork;
mod tokenize;
mod varint;

// CountingAlloc wraps the System allocator so `-benchmem` (or
// `b.report_allocs()`) reports B/op and allocs/op per benchmark epoch.
// It lives in this crate, so it is compiled by the backend under test —
// same few instructions on both sides of a bench/run.sh comparison.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc::new();

// Crash probe (windows): vectored handler prints faulting RIP before dying.
#[cfg(windows)]
fn install_crash_probe() {
    #[repr(C)]
    struct ExnRec {
        code: u32,
        flags: u32,
        _next: usize,
        addr: usize,
        nparams: usize,
        _pad: u32,
    }
    #[repr(C)]
    struct ExnPtrs {
        rec: *mut ExnRec,
        ctx: *const u8,
    }
    unsafe extern "system" fn h(p: *mut ExnPtrs) -> i32 {
        unsafe {
            let rec = (*p).rec;
            let ctx = (*p).ctx;
            let rip = *(ctx.add(0xF8) as *const usize);
            // ExceptionInformation[] at ExceptionRecord+0x20: [0]=access kind,
            // [1]=accessed address for AV.
            let info = (rec as *const usize).add(0x20 / 8);
            let (akind, aaddr) = (*info, *info.add(1));
            let base = {
                unsafe extern "system" {
                    fn GetModuleHandleW(n: *const u16) -> *mut u8;
                }
                GetModuleHandleW(std::ptr::null()) as usize
            };
            std::eprintln!(
                "CRASH code={:#x} akind={} aaddr={:#x} rip={:#x} exe+{:#x}",
                (*rec).code,
                akind,
                aaddr,
                rip,
                rip.wrapping_sub(base)
            );
            // Symbolize rip + stack slots via dbghelp.
            unsafe extern "system" {
                fn GetCurrentProcess() -> usize;
                fn SymInitializeW(h: usize, p: *const u16, invade: i32) -> i32;
                fn SymFromAddrW(h: usize, addr: usize, disp: *mut u64, sym: *mut u8) -> i32;
            }
            let proc = GetCurrentProcess();
            SymInitializeW(proc, std::ptr::null(), 1);
            let symname = |addr: usize| -> String {
                // SYMBOL_INFOW: hdr 88 bytes then WCHAR name
                let mut buf = [0u8; 512];
                let mut disp = 0u64;
                {
                    *(buf.as_mut_ptr() as *mut u32) = 88; // SizeOfStruct
                    *(buf.as_mut_ptr().add(80) as *mut u32) = 200; // MaxNameLen
                    if SymFromAddrW(proc, addr, &mut disp, buf.as_mut_ptr()) != 0 {
                        let nl = *(buf.as_ptr().add(76) as *const u32) as usize;
                        let np = buf.as_ptr().add(84) as *const u16;
                        let w = std::slice::from_raw_parts(np, nl);
                        format!("{}+{:#x}", String::from_utf16_lossy(w), disp)
                    } else {
                        format!("?{:#x}", addr)
                    }
                }
            };
            std::eprintln!("  rip = {}", symname(rip));
            // Virtual unwind via RtlLookupFunctionEntry + RtlVirtualUnwind —
            // walks .pdata RUNTIME_FUNCTION entries (ours AND llvm's).
            unsafe extern "system" {
                fn RtlLookupFunctionEntry(
                    pc: usize,
                    base: *mut usize,
                    hist: *mut usize,
                ) -> *const u8;
                fn RtlVirtualUnwind(
                    htype: u32,
                    base: usize,
                    pc: usize,
                    fnentry: *const u8,
                    ctx: *mut u8,
                    handler: *mut usize,
                    estframe: *mut usize,
                    ctxptrs: usize,
                ) -> usize;
            }
            let mut c2 = [0u8; 1600];
            ctx.copy_to_nonoverlapping(c2.as_mut_ptr(), 1600);
            for _ in 0..32 {
                let pc = *(c2.as_ptr().add(0xF8) as *const usize);
                if pc == 0 {
                    break;
                }
                let mut ib = 0usize;
                let fe = RtlLookupFunctionEntry(pc, &mut ib, std::ptr::null_mut());
                if fe.is_null() {
                    std::eprintln!("  leaf/no-entry pc={:#x} {}", pc, symname(pc));
                    // leaf: return addr on stack
                    let sp = *(c2.as_ptr().add(0x98) as *const usize);
                    *(c2.as_mut_ptr().add(0xF8) as *mut usize) = *(sp as *const usize);
                    *(c2.as_mut_ptr().add(0x98) as *mut usize) = sp + 8;
                    continue;
                }
                let mut hd = 0usize;
                let mut ef = 0usize;
                RtlVirtualUnwind(0, ib, pc, fe, c2.as_mut_ptr(), &mut hd, &mut ef, 0);
                let npc = *(c2.as_ptr().add(0xF8) as *const usize);
                std::eprintln!("  frame pc={:#x} -> {}", pc, symname(pc));
                if npc == pc {
                    break;
                }
            }
            // x64 CONTEXT GPRs: rax@0x78 .. r15@0xF0, rip@0xF8
            let names = [
                "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11",
                "r12", "r13", "r14", "r15",
            ];
            for (i, n) in names.iter().enumerate() {
                let v = *(ctx.add(0x78 + i * 8) as *const usize);
                let tag = if v >= base && v < base + 0x360000 {
                    " (in exe)"
                } else {
                    ""
                };
                std::eprintln!("  {n}={v:#x}{tag}");
            }
            let rsp = *(c2.as_ptr().add(0x98) as *const usize);
            for i in 0..160usize {
                let q = *(rsp as *const usize).add(i);
                if q >= base && q < base + 0x2c6000 {
                    std::eprintln!("  [rsp+{:#04x}] = {}", i * 8, symname(q));
                }
            }
        }
        0
    }
    type Veh = unsafe extern "system" fn(*mut ExnPtrs) -> i32;
    unsafe extern "system" {
        fn AddVectoredExceptionHandler(first: u32, h: Option<Veh>) -> usize;
    }
    unsafe {
        AddVectoredExceptionHandler(1, Some(h));
    }
}
#[cfg(not(windows))]
fn install_crash_probe() {}

fn main() {
    install_crash_probe();

    // Go-compatible flags: -bench <substr>, -benchtime 2s|Nx, -count N,
    // -cpu N (name suffix only), -benchmem, -cpuprofile, -memprofile.
    Suite::from_env("pliron-bench")
        .with_alloc(&ALLOC)
        .bench("arrayvec", |b| {
            let v = arrayvec::setup();
            b.iter(|| arrayvec::run(black_box(&v)));
        })
        .bench("chars_alpha", |b| {
            let s = chars_alpha::setup();
            b.iter(|| chars_alpha::run(black_box(&s)));
        })
        .bench("chars_ascii", |b| {
            let s = chars_ascii::setup();
            b.iter(|| chars_ascii::run(black_box(&s)));
        })
        .bench("chars_mixed", |b| {
            let s = chars_mixed::setup();
            b.iter(|| chars_mixed::run(black_box(&s)));
        })
        .bench("contains_rng", |b| {
            let v = contains_rng::setup();
            b.iter(|| contains_rng::run(black_box(&v), black_box(100), black_box(60_000)));
        })
        .bench("eqbug", |b| b.iter(eqbug::run))
        .bench("fib", |b| b.iter(fib::run))
        .bench("hashmap", |b| b.iter(hashmap::run))
        .bench("hmget", |b| {
            let m = hmget::setup();
            b.iter(|| hmget::run(black_box(&m)));
        })
        .bench("iterw", |b| {
            let a = iterw::setup();
            b.iter(|| iterw::run(black_box(&a)));
        })
        .bench("matmul", |b| {
            // run() accumulates into c; reusing it across iterations keeps the
            // same instruction stream (f64 fma count is data-independent).
            let (a, bb, mut c, n) = matmul::setup();
            b.iter(|| matmul::run(black_box(&a), black_box(&bb), black_box(&mut c), n));
        })
        .bench("nbody", |b| {
            // run() advances the simulation in place; restock the pristine
            // state each iteration so long epochs can't drift into inf/denormal
            // territory (280 B copy vs a 2M-step advance).
            let orig = nbody::setup();
            let mut bodies = orig;
            b.iter(|| {
                bodies.copy_from_slice(&orig);
                nbody::run(black_box(&mut bodies))
            });
        })
        .bench("parse_only", |b| b.iter(parse_only::run))
        .bench("sieve", |b| b.iter(sieve::run))
        .bench("sortw", |b| {
            // run() sorts in place; restock the shuffled input each iteration —
            // sorting an already-sorted vec would take the fast path.
            let orig = sortw::setup();
            let mut v = orig.clone();
            b.iter(|| {
                v.clone_from(&orig);
                sortw::run(black_box(&mut v))
            });
        })
        .bench("strwork", |b| b.iter(strwork::run))
        .bench("tokenize", |b| {
            let s = tokenize::setup();
            b.iter(|| tokenize::run(black_box(&s)));
        })
        .bench("varint", |b| {
            let buf = varint::setup();
            b.iter(|| varint::run(black_box(&buf)));
        })
        .bench("varint_postcard", |b| {
            let buf = varint::setup_postcard();
            b.iter(|| varint::run_postcard(black_box(&buf)));
        })
        .run();
}
