# Patches vs crates.io cranelift-jit 0.135.5

## `src/compiled_blob.rs`: x64 call veneers

- `CompiledBlob::new` reserves veneer space for `Reloc::X86CallPCRel4`
  (previously only `Reloc::Arm64Call`).
- `perform_relocations`: an out-of-range `X86CallPCRel4` no longer panics
  (`i32::try_from` overflow); the call is retargeted to a veneer appended
  after the code: `ff 25 00 00 00 00` (`jmp qword ptr [rip+0]`) + 8-byte
  absolute target. The retargeted rel32 counts from the field end
  (`veneer - at - 4`). `X86PCRel4` (data refs) still panics — a veneer can't
  reproduce the target's address for `lea`.

Motivation: JIT modules on Windows resolve imports (ucrtbase.dll, the host
exe) more than ±2GB from `VirtualAlloc`'d code memory. Without veneers every
external call overflows rel32 at link time.
