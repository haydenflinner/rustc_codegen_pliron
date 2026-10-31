# wasm runtime status, 2026-10-21

> Moved verbatim from `docs/PARITY.md` on 2026-10-30. Superseded; the current status page is [`../PARITY.md`](../PARITY.md).

## 2. wasm runtime

**2026-10-21** (`bench/wasm/run.mjs --layouts=3`, geomean 0.900). Measure
before you cut: two traps cost this session hours.
- **Code placement.** V8 lays out TurboFan code in tier-up order, and a
  hot loop's address alone swings a tight kernel up to ±25%:
  `chars_alpha` timed 4.0 vs 5.2 ms on byte-identical machine code
  (`--print-wasm-code` diff). `--layouts=N` re-instantiates and tiers up
  other kernels first; trust the median and look at the range.
  `parse_only` ranges 9-19% on code that didn't change.
- **std partitioning.** The LLVM column links rustup's std, built with
  `codegen-units-std = 1`; ours came from `-Zbuild-std` at the release
  default of 16, which leaves non-`#[inline]` helpers like `Big::cmp`
  out of line across CGUs. `bench/wasm/run.sh` now builds std at one
  CGU (strwork +22% → +15%).
- Tools in the session scratchpad that are worth re-creating: V8
  `--prof` ticks mapped onto `--print-wasm-code` listings (per-instruction
  heat), `--cpu-prof` per-function self time, a `panic_bounds_check`
  `Location` decoder over the data segments.

Full tried/rejected list: `docs/history/2026-10-21-wasm-experiments.md`; tools: `harness/v8/`.

Landed: `punroll` deepest-first (`PLIRON_PUNROLL_DEEP`: format_exact's
bignum `mul_small`/`sub` loops never got the 8-transform budget), waffle
`treeify` loop guard (`PLIRON_WTREE_LOOP`: single-use invariants licm
hoisted were re-emitted at their in-loop use — hmget +14% → +4%),
`bittab` i32 bit test (`PLIRON_BITTAB32`), `zext_chain`
(`PLIRON_ZXCHAIN`: hmget +4% → +2%). Measured and rejected: GOT
`global.get` → `i32.const` (binaryen `--simplify-globals`, flat); the
`PLIRON_INLINE_LOOPY` size rule (libcore −13% but strwork +28%, kept
opt-in); `PLIRON_WASM_INLINE=0`/`PLIRON_WASM_PEEP=0`/`PLIRON_DEM64=0`
(strwork flat).

