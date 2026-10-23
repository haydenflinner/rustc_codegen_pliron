# Real SEH catch for windows-msvc — design (investigated 2026-10-23)

## Why catch_unwind fails today

`catch_unwind` → `std::panicking::r#try` is *generic*, so it is compiled
by pliron into the caller's object — its `invoke` becomes our emulated
TLS-flag check (`eh_data` is always `Some` on COFF), which a real SEH
`_CxxThrowException` bypasses. `std::rt::lang_start` is non-generic and
lives in the stock std object — its LLVM C++-EH catch fires fine through
our `.pdata`/`.xdata` frames (verified: uncaught panic exits 101 via its
catch, not a crash).

Verified on the Windows box: `catch_unwind(|| boomer())` prints the panic
and exits 101; the `try` pad never runs.

## The model: claim + JumpTarget, no funclets

LLVM needs funclets because MSVC C++ EH *calls* pad code on the
unwinder's stack (`RtlpExecuteHandlerFor*`) — locals must be rebuilt via
the establisher frame in `rdx`. Our pads are mid-function blocks, so we
use the `__except`/`JumpTarget` mechanism instead: our own personality
claims in phase 1, sets `DispatcherContext->TargetIp`, and the
dispatcher resumes the frame's *own* context at the call site — exactly
what a post-call landing block expects. No funclets, no FuncInfo/UnwindMap
tables, no state machine.

Every invoke-scope record claims in phase 1. Catch pads return normally;
cleanup pads run drops then rethrow (`_Unwind_Resume` → real
`_CxxThrowException` of the stashed object+throwinfo). The rethrow
re-dispatches; our own frame's rethrow call-site is outside every scope
record, so the unwind correctly continues to the next outer handler.
Drops therefore run even for terminate-at-top — a superset of MSVC
semantics, fine for Rust.

## Pieces

Status: **`__pliron_seh_personality` landed** (0ffd64e, built in pliron
IR in codegen_allocator behind `PLIRON_SEH=1`, unused until #1 and #4
land). Remaining:

1. **`.xdata` EHANDLER + scope table** (`pliron-clif-lower/src/eh/mod.rs`,
   `emit_coff`): for functions whose `code.buffer.call_sites()` have
   exception handlers, set `UNW_FLAG_EHANDLER` in the emitted UNWIND_INFO,
   append `u32 handler_rva` (reloc `__pliron_seh_personality`, kind
   ImageOffset like the existing pdata relocs) and handler data:
   `u32 count` + `count` records
   `{u32 begin_off, u32 end_off, u32 target_off, i32 exn_delta}` —
   offsets relative to function start, ImageOffset relocs to the fn
   symbol. `begin = ret_addr - 1`, `end = ret_addr + 1` (covers only the
   call's return address = the frame's ControlPc). `target` = the
   handler's buffer offset (`FinalizedMachExceptionHandler::Tag(tag,
   off)`), `exn_delta` = `sp_offset - frame_offset` for Context handlers
   (or 0 if unused).

2. **`__pliron_seh_personality`** — asm stub via `st.asm`/rsasm, emitted
   once (allocator-module path like `__pliron_eh`; name must be stable
   across CGUs — COFF compiler-stub naming gets per-CGU suffixes, so emit
   in the same place `__pliron_eh` is defined, `codegen_allocator` in
   lib.rs). Signature: `(rcx=ExceptionRecord*, rdx=EstablisherFrame,
   r8=ContextRecord*, r9=DispatcherContext*) -> i32`.
   - flags test: `ExceptionRecord->flags (@4) & 0x26` → return 1
     (ExceptionContinueSearch) — unwind-phase calls are no-ops.
   - only claim `code (@0) == 0xE06D7363` (MS C++ EH). Ideally also
     require a `RustPanic` catchable type: params[2]=ThrowInfo*,
     params[3]=image base; `*(u32*)(ti+12)` = rva of CatchableTypeArray
     {u32 n, u32 rva[]}; per type `*(u32*)(ct+4)` = rva of
     TypeDescriptor; name at td+16, match ".?AURustPanic@@". Without
     this, foreign C++ throws get caught as Rust panics — wrong but
     rare; acceptable v1 behind the flag.
   - `pc_off = dc->ControlPc(@0) - dc->ImageBase(@8)`; walk records
     `handler_data = dc->HandlerData(@56)` (`u32 n`, then n×16B);
     match `begin <= pc_off < end`.
   - on match: write exn (`params[1] @ +40` in ExceptionRecord) into the
     frame slot `*(establisher + exn_delta)` (or TLS `__pliron_eh.exn`,
     see below); stash `{params[1], params[2]}` for rethrow (TLS buf or
     a second slot); `dc->TargetIp(@32) = image_base + target`; return 4
     (ExceptionExecuteHandler). No match → return 1.

3. **exn delivery — the Context channel, not payload regs.**
   `exception_payload_regs(WindowsFastcall)` and its
   `exception_payload_types` are both `[]` (abi.rs:1087, call_conv.rs:118),
   so `TryCallExn` block params can't bind under the platform conv.
   Use `ExceptionTableItem::Context(value)` instead: it becomes
   `MachExceptionHandler::Context(ExceptionContextLoc::SPOffset(off))`
   (machinst/abi.rs `TryCallInfo::exception_handlers`); the pad loads
   `self.exn` from that stack slot. Personality writes
   `*(establisher_frame + sp_offset - call_site.frame_offset) = exn`.
   (`frame_offset` = FP→SP delta at the call, already in the call-site
   record.)

   TLS alternative (probably simpler): personality stores exn into
   `__pliron_eh.exn` and `{throwinfo,imagebase}` into its 32B buf
   (layout `{flag:i32,pad:i32,exn:ptr,buf:32B}`, lib.rs ~1260); pad
   reads exn via `eh_tls()`. Stub then needs TLS access
   (`gs:[0x58]` + `_tls_index` + secrel — check rsasm COFF secrel
   support; `@SECREL` unverified).

4. **lower.rs changes** — third EH mode behind `PLIRON_SEH=1`
   (default-off per rule 2):
   - `eh_data`: keep `Some` for the TLS symbol if the TLS channel is
     used; the `try_call` emulated-EH branch (`lower.rs ~5358`) keys off
     `eh_data.is_some()` — split it so seh-mode emits real
     `b.ins().try_call` (the ELF path, already exercised on Linux).
   - pad exn binding: `pre`-block `TryCallExn(0)` only works with
     payload regs; in seh mode bind `self.exn` from the Context slot /
     TLS load instead.
   - `_Unwind_Resume` interception (~5755) and `pliron.eh.rethrow`
     (~6639): in seh mode emit a call to
     `_CxxThrowException(load exn, load throwinfo)` from the stash —
     NOT the flag raise. Do not intercept `_CxxThrowException` calls at
     all in seh mode (real throws stay real).
   - `eh_raise`/`eh_check` emulated path unused in seh mode.

5. **Ungate in test.sh**: `unwind`, `dseunwind`, `unwindmem`,
   `unwindstd`, `invokeblk` run-half; also `tests/std` `pliron_stock_seh`
   catch asserts.

## Semantics notes

- `catch_unwind` catches only RustPanic — the type-name filter above;
  plain `0xE06D7363` claim is the v1 approximation.
- `ExceptionTableItem::Tag` (EXCEPTION_HANDLER_CLEANUP=0 / CATCH=1)
  distinction is unused by our handler — pads self-distinguish (cleanup
  pads rethrow).
- try_call to a tail-CC callee clobbers every register — unchanged
  caveat from internal_fns.
- wasm path untouched (`eh_data`/`wasm` logic is separate).
