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

## 2026-02 wasm clif-path audit (fuzzer-driven)

- `ctz_nonzero` unary op unhandled on the wasm CLIF path — stubbed
  `core::mem::alignment::Alignment::fmt`. Plain `ctz` lowering is a
  superset (the nonzero guarantee only skips the zero case). Fixed.
- `binop()` tagged comparison results with the operand container (i64) —
  `minmax`/`addc`/`overflow` flags then saw I64 and wrapped an already-i32
  value (`i32.wrap_i64` on an `i64.lt_s` result; invalid module, seed-1
  k28). Added `cbinop` (result always i32) at all comparison sites.
- Shift/rotate amounts: CLIF `ushr.i32` may carry an `i64` amount
  (`iconst.i64 29` from bounds-check folding); wasm `i32.shr_u` requires
  both operands i32. `cvt()` the amount to the value's container in
  ishl/ushr/sshr/rot. Fixed seed-7/11 invalid-module failures.
- Added `PLIRON_WASM_DUMP=<substr>` — dumps the final waffle body to
  `pliron::dump` (the `PLIRON_WASM_LOOPS` knob only brackets loopopt).
- fuzz.sh seeds 1,2,7,11,17,23,42,99 × 40 kernels all green; simd.sh
  34/34; real.sh 0 mismatches both wasm and native.
- real.sh perf (wasm, pliron vs llvm geomean 2.38x slower):
  sort 4.1x, sort_stable 5.2x, bsearch 4.3x — sort/search code is the
  weak spot. Native: matmul 10.5x, bsearch 5.6x; sha256/base64 ~2-3x.
  Uninvestigated — likely missing x64 codegen quality (no pattern
  matching strength?) or an opt-pipeline gap. TODO: profile one of
  these (matmul is the cleanest target).

## 2026-02-16 wasm clif+simd push + windows test port

Vector lowering on the wasm-CLIF path (all pushed):
- `PLIRON_WASM_CLIF` is now default-on (`=0` falls back to the dialect
  path). simd.sh/fuzz.sh/real.sh/bench.sh all green on it; the dialect
  path stays as the minimal fallback and passes fuzz too.
  (Later: the dialect path was deleted outright — CLIF won on speed,
  size, and compile time, so the fallback only cost maintenance.)
- `lower_clif_wasm` gets `simd` from `+simd128`; loopvec now emits v128
  on wasm. bench.sh --simd: k_count_eq 0.87x and k_dot_i32 0.85x stock
  (faster than LLVM), histogram 0.88x, utf8 0.85x; geomean 2.81x.
- New ops: iadd_pairwise (phadd via evens/odds i8x16 shuffles + lane
  add), vall_true/vany_true ({lane}.all_true / v128.any_true), scalar
  bmask, i64x2 min/max + sat add/sub (vicmp+bitselect expansions; wasm
  has no such lanes), i8x16 mul (i16 extend halves + low-byte shuffle).
- va_sym: gated on pointer_width==U64 — the wasm pipeline runs on the
  x86_64 host ISA, so foreign variadics were getting __pliron_va8.*
  trampolines the wasm linker can't resolve (undefined __pliron_va8.fcntl
  on wasi). Wasm variadics already marshal via pliron.va.buf.

Windows test-harness fixes:
- pliron-prof-rt: msvc needs _open/_write/_close/_getpid + _O_BINARY;
  the atexit-driven dump works unchanged.
- test.sh pgo: staticlib is pliron_prof_rt.lib on msvc (same ar format,
  llvm-ar extracts it). The wasm leg's RUSTFLAGS must be entirely
  windows-spelled: cargo's POSIX-path translation corrupts ':'-bearing
  flag values (`-Clinker=C:/...` -> `C;C:\Program...`) when the backend
  path is /c/-spelled. Full test.sh now green end-to-end on this box.

Symbol-gap audit for CRTFREE (all already covered):
- __chkstk: exported by ntdll.dll (kernel32.dll forwards) -> implibs.
- atexit/_initterm/_initterm_e/_cexit/exit: exported by msvcrt.dll.

