# Overnight 2026-10-22 → 23: toolchain bump, unwind exits, trip bounds

## Toolchain: nightly-2026-10-06 → nightly-2026-10-22 (9b0eab9)

This was the first bump since the backend was created. The upstream API
changes were mechanical:

- `PassMode::Indirect` was split into `Indirect` and `IndirectUnsized`.
- New required trait methods: `ctpop` and `type_ppcf128`.
- `run_thin_lto` takes `incr_comp_session`.
- `rmeta_link::read_from_data` takes `is_like_aix`.

One change broke behavior: `rust_eh_personality` is now a mangled
std-internal symbol, so every binary that could unwind failed to link.
`State::eh_personality` now carries
`rustc_symbol_mangling::eh_personality_symbol(tcx)` down to the unwind
tables. The rustc fork (`haydenflinner/rust` main) was rebased to the
same nightly: three self-host patch commits on top of `1a2c4797e`. The old
main is tagged `archive/main-2026-10-08`.

MIR move elimination is in this nightly but only runs under
`-Zmir-move-elimination` (default off). Every gate stanza passes with it
on (`PLIRON_TEST_RUSTFLAGS=-Zmir-move-elimination`). `tests/mirshapes`
pins the MIR shapes it produces (see 2026-10-22-mir-move-elimination.md).

## Miscompiles: stores a panicking call's caller can see

Found by hashbrown's `hashmap_reserve_survives_panicking_build_hasher` on
the new toolchain. The bug is older; the new MIR shape exposed it.

| pass | bug | fix |
|---|---|---|
| dse (93e9541) | Calls were barriers only for non-isolated memory, and only `return` exposed caller memory. So a store through `&mut self` was deleted when a later store rewrote it, or when the path ended in `call; trap`, even though the call could unwind into a caller that catches the panic. | A plain call that may unwind is `Op::Exit`: it sets `top_non_stack` and drops `Root::V` coverage. |
| licm promote (7f8474f) | Scalar promotion of an isolated root across a loop containing a call that may unwind. The unwind leaves the loop without passing the exit-edge flush. | `Root::V` groups are not promoted when the loop has such a call. |

Both fixes need to know which calls can unwind. Lowering now records a
per-callee `nounwind` set: `FuncInfo::nounwind` plus asm stubs, the same
`maythrow` computation the emulated-EH path uses. It is threaded
FnJob → Pipe → dse/licm next to `nowrite`/`call_frozen`. `try_call`
needs nothing: its unwind edge stays in the function and reaches the
resume call, which is itself a plain call.

Tests:

- `dseunwind`
- `unwindmem`: 10 shapes, including loop promotion, Vec push and drop guards.
- `unwindstd`: std's own guards (SetLenOnDrop, sort holes, RefCell flags,
  BTreeMap, ...) under a panic at 12 points, compared with stock.
- dse unit tests for the unwind/nounwind pair.

## Trip bounds from dominating guards (4a0911b)

The arrayvec gap (+16%) came from loopvec and punroll cloning 16-lane and
×8 bodies for loops behind `len <u 9`; those clones can never run.
`loopidiom::trip_max` bounds `(bound - iv0) / step` using:

- dominating `brif`/`icmp` guards;
- block params, taking the max over incoming edges, each with its own
  edge guard (rotation copies the len check onto every entry);
- `(y + X) - (y + c) <= X`.

How the passes use it:

- loopvec skips loops whose bound is below one vector group.
- punroll keeps K within an eighth of the bound.

| arrayvec fn (µs) | stock | before | after |
|---|---|---|---|
| sum_all | 1439 | 1822 | **1301** |
| find_all | 1447 | 2165 | 1647 |
| push_iter | 2184 | 2165 | **1880** |

The bound is a cost input only. `PLIRON_TRIPMAX=0` turns it off.

## Fuzzing (rustlantis, nightly-2026-10-22)

| seeds | result |
|---|---|
| 520:776 (with selchain/mulcarry/stepcmp on) | 3 output mismatches, all on by default too |
| 776:2048 | 1268 pass, 2 ICEs, 1 mismatch, 1 compile timeout |

Fixed:

| seeds | bug | commit |
|---|---|---|
| 538, 683, 730 | slp xpack turned two loads of one address into `splat(first load)` without checking for a writer between them (`a = *p; f(p); b = *p`) | b0860b3 |
| 810, 1276 | loadfwd's dead-pair poisoning inverted all of `pre.start`, so single-pred pass-through aliases shadowed the owning pending pair. A removed param was then appended to an edge ("argument v14720 is not attached") | 12deef9 |

`gate.py`'s `fuzzreg` job replays these seeds. They're about 450 KB of
custom MIR each, too big to check in.

### Open: seed 1939 (output mismatch at -O)

