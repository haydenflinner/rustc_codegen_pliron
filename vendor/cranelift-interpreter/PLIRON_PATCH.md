# Why cranelift-interpreter is vendored

Vendored from crates.io cranelift-interpreter 0.135.5 because its `step`
matches `Opcode` exhaustively, and the vendored cranelift-codegen adds
`x86_psadbw`, `ctz_nonzero`, `sdot`/`udot`/`udot_acc` and `vswizzle`/`vswizzle2`.
Changes, all in `src/step.rs`:

- arms for those opcodes (marked `PLIRON:`); the dot-product arms
  compute the pairwise/quad sums lane-wise, 128-bit only like the
  rest of this interpreter;
- `vgather`/`vscatter` arms (marked `PLIRON:`): per-lane
  `p + idx[i]*E + off` loads/stores via `checked_load`/`checked_store`,
  sign-extended index math like HW;
- `bitcast` between a scalar and a vector, or between vectors with different
  lane counts, now reinterprets bytes (marked `PLIRON PATCH:`). Upstream
  returned `bitcast.i64x2 (i128)` as a single i64 scalar, which made the
  differential checker report a false miscompile in `vecslot`.
- `calculate_addr` sign-extends the load/store offset immediate to the
  address type (marked `PLIRON:`). Upstream zero-extends it, so a pass
  folding `iadd base, -K` into `load base-K` computed `base + 0xFFFFFF0`
  and produced bogus `heap_oob` traps / wild writes in the differential
  checker. Real codegen sign-extends Offset32. (`stack_addr`'s offset
  path via `sum_unsigned` shares the latent issue; nothing in our
  pipeline emits negative stack-slot offsets yet.)

Used by `pliron-clif-opt`'s differential checker (`src/diffcheck.rs`).
Drop this copy once those opcodes are upstream or no longer vendored.