Open:
- loopvec store-loop coverage: `z.iter_mut().zip(x).zip(y)` shapes
  thread ~7 header params (pointers+counters) through bounds-check
  islands; `param_kinds` classifies them all `?` -> "non-linear param"
  bail. k_vadd_f32 (68x), k_memset_xor (20x), k_saturating_u8 (27x),
  k_find_byte (10x), k_sum_i16_widen (7x) stay scalar. Not a miscompile
  — vectorizer gap. Extending param_kinds to recognize multi-IV zip
  headers is the next perf lever for wasm.
- native gaps unchanged: matmul ~1.5-10x, bsearch ~5x vs LLVM.

## Open: ui-wasm miscompiles found under wasmtime (2026-10-16, dialect-rm run)

The `ui-wasm` differential suite (`harness/run.py ui-wasm`, wasip1 under
wasmtime) surfaced 4 genuine miscompiles — reproduction details in
`pliron-dialect-rm/target/ui-wasm-notes.md`. All are run-fail vs the
stock-LLVM wasm side.

- `consts/cast-discriminant-zst-enum.rs`: enum discriminant `i8 -1` reads
  back `0xFFFF_FFFF` — zero-extension instead of sign-extension on the
  discriminant load/cast.
- `match/overeager-sub-match-pruning-13027.rs`: `0..=2 if false => 1,
  1..=3 => 2, _ => 3` returns 3 not 2 — range-match guard miscompiled.
- `macros/try-macro.rs`: `"1".parse::<i32>() as f32 + "2.0".parse::<f32>()`
  yields `Ok(1.0)` — second operand lost (f32 add or parse).
- `binop/infer-rhs-type.rs`: `"3.1415".parse::<f64>() <= 0.0` evaluates
  true — f64 parse or compare wrong (parse lives in pliron-built std).

The two float cases may share a root cause in the pliron-built wasip1
std float parse. Also: `wty(clt::F16)` panics to `unreachable` stub on
wasip1 (2 tests: f16-f128-lit, primitive-usage) — known f16 limitation.

## FIXED: xcgu local copies dead-stripped -> dangling `U` link failures

Repro: `cargo build` of `vendor/cranelift-assembler-x64-meta` (via
`/tmp/genisle`, a one-file runner calling `meta::generate_rust_assembler`)
with the pliron backend at `-O`, default CGU sharding (8+) -> 24
unresolved generic monomorphizations at link
(`IntoIterator::into_iter` for `[Operand; N]` / `IntoIter<Operand, N>`,
`Into::into` for `Location`/`Rex`/`Features`, `Iterator::collect`).
`-Ccodegen-units=1`, `PLIRON_XCGU=0`, `PLIRON_XCGU_CAP=max`, and
`PLIRON_DEADFN=0` each link clean and emit byte-identical generated
output to stock LLVM (145k-line assembler.rs, 0 diffs) — the bug is
emission, not generated code.

Root cause: `inline::dead_fns` marks `Linkage::Local` fns with zero
`call_counts`/`address_taken` callers and lower.rs skips emitting them.
The xcgu `extra` copies (`Linkage::Internal` -> Local) and `inlined=true`/
`Internal` mono items can still be *called* — by other copies and mono
items whose `llvm.call`s `direct_callee` doesn't count (rust `used`
lists don't model copy-to-copy edges at all) — so the body was stripped
while callers emitted direct `callq` relocs -> `U` with no def.

Fix (`e111bdd`): `State::xcgu_copies` records `extra` copies plus
`inlined`/`Internal` mono items; `dead_fns` exempts them. Keeping an
unused copy is object bloat; stripping a referenced one breaks the
link. Still open (cheaper to revisit than the dangling `U`s): the
*counting* blindspot itself — `direct_callee`'s `Indirect` arm resolves
only via `st.consts.get(p)`, which is stale/incomplete at iropt time
when passes rewrite call operands (the operand's ConstVal::Sym may sit
on an aliased-away value). dead_fns remains unsound for `Local`
non-xcgu fns with uncountable callers (fl.-clones, stubs).

