# Vendored benchfmt.rs

Upstream: https://github.com/bwplotka/benchfmt.rs
Pinned rev: f9c07f05c28a5c7c0fc5068fd83374525fd601c4 (main, 2026-11)

Used by `bench/benches/kernels.rs` (`benchfmt::Suite` +
`CountingAlloc`, `pprof` feature) so the native kernels emit Go `benchfmt`
epochs for `benchstat` instead of criterion's linear-ramp samples.

Local patches (marked `PLIRON:` in source):

- `src/lib.rs` `name_matches_filter`: `-bench`/positional filter matches
  case-insensitively and splits `|` as alternation, so `-bench fib` hits
  `BenchmarkFib` and `-bench 'sieve|hmget'` works like a Go regex.