| kernel | pliron/LLVM | lead |
|---|---|---|
| fib | **2.01** | Recursion turned into a loop by both, but our loop body is an `if/else` + `br` with 5 locals vs LLVM's single `br_if` with 2 (`wasm2wat` the `fib3fib` function in both modules). Shared cause below: the loop's `brif` lowers to `if c { get*;set*; br L } else { ret }` — the param copies sit in scratch locals inside the `if` arm where `wpeep`'s `pathcheck` (straight-line rule) can't retarget them, while LLVM updates `n`/`acc` unconditionally (`local.tee`) then `br_if`s. **DONE** (`PLIRON_WASM_BRF=0` off): `wpeep` now folds `if c { get*/set*/tee*; br L } [else { R }] end` → hoisted copies + `br_if` (depth−1), guarded by arm-purity, an operand-stack balance check (a `set`/`tee` popping the cond or a nonzero net depth rejects), no `Get`/`Nop` read of a copied local in the else arm or later, no depth-referencing/structured op in the else arm, `prel` empty. Unit tests `backedge_if_*`. NOTE: produces LLVM's shape + ~8B/fn smaller, but **V8-runtime-neutral on this box** (both control forms compile to the same work) — the residual gap is op *count*, below. |
| chars_mixed | ~1.4 | **partially DONE**: `NonNull`-i32 (`PLIRON_NN_ANYINT`) folds `eqz`-through-`phi`/`select`/`nonnull` → the `ptr==end`+`byte_ptr==0` `Option`-niche double-check is gone; `dem64` (`PLIRON_DEM64`) demotes `Option<u32>`/`usize`/`char` low-32 i64 webs → the `i64.extend; i64.and 128; i64.ne`→`i32` per-char overhead is gone (`chars_ascii` `i64` refs 23→0, +35→+9%). Residual = `next_code_point`/UTF-8 multi-byte decode (`load offset=1..3`, `&63`, `shl`, `or`, `ge_u` branches — LLVM's `byte<0?decode:cp<128` path is tighter) + locals churn. |
| parse_only | ~1.10 | 2026-10-21: TurboFan code matches LLVM's per instruction (7 per digit); the residual is ~2 extra *taken* branches per word — cold `len == 0`/`len == 1` arms sit inline so the hot path jumps around them. V8 schedules blocks itself — swapping wasm `if` arms (a `brif (x==K)` → `brif (x!=K)` heuristic, tried and reverted) left the machine code's layout unchanged and cost +12 pts. The real difference: our cold arm carries block-arg moves (`i32.const 1; local.set; br`), so V8 emits an edge block with moves inline after the branch; LLVM's cold edge is a bare `br_if` with nothing to move and lands far away. Levers: no moves on cold edges (pass the `Err` flag some other way), or wasm branch hints (`--experimental-wasm-branch-hinting` is on in node 26) marking the arm deferred. Earlier: **SwitchOp on wasm** (`PLIRON_SWITCH_WASM`, `builder.rs`): the "wasm.rs has no SwitchOp lowering" exclusion was stale — `dense_switch`→`br_table`/`ifconv` select chains work; the `from_str_radix` digit `match` became selects, +31%→+9%. |
| tokenize | ~0.67 | ahead |
| hmget | ~1.02 | 2026-10-21: waffle `treeify` was re-sinking the hoisted SipHash invariants into the probe loop (`PLIRON_WTREE_LOOP`), and `dem64` left the control byte as wrap/and/extend (`PLIRON_ZXCHAIN`). Earlier, **partially DONE**: `licm`'s `deref_fact` (`PLIRON_LICM_PEEL=0` off) peels a scope-named load address (`v37 = v0 + 16`, the inlined `hash_one` `&RandomState` operand) to the entry param and borrows its `dereferenceable`+readonly fact — the per-probe `k0` reload and the key-only half of the first SipHash round now hoist like LLVM's. `clobbers` keys disjointness on the concrete object so `iso` can't hide a scope-name/param overlap (`licm_frozen_region` regression). Residual: probe-loop `local.*` traffic ~1.4× LLVM's — emit-level density, below |
| hashmap | ~1.03 | at parity |
| strwork | ~1.14 | Native is 0.97 — the loss is wasm-only. 2026-10-21 per-function V8 profile: `format_exact` + bignum helpers 1.44× LLVM's ticks (~85% of the gap) before the std-CGU fix; profile is flat (no instruction >1%) with ~1.4-2.9× TurboFan code per function and stack reloads (FP/int pressure). Bounds checks: 27 `bignum.rs:390` sites survive in `format_exact` vs LLVM's 4 (e.g. `mul_pow2`'s copy loop keeps `i < 40` and `i + digits < 40`). Not the GOT, not the waffle inliner, not wpeep. After the std-CGU fix (+14%): `Big::cmp` loops are hot by load latency but run 1-2 limbs per call — `zipiv` (`PLIRON_ZIPIV=1`, opt-in) cut their per-limb ops ~2× (one exit test, no `select`s) with no runtime change. Both `size` fields stay in memory in LLVM too. Tick-to-wasm attribution (V8 `--prof` + TurboFan source positions) is flat; no single construct explains the rest. |
| fib | ~0.96 | ahead post `br_if` fold + `dem64`/`NonNull` |
| nbody | ~1.04 | Fully unrolled in both; ours does *less* work (53 vs 65 `f64.add`, same final stores) but TurboFan spills (`ldr d17, [sp, #24]`): ~30 f64 values live across the 10 unrolled pairs. Ordering/pressure, not op count. |
| sieve, sortw | ~1.0 | at/near parity post `dem64`/`NonNull` (sortw 0.9%, sieve 2.5%) |
| chars_alpha, iterw, matmul, eqbug, contains_rng | <1.0 | ahead |
| tokenize 0.66, iterw 0.70, contains_rng 0.78, hashmap 0.88, sortw 0.90, fib 0.95, chars_ascii (was 0.57 → now ~0.92) | | ahead |

`Option`-niche residual: the `NonNull`-i32 widening doesn't reach iter pointers that **round-trip through stack slots across calls** (`tokenize`'s `Split` spill/reload) — the reload isn't `!nonnull`-marked and the spilled value's arithmetic isn't in `derived`, so `ptr` can't be proven there. That needs nonnull facts carried through our own slot demotion — a separate, larger change.
| matmul 0.74, iterw 0.79, tokenize 0.79, chars_alpha 0.82, eqbug 0.88 | | ahead |

`black_box` on wasm is a stack store + reload under both backends (LLVM
does the same), so it is not a source of the gap — don't "optimize" it
to an identity; that would only make the benchmarks lie.
Module size (1.95x) is **op density**, not GC: the whole module runs
~2.3× LLVM's op count (`local.get/set/tee` 3.4×/2.4×/2.9×, `br` 6.8×,
`if`/`else` 2746 pairs vs LLVM's 0 — we emit structured `if` diamonds
where LLVM emits flat `block`+`br_if`), and `compute/mem` itself is
1.9×. Reachable-but-cold bodies are all data/vtable-rooted. wasm-opt
-O3 recovers −28% (189.7→136.4 KiB); `coalesce-locals` alone −13 KiB.
Remaining systematic anomalies vs LLVM: `global.get` 7.6× (GOT entries
pliron-wasm-ld synthesizes — LLVM writes resolved `i32.const`
immediates into the code section; folding ours post-link would *grow*
the module), `return` 6.4× (early returns vs joined epilogue),
`br_table` 0 vs 17 (SwitchOp now reaches wasm via `dense_switch`; the
CLIF pipeline if-converts most of them to select chains before emit),
`i32.and`/`i32.shl` ~3.2× (incl. ~44 dead `&31`/`&63` shift masks —
wasm shifts already mask). wpeep `ddrop` (`PLIRON_WASM_DDROP`, default
on) removed ~480 dead `drop`s; `brf` folded ~450 `if{…;br}` sites
(−2.5 KiB); `delse` (`PLIRON_WASM_DELSE`, default on) flushes dead
else arms `if{A}else{brK}`→`if{A}` (~56 sites, −315 B); `bri`
(`PLIRON_WASM_BRI`, default on) flattens `if{brK}else{A}`→`br_if;A`
(~116 sites — the arm1 `br`s it catches are `WPeek::Br` markers
synthesized by `brf`'s own retargets, which brf can't refold); `btr`
(`PLIRON_WASM_BTR`, default on) deletes `br K` tail-branches that fall
through their K+1 enclosing `end`s anyway, incl. unreachable `br`s
(~570 sites, −640 B). btr's reachability model keeps three states per
inst — dead-region-open / live / live-unknown-depth — after a
miscompile where a boundary `end` resuming with an unknown base marked
a live `loop` back-edge "dead" and folded it (wasmstdtest words trap).
`bri2` (`PLIRON_WASM_BRI2`, default on) is `bri`'s mirror:
`if{A}else{brK}`→`i32.eqz;br_if(K-1);A` (~380 sites, −882 B); the fold
emits a synthesized `WPeek::Eqz` (pop-1/push-1 in every `ins` model)
and re-runs to a fixpoint (≤8 rounds) because a fold's kept arm is not
rescanned for nested candidates. The CLIF pipeline now also skips
`brchain` on the wasm path (`p.wasm`): the compare-chain conversion
exists because native `br_table` lowers to an indirect branch, but
waffle emits the real wasm `br_table` — 15 tables now survive to the
module vs 0 before. Remaining `if`/`else` pairs ~2,300 vs LLVM's 0 —
the residual is merge-param materialization: waffle's relooper puts
block params through scratch locals at `if`/`else` boundaries (ours:
~36k local ops; LLVM: ~14k, its `br_if`-only shapes keep values on the
operand stack). Next levers: deeper `if`→`br_if` restructuring in
stackify `if`→`br_if` (LLVM has 0 if/else vs our ~5.7k; naive rewrite
grows CODE — needs wpeep-aware emission). `PLIRON_WCOAL_HOLES` is
default-on (kernels locals 764→636). Opcode gap on regex-wasm: local.set
~5×, local.get ~2.3×; no-inline floor ~1.25× LLVM post-WTREEREMAT.
`PLIRON_WIFRET` (default on) promotes same-merge `BlockParams` if-arms
to typed `if (result T)` + one post-if set (−1.1 KiB). Lowering
`WASM_LIMIT` 512→128 shrinks to ~1.40× but regresses chars/parse/fib.
`PLIRON_WASM_ELIDEBLK` (default on) deletes untargeted empty `block`/
`loop` pairs under a fact audit (`wpeep_elide`: assume_untargeted /
empty_type / wellformed; revert on fail) → regex-wasm **373 KiB
(1.78×)**. Multi-value typed if is wired (`PLIRON_WIFRET_MULTI=1`) but
default-off (grew CODE). Next: wpeep-friendly CondBr→br_if; block-typed
merges for Br-depth arms; make multi-WIFRET win.