Side bug found during the hunt: `PLIRON_STATS=1` kills the backend with
STATUS_STACK_BUFFER_OVERRUN (0xC0000409) — the stats/tracing path has a
latent smash; don't use it for diagnostics until fixed.

## FIXED: bittab sign-crossing isets silently dropped match arms

`bittab::signed_to_unsigned` emitted a sign-wrapping signed interval
`[a,b]` (a<0<=b) as `[(a_signed, umax), (0, b)]` — descending order in
the unsigned domain. `meet`/`complement` merge-walk assumes ascending
input, so intersecting a taken-arm iset silently produced empty (leaf
edge dropped as dead) and the complement swelled to cover it: a
guarded-range match like `match x+1 { 100=>0, 0..=2 if false=>1,
1..=3=>2, _=>3 }` folded to a 2-leaf mask routing `x+1 in 1..=3` to
the wildcard (3, not 2) at every signed int width, -O0 too. This is
the `tests/ui/match/overeager-sub-match-pruning-13027` miscompile
(also reported by the wasm differential suite). Fix (`2bc9495`, plus
the independent `a4c731e` in the diff-agent worktree adding sortedness
debug_asserts + a filetest): emit `[(0,b),(neg,max)]` ascending.

## PLIRON_STATS=1 crash fixed

`STATUS_STACK_BUFFER_OVERRUN` under `PLIRON_STATS{,_WHY}` was not a
real stack smash: `sroa::why`/`why2` grew a worklist over use chains
with no visited set; cyclic use graphs made the Vec allocation
unbounded (handle_alloc_error → fast-fail). `c114d2f` adds an
FxHashSet visited-set. `PLIRON_STATS_WHY=1` on genisle now completes.

## strwork/hashmap/call-chain pattern (perf, not yet fixed)

