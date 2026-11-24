# 2026-11-25 — LoadAddr remat, medium copy arm, residual audit

Commit `c6f97c84`. Two changes plus an audit of the four items on the
todo list after the xcgu-home fix.

## aarch64 `LoadAddr` remat (default on)

`MachInst::rematerialize` now re-emits `LoadAddr` for the fixed
stack-offset amodes (`SPOffset`, `FPOffset`, `SlotOffset`,
`IncomingArg`); the reg/label modes read a register and stay
non-remat. Also marked `rematerialize_cheap` so the vreg gets the
discounted spill weight (`RA2_REMATW=7`). Same treatment LLVM gives
frame-index forms. html: 109.16 -> 109.08M instr.

## `PLIRON_MEMFAST_MED=1` (opt-in)

`(2k, 4k]` copy sizes lower to four overlapping k-byte moves instead of
the libc call in `small_copy_or_call`. Flat on html/protobuf/pulldown
(copies there are <= 2k or large); kept opt-in for band-heavy
workloads.

## Audit of the four-item list

- **Shrink-wrap / dispatcher thunks**: `PLIRON_SHRINKWRAP` is
  default-on (dominator-based save-block placement + CLIF live-range
  split). The sampled thunks (drop_glue adapters, dispatcher:144) have
  `needs[entry]` true — the CSR use is at the call right in the entry
  — so no early-ret can skip saves. The extra reg (x19) in the
  drop_glue thunk is landing-pad-forced; per-reg sinking into a pad
  needs per-PC unwind info. Not cheap-actionable.
- **loadpre-with-points-to**: already built. `loadfwd`'s call arm
  keeps `iso` locs and `ncc` locs the call isn't handed (computed from
  operand roots); `edge_pre` does partial-availability load PRE on the
  missing edges. `vec:2955` our side is `str` vs LLVM's
  `ldr+add+str` — same or better per site; `raw_vec:635` is 153 vs 120
  hot copies, i.e. inlined-copy coverage, not alias analysis.
- **ptr:570**: copies at the hot site are bimodal (>=17B tags, <8B
  tokens); both small arms pay. `MEMFAST_SMALL=0` and `=hot` raise
  instr. Residual is the 57-insn dispatch footprint vs a 3-insn
  `bl memcpy` site — memcpy-quality territory, no cheap toggle.

Current html standing: ~1.06x instr, cycles at parity within run noise
(0.93-1.03x across asmvs profiles). protobuf 0.964x cycles.
