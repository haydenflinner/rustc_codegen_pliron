# SEH handoff — where to pick up (2026-10-23 session end)

Full design: `docs/seh-design.md`. Read that first.

## Landed

- `__pliron_seh_personality` — `src/lib.rs` `codegen_allocator`,
  built in pliron IR behind `PLIRON_SEH=1`. Claims phase-1 against the
  scope table, stashes `{exn,throwinfo,base}` into `__pliron_eh`
  +8/+16/+24, sets `dc->TargetIp`. Compiles; not yet referenced.
- Offenders fixed this session: rusttailcc byval (7b2a04c), cvariadic
  CRT-free link (751e535), non-local `extern "tail"` conv (dcd91d2).
- Gate was 53/53 green at 751e535; the stub + tail_cc change also gated
  green. Default path unaffected (`PLIRON_SEH` off).

## Remaining work (in order)

1. **`pliron-clif-lower/src/eh/mod.rs`** — in `add_function`'s `coff`
   branch, collect `code.buffer.call_sites()` entries that have
   exception handlers into scope records `{begin=ret_addr-1,
   end=ret_addr+1, target=Tag lp offset}` (fn-relative). In
   `emit_coff`, for fns with records: `buf[0] |= 1<<3`
   (`UNW_FLAG_EHANDLER`), append `u32` handler RVA reloc to
   `__pliron_seh_personality` (module symbol via
   `m.get_name`/`product.function_symbol`) + `u32 count` + records;
   each record field is an `ImageOffset` reloc against the fn symbol
   with addend=offset (same `RelocationFlags::Generic` pattern as the
   existing pdata relocs).
2. **`pliron-clif-lower/src/lower.rs`** — seh mode when
   `eh_data.is_some() && PLIRON_SEH`:
   - `try_call` (~line 5358): skip the emulated `call + eh_check` arm,
     take the real `try_call` path. `pcall` block args =
     `sig.call_conv.exception_payload_types(pt).len()` TryCallExn's
     (0 under fastcall, 2 under Tail). If 0, bind `self.exn` from a TLS
     `__pliron_eh`+8 load instead of a block param (the personality
     stashed it there).
   - `_Unwind_Resume` interception (~5759): in seh mode emit
     `libcall("_CxxThrowException", [pt,pt], [], [exn, ti])` where
     ti = load(tls+16), then trap — a real rethrow so foreign outer
     frames / stock catches see it.
   - `pliron.eh.rethrow` (~6639): same.
   - `_CxxThrowException` interception (~5760): skip it in seh mode —
     real throws stay real.
   - `eh_check` for non-invoke maythrow calls (~5814): skip in seh mode.
3. **Tests** — run `tests/unwind`, `dseunwind`, `unwindmem`,
   `unwindstd`, `invokeblk` run-half with `PLIRON_SEH=1` (stanzas stay
   gated until it's default-on). Repro: `target/tc/seh2.rs` prints
   `caught=true` when working (currently exits 101).
4. Then flip default (separate commit per rule 2), ungate stanzas.

## Known caveats to revisit

- Claims any `0xE06D7363` — foreign C++ throws get caught as Rust
  panics; filter by RustPanic TypeDescriptor name via ThrowInfo
  (params[2]/params[3]) — details in the design doc.
- Cleanup pads claim then rethrow — a superset of MSVC terminate
  semantics; fine for Rust.
- `PLIRON_SEH` path is debug-only-landed; corpus/diffcheck may not
  cover try_call — expect fuzz cycles.
