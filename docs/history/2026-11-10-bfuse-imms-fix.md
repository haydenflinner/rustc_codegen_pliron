# BFUSE `(x<<k)&m` imms miscompile (fuzz seed 6279)

## Symptom

`harness/run.py --suite fuzz` seed 6279: pliron hash
`5767878378028100228` vs stock `18437771903820658465`. Reproduced only
under `-Zmir-opt-level=0` (the harness's mir-opt-0 leg). Bisect blamed
`dem64` on `_RNvCs7J7XABzWZyi_1p4fn14`; `PLIRON_DEM64=0` restored the
stock hash, so it looked like a dem64 bug — but dem64's CLIF rewrite
was provably value-preserving and the dem64-off/on *final* CLIF was
semantically identical.

## Root cause

Lowering, not dem64. The function contains
`v10813 = band (ishl v10803, 8), -256`. Normally that `band` is
absorbed into `ushr (band x, m), k` (BFUSE rule 66 → `ubfx`), so it
never materializes. dem64 demoted the consuming `ushr.i64` to
`ushr.i32` *behind* an `ireduce.i32` — and `ireduce.i64→i32` lowers as
a register passthrough, so the `ushr` no longer pattern-matched the
`band`. The `band` materialized standalone through rules 62/63 —
which encoded the UBFM `imms` operand with the `ubfx` convention
(`bf_field_imms` = `k+w-1`) instead of `ubfiz` (`bf_field_ubfiz_imms`
= `w-1`). For `k=8, w=56` that produced `UBFM(56,63)` =
`ubfx x, #56, #8` (`lsr x, x, #56`) — extracting byte 7 instead of
computing `x<<8`.

## Why the shipped test missed it

`aarch64_bitfield.rs::ishl_then_band_is_ubfiz` only asserted a `ubfiz`
line containing `#8` existed; the buggy `ubfiz w, w, #8, #16` contains
`#8` too. The assert now pins `#8, #8`, and a new i64 case
(`(x<<8) & ((1<<50)-1)<<8`, `w=50` so `imms≥immr` on the buggy path)
must print `ubfiz x, x, #8, #50` and no `ubfx`/`lsr`. Both fail red on
the pre-fix build.

## Fix

`vendor/.../lower.isle` rules 62/63: `bf_field_imms` →
`bf_field_ubfiz_imms`. Seed 6279 back to stock hash; quick 13/13,
gate 94/94, fuzz 400/400. Commit 02baa35.

## Same-session landings

- narrowext bit 32 flipped default-on (c1fc7d8): `Ushr` in
  `narrow_uext_clean` + `bor`/`band`/`bxor` constant-`ushr`-operand
  fusion (`orr w, w, w, lsr #k`) for clean narrow values.
- `utf8_valid_up_to` structural note: 686 vs 220 machine insns, but
  `PLIRON_PUNROLL=0` → 311 while timing the benchmark moves only
  ~1.3% (punoff/pliron 0.987) — code-size bloat and the perf gap are
  *not* the same thing here; the residual gap is looprot (+48) and
  per-exit block-arg stubs, and the samples sit elsewhere.
