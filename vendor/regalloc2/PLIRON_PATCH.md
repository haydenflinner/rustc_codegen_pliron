# Why regalloc2 is vendored

Vendored from crates.io regalloc2 0.15.2 for **one** change only: in
`src/ion/process.rs` the spill priority divides the spill weight by
`4 * isqrt(len)` instead of `len`, so long-lived values with many uses (e.g.
`self` across a large inlined lexer function) keep a register. Build with
`RA2_LINEAR=1` in the environment to get upstream behavior back.

Measured (codegen-improvements.md): rustc_lexer `Cursor::advance_token` stack
references 292 -> 91; stage2 regex-syntax instructions -0.2% / -0.3%.

TODO: once proven out, submit upstream (bytecodealliance/regalloc2) and drop
this copy plus the `[patch.crates-io]` entry in the root Cargo.toml.
