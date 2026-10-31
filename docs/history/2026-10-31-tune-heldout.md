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
