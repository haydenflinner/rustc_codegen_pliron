# Tuning on the held-out programs: blake3 3.11x -> 1.20x

Started from `bench/heldout/run.py` at 1.57x (2026-10-31-heldout.md). Tuned
on blake3, blind-sig, pulldown-cmark, regex and serde_json; lol_html,
flate2 and prost are kept untouched as a check that gains carry over.

## blake3 (3.11x)

Profiling a 300-iteration driver (samply can't attach to `/bin/sh`, so a
small crate that calls `blake3::hash` in a loop, built both ways): 95% of
pliron's time in `portable::compress_pre`, 99% of LLVM's in the
`compress_in_place` it is inlined into.

### Cause 1: a `const` table copied to the stack (`PLIRON_CONSTCOPY`)

pliron's `compress_pre`: 2,435 instructions, 112 `panic_bounds_check`
calls, an 896-byte `memcpy` at entry. LLVM's: 899 instructions, no branches,
no calls.

`round` does `let schedule = MSG_SCHEDULE[round]` and then reads
`msg[schedule[i]]` 16 times. rustc materializes the `const` table as a
stack copy of a private global (896 B), and the row as a second copy out
of the first. pliron kept the copies, so every `schedule[i]` was a load,
every `msg[...]` a dynamic index with a bounds check (7 rounds x 16).
LLVM's InstCombine replaces an alloca whose only write is a copy from
constant memory with the global (`isOnlyCopiedFromConstantMemory`), after
which the loads fold.

`constload::forward_copies` does the same before SROA: an alloca whose
only write is one non-volatile whole-object `llvm.memcpy` from a foldable
global, and whose other uses are loads, GEPs, lifetime markers and copy
sources, becomes the global's address; the copy and markers go. To a
fixpoint, so the row copy follows. `constload` then folds the reads.
Opt-in (`PLIRON_CONSTCOPY=1`) until fuzz and crates have run on it.

`compress_pre` after: 920 instructions, no branches, no calls. blake3
3.11x -> 1.48x.

### Cause 2: rotates folded into shifted operands (vendored Cranelift)

The remaining code matched LLVM's instruction mix almost exactly (337 add,
224 eor, 224 ror vs 337/232/220) and still ran 1.39x slower. The
difference was in the eors: `eor w23, w1, w25, ror #8` followed by
`ror w25, w23, #16`, against LLVM's plain `eor` + `ror`.

Cranelift's aarch64 rules fold `bxor x, (rotr y, k)` into `eor x, y,
ror #k` whatever else uses the rotate. In blake3's `g`, `d =
(d ^ a).rotate_right(8)` feeds both `c += d` and the next `d ^ a'`, so the
`ror` is emitted for the add and the eor re-rotates its input. On Apple
cores the shifted-operand form has 2-cycle latency (plain eor: 1), and the
eors are on the serial add/xor/rotate chain: four per `g`, about +4
cycles on a ~12-cycle chain. LLVM folds only single-use shifts
(`isWorthFoldingALU`).

The `*_ror_op`/`*_rol_op` rules now require a single-use rotate
(`rot_fold_ok`; PLIRON_PATCH 71; `PLIRON_RORFOLD_MULTI=1` restores).
blake3 1.48x -> 1.20x.

### What is left

`compress_pre` is 1.13x LLVM's (6,105 vs 5,403 samples): slightly more
reloads (61 `ldur` + 24 `ldr` vs 35 `ldr` + 21 `ldp`). The callers add
about 6% (`compress_chunks_parallel`), where LLVM inlines the compression
into the chunk loop.

## Validation

- Held-out, 7 rounds, both changes: blake3 1.199x, blind-sig 2.078x,
  pulldown-cmark 1.531x, regex 1.326x, rust-compression 1.461x,
  rust-html-rewriter 1.428x, rust-json 1.281x, rust-protobuf 1.083x
  (11-16% spread); geomean 1.57 -> 1.40.
