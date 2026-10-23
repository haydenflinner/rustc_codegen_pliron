# Overnight 2026-10-22 → 23: toolchain bump, unwind exits, trip bounds

## Toolchain: nightly-2026-10-06 → nightly-2026-10-22 (9b0eab9)

This was the first bump since the backend was created. The upstream API
changes were mechanical:

- `PassMode::Indirect` was split into `Indirect` and `IndirectUnsized`.
- New required trait methods: `ctpop` and `type_ppcf128`.
- `run_thin_lto` takes `incr_comp_session`.
- `rmeta_link::read_from_data` takes `is_like_aix`.

One change broke behavior: `rust_eh_personality` is now a mangled
std-internal symbol, so every binary that could unwind failed to link.
`State::eh_personality` now carries
`rustc_symbol_mangling::eh_personality_symbol(tcx)` down to the unwind
tables. The rustc fork (`haydenflinner/rust` main) was rebased to the
same nightly: three self-host patch commits on top of `1a2c4797e`. The old
main is tagged `archive/main-2026-10-08`.

MIR move elimination is in this nightly but only runs under
`-Zmir-move-elimination` (default off). Every gate stanza passes with it
on (`PLIRON_TEST_RUSTFLAGS=-Zmir-move-elimination`). `tests/mirshapes`
pins the MIR shapes it produces (see 2026-10-22-mir-move-elimination.md).

## Miscompiles: stores a panicking call's caller can see

Found by hashbrown's `hashmap_reserve_survives_panicking_build_hasher` on
the new toolchain. The bug is older; the new MIR shape exposed it.

| pass | bug | fix |
|---|---|---|
| dse (93e9541) | Calls were barriers only for non-isolated memory, and only `return` exposed caller memory. So a store through `&mut self` was deleted when a later store rewrote it, or when the path ended in `call; trap`, even though the call could unwind into a caller that catches the panic. | A plain call that may unwind is `Op::Exit`: it sets `top_non_stack` and drops `Root::V` coverage. |
| licm promote (7f8474f) | Scalar promotion of an isolated root across a loop containing a call that may unwind. The unwind leaves the loop without passing the exit-edge flush. | `Root::V` groups are not promoted when the loop has such a call. |

Both fixes need to know which calls can unwind. Lowering now records a
per-callee `nounwind` set: `FuncInfo::nounwind` plus asm stubs, the same
`maythrow` computation the emulated-EH path uses. It is threaded
FnJob → Pipe → dse/licm next to `nowrite`/`call_frozen`. `try_call`
needs nothing: its unwind edge stays in the function and reaches the
resume call, which is itself a plain call.

Tests:

- `dseunwind`
- `unwindmem`: 10 shapes, including loop promotion, Vec push and drop guards.
- `unwindstd`: std's own guards (SetLenOnDrop, sort holes, RefCell flags,
  BTreeMap, ...) under a panic at 12 points, compared with stock.
- dse unit tests for the unwind/nounwind pair.

## Trip bounds from dominating guards (4a0911b)

The arrayvec gap (+16%) came from loopvec and punroll cloning 16-lane and
×8 bodies for loops behind `len <u 9`; those clones can never run.
`loopidiom::trip_max` bounds `(bound - iv0) / step` using:

- dominating `brif`/`icmp` guards;
- block params, taking the max over incoming edges, each with its own
  edge guard (rotation copies the len check onto every entry);
- `(y + X) - (y + c) <= X`.

How the passes use it:

- loopvec skips loops whose bound is below one vector group.
- punroll keeps K within an eighth of the bound.

| arrayvec fn (µs) | stock | before | after |
|---|---|---|---|
| sum_all | 1439 | 1822 | **1301** |
| find_all | 1447 | 2165 | 1647 |
| push_iter | 2184 | 2165 | **1880** |

The bound is a cost input only. `PLIRON_TRIPMAX=0` turns it off.

## Fuzzing (rustlantis, nightly-2026-10-22)

| seeds | result |
|---|---|
| 520:776 (with selchain/mulcarry/stepcmp on) | 3 output mismatches, all on by default too |
| 776:2048 | 1268 pass, 2 ICEs, 1 mismatch, 1 compile timeout |

Fixed:

| seeds | bug | commit |
|---|---|---|
| 538, 683, 730 | slp xpack turned two loads of one address into `splat(first load)` without checking for a writer between them (`a = *p; f(p); b = *p`) | b0860b3 |
| 810, 1276 | loadfwd's dead-pair poisoning inverted all of `pre.start`, so single-pred pass-through aliases shadowed the owning pending pair. A removed param was then appended to an edge ("argument v14720 is not attached") | 12deef9 |

`gate.py`'s `fuzzreg` job replays these seeds. They're about 450 KB of
custom MIR each, too big to check in.

### Open: seed 1939 (output mismatch at -O)

- It reproduces with only `fn13` optimized
  (`PLIRON_OPT_FNS=@fn13`); bisect unit 363 is step `ifconv2`.
- Within that step it is `cparm`'s `const_sweep`: `PLIRON_CSWEEP=0` fixes
  the fn13-only build but not the full build.
- The interpreter finds the pre- and post-`cparm` CLIF equivalent (400
  trials), and the final CLIFs too.
- The one fold examined, `v2665 -> i64::MAX - 1`, is correct for its
  input: every SSA path into the param carries `i64::MAX`.
- So the divergence is in a fold the interpreter shares, or in
  memory/uninitialized-read behavior it doesn't model.
- Next step: reduce the program (only `fn13` matters), or diff `fn13`'s
  disassembly at units 362/363.

`PLIRON_CLIF_STEPS=1` (with `PLIRON_CLIF`) now dumps after every step
that reports a change; several steps don't dump on their own.

Seed 1776 (pliron compile timeout, stock fine) is not investigated yet.
