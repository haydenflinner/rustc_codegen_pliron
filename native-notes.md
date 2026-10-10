# Native aarch64 opt notes (wt/native-opt)

## Landed this session (rev_copy32 follow-up + reduction epilogue)

Prior uncommitted WIP (~44 lines in loopvec.rs) was inst-major hoisted
load emission; kept and extended (two-pass lane reversal, see below).

- `loopvec apply()`: non-direct stream bases are now strength-reduced
  into `vh` block params (initialized in the check block `cb` from
  `subst(base, iv, iv0+adj)`, stepped ±vf*UNROLL elements on the
  backedge). Kills the per-iteration scaled-index `mul`+`iadd` tree and
  lets all group offsets ride load/store immediates. Descending (`neg`)
  streams anchor at the UNROLL block's lowest byte so offsets are
  non-negative (`UnsignedOffset` amodes — required for pair fusion).
- Hoisted loads emit inst-major AND lane-reversals in a second pass, so
  the `ldr` run and the `rev`/`ext` run each stay contiguous.
- Reduction epilogue: group accs combine lane-wise, then a log2(lanes)
  rotate-shuffle+op tree replaces extractlane-per-element
  (extractlane×lanes×UNROLL → ~3+2log2 ops).
- vendored aarch64 `fuse_with_next` now also accepts `AMode::Unscaled`
  (`ldur`/`stur`) inputs, not just `UnsignedOffset`.

Codegen for rev_copy32's vector loop went from ~25 insts to:
`ldp [x4,#32]; ldp [x4],#-64; rev/ext x4; stp [x5],#32; rev/ext x4;
stp [x5],#32` + loop control — the same 2x ldp + 2x stp(post-index)
shape as LLVM (its 15 insts vs our ~19, remainder is loop-control movs
and a remat'd trip bound). `scaled` similarly got 2x post-indexed
ldp + 2x post-indexed stp + 4x fmul.

Note: the store pairing succeeded *despite* producers sitting in the
ALU gap — ok_down fires when regalloc gives the pair's data operands
and the gap producers disjoint phys regs (luck of allocation). It's not
guaranteed; kernels where the producer regs collide will still miss
stp. Not structural this round.

## Measurements (loaded box, load avg ~7; ratios over absolute)

`wide` suite pliron vs stock rustc (-Ctarget-cpu=apple-m1):
- rev_copy32: 0.316 vs 0.317 — PARITY (was a stable ~11% deficit)
- scaled: 0.639 vs 0.640 — parity (previously ~-8% wobble)
- dot_u8: 0.147 vs 0.291 — 2x win
- has_val 0.329 vs 0.744, cnt_vowel 0.310 vs 1.163, find_off 0.082 vs
  1.164, fillzero 0.156 vs 0.189 — wins
- max_u/min_i/sum_sq/even_sum/xor_fold/sum2d — parity
No regressions observed.

Validated: ./test.sh green; targeted correctness tests for
rev/neg-store/neg-load/dot/early-exit kernels across edge lengths
(0..1000 incl. non-multiples of VF*UNROLL).

## Remaining gaps / next items

- Post-index addressing for *single* (non-paired) ldr/str would need a
  general-reg `AMode::PostIndex` — only SP-scoped variants exist today;
  the emit-site amode match arms would all need a new case. Deferred.
- `nm`/trip bound remat inside the vector loop (`and x, len, mask`)
  — cheap but a wasted slot; an anti-remat hint could help.
- Store pairing across producer gaps is opportunistic (depends on
  regalloc not reusing the data reg); a guaranteed path needs CLIF or
  scheduling support.