- Kernels, interleaved, 3 rounds vs main: geomean 0.999. Only SipHash's
  `hash_one` changed size (86 -> 103 instructions: the rotates are now
  separate), and both hash kernels got faster (hashmap 0.969x, hmget
  0.986x). strwork 1.021x with same-length code (layout).
- Tests: `pliron-clif-opt/tests/rorfold_multi_use.rs` (the multi-use case
  fails with `PLIRON_RORFOLD_MULTI=1`); `test.sh t_constcopy` (a
  blake3-shaped table, a copy written afterwards, a copy that escapes;
  pinned outputs; requires the pass to fire).

## blind-sig (2.08x)

Driver: 300 iterations of the benchmark's blind/sign/finalize. 87% of
both sides' time in num-bigint-dig's `montgomery` (4,647 vs 2,477
samples). Its inner loop is `add_mul_vvw`: a u64 x u64 -> u128
multiply-accumulate over a zip of two slices. LLVM: 11 instructions per
limb. pliron: 19, from three causes.

1. **i128 shift by a constant** (`clif.lower.shift128k`). `(z >> 64) as
   u64` reached Cranelift as `ushr.i128 z, (uextend.i128 (iconst 64))`:
   rustc's i128 shift amount is an i128, Cranelift has no i128 `iconst`,
   and the aarch64 rules only fold a plain `iconst` amount. So the high
   half was a nine-instruction variable shift (`lsr; lsr; mvn; lsl; lsl;
   orr; tst #0x40; csel; csel`). Lowering now splits a constant-amount
   i128 shift into its 64-bit halves (`PLIRON_SHIFT128K=0` disables), and
   `const_bits` (which had no cast handling) folds `zext`/`sext`/`trunc`.
   1.20 s -> 0.89 s (LLVM 0.53 s).
2. **The zip's second end test** (`ivcmp`). After `ivretro` the loop
   walked one pointer and tested `p < x_end` and `p + (z - x) == z_end`.
   `ivcmp` rewrites `icmp eq|ne (iadd p, d), e` with `d`, `e` invariant or
   constant into `p == e - d` computed before the loop. Two instructions
   per limb; about 2% (the core hid most of it).
