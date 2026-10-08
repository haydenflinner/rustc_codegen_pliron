# Vendored cranelift-codegen 0.135.5

Vendored (like `vendor/regalloc2`) only for small x64 backend changes we want
tight control over; each is marked `PLIRON:` in the source. Submit upstream
once proven out on the stage2 benchmark, then drop this copy.

1. `src/isa/x64/abi.rs`: leaf functions with no calls, clobbers, stack slots
   or stack arguments get no `push rbp; mov rbp, rsp` / `mov rsp, rbp; pop rbp`
   (aarch64 already omits the frame this way). Skipped when
   `preserve_frame_pointers` is set, i.e. `PLIRON_OMIT_FP=0` or a target that
   requires frame pointers.
