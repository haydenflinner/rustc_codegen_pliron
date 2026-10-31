# Why cranelift-object is vendored

Vendored from crates.io cranelift-object 0.135.5 for two changes, each marked
`PLIRON:` in `src/backend.rs`. Both serve patches described in
`vendor/cranelift-codegen/PLIRON_PATCH.md`:

- `Reloc::ElfX86_64GotTpOff` maps to `R_X86_64_GOTTPOFF`, for initial-exec
  ELF TLS (`tls_model = "elf_ie"`, item 3).
- `define_function_inner` raises each function symbol's alignment to the
  ISA's `function_alignment().preferred`, so buffer-relative loop-header
  alignment (item 16) holds in the final image.