3. **The carry chain** (PLIRON_PATCH 72). `c = z1 + (z0 + c <u z0)` was
   `add; cmp; cset; add`, four dependent instructions on the loop-carried
   carry, against LLVM's `adds; cinc`. An aarch64 rule lowers `x + (a + b
   <u a ? 1 : 0)` (and the `uextend` form) to `cmn a, b; cinc x, hs`.
   0.88 s -> 0.67 s.

blind-sig driver: 1.20 s -> 0.67 s against LLVM's 0.53-0.54 s (about
1.24x, from 2.08x).

## pulldown-cmark (1.53x)

The escape scan (`escape_html_scalar`, `position` over a 256-byte table)
rebuilt the table's address (`adrp` + `add`) every byte. Three places put
it there: `licm` skipped `symbol_value` as free, and `splink` plus the
post-egraph `remat_sink` (PLIRON_PATCH 73) clone addresses into loops on
purpose, from an x86-64 measurement where the clone is one `lea`. On
aarch64 it is two instructions. licm now hoists them; both cloners run on
x86-64 only.

The driver time did not move (2.09 s vs LLVM's 1.25 s): pulldown-cmark's
gap is broad (first pass 1.86x, HTML writer 1.65x, escape 1.40x of LLVM's
inclusive samples) and needs per-function work on branchy parsing code.
Left for later.

## Census, then serde_json and memchr (suite 1.31x -> 1.27x)

`bench/heldout/census.py` profiles both binaries of every program under
samply and attributes the gap per function, with static counts beside
it. Hot functions had 2-7x LLVM's instructions and 5-40x its stack
accesses; `PLIRON_FRAME_INFO` showed the stack traffic is register
allocator spills (regex `Utf8Sequences::next`: 464 B of spills),
not locals kept in memory.

1. **memchr's movemask ran lane by lane.** `src/simd.rs` scalarized
   portable-SIMD shifts and narrowing casts; memchr's `vshrn_n_u16`
   emulation (serde_json, regex, lol_html) was 1,322 instructions in
   serde's `skip_to_escape` against LLVM's 181. Shifts by a splat now
   lower to one vector shift, half-width casts to `unarrow`
   (`clif.lower.simdshift`). Two follow-ups the batch caught:
   portable-simd masks the amount (`band(splat 4, splat 15)`), which hid
   the splat until lowering read through it; and Cranelift's aarch64
   backend shifts only 128-bit vectors, so zmij's `vshr_n_u32` ICEd five
   crates until 64-bit vectors went lane by lane.
2. **Loop placement.** chars_ascii ran 1.77x slower with identical code
   when its loop straddled a 64-byte line. Small loops are now aligned
   (patch 74), except loops with a call: fib's tail-recursion loop is
   entered once per call and paid 10 NOPs each time (+16%).
3. **A call argument pinned a loop value to a clobbered register**
   (patch 75, `PLIRON_ARGCOPY`). serde's string base was also `memcpy`'s
   `x1`; regalloc2's bundle got a fixed-register requirement for its whole
   range and split around every call, so the escape loop stored and
   reloaded the base every byte. A copy at the call fixes it, as LLVM's
   `COPY` to the physical register does.
4. **No rematerialization** (patch 76, `PLIRON_REMAT`, regalloc2
   `Output::edit_vregs`): the escape table's address was reloaded from a
   spill slot every byte; it is now re-executed (`adrp` + `add`). With 3,
   serde's escape loop has no stack traffic; the function went from 64 to
   32 stack accesses.

Also `invparam` (opt-in): LoopSimplify-style separation of loops whose
back edges disagree, moving inner-loop invariants off the header. It did
not help on its own (regalloc2 still splits the web), so it stays off
until regalloc work.

Per-program after this (pliron/LLVM): blake3 1.21, blind-sig 1.24,
pulldown 1.40, regex 1.33, compression 1.42, html 1.33, json 1.30,
protobuf 1.00. Geomean 1.27x.

## Measuring the gap dynamically (suite 1.27x -> 1.25x)

The census counts instructions statically, and its stack-traffic numbers
pointed at register allocation. `bench/heldout/dynstack.py` classifies
every profile sample by instruction class instead (pliron minus LLVM):
allocator spills were a small share; prologue/epilogue cost and loads
LLVM avoids were the big ones. Stack accesses explained 3 of regex's 226
extra samples; prologue/epilogue 171 of pulldown's 535 and 114 of
serde_json's 347. The dev loop for this is `racorpus.py` + `clif-check
codegen` (hot functions' pre-pipeline CLIF, scored in seconds).

What changed, each A/B'd on the held-out suite (interleaved variants):

- **Flat callee saves** (Cranelift patch 82): one `sub sp` and stores at
  offsets instead of pre-indexed pushes chaining through `sp`.
  1.292 -> 1.269.
- **slotprom container fields**: a slot written whole and read by
  narrower loads is promoted; regex's six-bool flags argument went
  through memory and then spilled. 1.289 -> 1.269.
- **Latch speculation**: a latch of at most three pure instructions folds
  into its predecessor's `brif`, one taken branch per iteration
  (pulldown's escape scan took two). 1.274 -> 1.250.
- **Vector-immediate rematerialization**: q registers are caller-saved,
  so splat constants spilled across calls.
- **Shrink-wrapping** (opt-in, `PLIRON_SHRINKWRAP=1`): saves move to the
  nearest closed dominator of the blocks that need them, with a
  live-range split at that block (no regalloc2 blockparam merge there)
  and duplicated shared returns. Correct (full test suite) but it
  applies to 2 of 26 hot functions; neutral in two A/Bs, so it stays off.
- **Upward load PRE**: a load below a loop header whose location is
  available on the back edges but not on entry is PRE'd at the header
  when the address is dereferenceable (rustc's `dereferenceable`, now
  passed to loadfwd). regex's `follow_epsilons` reloaded its `&mut
  SparseSet`'s buffer pointer and length every iteration; LLVM keeps them
  in registers.
- **bittab exploration**: membership trees explore only their own value's
  tests; serde_json's whitespace skip followed by the parser's dispatch
  had exhausted the eight-candidate budget. A second late bittab run
  (after jtround) is opt-in: neutral at 15 rounds.
- **Loop-exit constants** (Cranelift patch 85): constants used only as
  arguments of a loop-exit edge move onto that edge instead of being
  materialized in the latch each iteration.
- **Shared amodes need two direct uses** (patch 86): the propagated use
  state kept single-use `base + index` addresses whole (`add; ldurb [x]`
  instead of `ldrb [base, index]`). Held-out 1.243 -> 1.236.
- Tried and kept opt-in (sound, no measured win): straight-line block
  merging before jumpthread, `assume(x != null)` facts. The zip niche
  null test (num-bigint-dig, brotli) needs a non-null fact from somewhere
  else; that is the facts-service work's first open question.
- A pre-existing O0 miscompile surfaced in fuzz window 18800-19199 (seed
  18889; reproduces at this morning's commit).

## Evening: niche folds, a threading miscompile, facts through inlining

- **Second-level `Option` niche folds** (`PLIRON_NULLSRC=1`, opt-in):
  `fold_null_tests` looks through `select (a != 0), a, 0` and single-edge
  params. Removes zip/enumerate per-element null tests, but in two A/Bs
  regex lost ~5% and compression gained ~4% (geomean 1.222 on vs 1.213
  off), so it stays off.
- **Jump-threading miscompile** (found by crates: indexmap `drain_range`,
  exposed by that fold): a loop back edge threaded past a merge block
  kept that block's own params, which the threaded path no longer
  rebinds. Fixed in `jumpthread::round` (docs/history/2026-10-31-jt-backedge-stale.md).
- **`bytescan`** (opt-in): 16-byte NEON scans for byte-set loops over
  constant tables, with table bytes from rustc's evaluation of imported
  statics and an aarch64 `shrn` movemask (patch 89). pulldown -2.2%.
- **Facts through inlining** (opt-in, all needed together):
  `PLIRON_RETNN` (rustc's `nonnull` return attributes), `PLIRON_INLINENN`
  (an inlined callee's `nonnull` params on the caller's operands, scoped
  to the clone's region, with null folds before `merge_chains`),
  `PLIRON_BRIFSEL` (`brif c` args `select c, a, b` per edge),
  `PLIRON_IVPTR` (index-rebuilt pointers back to `p + s`),
  `PLIRON_EDGESINK_LOADS` (exit-only loads onto the exit edge).
  montgomery's zip loop goes from two exit tests and pointer
  recomputation to LLVM's 13-instruction post-increment shape. blind-sig
  went 1.69 -> 1.33 under the facts, but is not yet below the default
  1.27 (docs/codegen-plan.md). A first, pinned-param INLINENN design
  miscompiled regex and html-rewriter in the A/B; the region-scoped
  design replaced it, and pointer-only leaf marking was added.

## Night: replaying region facts, the real pulldown culprit, LLVM's montgomery loop

- **Region facts in the corpus**: `PLIRON_CORPUS` headers gain
  `diffcheck-bregion/rparent/rnn` and clif-check replays them with
  stand-in pliron blocks; it also maps the header's callee names back
  into `fn_names`.
- **pulldown regression root cause**: `EDGESINK_LOADS`, not BRIFSEL (the
  1.550 BRIFSEL figure predated the BRIFSEL restriction; a four-way
  build split showed EDGESINK_LOADS alone 1.295 -> 1.537). Slot reloads
  after a call in parse_block's byte-scan loop moved onto the loop exit
  edge; regalloc then kept the scan index in a stack slot (ldr/add/str
  per byte, 308 -> 667 samples). Loads now sink only onto edges leaving
  the source block's innermost loop, from blocks without a call.
- **`ivdown`**: an index used only by its step and an eq/ne exit test
  counts down to zero; the counter is the brif condition (`sub; cbnz`).
  Exact under wrapping. Mutation-checked tests; 15,694 corpus functions
  diffchecked per step with no mismatch.
- **Default flip** (92bf1ac, batch 20: gate, crates, fuzz 21200:21600):
  INLINENN, BRIFSEL, IVPTR, EDGESINK_LOADS, IVDOWN. Held-out geomean
  1.208 vs 1.230 off; blind-sig 1.275 -> 1.156.
- **Flag twins** (Cranelift patch 91): `add d, a, b` ... `adds xzr, a, b`
  -> `adds d, a, b`. montgomery's inner loop is LLVM's 11 instructions;
  blind-sig 1.160 -> 1.113.
- **Opt-in, measured on one bench each**: `memcmpk` (small constant
  memcmp inline; pulldown 1.309 -> 1.260), `PLIRON_INLINE_COLDONCE`
  (no free inlining of single-site `#[cold]` callees), `bittab` through
  single-pred params plus `PLIRON_BITTAB2` (rust-json 1.261 -> 1.187),
  `demand` (xor constants under demanded-bits masks; rust-json
  1.195 -> 1.179).
