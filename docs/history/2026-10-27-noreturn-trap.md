# No trap after a noreturn call (`PLIRON_NORETTRAP`)

Follow-up to 2026-10-27-jump-tables ("Next" 2). Default off.

## The traps

regex-syntax, `-O -Ccodegen-units=1`, aarch64, baseline env
`PLIRON_REMATCALL=1 PLIRON_NARROWEXT=1 PLIRON_EDGEDEDUP=1` (COLDCOPY on by
default). `llvm-objdump` of the object, by the instruction before each
`udf #0xc11f`:

| before the `udf` | count | of which the function's last insn |
|---|---|---|
| `bl` | 1,508 | 196 |
| `b` (a lone trap block) | 82 | 0 |
| another `udf` | 66 | 4 |
| `blr`, anything else | 0 | |
| total | 1,656 | |

Callees of the 1,508: `_Unwind_Resume` 536, `panic_in_cleanup` 283,
`raw_vec::handle_error` 141, `option::unwrap_failed` 109,
`handle_alloc_error` 92, `panic_already_borrowed` 86, `panic_bounds_check`
65, `panic_fmt` 50, `slice_index_fail` 29, `str::slice_error_fail` 26,
`panicking::panic` 24, the rest under 21 each. LLVM emits 71 `brk #1`.

In the final CLIF (`PLIRON_PRECOMPILE`) every trap in the crate follows a
call; there is no "real" `unreachable` (a match arm, say) left, because
`unreach` already removes the edges into those:

- 780 `call f; trap user2` in one block (MIR `Call` with no target:
  `call` + `unreachable`);
- 725 `try_call f, ret=B, [pad]` where `B` is a lone `trap user2` block
  (133 such blocks, shared). These are invokes of noreturn callees,
  `_Unwind_Resume` included (an inlined callee's resume becomes an invoke
  of the caller's pad). Vendored patch "continuation_trap" already emits
  the trap inline after the `bl`, so each costs one `udf` per site, and
  the lone block still costs its own `udf` (the 82 + 66 above).

Every one of those 1,505 calls has an uninhabited ABI return or is
`_Unwind_Resume`: with the flag on, all of them get the new code.

## Mechanism

1. `src/builder.rs`: `CallInfo::noret` = the call site's
   `fn_abi.ret.layout.is_uninhabited()` (what rustc_codegen_llvm turns into
   `noreturn`), and `true` for the `_Unwind_Resume` that `resume` emits.
   The flag rides `st.calls` through the IR passes (calls keep their entry
   when moved or retargeted; new calls a pass makes get `false`).
2. `pliron-clif-lower/src/lower.rs`, under `PLIRON_NORETTRAP=1` (read once
   per lowering): an `unreachable` that directly follows a `noret` call
   (in its block, or as an invoke's normal-return block) traps with
   `TrapCode::NORETURN_CALL` (user code 250) instead of `user2`; so does
   the `br` after a `noret` invoke whose destination starts with
   `unreachable`. Nothing else changes in CLIF, so the CLIF passes see the
   same shapes (a block ending in `call; trap` is still cold, edgefwd still
   copies trap tails, keeping the code).
3. Vendored Cranelift (PLIRON_PATCH.md 65), in `VCode::emit`, only when
   another block follows in emission order:
   - a `NORETURN_CALL` trap right after a regular call in its block, at
     exactly that call's return address (no regalloc edit, pad or island
     between), is not emitted;
   - a `try_call` whose continuation is a lone `NORETURN_CALL` trap emits
     nothing after the call;
   - blocks reached only through such a `try_call`'s normal-return edge
     (the lone trap block or its split-edge block) emit nothing.

   The last block keeps its trap: a return address must stay inside the
   function. LLVM's `NoTrapAfterNoreturn` drops the trap after every
   noreturn call, and its backends put one back only where such a call
   would otherwise end the function (its 71 `brk #1`). Unwinding is unaffected: the personality and the unwinder look
   up `ip - 1`, which is inside the call, and the call site's LSDA entry
   is unchanged.

Correctness envelope: the trap is dropped only when the type says the
callee cannot return, so a return would already be UB. A call that may
return keeps `user2` and its trap (test below). Tail calls are terminators
(`return_call`) and never reach this. Emulated EH (Windows `eh_data`)
puts a flag check after every call, so the return-address test fails
there and the trap stays. wasm lowering keeps `user2` (`noret_trap` is
false on the wasm path; waffle has no trap bytes to save).

## Numbers

`__text` bytes and `udf #0xc11f` count, same dylib, flag off / on:

| crate | `__text` off | `__text` on | delta | `udf` off | `udf` on |
|---|---|---|---|---|---|
| regex-syntax | 373,132 | 366,412 | -6,720 (-1.8%) | 1,656 | 242 |
| syn | 1,384,236 | 1,365,900 | -18,336 (-1.3%) | 4,997 | 858 |
| bench kernels | 47,240 | 46,968 | -272 (-0.6%) | 118 | 35 |

LLVM's regex-syntax is 184,752 B with 71 traps. Of the 242 left, 227 end
their function (ignoring alignment padding) and 15 are mid-function (6
after a `bl`, 9 after another `udf`). pliron sinks cold noreturn blocks
to the end, so a function ends in `bl panic_*; udf` more often than
LLVM's does. Ordering a non-noreturn block last would save most of those
4-byte traps (~0.9 KB here); not done.

Without the dead-continuation-block step (lowering + call-adjacent drop
only) regex-syntax was 367,692 B / 358 traps; without the invoke case
(plain calls only) 369,996 B / 961.

Runtime: see below.

## Tests

- `pliron-clif-opt/tests/noret_trap.rs`: compiles for the host and reads
  the trap and call-site records. `NORETURN_CALL` after calls is dropped
  except at the function end; `user2` after the same calls is kept at
  every return address; a lone `NORETURN_CALL` trap with no call before it
  is kept; a noreturn `try_call` has nothing at its return address, a
  `user2` one keeps its inline trap. Passes on aarch64 and on x86-64 under
  Rosetta (`--target x86_64-apple-darwin`).
- `test.sh noret` (`tests/noret`): panics through `-> !` helpers, an
  uninhabited-enum return, `Result<_, Infallible>`, bounds/unwrap/expect/
  str-slice failures, with drops live across the calls (invokes of
  noreturn callees, `_Unwind_Resume` out of cleanups) and a recursive
  unwind through 6 frames; at -O0, -O and `-O -Cpanic=abort` (the hook
  exits). Output is diffed against stock rustc; the stanza also checks the
  flag drops traps in the object.
