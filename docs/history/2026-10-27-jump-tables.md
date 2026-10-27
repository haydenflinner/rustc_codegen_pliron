# Jump tables: narrow deferred tables and a table/chain policy

Follow-up to 2026-10-27-code-size, which counted "about 16 KB" of inline
jump tables in regex-syntax's `__text`. Both flags below are default off.

## Correction: 9.3 KB of tables, not 16 KB

The ~4,000 `udf #imm` words objdump shows are two things:

- **2,336 jump-table entries** (9,344 B) in 79 tables.
- **1,654 `udf #0xc11f` traps** (6.6 KB), 1,506 of them straight after a
  `bl` to a noreturn function (panics, `handle_alloc_error`). LLVM emits
  71 `brk #1`, only where the call would otherwise be the function's last
  instruction. This is a separate lever (see "Next").

## Where switches become tables today

- `pliron-clif-lower/src/lower.rs` `SwitchOp`: `dense_switch` emits one
  `br_table` over `[min, max]` when there are >= 4 cases at >= 10% density
  (`PLIRON_SWITCH_DENSE`); holes go to the default. A dead default pads to a
  power of two and masks (`PLIRON_SWITCH_MASK`, x64 skips the clamp).
  Otherwise `cranelift_frontend::Switch` (runs of contiguous cases become
  tables, binary search between them).
- `pliron-clif-opt/src/brchain.rs` (last CLIF step): a `br_table` with at
  most 4 non-default entries becomes an `icmp eq` + `brif` chain; also
  `select`-fed dispatch.
- Cranelift: aarch64 `JTSequence` = `cmp; b.hs; csel; adr; ldrsw [uxtw #2];
  add; br` with 4-byte entries inline after `br`; x64 `JmpTableSeq` =
  `cmp; cmov; lea; movslq; add; jmp` with the default as a last entry.

## Distribution (regex-syntax, `-O -Ccodegen-units=1`, aarch64)

From `llvm-objdump` of the pliron object (script: scratchpad `jtstat.py`).

| | |
|---|---|
| tables / entries / bytes | 79 / 2,336 / 9,344 |
| entries equal to the default target | 1,126 (48%) |
| entries per table | 6–8: 34, 12–16: 19, 17–64: 11, 65–128: 15 (max 92) |
| distinct non-default targets | 1: 10, 2–3: 6, 6–8: 34, 9–16: 16, >16: 13 |
| clusters (runs of one non-default target) | <= 4: 11, 5–8: 38, >8: 30 |
| target span (min..max target) | <= 1,020 B: 60 tables, <= 16 KB: 19, none larger |
| tables with a target before the table | 9 (55 entries) |

LLVM has 12 tables for the same crate (1–2 B entries in `__const`): it
emits fewer tables mostly because it inlines and merges more switches
before lowering (6 of pliron's tables are 92-entry copies of
`hir::visitor::visit`), not because of a sharper density rule.

Entry width needed, by encoding (bytes of entries):

| base / entry | 1 B | 2 B | 4 B | bytes |
|---|---|---|---|---|
| stock: table start, signed, 4 B | | | 2,336 | 9,344 |
| table start, signed /4 | 536 | 1,800 | | 4,136 |
| table start, forward-only unsigned /4 | 1,248 | 915 | 173 | 3,770 |
| **lowest target, unsigned /4** | 1,844 | 492 | | **2,828** |

## Policy

Two separate decisions: how a table is encoded, and whether a switch is a
table at all.

**Encoding (`PLIRON_JT_NARROW=1`, vendored Cranelift).** The problem with
narrow entries in Cranelift is that the emitter does not know forward
target offsets when it reaches the dispatch. Instead of estimating or
emitting twice, the table is *deferred*: the dispatch is emitted with
three placeholders and the table is written after the function's last
island, when every label is bound and no branch can be chomped. Then each
table gets the narrowest width for its exact span, relative to its lowest
target:

```
aarch64:  cmp; b.hs dflt; csel; adr x1, T; ldrb/ldrh/ldr w2, [x1, w2, uxtw #s]
          adr x1, lowest_target; add x1, x1, x2, lsl #2; br x1
x86-64:   cmp; cmov; lea r1, [rip+T]; movzbl/movzwl/movl r2, [r1+i*w] (NOP-padded)
          lea r1, [rip+lowest_target]; add r1, r2; jmp *r1
```

One instruction more than stock per dispatch; tables leave the hot code
path (they sit after the function, not between blocks). The dispatch size
is fixed before the width is known, so nothing moves. The one hazard is
reach: `adr` covers ±1 MB, so `VCode::emit` hands the buffer, per block,
a sound upper bound on everything still to be emitted (worst-case inst +
veneer + constant re-emission per inst and regalloc edit, epilogues, 4 B
per CFG edge, padding). A dispatch defers only when its placeholders are
provably in range; past that the stock inline table is used. On x86-64
`lea`'s ±2 GB reach makes that moot, and big tables get 4-byte entries.

Not done: a separate read-only section. It would need a relocation per
table (or an `adrp` pair) to buy what end-of-function placement already
buys (no table bytes inside hot code). `__const` vs `__text` is accounting.

**Table vs chain (`PLIRON_JT_SHAPE=1`, `brchain::run_with`).** Static
rule: count *clusters* (maximal runs of consecutive indices with the same
non-default target) instead of live entries. A cluster is one range test
(`isub` + `icmp ule` + `brif`); with narrow tables a dispatch is ~8
instructions + n bytes, so <= 4 clusters becomes a chain (stock brchain
keys on <= 4 live entries, so `[A A A A _ _ B B B _ _ A]` stayed a table).
With a profile (`PLIRON_PGO_USE`, block counts via `Pipe.prof`):

- chain tests are ordered hottest first;
- **hot-case peeling**: if one target takes >= 60% of >= 64 dispatches
  and costs <= 2 range tests, test it first and keep the table for the
  rest.

Edge counts come from the target's block count only when the dispatch
block is its sole predecessor; otherwise the target is "unknown" and
never drives a peel. No bit-test lowering: the data has 6 tables where it
would win on size, ~0.3 KB in total.

## Numbers

regex-syntax `__text` (aarch64; `sz/build.sh`, release dylib):

| config | `__text` B | vs base | `br xN` |
|---|---|---|---|
| base | 401,872 | | 79 |
| `PLIRON_JT_NARROW=1` | 395,664 | -6,208 (-1.5%) | 79 (60 1-byte, 19 2-byte) |
| `PLIRON_JT_SHAPE=1` | 400,048 | -1,824 (-0.5%) | 58 |
| both | 394,576 | -7,296 (-1.8%) | 58 |

LLVM is 184,752 B. x86-64 sizes are not measured here (the dylib is built
`host-arch`); the x86-64 encoding is tested under Rosetta (below).

(Rebased tree: base 401,680, the same deltas.) On top of the other
size flags of the day (`PLIRON_COLDCOPY=1 PLIRON_NARROWEXT=1
PLIRON_EDGEDEDUP=1`, base 377,100): narrow -6,464, shape -896, both
-6,560 (-1.7%). Edge dedup shares split-edge blocks between equal table
entries, so fewer distinct targets are left for shape to win on; the two
compose (shared edge blocks are just labels to the deferred table).

Runtime, regex-syntax parse + translate + print probe (µs per pass over 16
patterns; 31 interleaved rounds under the shared heavy-work lock, load
average ~24; order alternates per round):

| build | median | min | max | median vs base |
|---|---|---|---|---|
| base | 208.0 | 196.3 | 398.0 | |
| narrow | 214.1 | 199.2 | 402.0 | +2.9% |
| shape | 204.5 | 197.3 | 353.1 | -1.7% |
| both | 206.4 | 196.8 | 472.5 | -0.8% |
| PGO, no JT flags | 212.3 | 203.1 | 511.4 | +2.1% |
| PGO + shape | 210.7 | 201.8 | 432.8 | +1.3% |
| PGO + both | 212.2 | 203.3 | 314.7 | +2.0% |
| LLVM | 183.7 | 176.1 | 271.1 | -11.7% |

