# 2026-11-16 overnight wasm parity session

Running log. Newest decisions that want a second look are under
"Review these".

## Review these

1. **sccache made rebuilds stale (found 02:05).** `~/.cargo/config.toml`
   got `build.rustc-wrapper = "sccache"` on 2026-11-14 22:30. sccache
   keys on rustc's arguments and inputs; the backend dylib behind
   `-Zcodegen-backend` and every `PLIRON_*` variable are not in the key.
   A rebuild into a target dir path that was used before returned the
   objects of the earlier build. Seen directly: `variant.sh uear` gave
   the same module before and after a backend change, `variant.sh uear2`
   (new path) gave the new one. `bench/wasm/run.sh` always builds into
   `target/wasm-bench`, so every run.sh after 11-14 22:30 could have
   timed an old backend. Variant names that were reused have the same
   problem (`half` built with `PLIRON_HALFUNROLL=1` was byte-for-byte
   the default body of `lookup_slow`). Numbers recorded between
   11-14 22:30 and this note should be re-measured before they are
   trusted. Fix: `.cargo/config.toml` in the repo sets
   `rustc-wrapper = ""` (verified: no `sccache` in `cargo build -v`).
   Builds outside the repo (`~/code/dsa`) need `RUSTC_WRAPPER=` in the
   environment.
2. **I wiped `target/` by accident (02:07).** A scratch `cargo init`
   under `target/` joined the workspace, and `cargo clean` in it removed
   the workspace target dir. Build artifacts only: backend builds,
   recorded corpora (`target/corpus/*`), held-out build caches, earlier
   agents' `target/v8prof` variants and `target/wasm-ab-*` dirs. The
   `Cargo.toml`/`Cargo.lock` edit `cargo init` made was reverted.
   Corpora were re-recorded (see below).
3. **`corpus:clcg` has one mismatch under `gate.py --full` (found
   02:58, not from tonight).** The default gate replays every 8th clcg
   function and is green; the full replay reports
   `..cranelift_codegen3isa7aarch644inst4emit.._8f1cba34.2629.clif`.
   It reproduces with `clif-check report <dir> out.json --check` on
   that one file, at `b903d33` too, and a `PLIRON_PIPELINE` prefix
   bisect puts it at `slotmerge` (prefix of 81 steps). Calls are
   identical; "before" writes 11 zero bytes at `arg3+0x44..0x6c` that
   "after" does not. The known list has the same step as "undef-byte
   reshuffle" noise, but those entries say the writes match, and here
   they do not. Not triaged further. The clcg corpus was re-recorded
   tonight, so the file name's crate hash is new.
4. **`willret` was red on main before tonight** (`b903d33`): fixed in
   `06626f0` (see below). The gate runs that passed it were presumably
   stale objects from sccache.
5. **tokenize gives back ~3 points** (-17.5% -> about -14.5%, still
   ahead of LLVM) with the new defaults. `PLIRON_UNREACH_PHI` and the
   assume folding each cost about 2.5 points on their own (`kphi`,
   `kar` variants), though both only remove work from the multi-byte
   decode arm. Not explained; the wasm for `tokenize::run` is in the
   log below. Left on because chars_mixed (+19.9% -> -1.4%) and the
   module size depend on the same two knobs.

## What landed

| commit | what |
|---|---|
| `06626f0` | punroll: the peel's `full_unroll` deleted the residual loop on `trip_max`, a cost estimate (unknown start read as 0, `!=` bounds a stepping counter can skip). `while i != 0 { i += 2 }` lost its loop. `loopidiom::trip_max_exact` is the bound that may delete code. |
| `c29809a` | `.cargo/config.toml`: no rustc wrapper inside the repo. |
| `aa5dbf3` | `PLIRON_UNREACH_PHI`, `PLIRON_ASSUME_RANGE` as a marker op + `PLIRON_UNREACH_EARLY` (`vrp0`, `unreach0`), `PLIRON_UNROLL_SYM`; all opt-in in that commit. |

## Findings worth keeping

- **V8 inlines wasm functions itself, on a budget that depends on the
  whole module.** `fib` went from -7% to +97% between two builds whose
  `fib` body was byte-identical. `--trace-wasm-inlining` shows why:
  TurboFan inlines `fib` into itself once when `2 * size` fits the
  budget, and the budget is `factor * size` with a factor that rises
  with the module's share of small functions (LLVM's module 2.2, ours
  2.06, the regressed build 2.0: `budget=min(154, 600), size 77->154
  not enough inlining budget`). Two helpers that stopped being inlined
  by *our* inliner moved the share from 37.6% to 36.8%. Consequences:
  a kernel can flip by 2x on an unrelated change in function count;
  our `fib` is 77 wire bytes against LLVM's 59; and a module that
  ships more small functions gets more engine inlining. Check
  `node --trace-wasm-inlining` before believing a large swing on a
  call-heavy kernel.
- **An assume has to be invisible to the IR passes.** As a `cond_br`
  in the frontend it changed the inliner's op counts (flat 40-op limit
  on wasm) and the function set. As one `llvm.assume` marker that is
  free in `callee_info` and lowered to a branch in `lower.rs`, the
  function set is identical to a build without it.
- **`zipiv` and `cmp3` do nothing on the wasm bench** (byte-identical
  modules with the knob on). `revcmp` moves strwork by about one point.
- strwork's gap is all in `dragon::format_exact` (ours 5686 wat lines
  against LLVM's 2600 plus an out-of-line `mul_pow2`). LLVM's limb
  compare is `load a[off]; load b[off]; br_if ne; off -= 4; br_if`;
  ours walks two pointers with two end tests and builds an `Ordering`
  it then re-tests.
