# Why cranelift-interpreter is vendored

Vendored from crates.io cranelift-interpreter 0.135.5 because its `step`
matches `Opcode` exhaustively, and the vendored cranelift-codegen adds
`x86_psadbw`, `ctz_nonzero`, `sdot`/`udot`/`udot_acc` and `vswizzle`/`vswizzle2`.
Changes, all in `src/step.rs`:

- arms for those opcodes (marked `PLIRON:`); the dot-product arms
  compute the pairwise/quad sums lane-wise, 128-bit only like the
  rest of this interpreter;
- `bitcast` between a scalar and a vector, or between vectors with different
  lane counts, now reinterprets bytes (marked `PLIRON PATCH:`). Upstream
  returned `bitcast.i64x2 (i128)` as a single i64 scalar, which made the
  differential checker report a false miscompile in `vecslot`.

Used by `pliron-clif-opt`'s differential checker (`src/diffcheck.rs`).
Drop this copy once those opcodes are upstream or no longer vendored.