A second run on a quieter machine (61 rounds, load ~9, max/min 1.1x):

| build | median | min | max | median vs base |
|---|---|---|---|---|
| base | 199.2 | 194.7 | 216.8 | |
| narrow | 201.6 | 195.5 | 219.3 | +1.2% |
| shape | 200.2 | 193.9 | 220.1 | +0.5% |
| both | 201.4 | 195.8 | 220.2 | +1.1% |

The first run's spread is 2x, so read only the signs. The one consistent
sign over four runs (+4%, +1.2%, +2.9%, +1.2% medians; +0.4–1.5% minima)
is that `narrow` is about 1% slower than inline tables: the table sits on
a line at the end of the function, while an inline table shares lines
the front end already fetched, and there is one more `adr`. `shape` is
neutral (-1.7%, +0.5%). PGO + shape vs PGO alone is -0.7%, but PGO itself
does not help this probe. The PGO flow was docs/pgo.md's: `PLIRON_PGO=1` build of both crates
with the thinned runtime, 300 training iterations, `pliron-prof merge`,
`PLIRON_PGO_USE` builds (every CGU matched, 0 stale).

The `bench/run.sh` kernels have no jump tables left in pliron's output
(brchain already turns their small switches into chains), so they cannot
move; the runtime probe is a regex-syntax parse/translate/print driver
(scratchpad `rx.rs`, 16 patterns), every crate built by the backend under
test.

## Tests

- `pliron-clif-opt/tests/jt_narrow.rs` (+ `jtcommon/`): compiles CLIF for
  the host, maps and runs the machine code, and decodes the emitted
  dispatch: width steps 1 -> 2 exactly at 255 units (a mutation to `< 0xff`
  fails it), wide 2-byte table, backward + aliased (jump-only) targets,
  two tables in one function, big functions (aarch64: falls back to
  inline; x86-64: 4-byte entries). `jt_narrow_w4.rs`: forced 4-byte
  entries (`PLIRON_JT_WIDTH=4`). All pass on aarch64 and on x86-64 under
  Rosetta (`cargo test --target x86_64-apple-darwin -p pliron-clif-opt
  --test jt_narrow`).
- `pliron-clif-opt/tests/jt_shape.rs`: cluster chains, nonzero-start
  cluster with wraparound, no-op without clusters, peel and no-peel
  (share, count, multi-pred target), profile-ordered short chain; each
  rewrite runs natively against the original over every nearby index and
  the extremes.
- The 2-byte -> 4-byte boundary (65,535 units = 256 KB on aarch64) is not
  reachable in deferred mode: the reach bound rejects functions that big.

- Fuzz with both flags on, seeds 10400:10700: 299 pass, 1 compile-timeout
  (seed 10491, a 371 KB program; it times out with the flags off too).
- `harness/gate.py`: passed both plain and with `PLIRON_JT_NARROW=1
  PLIRON_JT_SHAPE=1`.

## Next

1. Flip `PLIRON_JT_SHAPE` (smaller, runtime-neutral) after a crates run.
   `PLIRON_JT_NARROW` is the size win but costs ~1% here. To win that
   back, flush a table at the first island or block boundary after its
   dispatch's targets are all bound, rather than at the function end
   (same deferral, earlier flush point; tables near hot code), or keep
   profile-hot dispatches inline.
2. Traps after noreturn calls: drop the `udf` unless the call ends the
   function (LLVM's `NoTrapAfterNoreturn`), ~6 KB here.
3. `dense_switch` sees clusters too: emit range tests at lowering instead
   of building a table brchain later takes apart, and let the profile pick
   the case order there (pliron IR edge counts are exact; CLIF block
   counts are not after splitting).
4. The `csel` Spectre clamp in aarch64 `JTSequence` (LLVM has none for
   native Rust) costs one instruction per dispatch.
