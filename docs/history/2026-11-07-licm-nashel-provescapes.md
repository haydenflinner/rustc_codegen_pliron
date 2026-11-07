# 2026-11-07 LICM noalias provenance shelter (`PLIRON_LICM_NASHEL`)

Two commits: `220aa2a` (opt-in) and `024fc2e` (default-on via
`DEFAULT_VALUES`), plus the `provescapes` rewrite committed on top.

## The mechanism

`licm::clobbers` decides whether a loop instruction can touch a `Loc`.
For a loc rooted at a rustc-`noalias` entry param (`&mut self`, `&mut T`
by value): LangRef arg-`noalias` means the pointee may be written only
through a pointer *based on* that param during the call — so a store
through a provably non-based address cannot clobber it.

The catch (LangRef "based on"): GEP/bitcast/transitive only — a pointer
*loaded from* memory is NOT based on the pointer it was loaded through;
it carries the provenance of whatever was stored. So `q = load [m+8]`
can legally be `m`-based (a self-pointer stashed into `*m`). The gate is
therefore **provenance-never-planted**: if no `m`-derived pointer ever
reaches memory/other scopes, no loaded pointer can be `m`-based.

`loadfwd::provescapes` computes this function-locally (LLVM's
`captures(none)`, done in one worklist pass over all candidates):

- seeds: `noalias` entry params, each a bitmask bit, alias-resolved;
- `taint[v]` = params `v` is derived from; `feeds` propagates taint
  across edge args into block params;
- leaks: store of a tainted value as the *stored value* (store address
  is fine), `return`, any call arg the callee doesn't mark `nocapture`,
  any unclassified use of a tainted pointer;
- `esc[b]` = mask of params that leak in block `b`.

In `licm`, a param's bit joins `Shel::live` for a loop when none of its
escape blocks reach the loop (same `reachers` idea as `promote`).
Under shelter: a store clobbers the pointee only through a still-tainted
address, and a call clobbers it only if some operand is tainted —
both read straight from `taint`. Without shelter the older
`not_derived` arm still proves per-store non-derivation for the clean
opcode set.

`isolated_escapes`-based first cut was replaced because it counts every
use it can't address-track as an escape (`iadd(v1,var)` indexed field
accesses), which any real `&mut self` body trips.

## What it did NOT fix: `compress_inner`

The motivating case (`miniz_oxide` deflate hash-insert loop, ~1.19x
held-out gap) stays unsheltered:

- `v1` (the `&mut` compress-state param) has **real** escapes —
  `call fn5(v1,v2,flag)` / `call fn8(v1,v2)` where the callees' args
  are not deduced `nocapture` (`own=None`/missing positions).
- LLVM agrees `%d` is **not** `captures(none)`, and its disassembly
  reloads `[x27,#0x18]/#0x20/#0x28` every iteration too — LLVM does
  NOT hoist those field loads.
- LLVM's actual machinery there is the **489 `!alias.scope`/`!noalias`
  tags** rustc emits per-borrow via codegen_llvm — metadata not exposed
  through `rustc_codegen_ssa`, so we can't reproduce it, and it still
  doesn't buy the hoists we suspected.

Held-out A/B (`bench/heldout/run.py --variant NASHEL0`): ratio
0.986 [0.955, 1.011] vs default — neutral. `v1`'s escaping means the
feature simply doesn't apply to that function; it remains correct for
cases where the param's provenance never leaves memory.

## The real `compress_inner` gap (not LICM)

LLVM's hash-insert loop is ~13 insns vs our ~20:

- LLVM `ldp`-fuses the two `[x27,#0x20/#0x28]` field loads (one insn),
  still reloading per-iter — ours emits two separate `ldr`s plus extra
  address `add`s.
- Same-`d` derived stores (`prev[]`/`head[]`/`window[]` through
  pointers loaded from `*d`) keep `*d` fields unpromotable in BOTH
  compilers.

So the remaining gap is codegen shape: `ldp`/`stp` pairing, addressing
modes, register pressure — a different axis entirely. A
bound-aware `clobbers` (min-offset disjointness via `root_min`+`bnd`)
could let our version hoist *more* than LLVM does here, but that's a
separate experiment.

## Diagnostics added

- `pe-esc {fn} {block} m={mask} k={arg}` — provenance leak sites
  (`pliron::loadfwd` debug).
- `licm pe {fn} noalias=… scoped=… escapes=…`, `licm shel {fn} {block}
  live=…`, `licm iso {fn} …` — all carry `func.name` since parallel
  CGUs interleave identical `u0:0` names.
- `PLIRON_FACTS=<substr>` prints `nocapture` verdicts per symbol.

## Files

- `pliron-clif-opt/src/loadfwd.rs` — `ProvEsc`/`provescapes`.
- `pliron-clif-opt/src/licm.rs` — `Shel`, `clobbers` taint arms,
  `run_loop` shelter computation, `promote` candidacy for sheltered
  roots, tests.
- `pliron-cfg/src/lib.rs` — `DEFAULT_VALUES["PLIRON_LICM_NASHEL"]=1`.