- It reproduces with only `fn13` optimized
  (`PLIRON_OPT_FNS=@fn13`); bisect unit 363 is step `ifconv2`.
- Within that step it is `cparm`'s `const_sweep`: `PLIRON_CSWEEP=0` fixes
  the fn13-only build but not the full build.
- The interpreter finds the pre- and post-`cparm` CLIF equivalent (400
  trials), and the final CLIFs too.
- The one fold examined, `v2665 -> i64::MAX - 1`, is correct for its
  input: every SSA path into the param carries `i64::MAX`.
- So the divergence is in a fold the interpreter shares, or in
  memory/uninitialized-read behavior it doesn't model.
- Next step: reduce the program (only `fn13` matters), or diff `fn13`'s
  disassembly at units 362/363.

`PLIRON_CLIF_STEPS=1` (with `PLIRON_CLIF`) now dumps after every step
that reports a change; several steps don't dump on their own.

### Seeds 2048:4096 and the rest of 776:2048

| seed | bug | fix |
|---|---|---|
| 1776 | compile hang: Cranelift's `LoopAnalysis` consulted dominance for unreachable predecessors (17 dead blocks jumping into loops after paramsink); loop parent links closed a cycle and `compute()` spun in postinc2 | a678d9a: skip unreachable preds (vendored `loop_analysis.rs`) |
| 2542 | ICE: `o + n` overflowed in loadfwd's store-kill overlap test; in release a wrapped end can call a real overlap disjoint | 2f74fad: loadfwd saturates, dse uses i128 |
| 3938 | slp `may_overlap` called every (slot, untracked pointer) pair disjoint; a loop-carried param `param_src` couldn't resolve still pointed into the slot | 0fa0839: disjoint only for `loadfwd::isolated` slots |

`PLIRON_XPACK_LIMIT=N` was the tool for 3938: bisect over N, with
`PLIRON_OPT_FNS` naming one function, finds the bad pack (#79).

Open:

- **2598: fixed by 23bb4c8** (Apple-packed stack arguments: Cranelift
  stored/loaded them at the slot width, not their own; test
  `applestackargs`). Old notes follow.
- **(old notes) 2598, mismatch at -O0, not an optimization bug.** `PLIRON_OPT_BISECT=0`
  still fails. A logging hasher showed one `[isize; 8]` element of
  `fn8`'s by-value param arriving as `0xffff_ffff_ffff_0000` instead of
  -1. The `--debug` build of the same seed (values printed, not hashed)
  matches stock, so the program reduces only with a UB-safe oracle.
- **1939 and 3441: fixed.** Root cause: Cranelift isel, not the passes.
  Our aarch64 `iadd_ushr_*` rules (add-with-`lsr` fold, a47132a) matched
  i8/i16, where the register has garbage above the value's width and
  `lsr #k` shifts it into the sum. The param-merging steps only made the
  shift amount a constant, which is why the interpreter saw no difference.
  Found by building rustlantis' `--debug` variant (it prints values, and
  unlike 2598 it reproduced). One printed field differed only in its high
  byte. That pointed to an inlined, self-contained `fn18`, which reduced
  to a 548-line program that `harness/bisect.py` could narrow. Test:
  `narrowlsr` (pinned checksum: the inline reference computation goes
  through the same rule). Both seeds are now in `fuzzreg`.
