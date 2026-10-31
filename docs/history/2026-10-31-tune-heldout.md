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
