# Continuing notes — wasm loop optimizer (wseal leak bug)

## Where we are

Wasm backend performance vs stock LLVM (fresh artifacts, same box, adjacent
runs, `wbench` suite):

    axpy_f32_4M   1.540  (stock 1.759)   0.88x  win
    vadd_u32_4M   1.474  (stock 2.063)   0.71x  win
    sum_f32_4M    3.053  (stock 3.198)   0.95x  win
    dot_i32_1M    0.454  (stock 0.442)   1.03x
    sum_u8_4M     1.216  (stock 1.178)   1.03x
    cnt_ab_4M     1.912  (stock 1.661)   1.15x  worst gap: waffle locals noise
    max_u32_4M    1.245  (stock 1.266)   0.98x
    matmul_256   12.313  (stock 11.281)  1.09x

Native aarch64 `wide` suite: parity or better on every kernel (has_val 0.45x,
dot_u8 0.50x, cnt_vowel 0.26x, find_off 0.07x; scaled/rev_copy32/xor_fold
previously -7/-8% are now 0.91-0.98x). x86_64/Rosetta has known lowering gaps
(AVX2 vs our 128-bit; int min/max reductions, dot_i32) — separate item.

## Committed on branch devin/1791392401-pliron-cranelift-backend

- `2fe5f38` wasm: loop-closed exit sealing + PickOutput cloning for
  wbcheck/unroll. wchk torture: 22 loops versioned (was ~1), outputs identical
  to stock incl. mid-loop panics. All wasm suites green.
- `6503df5` wbcheck (waffle bounds-check loop versioning) + adaptive unroll.
- `2b6bfff` waffle-level loop unroll.
- `7dc1da0` PLIRON_NO_PAIRSTFUSE=1 kill switch (vendored cranelift vcode.rs).
- `a8b7f93` wasm alloca promotion (LocalTracker-style lazy SSA) + waffle
  `optimize()` — const-patch bug fix: patch frame placeholders BEFORE optimize
  (GVN aliases i32const<0> onto frame_c).

## Uncommitted WIP in src/wasm.rs (build clean, untested semantically)

