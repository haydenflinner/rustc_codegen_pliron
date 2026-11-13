# 2026-11-12: PGO baseline on the held-out programs

`bench/heldout/run.py --pgo` (new): per program, each side profiles one
run of the benchmark and rebuilds with the profile — ours with
`PLIRON_PGO` / `pliron-prof merge` / `PLIRON_PGO_USE`, LLVM with
`-Cprofile-generate` / `llvm-profdata merge` / `-Cprofile-use`. 7
interleaved rounds, quiet machine (load ~3), backend 705dc5e + runner.

| | ratio | 95% interval |
|---|---|---|
| pliron / llvm | 1.089 | [1.081, 1.118] |
| pliron-aa / pliron (noise floor) | 0.995 | [0.974, 1.006] |
| **pliron-pgo / pliron** | **1.005** | [0.980, 1.032] |
| llvm-pgo / llvm | 0.922 | [0.904, 0.943] |
| pliron-pgo / llvm | 1.095 | [1.078, 1.130] |
| pliron-pgo / llvm-pgo | 1.188 | [1.168, 1.215] |

Over 6 of 8 programs (two fail under our profile, below). Our whole
profile loop (instrumented build + one run + profiled build) costs
2.59x a plain LLVM build.

Per program, pgo/pliron: blake3-scalar 0.991, pulldown-cmark 0.999,
regex 1.039*, rust-html-rewriter 0.944*, rust-json 1.049,
rust-protobuf 1.013. LLVM's own profile, llvm-pgo/llvm: about 0.98
(blake3), 0.85 (pulldown-cmark), 0.96 (regex), 0.83 (html-rewriter),
0.94 (json), 0.99 (protobuf).

So today our profile is matched (2,098 functions on pulldown-cmark,
9,633 on regex, none stale) and buys nothing, while LLVM gets 8% from
the same single run. The consumers that exist (cold layout, the
inliner's cold-site reject and warm bonus, branch expectations, loop
trip estimates, switch ordering) are not where the time is.

Failures under `PLIRON_PGO_USE`:

- rust-compression: build panics, `cranelift rejected ...
  brotli::enc::block_splitter::FindBlocks: should be implemented in
  ISLE: ineg.i32x4` — the profile sends a loop down a `loopvec` path
  that emits a vector `ineg` the aarch64 backend has no rule for.
- blind-sig: the profiled binary dies with SIGBUS (the instrumented one
  runs).
