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

- Bounded full-unroll of the varint loops (LLVM's approach; the Oct-22
  doc ranked it #2 and measured −17–19% potential on varint shapes).
- PGO-guided ifconv gating (the project has PGO plumbing).