- Not pursued: regex's `Utf8Sequences::next` (976 vs 321 instructions,
  spill reloads) and blake3's message-word reloads (91 loads vs 56 with
  `ldp`): both are the register-allocation work (W1).

## Early morning (11-01): load PRE, narrowing, a constant-folder miscompile

Held-out geomean 1.192 (c9c6b6d) -> 1.182 (mintrip on) -> 1.172
(`#[inline(always)]`, cold xcgu promotion) -> 1.171 (load PRE fix).

- **Load PRE clobber check** (e499f46): upward PRE now uses the same
  iso/ncc-aware clobber test as forwarding (`may_write_iso`), and
  rebuilds an in-loop address from a dominating base (`peel_const`).
  regex's `follow_epsilons` keeps the `SparseSet` fields in registers
  across the inner loop: regex 1.343 -> 1.205.
- **Small fixes** (batch 28: gate, crates, fuzz 24200:24600 clean):
  memcmpk takes the length from a dominating `len == K` guard (serde's
  derived field visitors); bytescan treats header params passed
  unchanged as loop-invariant (`PLIRON_BYTESCAN_PASS=0`); loadfwd
  forwards narrower and extending loads from a wider store
  (`PLIRON_LOADFWD_NARROW=0`; regex `cached_state`'s u32 reload).
- **Miscompile, fuzz seed 24242** (e46f7ad): `clifpeep::ceval` masked an
  `ireduce` to its source width, so the high bits survived into a later
  `sshr`. `bitmerge` then folded a field insert to 0x2e655 instead of 5.
  Every `ceval` result is now masked to its own type. Found by batch 27's
  fuzz; `harness/bisect.py` pointed at jumpthread (the first unit whose
  output differed), and the step checker then named clifpeep. A
  temporary hit limit inside `bitmerge` found the single bad fold.
- **Two more miscompiles, from a 2000-seed sweep** (25000:27000, after
  batch 29 had pushed): slotprom's skip of the writeback before a call
  taking the slot as its sret argument (9696857) assumed the return value
  covers the whole slot. In rustlantis's custom MIR it can cover only
  the front (`RET.0` of a 224-byte local); the fields past it never
  reached memory before the reload after the call. Seeds 26212/26694,
  both bisected to slotprom; the skip is reverted (the CLIF signature
  doesn't carry the return size) with a unit test that fails on the old
  code. Batch 28's 400 seeds had missed it: memory-pass changes now get a
  2000-seed sweep.
- **Second 2000-seed sweep** (27000:29000, on 55f1e4b): two more.
  Seed 27004: celim's `snnowrap` checked a downward offset as
  `l - d >= smin` (always true), so `x s< x - 94` under `x == i64::MIN`
  folded to false; fixed (`l + d`), with a test. Seed 27992: upward load
  PRE of an isolated `&` param's 16-byte load past stores to other roots
  changed control flow. It needs e499f46's iso-aware "clobbered earlier
  in this block" check. The plain check is back at no measured cost
  (regex 1.200 vs 1.201); the root cause is not identified, and every
  piece (in-block stores, the merge block's preservation, each in-edge
  value) reads as sound. A lead for later: the seed's corpus dump
  (`PLIRON_CORPUS`) replays it, and `XUP_LIM`-style counting on upward
  hits isolates the single PRE at block35 -> block27. All five seeds of
  the night are in the gate's `FUZZ_REGRESS`.
- **Knob sweeps on the five tuned benches** (interleaved, 8 rounds):
  `PLIRON_INLINE_DUPCOST=1` and `PLIRON_INLINE_COLD=1` both give json
  -2.7% and pulldown +5%; `SCCLIMIT`/`ONCE1` change no binary. Not
  remat'ing `band x, k` (Cranelift `remat.isle`) shortens regex's
  dispatch cascade (4 `and #7` -> 1) yet times json -1.4%, regex +1.7%,
  pulldown +1.5%. blind-sig's montgomery inner loops match LLVM's
  instruction for instruction; its 7% is outer-loop SmallVec handling.
  blake3's compress has LLVM's exact ALU mix (337 add, 232 eor, 220 ror)
  with ~21 more spill reloads and fewer `ldp` pairs: register
  allocation.
- **Escapes in unreachable blocks** (`PLIRON_ESC_REACH=0` off):
  `isolated_escapes` scanned every layout block. regex's
  `SparseSet::insert` asserts `len < capacity` before its push, so the
  `Vec` grow call is dead once the branch folds (looprot/licm2), but the
  dead block stayed in the layout until edgefwd's sweep and kept `qcur`
  escaped through licm3. Only blocks reachable from the entry count now;
  licm3 hoists the set's four header loads out of the insert loop
  (regex 1.218 -> 1.200, interleaved).
- **Split the batch's regression per flag first**: batch 28 read 1.176
  vs 1.172 with narrowing and bytescan pass-through off. Per flag,
  bytescan pass-through changes no held-out binary and narrowing is
  neutral (regex 1.228 vs 1.226, json 1.165 vs 1.171): the gap was noise.
- **Measured, left off**: emit-time reload forwarding (a `ldr` from a
  spill slot already held in a register becomes a `mov`) removed only
  17 of 778 stack loads in pulldown's `parse_block`: the redundant
  reloads sit across calls or register redefinitions, so the fix belongs
  in regalloc's splitting (W4). `PLIRON_INLINE_COLDONCE` (protobuf
  +1.5%), `PLIRON_XCGU_PROMOTE=1` (regex +3%), SHRINKWRAP (noisy). Not counting
  calls in a callee's doomed blocks toward the inline score would inline
  serde_json's `parse_object_colon` (json -1%), but it also inlines
  `follow_epsilons` into `next_state`, and the two cost 40% more together
  (regex +14%): its live calls fell under the loop-site bonus gate.
  Charging only live calls in the cost but gating the bonus on all calls
  keeps `follow_epsilons` out and inlines `parse_object_colon`, but nets
  negative (json -0.7%; pulldown, blind-sig, blake3 +0.7-0.9%). licm hoists that see roots uncaptured until after the
  loop as isolated (`promote`'s rule) changed nothing on regex, and a
  loop-load pass that reloads after cold clobbering calls never fired
  there either (both shelved): the clobber was the dead grow call above.
