# Why cranelift-interpreter is vendored

Vendored from crates.io cranelift-interpreter 0.135.5 because its `step`
matches `Opcode` exhaustively, and the vendored cranelift-codegen adds
`x86_psadbw` and `ctz_nonzero`. Changes, all in `src/step.rs`:

- arms for those two opcodes (marked `PLIRON:`);
- `bitcast` between a scalar and a vector, or between vectors with different
  lane counts, now reinterprets bytes (marked `PLIRON PATCH:`). Upstream
  returned `bitcast.i64x2 (i128)` as a single i64 scalar, which made the
  differential checker report a false miscompile in `vecslot`.

Used by `pliron-clif-opt`'s differential checker (`src/diffcheck.rs`).
Drop this copy once both opcodes are upstream or no longer vendored.
