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
