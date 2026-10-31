# Why regalloc2 is vendored

Vendored from crates.io regalloc2 0.15.2. Changes, each marked `PLIRON:`: the
spill-priority change below (default on), and the runtime knobs
`PLIRON_RA2_XBLOCK` and `PLIRON_RA2_MERGEDEPTH` (default off; sections
further down). The spill priority: in `src/ion/process.rs` it divides the spill weight by
`4 * isqrt(len)` instead of `len`, so long-lived values with many uses (e.g.
`self` across a large inlined lexer function) keep a register. Build with
`RA2_LINEAR=1` in the environment to get upstream behavior back.

Measured (codegen-improvements.md): rustc_lexer `Cursor::advance_token` stack
references 292 -> 91; stage2 regex-syntax instructions -0.2% / -0.3%.

TODO: once proven out, submit upstream (bytecodealliance/regalloc2) and drop
this copy plus the `[patch.crates-io]` entry in the root Cargo.toml.

## `PLIRON_RA2_XBLOCK=1` (runtime, default off): redundant moves across a call's return

`src/ion/moves.rs`: the redundant-move eliminator clears its state at
every block boundary. Under the flag it keeps the state when the next
block's only predecessor is the block right before it (Cranelift's
`try_call` return blocks); the instructions in between still clear what
they define or clobber. That elides a reload's store back into its own
spill slot after the call, and other moves whose source and destination
already hold the same value. `src/lib.rs` links `std` under the default
`std` feature for the env read. regex-syntax: `__text` -2,272 B,
spill stores -115 (docs/history/2026-10-27-spills2.md). Checked with
`PLIRON_RA_CHECK=1` (the regalloc2 checker).

## `PLIRON_RA2_MERGEDEPTH=1|2|3` (runtime, default off): loop-depth merge order

`src/ion/merge.rs`: blockparam merging normally walks the edges in order.
Under the flag it merges the edges of the deepest loops first (by
`approx_loop_depth`; ties: `1` keeps edge order, `2` shortest live range
first, `3` longest first), so a loop-carried value
shares the parameter's register and the remaining move lands on the
preheader edge instead of the back edge.
