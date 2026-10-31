# Experiment ledger

What was tried, what it measured, and where it stands — so the next
attempt starts from the result instead of re-running it. PARITY.md is the
scoreboard; this is the lab notebook. Newest first. Numbers are
pliron/LLVM deltas from `bench/wasm/run.mjs --layouts=3` unless noted.

Status: **landed** (default on, `PLIRON_X=0` off) · **opt-in** (kept,
default off) · **rejected** (reverted or env-only) · **finding**
(measurement, no code).

## 2026-10-21 — wasm runtime (geomean 0.925 → 0.898)

| what | status | result |
|---|---|---|
| V8 code placement | finding | `chars_alpha` timed 4.0 vs 5.2 ms on **byte-identical** TurboFan code; tiering any other kernel first moves it. `run.mjs --layouts=N` added. `parse_only` ranges 9-19% on unchanged code. Always `harness/v8/mcode.sh` both modules before believing a delta. |
| std at 1 CGU in `bench/wasm/run.sh` | landed (harness) | The LLVM column links rustup's std (`codegen-units-std = 1`); ours was `-Zbuild-std` at 16 CGUs, leaving `Big::cmp` out of line in `format_exact`. strwork +22% → +15%. |
| licm `deref_fact` scope-name peel (`PLIRON_LICM_PEEL`) | landed | hmget's SipHash `k0` reload + key-only half-round hoist. hmget +18% → +14%. |
| waffle `treeify` loop guard (`PLIRON_WTREE_LOOP`) | landed | Single-use invariants licm hoisted were re-emitted at the in-loop use. hmget +14% → +4%, sortw −1.5 pts; iterw +1.3 pts with one op *fewer* in its loop (placement). |
| `punroll` deepest-first (`PLIRON_PUNROLL_DEEP`) | landed | The 8-transform budget went to cold loops; `format_exact`'s bignum `mul_small`/`sub` never unrolled. strwork +30% → +20.5%, +930 B. |
| `PLIRON_PUNROLL_MAX=32` | opt-in knob | strwork +19%, module +8 KB; deepest-first at 8 gets nearly the same for 1/9 the size. |
| `zext_chain` (`PLIRON_ZXCHAIN`) | landed | `uextend.i64` of `ireduce`/`band`/`uextend` chains → one `band` (dem64 residue on hashbrown's control byte). hmget +4% → +2.2%. |
| `bittab` i32 bit test (`PLIRON_BITTAB32`) | landed | `is_ascii_whitespace` per byte 26 → 22 non-local ops; strwork −0.9 pts. |
| `zipiv` lockstep pointer IVs (`PLIRON_ZIPIV=1`) | opt-in | Correct (filetest + corpus), `Big::cmp` per-limb ops ~2× fewer, **runtime neutral**: the cmp loops run 1-2 limbs per call. Worth re-measuring on long equal-slice compares/zips. |
| `brzh`: swap `brif (x==K)` arms (LLVM zero/eq heuristic) | rejected (reverted) | V8 schedules blocks itself — arm order didn't change the machine layout; parse_only +9% → +21%. |
| `PLIRON_INLINE_LOOPY=96/256` (keep big loopy callees out of line) | opt-in knob | libcore −13%, `pad_formatted_parts` 4.9 → 0.9 KB, but strwork +28%/+31% (vs +23%). Code size is not strwork's problem. |
| GOT `global.get` → `i32.const` (binaryen `--simplify-globals`) | rejected | 488 → 68 `global.get`s, all kernels flat. V8 handles immutable globals cheaply. |
| `PLIRON_WASM_INLINE=0`, `PLIRON_WASM_PEEP=0`, `PLIRON_DEM64=0` | finding | strwork flat under each — the wasm-only stages don't explain it (`DEM64=0` helped hmget 2 pts → `zext_chain`). |
| strwork native vs wasm | finding | Native 0.97 (ahead); wasm-only loss. `format_exact` ~1.4× LLVM's ticks, profile flat at instruction and wasm level; 27 surviving `bignum.rs:390` bounds checks vs LLVM's 4. |
| nbody | finding | Ours does less work (53 vs 65 `f64.add`, same final stores) but ~30 live f64 across 10 unrolled pairs spill in TurboFan. Ordering/pressure. |
| parse_only | finding | TurboFan code matches LLVM per instruction; ~2 extra *taken* branches per word because the cold `Err` arm carries block-arg moves (V8 emits the edge block inline). Levers: no moves on cold edges, or wasm branch hints (`--experimental-wasm-branch-hinting` is on in node 26). |
| hmget "merge-param local materialization" (3× `local.*`) | finding (rejected diagnosis) | TurboFan SSA-izes locals; the real costs were the un-hoisted key and treeify's re-sinking. Count non-local ops in the hot loop before blaming locals. |
| strwork "bounds-check splitting at min(len_a, len_b)" | finding (rejected diagnosis) | LLVM keeps the `i+j < 40` check in `mul_digits` too; `mul_pow10` runs once per format. |
| `PLIRON_INLINE_MAXDUP=2` (Devin) | rejected | Module grew to 194 KB, strwork +31.9%. |
| wpeep `if{br}else{br}` same-target fold, `eqz;eqz` fusion (Devin) | rejected | 0 sites; the 10 `eqz;eqz` hits were name-section bytes. |

Tools for this kind of work: `harness/v8/README.md`.
