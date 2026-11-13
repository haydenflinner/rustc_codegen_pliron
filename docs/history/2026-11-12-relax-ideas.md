# 2026-11-12: ideas from relax, built and measured

relax (`../relax`, a from-scratch Rust compiler + JIT aimed at the
edit-run loop) was surveyed for ideas this backend lacks. Nine were
shortlisted; this is what each turned into. Branch `relax-ideas`.

| # | idea | outcome |
|---|---|---|
| 1 | `asm <fn> vs=rustc` | built: `bench/heldout/asmvs.py` |
| 2 | optimization remarks | built: `PLIRON_REMARKS`, `harness/remarks.py` |
| 3 | post-allocation spill reducers | part built: `PLIRON_RA2_POSTOPT` (opt-in) |
| 4 | pressure-aware hoisting | built: `PLIRON_ELABLOOPS=2` (opt-in) |
| 5 | per-site inline pricing, priority queue | already here / no effect |
| 6 | bounds-check loop versioning | already here (`bcheck.rs`) |
| 7 | callee-renaming back-end memo | measured, not built |
| 8 | hot-reload pieces | layout guard built (`PLIRON_HOT_LAYOUT=1`) |
| 9 | measured caps on repeat rounds | measured, tool built, no cap needed |

## 1. asmvs

Keys both builds on source line (the line-table row, so inlining
differences don't break the pairing), classifies every instruction and
weights by self samples. First run, pulldown-cmark (1.19x that run): the
gap by class was stack loads +79 samples, stack stores +62, conditional
branches +47, register copies +41 of 235; the worst lines were
`slice/index.rs:90`, `vec/mod.rs:1002` and `parse.rs:2837` (73 insts of
slot shuffling against LLVM's 18).

## 2. remarks

One fixed-shape record per call the inliner kept and per loop `loopvec`
left scalar. Across regex, rust-compression and rust-html-rewriter the
non-external rejections are `score` (5,697), `fanin` (212), `noinline`
(299); `cap` fired twice and `growth` never — so the inline budgets do
not bind and a priority order over sites (relax's heap) would change
nothing (#5). Site-specialized pricing already exists as
`PLIRON_INLINE_SPEC`.

## 3. PLIRON_RA2_POSTOPT

Whole-CFG "register mirrors slot" dataflow plus slot liveness over the
finished regalloc2 output. regex-syntax at `=15`: instructions 86,222 ->
86,128, stack loads 7,679 -> 7,574, stack stores 7,752 -> 7,674. Most of
what bit 1 and bit 4 delete (276 reloads, 550 stores) Cranelift's
emission already elided; the net is bit 2 (load -> register copy) and
bit 8. Not built: relax's region windows as real allocation windows,
cold-call splitting and through-pieces — `PLIRON_RA2_LOOPSPLIT` /
`FREQSPLIT` cover that ground from inside the allocator.

## 4. PLIRON_ELABLOOPS=2

Cheap invariants stay inside loops that contain a call. Stack loads vs
default: pulldown +3.9% at `=1`, +2.5% at `=2`; rust-compression +3.8%
/ +1.7%; regex +3.3% / +3.0%.

## Timing

`bench/heldout/run.py --rounds 5 --aa` with the machine under foreign
load (load average ~20): post/pliron 0.986 [0.955, 1.097], el1 0.999
[0.965, 1.111], el2 0.989 [0.953, 1.071], aa (noise floor) 0.998
[0.961, 1.107]. Nothing separates from the floor; all three stay
opt-in until a quiet-machine run.

## 7. memo census

`harness/memo_census.py target/corpus/std`: duplicate bodies with
callees keyed by symbol (today) 11.6% of functions / 3.6% of lines; by
position (relax's renaming) 16.1% / 4.5%; data symbols by position too
20.9% / 5.8%. relax memoizes before inlining, where generic instances
are still identical; our unit is the post-inline body.

## 8. hot reload

`PLIRON_HOT_LAYOUT=1` writes a struct/enum/union layout table into
`base.ref` and each patch; the loader refuses a patch whose table
differs and names the types (`tests/hot/run.sh`). Not built: direct-jump
thunk patching, tests-on-save reach marking, protecting stale code.

## 9. no-effect time

`clif-check bench` now prints each step's time on functions it left
unchanged: 31-33% of the pipeline on the std corpus (top: `dse` 2.4%
fire rate, `loadfwd2`, `ifconv2`, `dse3` 0.3%, `loopdel` 0.1%, `ldrot`
0.0%). Repeat rounds that ran with nothing changed since the same
family's last no-effect run: 0.9% of pipeline time, none of which
fired. relax's "two rounds is enough" does not transfer; the cost is
first runs that find nothing, i.e. shape gates.

## Found on the way (both on main before this work)

- `memcpyopt` dropped a per-iteration re-initialising copy when the
  temp had partial writers outside the loop: regex-syntax
  `alternate_literals` double-freed, held-out `regex` aborted. Fixed
  with a wrap-route check and a unit test (2bf3d01).
- `pliron-clif-opt` tests did not compile (`iconst128` gained an
  argument), so the gate's unit leg was red.

## Second pass (same day)

**Fuzz sweep 30000..32000 at the first pass's head found five `-O`
mismatches, all older than this work:**

- 30621, 30696, 31403: `memcpyopt`'s `base_touches` counted an escaped
  destination address (stored as a value) as one touch at the store;
  reads through the reloaded pointer inside the writer-to-copy window
  were invisible. After an escape every access that does not resolve to
  another alloca, and every call, is a touch (58405d1, unit tests).
- 30694, 31084: aarch64 `ubfiz`/`bfi` fusion (`PLIRON_BFUSE`) computed
  `immr` from the type's width; an i8/i16 op runs as a 32-bit `ubfm`,
  so the field landed at bit `16 + k` (8d3fd9e). `optbisect` blamed
  `csweep2`, which only made the operand a constant.

All five are in the gate's fuzz regression list.

**Item 3, continued.** Cranelift's `vcode.rs` already had whole-CFG
reload CSE and reload PRE (`PLIRON_RELOADCSE`, `PLIRON_SPILLCSE`,
`PLIRON_RELOADPRE`), which is why `PLIRON_RA2_POSTOPT` found so little.
`asmvs` now splits our stack traffic into `spill-*` and `slot-*` from
frame remarks; on pulldown-cmark the excess over LLVM is about half
each. The hottest spill site was a dozen slot-to-slot copies of
constants on entry to `scalar_iterate_special_bytes` — block-parameter
moves whose `edit_vregs` label (the parameter) hid that the value was a
constant. `PLIRON_RA2_SRCVREG=1` labels them with the argument: stack
loads -4.3% (pulldown-cmark), -3.5% (regex), -3.6% (rust-compression),
spill-load samples on pulldown 136 -> 73. What is left there is one
slot reloaded into `x20` after each call (callee-saved pressure).

**Item 8, continued.** Direct-jump thunks (`PLIRON_HOT_DIRECT=1`, arm64
macOS: own page-padded thunk section, threads suspended for the flip),
tests on save (`PLIRON_HOT_REACH=1`, `examples/hot-test.sh`, `hotdiff`),
`PLIRON_HOT_FREE=protect`. The loader's fingerprints no longer hash an
anonymous constant's address, which had made every function naming a
constant look changed after an unrelated edit.
