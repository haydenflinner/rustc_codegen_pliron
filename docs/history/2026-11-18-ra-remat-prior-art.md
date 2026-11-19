# 2026-11-18 — remat spill weights: prior art → cheap-only

## What the literature and production RAs do

**LLVM** (`CalcSpillWeights`, `LiveRangeEdit::allUsesAvailableAt`,
`TargetInstrInfo::isTriviallyReMaterializable`): a live range's spill
weight is halved only when the def is *trivially* rematerializable —
typically a constant or an unallocatable physreg copy — and every use
site can rematerialize it. GOT / PC-relative symbol materializations
are rematerializable for *reload*, but do not get the weight discount.
Prefer remat over a stack reload when editing.

**GCC LRA** (`lra-remat.c`): after spilling, rematerialize from a
register-only expression; candidates that touch memory are rejected.
Remat is a post-spill repair, not a reason to under-weight a long
symbol address.

**Go** (`cmd/compile/internal/ssa`, rematerializeable): constants and
`SP`/`SB`-relative addresses only. Each use site gets a fresh copy;
there is no half-weight heuristic for "almost free" values that still
cost a load.

**Briggs / Chaitin-style rematerialization** (and LLVM's inheritance of
it): tag values whose def is cheaper than a memory reload; never treat
a multi-insn or memory-backed address as cheaper than keeping the
register. Region / min-cut splitting (LLVM `tryRegionSplit`, our
`PLIRON_RA2_REGION`) is the orthogonal pressure valve — already on.

## What we had wrong

`VCode::compute_remat_vregs` marked every `MachInst::rematerialize`
def — including aarch64 `LoadExtNameNear` and `LoadExtNameGot` — as
`is_remat` for spill *weights*. That made address constants look half
as expensive to spill as they are to rematerialize (adrp+add / GOT
load), so the allocator kept the wrong values and spilled hot
non-remat ones (protobuf `merge_loop` under plain `REMATW=1`).

Mode 7 (`uses ≥ 8`) papered over that by only discounting dense use
webs; it did not fix the membership of the remat set.

## Change

- `MachInst::rematerialize_cheap` (default false; aarch64: `MovWide`,
  ImmLogic `orr xzr,#imm`, `VecDupImm`/`VecDupFPImm`, `FpuMoveFPImm`).
- `compute_remat_vregs` uses cheap by default; `PLIRON_RA2_REMATW_CHEAP=0`
  restores the old full `rematerialize` set for A/B.
- Reload remat (`edit_vregs` + `rematerialize`) is unchanged — symbol
  loads still rematerialize at reloads.

## Next measurement

Against the post-`REMATW=7`/`REGION_CAP=2` base:

1. cheap (default) vs `REMATW_CHEAP=0` under mode 7 — is the filter alone
   a win / neutral?
2. `REMATW=1` with cheap — does LLVM-style trivial remat unlock plain
   `/2` without the protobuf +205k?

Logs: `target/placement/ra-night-remat-cheap*.log`.

## A/B results (callgrind non-nop, copies=1)

Base = mode 7 + cheap (new). Candidates vs base:

| program | CHEAP=0 (all) | REMATW=1 cheap | REMATW=1 all |
|---|---|---|---|
| blake3-scalar | +2 | 0 | +1 |
| blind-sig | **+52k** | +35k | +40k |
| pulldown-cmark | 0 | −47k | −68k |
| regex | +0.3k | +10k | +10k |
| rust-compression | 0 | −13k | −13k |
| rust-html-rewriter | −5 | −9k | +4k |
| rust-json | +0.3k | +16k | +17k |
| rust-protobuf | −0.1k | **+278k** | +277k |

- Keep `REMATW_CHEAP` default on: under mode 7, restoring GOT/near
  into the weight set costs blind-sig +0.15% (montgomery / alloc churn).
- Still reject plain `REMATW=1`: protobuf `merge_loop` +278k even with
  cheap-only — the damage is dense immediate remats, not symbol loads.
  Mode 7 stays.