- `wseal_exits` "picks" handling: for `PickOutput(from,idx,ty)` where `from`
  is loop-defined, insert `PickOutput` in each pred and pass its result as the
  edge arg (a param can't replace a multi-result source).
- Verbose instrumentation: `wseal:` prints `leaks={...} picks={...}`; leak
  messages now name the value; exotic insts print their kind.
- `PLIRON_WASM_SEAL=0` env toggle.
- NOTE: `Terminator::update_uses` exists — the manual clone/match/assign in
  wseal_exits (~line 3508) could use it.

## OPEN BUG — the resume thread

After seal commits, `PLIRON_WASM_VERBOSE` builds still show **122**
`skipped leak via terminator` bails. Concrete evidence from a wchk
std-build log:

    wseal: block9 leaks={v187} picks=[]        <- seal saw it
    ...
    wloop: block7 skipped leak via terminator v187 in block9   <- still there

So `wseal_exits` found `v187` in `block9`, ran the rewrite, and `block9`'s
terminator STILL uses `v187` when `loop_cloneable` later checks loop `block7`.
All terminator operand positions are covered by the rewrite (cond, select
value, return values, target args — verified against waffle's `visit_uses`).

Hypotheses, ranked:

1. The seal ran under a *different* loop's `inloop` (v187's def is in a nested
   or sibling loop's block set but not block7's), so the rewrite fired for the
   wrong block set — or `defb[v187]` placement makes it look in-loop to loop7
   while block9 itself isn't sealed for loop7. Next step: print `h` in the
   `wseal:` message and `defb[a]` in the leak message; diff which loop each
   side thinks owns the block.
2. `picks`-style values: a PickOutput in block9's terminator-adjacent path.
3. The `ty()==None -> break` in the leaks loop silently abandoning remaining
   leaks (Alias/None defs resolve to ty None... resolve_alias prevents this
   for direct aliases, but a Placeholder or multi-result op would hit it).

Repro:

    cd /tmp/wchk
    RUSTFLAGS="-Zcodegen-backend=$ROOT/target/debug/librustc_codegen_pliron.dylib -Clinker=$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld" \
      PLIRON_WASM_VERBOSE=1 CARGO_TARGET_DIR=tplvN RUSTUP_TOOLCHAIN=nightly-2026-10-06 \
      cargo build -q --release --target wasm32-unknown-unknown \
      -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem \
      2>&1 | grep -E "wseal|skipped leak"

(ROOT=/Users/wow/code/purerust/rustc_codegen_pliron; cargo doesn't track the
backend dylib — `rm -rf <target dir>` for a clean rebuild.)

## Validation recipe (do before any new commit)

    cd /Users/wow/code/purerust/rustc_codegen_pliron
    bash tests/wasm/run.sh     # core + std + wasi + EH, all must pass
    cd /tmp/wchk && node run.mjs <fresh>.wasm   # must match stock exactly
    ./test.sh                  # smoke suite

## ui-wasm corpus result

- `python3 harness/run.py --suite ui-wasm` completed: **OK, matches
  expectations** (16,508 pass / 4,882 skipped baseline holds; 20 new ICE
  dumps recorded, consistent with baseline's classified env-ice bucket).
  The run built the backend from the working tree, so it validated the
  seal commit plus part of the uncommitted WIP against 21,482 files.

## Test assets on this box

- `/tmp/wchk` — bounds-check torture crate (sumc/over/off/step/nested/two,
  panic-handler = wasm unreachable). `tstk` target dir = stock build.
- `/tmp/wbench` — 8-kernel wasm bench, `target` = stock artifact (28KB, LTO —
  beware stale 1.4MB non-LTO artifacts), `target_pl` = pliron build.
- `/tmp/cpubench` — native `wide.rs` bench + `mk.sh` (pliron rustc wrapper).
- llvm-objdump for wasm:
  `/Users/wow/.rustup/toolchains/nightly-2026-10-06-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin/llvm-objdump -d x.wasm`

## Known structure (src/wasm.rs)

`wloop_opt` pipeline: merge multi-entry headers (`mk_pre`) -> per-loop
[LICM, indvars, seal] -> `wbcheck` versioning -> `wunroll` (innermost only)
-> `wdce`. `defb` is a live PerEntity<Value,Block> def map (cfg.def_block is
stale once insts move — keep it updated on every move/clone). CFG is recomputed
after wbcheck inserts clone blocks.

- Sinks: `unreachable`-terminated pure blocks are cloned per copy
  (`clone_sink`/`cloneable_sink`); other exits must be sealed or the loop
  can't clone.
- Edge args are uses in the EDGE SOURCE block for dominance purposes —
  `loop_cloneable` must check `visit_uses` on outside-block terminators, not
  just inst args. (Fixed in 2fe5f38; was a miscompile.)
- Clone whitelist: Operator/Alias/PickOutput insts; Br/CondBr/Select terms.
- Env toggles: PLIRON_WASM_WLOOP=0, PLIRON_WASM_SEAL=0, PLIRON_WASM_UNROLL=n
  (adaptive, ~8 max), PLIRON_WASM_VERBOSE, PLIRON_WASM_LOOPS=<pat>,
  PLIRON_WASM_TRIP=<n>, PLIRON_WASM_STUBLOG, PLIRON_NO_PAIRSTFUSE=1.

## Other pending avenues (user's roadmap)

- GVN/load PRE (loadfwd.rs exists = load forwarding, no PRE).
- Profitability-aware inlining (inline.rs has fixed 320/40-op budgets).
- IPO attribute inference (nounwind.rs, nowrite.rs exist).
- cnt_ab residual is waffle SSA->locals shuffle noise (every block param and
  multi-use value becomes a local; waffle is a crates.io dep, not vendored).

## Rules reminders

- AGENTS.md here: ask user to review full diff before any push; disclose LLM
  use in PR descriptions; no Co-Authored-By trailers.
- The strict rust/rust-sh AGENTS.md gates apply only to those checkouts.
- Watch box load before trusting timings (selfhost builds saturate it).
