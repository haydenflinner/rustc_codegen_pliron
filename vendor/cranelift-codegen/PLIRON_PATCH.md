# Vendored cranelift-codegen 0.135.5

Vendored (like `vendor/regalloc2`) only for small x64 backend changes we want
tight control over; each is marked `PLIRON:` in the source. Submit upstream
once proven out on the stage2 benchmark, then drop this copy.

1. `src/isa/x64/abi.rs`: leaf functions with no calls, clobbers, stack slots
   or stack arguments get no `push rbp; mov rbp, rsp` / `mov rsp, rbp; pop rbp`
   (aarch64 already omits the frame this way). Skipped when
   `preserve_frame_pointers` is set, i.e. `PLIRON_OMIT_FP=0` or a target that
   requires frame pointers.
2. `src/isa/x64/lower.isle` (+ `jump_table_size` made `pure` in
   `src/prelude_lower.isle`): a `br_table` whose index is `band x, k` with
   `k < table size` skips the `cmp; cmov` bounds clamp. Our `dense_switch`
   emits that form (`PLIRON_SWITCH_MASK`) when the switch default is
   `unreachable`, padding the table to a power of two.
3. ELF initial-exec TLS on x64: `tls_model = "elf_ie"` (added to
   `vendor/cranelift-codegen-meta/src/shared/settings.rs`) lowers `tls_value`
   to `mov %fs:0, r; add sym@gottpoff(%rip), r` (`Inst::ElfTlsIe`) instead of
   a `__tls_get_addr` call. New `Reloc::ElfX86_64GotTpOff`, mapped to
   `R_X86_64_GOTTPOFF` in `vendor/cranelift-object`; both of those crates are
   vendored only for this.

## x64 PIC calls use PLT32

`CallKnown`/`ReturnCallKnown` emit `R_X86_64_PLT32` instead of `R_X86_64_PC32`
when `is_pic` is set (`isa/x64/inst/emit.rs`). Together with the backend marking
direct-call `FuncRef`s colocated (`PLIRON_PLTCALL`), calls to symbols from other
CGUs become `call rel32` (the linker resolves PLT32 directly when the definition
is local, else through the PLT) instead of a GOT load plus `call *reg`.
