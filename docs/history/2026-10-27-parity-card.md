# First full parity cards (2026-10-27, after the overnight run)

`harness/parity.py`, aarch64-apple-darwin, backend 10354e8. Each card
took 10–12 min. No pliron jobs ran meanwhile, but other apps did
(VoiceMemos at about 135% CPU, Firefox), so load was 11–14. The
runtime-speed (SPD) and compile-time (CMP) ratios carry about ±3–5%
noise. Memory, code size and stack don't depend on load.

| | SPD | CMP | MEM | SIZ | STK | COR | **overall** |
|---|---|---|---|---|---|---|---|
| A: defaults | 106 | 107 | 36 | 37 | 87 | 100 | **88** |
| B: A + candidates | 107 | 105 | 59 | 40 | 88 | 100 | **91** |

(Corrected 11:20: the first version of this page scored COR 122, so 92 and
95 overall. See "COR is overstated" below; the card now caps the UI ratio
at parity and these are the recomputed numbers from the same runs.)

B adds the candidate flags that are clean so far: `EDGEDEDUP JT_SHAPE
LOWERFREE JEARENA CGU_ACAP=8 CMP3 LTUNDEF NORETTRAP EDGEMERGE=1 EPIMERGE=1
NARROWEXT`.

Ratios behind the ratings (pliron / LLVM):

| | A | B |
|---|---|---|
| native runtime geomean, 19 kernels | 0.914 | 0.888 |
| wasm runtime geomean, 16 kernels | 0.922 | 0.926 |
| compile time geomean | 0.913 | 0.933 |
| peak RSS geomean | 2.42 | 1.76 |
| machine code geomean | 2.41 | 2.30 |
| stack frames, regex-syntax, 327 shared fns | 1.205 | 1.181 |

Per input (time / peak RSS / machine code):

| input | A | B |
|---|---|---|
| kernels | 0.56 / 1.63 / 1.89 | 0.65 / 1.30 / 1.76 |
| kernels-wasm | 0.99 / 1.99 / 2.32 | 1.09 / 1.59 / 2.38 |
| regex-syntax | 1.08 / 2.82 / 2.06 | 1.01 / 1.68 / 1.84 |
| regex-wasm | 1.01 / 3.06 / **5.73** | 0.93 / 2.66 / **5.68** |
| syn | 1.05 / 2.99 / 1.99 | 1.06 / 1.81 / 1.76 |

## Reading the cards

- **SIZ is the weakest category, and wasm is most of it.** regex-syntax
  as a wasm32 object is 5.7x LLVM's `CODE` bytes, and kernels-wasm 2.3x.
  The wasm path has its own emitter (waffle), so none of tonight's
  aarch64/Cranelift work reaches it. It is the biggest lever left on
  the card.
- **MEM** goes from 36 to 59 with the memory flags (LOWERFREE, JEARENA,
  ACAP). That is the largest single move.
- **Native code size** (regex-syntax, `-O -Ccodegen-units=1`, `__text`;
  LLVM 184,752 B): 401,680 at the start of the night (2.17x), 382,736 with
  tonight's defaults COLDCOPY + REMATCALL (2.07x), and 338,476 with card
  B's flags (1.83x). `parity.py` measures default CGUs, so its ratios
  differ from these cgu=1 numbers.
- **SPD:** B's native geomean is 0.888 vs 0.914. Within the noise, but no
  sign of a regression. Kernels still behind LLVM in both cards:
  parse_only, strwork, varint and eqbug, plus chars_alpha in B.
- **COR was overstated** in the first version of this page (122). The UI
  input counted 4 stage-1-only failures (environment flakes on the
  LLVM-built compiler: dsymutil, backtraces) in pliron's favour, and the
  fuzz and crates inputs read the last saved result files, which were a
  one-seed and a one-crate rerun. `parity.py` now caps the UI ratio at
  parity and ignores result files from fewer than 100 seeds or 10
  crates. With fuzz at 1 failing seed (13475, since fixed) in 400 and 37
  of 37 crates clean, COR is 100 either way.

## Open, in priority order

1. ~~Seed 13475~~: a slot-merging miscompile in the defaults at `-O0`.
   **Fixed in f68d323** (starts-mode coalescing ignored earlier copy
   windows; 2026-10-27-batch.md). The seed is in the gate's `FUZZ_REGRESS`.
   Rerun crates and stage-2 UI on the fix.
2. Wasm code size (above).
3. Flip what's ready after the missing checks:
   - EDGEDEDUP, JT_SHAPE, CMP3: a quiet bench A/B.
   - LOWERFREE, JEARENA, ACAP, NARROWEXT (fixed in c689c75), LTUNDEF
     (memory fixed in 922d8f7), NORETTRAP, EDGEMERGE, EPIMERGE: crates
     tiers 1–3 and stage-2 UI with each change.
   - New candidates from 2026-10-27-slow-kernels.md: SELCHAIN, MULCARRY,
     STEPCMP, REMATLOOP=0 (parse_only 1.17 → 1.02, eqbug 1.21 → 1.09 on
     criterion; sieve layout-checked). ELABLOOPS stays out until its
     strwork pressure cost is handled.
4. slotprom/regalloc2 work (−7 KB) is committed but **not on main**:
   fa68d6a and e83e4b2 on branch `worktree-agent-a60fd0dd88e78e28d`.
   Its gate and fuzz runs didn't finish.