- **(old notes) 1939 and 3441, mismatch at -O.** Both are after a param-merging step
  (`cparm`'s `const_sweep`, `sameargs`). Every fold checked by hand is
  correct, and the interpreter finds before/after equivalent, even with
  non-zero stack fills (`PLIRON_DIFFCHECK_FILL`, new). Yet native output
  differs, and rolling back that one unit fixes it. That points at
  Cranelift lowering/regalloc of the post-merge shape. Next step: a
  native-vs-interpreter runner (JIT) in clif-check.
- **2187: compile timeout, gone.** It compiles in 3 s at -O on current
  main and matches stock (likely the loop-analysis fix a678d9a). It stays
  in `fuzzreg`.

## nushell polars selector: ifconv `br_table` arm dedup

The last crates failure (`nu_plugin_polars` `selector_signed_integer` /
`selector_unsigned_integer` returning no columns) was a miscompile in
polars-plan, not nushell. It was found by bisecting:

1. Crates: pliron only for a subset, the rest stock. That took 14 builds
   of about 8 minutes each and named `polars_plan`. LTO was not involved:
   an all-pliron build without LTO failed too.
2. Functions: `harness/fnbisect.py` with each step rebuilding only
   `polars_plan` and its dependents (about 2 minutes). It named the
   `filter_fold` closure of `DataTypeSelector::into_columns`.
3. Units: `harness/bisect.py` with `PLIRON_OPT_FNS` set to that function.
   Unit 80 is `jumpthread`, and `PLIRON_IFCONV=0` alone fixes it.

The cause is in `ifconv`'s `br_table` conversion. The arm dedup compared
`(m, side, args)`, but an arg that is a result computed inside a shared
forwarder hop (`v22 = band v20, 1`) is the same `Value` for every arm,
even though the hop's param `v20` gets a different value per arm. A
`matches!` table (in-range indices give 1, the default gives 0) collapsed
to the default, so the filter rejected everything. Arms now also need
equal chains and param substitutions.

The step's diffcheck missed it twice. The function calls externs, and
the interpreter can't run those. In the reduced CLIF, the bad path took
5 of 256 tag values and the default 16 trials never hit it. Test:
`pliron-clif-opt/tests/ifconv_table_chain.rs` (fails without the fix).

## Crates on nightly-2026-10-22

Tiers 1–3 (37 crates, at 7f8474f): all match stock except nushell's
`nu_plugin_polars` `selector_signed_integer` / `selector_unsigned_integer`.
In those tests `polars select (polars selector signed-integer)` returns
no columns. This still reproduces with tonight's later fixes. The
`DataType` predicates and polars-plan's `Selector::into_columns` are
correct in a standalone crate with default polars features, so the next
repro uses nushell's full polars feature set.

Stock tokio stalls at 518/2310 tests under this nightly. `crates.py` now
reports a stock-side timeout as `STOCKTO` instead of `ok`.

## Selfhost UI run (stage 1, backend 0fa0839)

`tests/ui` with the pliron-built stage 1 compiler: 63 failures (the old
baseline was 102). Most are expected: `codegen` (24, LLVM-specific output),
`sanitizer`, `scalable-vectors`, `lto`, `target_modifiers`, `darwin-objc`,
backtrace/debuginfo, LLVM pass flags. The stage 2 build was killed by
earlyoom while other jobs ran, so there is no stage 1/2 comparison yet.

Real bugs found and fixed, each with a gate stanza that fails without
the fix:

| UI test | bug | fix |
|---|---|---|
| `attributes/fn-align-dyn.rs` | `#[rustc_align(N)]` ignored: functions and their `dyn` shims got Cranelift's code alignment only | e7b9eb2 (`fnalign`) |
| `abi/rust-tail-cc.rs` | ICE: rustc passes `extern "tail"` large aggregates OnStack (byval); Cranelift has no `StructArgument` on arm64 | 27dc9de: plain pointer off x86-64 (`rusttailcc`) |
| `asm/binary_asm_labels_allowed.rs` | vendored rsasm rejected the local label ref `01b` as a bad octal literal | f631f23: rsasm lexer (`asmlabels`) |
| `c-variadic/copy.rs`, `consts/const-eval/c-variadic.rs`, `abi/variadic-ffi.rs` | every C-variadic definition crashed on aarch64-apple: `va_start` wrote a 24-byte SysV `__va_list_tag` into an 8-byte `VaList` | ccd395e: `va_start`/`va_arg` follow the target's `va_list` (SysV / AAPCS64 / pointer) (`cvariadic`) |

Not reproduced: `thread-local/tls.rs` (`assert_ne!` of a TLS address
seen from two threads) failed once in the run; 200 runs with stock and
stage 1 builds pass.

Left: `asm/label-operand.rs` (asm goto unsupported),
`asm/aarch64/may_unwind.rs` (unwinding out of `asm!`),
`statics/const_generics.rs#opt` (we keep a separate copy of `&FOO` where
LLVM merges it; status-quo test, not a bug),
`traits/next-solver/...-129372.rs#old` (expects a compile error our
backend does not hit).

## Tried and dropped: gating `bump_hoist` phase 2

arrayvec `find_all` (pliron +14% vs LLVM) has `sub x9, x11, #36` in every
element block of its unrolled search. The vendored Cranelift `bump_hoist`
(context.rs) phase 2 rewrites every later use of the pre-bump pointer
into `q - 36` so the base dies at the header load. But that load is at
offset 32, so the post-index fusion it enables can't happen. Gating
phase 2 on a zero-offset access, and on no rewritten use inside the
header's own loop, removed all 10 subs. The element path became LLVM's
5-instruction sequence, at the cost of one back-edge `mov`. The full
kernel A/B (3 interleaved rounds, gate on vs `=0`) was neutral: every
kernel within ±1.6%, arrayvec −0.3% in favor of the old code. The core
absorbs the extra `sub`, so the change was not landed.

The remaining `find_all` difference is the loop end pointer `v0 + n*36`:
Cranelift's egraph rebuilds it with a `madd` at every exit check. The
egraph rewrote `block0`'s only out-of-loop use (`v0 == v0+v16` became
`v16 == 0`), and elaboration then doesn't hoist it to the preheader.
`PLIRON_COMPILE_EGRAPH=0` computes it once. Not investigated further.
