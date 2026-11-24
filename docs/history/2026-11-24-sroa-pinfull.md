# 2026-11-24: sroa through drop glue (`PLIRON_SROA_PINFULL`), two ICE fixes

Held-out `ab.py pliron llvm` at day's end (15 rounds, ours / LLVM):
instructions — html 1.081, blind-sig 1.051, compression 1.027, blake3
1.024, regex 1.023, json 0.998, pulldown 0.993, protobuf 0.988.
cycles — html 1.056, compression 1.032, blind-sig 1.018, regex 1.017,
blake3 1.015, pulldown 1.010, json 0.962, protobuf 0.954.
json and protobuf run faster than LLVM on both axes now.

## `PLIRON_SROA_PINFULL` (61e32244 opt-in, c58871f3 default on)

html's `SelectorMatchingVm::exec_without_attrs` (the +8% program's
hottest function) builds a 144-byte `StackItem` field-by-field in a
temp, whole-copies it into *two* arg temps on different paths, and
drops the temp through `drop_glue` **invokes** on bail paths. IPO
capture marks the drop operand captured — nested drops pass derived
pointers to callees the solver can't prove non-retaining — so the
alloca refused every split rule and the field stores + two 144 B
`memcpy`s stayed. asmvs 1.07x -> 1.042x cycles; the same writeback
stores replace the memcpys, so the instruction count barely moves —
the win is shape (slot traffic + spill samples), not count.

Three pieces:

* `pin_full`: operand `idx` of a call that is the alloca's base and
  neither diverging, copyable nor extent-pinned is a `Pin` over the
  whole alloca when the callee can't retain it (or is bodiless, so
  `TAILSPLIT` can still defer). It fills the gap in `accesses_inner`
  where a call arg with no known extent used to refuse the alloca.
* `consuming_drop`: `core::ptr`'s `drop_glue`/`drop_in_place` (matched
  on `4core3ptr`-anchored mangled components) consumes the object —
  keeping the pointer past the call is UB, so the operand is
  non-retaining *by semantics*, no IPO fact needed, and that holds on
  an invoke's unwind edge too. `split`'s sync emits the writeback
  stores before the call but **no reloads and no pad trampoline** for
  these pins — which is what lets invoke pins sync at all without
  touching `SYNC_INVOKE` (still opt-in; the sightglass
  `constructor_simplify` miscompile behind it was never found, see
  `2026-10-24-cranelift-sightglass.md`).
* `built_in_place` (the `FRAGCOPY_OUT` rule) under the same knob also
  admits syncable pins, copies *in* (the arg-temp pass-through shape)
  and `outs >= 1` — each extra copy out is independently rewritten
  into stores on its own destination.

Correction to the working notes: `env_is("PLIRON_X", "1")` compares
the *value*, so `PINFULL` at `"1"` was default-off all along — there
was no default flip needed before the opt-in commit; the flip is the
`DEFAULT_ON` entry as usual.

## `bf_ubfiz_immr` ICE — fuzz seed 20430 (2a41025c)

`harness/quick.py` found a latent vendored bug unrelated to sroa:
`bf_ubfiz_immr(ty, k)` computed `(rs - k)` inside `then_some`, which
evaluates eagerly — an `ishl` `iconst` >= 64 reaching ISLE *unmasked*
underflowed `64 - 100` at `isle.rs:1928` and ICE'd at O0. The reach
is real: cranelift's own preopt masks shift constants, but only at
`opt_level != none`; our pipeline (or any pass emitting `ishl` consts
late) can hand ISLE a raw `iconst.i64 200`. Same eager-eval class as
`ivretro::scaled`'s `1i64 << c` panic (ad61964e, earlier today).
`bf_ubfiz_imms`/`bf_ubfx_imms` `k + w` now `checked_add` — a wrapped
sum would pass `<= bits` and emit a *wrong* immediate in release, so
the debug panic was a canary for a release miscompile path. Test:
`oversized_shift_const_lowers` in `tests/aarch64_bitfield.rs` at
`opt_level=none` (panicked on the unfixed helper); seed 20430 in
`FUZZ_REGRESS`.

## Still open

* html residual: +8.1% instr / +5.6% cycles — spread over `Vec` field
  reloads, `copy_nonoverlapping`'s inline cascade (LLVM tails into
  `_platform_memmove`), the `call_mut` vtable shim (~1%), and
  remaining sroa refusals (`r6` empties, `PINWHOLE` whole-copies).
* The `ExecCtx` copy (`v2966`) split shows `built_in_place`'s write-
  through shape works; `v30085`/`v30078` are the arg-temp half.
* callgrind / OrbStack still unavailable, so per-instruction splits
  on the held-out programs remain estimate-based.
