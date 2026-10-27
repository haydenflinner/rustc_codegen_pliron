# varint_postcard +24% on Windows: ifconv folds in still-branchy loops (2026-10-26)

`varint_postcard` measured +23.8% vs LLVM on Windows x64 (the Oct-22
notes had it at −6% standalone on aarch64; verified unchanged vs
c2e944b, so it's a standing gap, not a pull regression).

## Root cause

LLVM's `sum_postcard` (~200 insns) fully unrolls the u32/u16 varint
loops inline with static shifts (`shll $7/$14/$21/$28`) and keeps
`try_take_varint_u64` outlined. Ours inlines everything, leaves the u32
loop rolled (`imull $0x7` per byte), and — the real cost — `ifconv::run`
(the `ifconv2` step) folds each decoder's error-path diamonds (`i==9`,
`byte>=2` invalid) into `select` chains feeding the `Result`
discriminant `brif`. ~20 cmovs per record on the hot path, serialized
ahead of a branch that stays anyway; 743 insns with a 208B spill frame.

- `PLIRON_IFCONV2=0` → 3.68 ms vs LLVM ~3.60 ms (parity).
- `PLIRON_VEC=0` also "fixes" it only because `FlagId::Loopvec` gates
  `ifconv2` too — loopvec itself never fires on this function.
- slotmerge / SM_COALESCE / NOCAP_FIELDS / SM_PRECISE / punroll / inline:
  all neutral.

## Gating variants tried (all behind env flags, 19-kernel sweep)

| variant | wins | losses |
|---|---|---|
| `PLIRON_IFCONV_FLATALL` (flat gate for every fold, not just chained arms) | varint_postcard −20%, tokenize −14% | chars_ascii +47%, chars_alpha +31%, contains_rng +13% |
| merge re-branches on a merged param (`NODISP`) | varint_postcard −20%, tokenize −12% | same chars_* regressions + fib +13% |
| reject select-of-select merge args | chars_* preserved | varint_postcard only −7%, fib +13% |

The chars kernels (`chars().filter(is_ascii).count()`) need those
in-loop folds — the UTF-8 width-decode and range-check diamonds merge
bools consumed as data. The varint folds merge an error triple that is
*re-branched* on anyway — but so do the `Option<char>` tags in chars,
so merge-use shape doesn't discriminate either. The real difference is
branch predictability: varint's error branches never fire, chars'
dispatch branches mispredict. Static shape can't see that.

## Landed

`PLIRON_IFCONV_FLATALL` as an opt-in knob only (5d9555a): the flat gate
applied to every fold. Useful for varint-heavy code; not default —
needs a predictability signal (PGO or a better static proxy) to flip.

## Alternatives not tried

- PGO-guided ifconv gating (the project has PGO plumbing).

## Follow-up: `bunroll` (bounded full unroll, ea80617)

Implemented LLVM's `unroll-max-upperbound` shape as an opt-in pass
(`PLIRON_BUNROLL=1`): K clones of a `0..K` loop's body, early exits kept
per copy, back-edges chained to the next copy, original loop left as the
post-bound tail. Handles the rotated-latch shape (IV resolved through
relay latch params, incl. delayed-copy params). Verified on a
postcard-shaped `for i in 0..5` decode loop — unrolls, verifies, corpus
clean (12,457 fns).

**It does not fire on `sum_postcard`'s actual varint loops**: they bail
on the LCSSA cone check. Earlier passes (jumpthread/cparm) leave value
aliases (`v588 -> v544`) that let post-exit merge blocks reference
loop-defined values as free vars; the clone paths aren't dominated by
the original header, so folding would leave those uses def-less (the
verifier caught exactly this before the check existed — `iadd v544`
non-dominating in `Rec::serialize`). Making the loops eligible needs
exit-cone cloning (punroll's `analyze_exit_cone` machinery) or LCSSA
canonicalization before bunroll.
