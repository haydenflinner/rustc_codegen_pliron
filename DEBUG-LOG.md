# Overnight debug log — windows-msvc native bench crash + MSVC-free toolchain work

## What was broken (root cause, FIXED)

Criterion `--baseline` crashed with `0xc0000005` inside LLVM-compiled
`criterion::analysis::compare::estimates::stats` — iterating a `&[f64]` whose
data pointer was actually a stack address.

Root cause chain:
- `stats(a: &Sample, b: &Sample) -> (f64, f64)` — Rust ABI returns the pair in
  xmm0/xmm1 (verified against stock LLVM output).
- Our dialect/CLIF for the call was correct: `(i64,i64,i64,i64) -> f64,f64`.
- BUT vendored cranelift's `get_fltreg_for_retval(WindowsFastcall)` listed only
  `xmm0` — the second f64 return couldn't fit → `compute_arg_locs` saw
  `sized_stack_ret_space > 0` → `add_ret_area_ptr` silently injected a hidden
  return-area pointer as arg0 (rcx). Emitted call: `(sret_buf, a.ptr, a.len,
  b.ptr, b.len)`. LLVM callee read rcx=a.ptr → wild pointer → crash.
- Same path explains the "thunk" (s139 shim) oddities: every `{f64,f64}`
  multi-ret call/def on windows-msvc got the phantom first arg.

Fix (in `vendor/cranelift-codegen/src/isa/x64/abi.rs`): added
`1 => Some(regs::xmm1())` to the WindowsFastcall float-retval list — the same
way the i128 fix added `rdx` to the int-retval list ("The Rust ABI for i128s
needs this"). Only affects sigs with >=2 float rets, which only come from
rustc ScalarPair returns; extern-"C" large-struct returns are Indirect at the
dialect level anyway (sret param), so MSVC C ABI is unaffected.

Minimal repro (worked before fix):
    // callee.rs (stock rustc rlib): pub fn stats(a:&Sample,b:&Sample)->(f64,f64)
    // main.rs (pliron): let (x,y) = callee::stats(mk(&a), mk(&b));
    → segfault pre-fix, correct output post-fix.

Full bench now runs `--baseline` comparisons to completion. Perf deltas vs
LLVM baseline are noisy (a few +/-10-30%); worth a proper look later.

## Diagnostics added

- `pliron::lower` tracing: `callsig=` (param/ret count per lowered call),
  `lowerfn=` (per-function marker). `PLIRON_TRACE='pliron::lower=debug'`.
- bench/benches/kernels.rs: VEH crash-probe printing rip/aaddr (dbghelp
  symbolization attempt was unreliable; .map file + manual RVA math worked).

## Known remaining issues

- `pliron-clif-opt/src/loadfwd.rs:1084` ICE at `-Ccodegen-units=1`:
  `loadpre: location not available at end of block` — latent, separate.
- Vec<f64> in-memory order is {cap@0, ptr@8, len@16} on this toolchain —
  verified empirically (relevant to any hand-rolled fat-ptr handling).
- Criterion perf vs LLVM: strwork -30% (we're faster), sortw +20%, tokenize
  +12%, parse_only +10% (slower) — mostly small-N noise, but sortw is worth
  profiling.

## Questions for review

- OK to keep the temporary repro bin + serde deps out of the repo? (dropped
  from the commit; they were only crash repro scaffolding.)
- Next: MSVC-free Windows link path — see plan below.


## Overnight progress — MSVC-free linking (DONE, working)

Achieved: full Rust compile+link+run on windows-msvc target with ZERO
Visual Studio / Windows SDK inputs.

Stack that works (all verified running correctly):
- `-Clinker=rust-lld` replaces link.exe (lld is shipped in the toolchain)
- `tools/mkwinlibs/mkwinlibs.py` generates import .libs from
  C:\Windows\System32 DLL export tables via llvm-readobj + rust-lld /def
- `PLIRON_CRTFREE=1` makes the backend emit into symbols.o: TLS directory
  (_tls_used/_tls_start/_tls_end/_tls_index/_pliron_tls_cbs), CRT entry
  thunks, _fltused, type_info vftable, GS-cookie symbols; plus .drectve
  /DEFAULTLIB directives for ucrtbase+vcruntime140

Final recipe:
    uv run --no-project tools/mkwinlibs/mkwinlibs.py <out>
    PLIRON_CRTFREE=1 rustc -O foo.rs -Zcodegen-backend=<be>         -Clinker=rust-lld -Lnative=<out>

Traps hit on the way (all fixed):
- lld only emits the PE TLS dir when _tls_used symbol exists; the struct
  fields are real relocations, not linker-filled
- `.tls` merge ordering: unsuffixed .tls first, then .tls$AAA..$ZZZ by
  suffix — _tls_start must be a plain byte in unsuffixed .tls from the
  first object (symbols.o); _tls_end rides .tls$ZZZ; cranelift TLS data
  goes to `.tls$$subsection` which sorts between
- implib DLL binding name comes from lld-link's /out:, not def LIBRARY
- `.def` can't spell `=X.#N` ordinal forwards or `=`/` ` in names — skipped
- llvm-ar can't merge implibs (member-name mangling) — use per-DLL implibs
  + .drectve /DEFAULTLIB instead of a fat merged lib
- api-ms-win-* sync functions aren't in kernel32.dll — synchronization.lib
  needs forwarder defs to API-MS-WIN-CORE-SYNCH-L1-2-0
- `PLIRON_CRTFREE` also drives `.drectve` emission — object carries
  /DEFAULTLIB:ucrtbase /DEFAULTLIB:vcruntime140 so no extra -l args needed

Still TODO / open questions:
- `_tls_start` ordering relies on symbols.o being first + our emit-order
  sort — works; a deeper fix would name-boundary the whole template
- gnullvm target tried first: needs mingw-w64 libs not shipped with rustup
  (mingw32/mingwex/libunwind) — abandoned in favor of msvc+lld approach
- crtstatic/CRT statics not emitted: __chkstk (MSVC name — only referenced
  if a /GS+chkstk foreign .o needs it — alloca.o didn't), _initterm,
  atexit-driven cleanup — minimal CRT entry skips C++ static-init lists;
  `std::process::exit` works (42 verified), println works
- `panic=abort` still fails on alloca.o unwind requirement — unrelated
- benchmark under CRTFREE runs clean end-to-end; perf deltas unchanged
  (strwork -30% faster, sortw +20% slower vs LLVM baseline)
- the `loadfwd` CGU=1 ICE noted earlier remains open
