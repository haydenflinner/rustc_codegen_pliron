# 2026-11-18: greedy `PLIRON_RA_GREEDY_COLOR=1` checker-clean

All-spill greedy (`PLIRON_RA_ALGO=greedy`) was already rafuzz-clean.
Colouring failed ~50–80% until the emit/colour contract was narrowed:

- Branch args are not in `inst_operands` — extend intervals for them.
- Reuse: tied use that is still live after the inst gets a throwaway
  temp (def must not clobber the use's home); always move def → home.
- Colouring refuses pregs clobbered in the interval and pregs used as
  foreign `FixedReg` temps in the interval.
- Emit: if a use's home preg is already taken by an earlier operand at
  the same inst, copy to a fresh temp.
- **Promote single-block non-param vregs** (branch args OK: edge moves
  are reg→stack; block params stay spilled). Multi-block stays spilled.
  Per-block linear scan from an all-spill baseline.

Rafuzz: `COLOR=1` 10k seeds 0 failures; default greedy 2k 0 failures.
Stack-edit share drops ~94% → ~49% under COLOR with args coloured.

Tried and rolled back: colouring block params (~25% fail); multi-block
non-edge (~18% fail); arg↔param coalesce (panics). Unlabelled edge
edits kept. Temps may steal dead homes.

Param colouring attempts (all parked): all params ~25% fail; pred-home
refusal ~11%; single-pred-only ~12%. Multi-block non-param ~20% fail.

Next: diagnose ParallelMove vs physical edge moves for param homes;
held-out when RAM allows.
