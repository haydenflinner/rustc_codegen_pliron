# harness/v8 — wasm runtime triage under node/V8

Tools from the 2026-10-21 wasm parity work (`docs/history/2026-10-21-wasm-experiments.md` lists
what they found). All take a built module (`bench/wasm/run.sh` output, or
`variant.sh`), a `k_<kernel>` export, and a function-name substring.

| tool | what |
|---|---|
| `variant.sh NAME PLIRON_X=v…` | build `bench/wasm` with knobs → `target/v8prof/NAME.wasm` (std at 1 CGU, like run.sh) |
| `cpuprof.sh mod k_x iters reps` | per-function self time (`--cpu-prof`); compare ours vs LLVM's module |
| `mcode.sh mod k_x fn` | the function's TurboFan machine code; `diff` two modules to tell codegen changes from placement |
| `hot.sh mod k_x fn iters reps` | per-instruction tick heat on that listing (`--prof`, 50 µs) |
| `hotw.sh mod k_x fn iters reps` | ticks attributed to wasm instructions via TurboFan source positions |
| `place.mjs mod k_x [warm,…]` | time a kernel after tiering up other exports first — code placement probe (keep iters small for slow kernels) |
| `wasmloops.py f.wat` | innermost loops of a `wasm2wat` function: op count, non-local op count, op sequence |
| `bclocs.py mod.wasm mod.wat fn [callee]` | decode each `panic_bounds_check` call's `&Location` (file:line:col) via GOT globals + data segments |

Before trusting a delta: `run.mjs --layouts=3` (placement swings tight
kernels ±25% on identical machine code) and `mcode.sh` both modules.
