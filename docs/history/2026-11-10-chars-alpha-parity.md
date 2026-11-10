# 2026-11-10 — chars_alpha to parity (RETNN+NNBB promotion)

`chars_alpha` (`s.chars().filter(is_alphabetic).count()` on all-ASCII)
went 3.26 ms → 2.46 ms → 2.098 ms vs LLVM's 2.082 ms (benchstat
`~`, p=0.200) — effectively closed after sitting at 2.15x.

The pieces, in the order they landed:

- `brhint` single-bit `band` direction: `c & 0x80 != 0` means UTF-8
  decode, the *uncommon* path, so the bit-set arm is marked unlikely
  and the emit-order pick falls through to ASCII (`tbnz`-away, the
  shape LLVM picks). Multi-bit masks and `bor` stay unhinted.
  2.43 ms → the emitted loop gained LLVM's direction.
- `rebase` (opt-in `PLIRON_REBASE`): a dominated `iadd x,K` rewrites
  through a dominating `iadd x,J` as `iadd w,K-J`, funneling the
  cursor's post-increment uses through the bump the load's block
  already computes. Held late in the pipeline so `addnorm`'s
  `(x+k1)+k2` re-fold can't undo it; `c==0` detaches before
  `change_to_alias` (filetest `rebase_dom.clif`, verified red).
- `bump_hoist` (vendored, PLIRON_PATCH #116, default-on): after the
  compile-path egraph's elaboration sinks `iadd p,k` into its use
  blocks, hoist the bump that feeds the next iteration's terminator
  args back into the memory base's block; other path-specific bumps
  rematerialize in their own blocks, and an `iconst` sunk into the
  candidate block is remat'd at the hoist point instead of refusing.
  With the AArch64 post-index path this emits `ldrb [x23], #1`.
- `PLIRON_RETNN`+`PLIRON_NNBB` (promoted to `DEFAULT_ON` this commit):
  rustc's `nonnull` return attribute seeds call results, and the
  nonnull proof walks through `black_box` (value-preserving). The
  `Option<NonNull>` niche `cbz` in `Iter::next` folds — the last
  per-char overhead vs LLVM.

`PLIRON_BUMPHOIST=0` was pinned in `test.sh`'s `slotpromlt`
RA2_XBLOCK/SPILLCSE size ratchet: bump_hoist deletes the same
redundant moves pre-regalloc (1787→1773 vs XBLOCK's own 1785), so the
comparison now isolates what the ratchet exists to measure.

Validation: corpus replay 4870 fns clean with all flags on and again
at the promoted defaults; `cargo test -p pliron-clif-opt` 144 green;
`harness/gate.py` green twice (94 jobs). `chars_alpha` appended to
`bench/history.tsv` (2082133 LLVM / 2098439 pliron ns).

Residual notes: the criterion-inlined copy threads `ptr` and `ptr+1`
as two block params — survivor rebase across the merge is still
unimplemented, and the `ldrsb` post-index form (LLVM's sign-extending
byte load) is unused. Neither shows in the measured number.
