# Coordination ledger (overnight run)

## Merged to devin/1791392401-pliron-cranelift-backend
- e0e2416: keep cranelift host-arch on shared branch (x64 wt keeps all-arch)
- 97e3f0b/b41ba7f/730dcdf (wt/x64-codegen): -Ctarget-cpu→Cranelift ISA flags,
  Mach-O variadic trampolines, Intel-syntax asm stubs on macho. Verified:
  host std diff-clean + unwind; agent verified x64-apple-darwin under Rosetta.
- e89a849 merge: 4f21915 punroll pass (scalar partial-unroll, exit-cone clone,
  PLIRON_PUNROLL=0 off) + 7dd0f49 x64 lowering audit notes. Verified on shared
  checkout: cargo build, std diff-clean, unwind pass, cpubench+wide re-run —
  no regressions; sum2d now parity; clamp earlier-9% deficit was load noise.

## Perf-validated on shared HEAD (M3 Max, quiet box)
- micro: parity across axpy/vadd/clamp/dot_i32/dot_i8; matmul ~1.15x win;
  sum_u8 earlier "2.9x win" was NOISE (stock 0.077, not 0.216).
- wide: find_off 14.3x, cnt_vowel 3.7x, has_val 2.2x, dot_u8 2x; sum2d parity;
  rev_copy32 0.263 vs 0.237 (11% wobble — re-check, possible noise).

## Not perf-validated
- punroll on aarch64 beyond benchmark kernels (agent tested x64+host test.sh).
- x64 min/max/dot_i32 "gaps" largely explained by missing ISA flags
  (baseline SSE2); psadbw for sum_u8 blocked on vendoring
  cranelift-assembler-x64 — documented in x64-notes.md.
