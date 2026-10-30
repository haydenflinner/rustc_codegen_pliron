# 2026-10-30: wasm `words(100)` trap = `spec` vs the linker's body fold

Symptom: the wasm gate's `wasmstdtest` failed `words(100) = trap: unreachable`
(panic inside `__rdl_dealloc`, from `Vec<(&str,u32)>::from_iter` over a
`hash_map::IntoIter`). First bad commit 6b688c6 (`WASM_SOLO` 300 -> 100); it
only exposed the bug by leaving a call to a shared body that used to be inlined.

Root cause: two passes disagree about what a same-named function is.

- `pliron-wasm-ld` folds same-mangled-name bodies across objects (xcgu local
  copies) and keeps the **smallest**, redirecting every other copy's callers.
  That is only sound if all copies have the generic semantics.
- `spec` (IPSCCP-lite) specializes a `Local` fn's params on constants when every
  *visible* callsite in the CGU passes the same one. An xcgu copy is `Local`.
  In cgu.4, `RawTableInner::fallible_with_capacity` had one caller
  (`hm_u64`: size 16, ctrl_align 8, capacity 64, fallibility 1), so the copy
  there was specialized, became the smallest body, and won the fold. cgu.0's
  `RawTable<(&str,u32)>::reserve_rehash` then called it and got a 128-bucket,
  16-byte-element table (alloc 2184) but deallocated with its own 12-byte layout
  (1672); dlmalloc's size assert trapped.

Evidence: `PLIRON_SPEC=0`, `PLIRON_LDEDUP=0`, or skipping spec on
`fallible_with_capacity` each fix it; `PLIRON_OPT_BISECT=0` does not (spec is not
bisect-gated); none of the wasm peephole/coalesce switches matter.

Fix: on wasm, `spec` skips `xcgu_copies` (`State::spec_skip_copies`). Escape
hatch `PLIRON_SPEC_COPIES=1` restores the old behavior. Native is unchanged
(its linker does not fold by name).

Test: `pliron-ir-opt` unit test `spec::tests::wasm_xcgu_copy_not_specialized`
(fails with `spec_skip_copies = false`); end to end, `tests/wasm/run_std.mjs`
`words(100)`.

Other callee-side IPO that rewrites a copy's body (retarg/nnret/arg dropping)
has the same hazard with the fold if it ever changes signatures or semantics
per CGU; none was seen failing.