Disasm compare on `strwork`-shaped and `vpush` probes: our codegen
keeps the std machinery call chains ~2x deeper than LLVM —
`Vec::push` → `push_mut` → `reserve` → `try_allocate_in` →
`finish_grow`/`grow_amortized` → `alloc_impl_runtime` → `__rust_alloc`
survives as separate calls; LLVM folds the amortized fast path into
the caller and keeps ~3 of those call sites. `strwork::run` obj: 82
call sites (LLVM 19); `vpush::main`: 58 (LLVM 15). Same class as the
wasm `hash_one` gap — inlining-depth/call-chain, not per-call codegen.
`PLIRON_INLINE_LIMIT` has no effect (limit isn't the gate — the
callees aren't scored inlinable or the model's growth cap binds
first); `PLIRON_INLINE_GROWTH=16` made it worse. Also seen:
`unicode::alphabetic::lookup_slow` call inside the chars() loop —
`is_alphabetic`'s ASCII fast path IS inlined (LLVM-fused `c&~0x20 -
'A' < 26` arithmetic) but non-ASCII ≥0xAA calls into the TLS-checked
`eh_check` path every call: `_tls_index` + `gs:0x58` + `__pliron_eh`
load + flag test = ~7 extra insts/call for any extern callee rustc
can't prove `nounwind`.

## rustc leaf crates differential — all clean

Standalone differential probes (pliron vs stock LLVM, release +
8 CGUs + `-Zbuild-std` pliron-built std): `rustc_lexer` token stream
on ~300 ui files + rustc sources — byte-identical; `rustc_arena`
TypedArena alloc/drop of nested enum/box/vec structures;
`rustc_hashes` SipHash128; `rustc_serialize` FileEncoder/MemDecoder
roundtrip; `rustc_parse_format` pieces on format strings — all
IDENTICAL outputs. The stage-2 miscompile is not in these crates'
codegen. `PLIRON_DIFFCHECK=1` on the genisle crate (cranelift-meta
build) produced zero reports (dir only created on mismatch).
`PLIRON_VERIFY=full` clean on genisle.

## hashmap/hmget residual � probe-loop codegen, not call overhead (perf)

Native `HashMap::get` in a hot loop: the inliner DOES reach `hash_one`
correctly � `run -> get` (n=286, t=3840) and `run -> hash_one`
(n=932, cost=935, t=960 via the `PLIRON_INLINE_LOOP` bonus) both
ACCEPT: `run`'s emitted body has `hash_one` fully inlined. The
remaining `hash_one` callsites in the object are `main`'s cold
setup/rehash path (`insert`/`reserve_rehash`/`make_hasher -> hash_one`
scored at flat t=320 � rejected � correctly, they're not hot).
So the +38% hashmap / +52% hmget gap is inside the inlined probe loop
itself: our `run` emits ~32 xmm ops vs LLVM's 53 � hashbrown's SSE2
group-scan (loadu/cmpgt/movemask/shuffle) partially un-lowered, plus
per-iteration structure differences, not missing hash inlining.

`PLIRON_INLINE_VERBOSE` maps to `pliron::inline` at DEBUG but the
scoring path gates on `enabled!(pliron::frozen)` � set
`PLIRON_TRACE=pliron::frozen=debug,pliron::inline=debug` to see
cost/t/reject lines until the target is renamed.

## ui-wasm miscompiles — all four root-caused and fixed

Two wasm-emit bugs (`src/wasm.rs`), both sign-extension:

1. `cast-discriminant-zst-enum` — `extend()`'s i128 branch did
   `cvt(get1, I64)` = `I64ExtendI32U` for the low word of a signed
   narrow->i128 cast: `-1` enum discriminant -> i128 `0xFFFFFFFF`.
   Now sexts to i32 then `I64ExtendI32S`. Fixed in `11d5810`.

2. `try-macro` + `infer-rhs-type` (all float parsing!) — `Brif`/
   `CondTrap`/`Select` on an i64 condition went through `cvt` =
   `I32WrapI64`: `(x & 0x8000..) != 0` sign tests wrapped to 0,
   flipping `x < 0` branches. In dec2flt `exponent < 0` took the
   multiply arm → `10 * TABLE[(-1) & 31]` = `10 * TABLE[31]` = `10 * 0`
   = 0, so EVERY fractional float parsed as 0.0 (`"1.0".parse() -> 0`).
   New `cond()` emits `i64.ne v, 0`. Fixed in `11d5810`.

3. `overeager-sub-match-pruning` — bittab sign-crossing iset ordering
   (`4e9195d`/`2bc9495`) fixed it on wasm too — confirmed passing.

Plus `trait-object-arrays-11205` (dse/loadfwd, fixed upstream earlier).

NOTE for sysroot repro: `ui-wsys-build` cargo fingerprinting does NOT
track the backend dll — must `rm -rf target/ui-wsys{,-build}` after a
backend fix or the sysroot rlibs stay stale (cost me a false negative).

## wasm peephole miscompile — `str::lt`/`slice::cmp` prefix-equal (fixed `1928c86`)

`<str as PartialOrd>::lt` returned wrong results in some contexts
(`estr-slice.rs` failing `assert!(c < cc)` for "cccc" < "ccccc", and a
minimal `let lt = c < cc` repro). `PLIRON_WASM_PEEP=0` fixed it — the
bug was a lost-write hazard between two `peep_body` sub-passes in
`src/wasm.rs`:

- Adjacent-pair coalescing deleted a `local.get 4; local.set 4` self-copy
  (a no-op) outright and fused a `set 5; get 5` to `tee 5`.
- The copy-forward fixpoint then paired `get 5 -> set 4`, retargeted
  `tee 5 -> tee 4`, and deleted the copy — but the deleted self-copy was
  the only marker that local 4's value was consumed/reaffirmed on that
  path, so the block exited with l4 = the wrong source on every path.

Fix: a `WPeek::Nop(l)` marker for each deleted `get l; set l` — it acts
as a use+identity-def in the fixpoint's reaching-def analysis and a
motion barrier in FWD/KOS, so producer-retargeting can't drop a real
write. Verified: estr-slice/infer-rhs/try-macro/overeager all pass, the
4 original ui-wasm bugs are dead, 16468-pass full suite. Remaining
noise: 2 `output-mismatch` (raw pointer addresses — expected) + 2
`run-fail` (f16/f128 unsupported — same under PEEP=0).
