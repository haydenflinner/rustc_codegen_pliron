Pliron patch: x86-64 TLS @-modifier relocations.

Upstream rsasm 0.1.3 maps `tpoff`/`dtpoff`/`gottpoff`/`tlsgd`/`tlsld` to
relocations only for the i386 ABI; on x86-64 they fell back to a plain
absolute reloc (R_X86_64_32S), which fails to link in PIE objects and is
semantically wrong for TLS.

Changed:
- src/arch/x86/reloc.rs: added x86-64 TLS constants (DTPMOD64=16,
  DTPOFF64=17, TPOFF64=18, TLSGD=19, TLSLD=20, DTPOFF32=21, GOTTPOFF=22,
  TPOFF32=23).
- src/arch/x86/mod.rs `modifier_reloc`: x86-64 entries for `tpoff`
  (TPOFF32/TPOFF64), `dtpoff` (DTPOFF32/DTPOFF64), `dtpmod` (DTPMOD64),
  `gottpoff` (GOTTPOFF), `tlsgd` (TLSGD), `tlsld`/`tlsldm` (TLSLD).

`modifier_symbols` already marked these TLS and adds
`_GLOBAL_OFFSET_TABLE_` where needed, matching gas behavior.

Pliron patch: AArch64 CRC32 instructions.

`crc32{,c}{b,h,w,x}` (FEAT_CRC32) were unknown mnemonics; zlib-rs's ACLE
path writes them in `asm!` (uiua build, 2026-10-21).

Changed:
- src/arch/aarch64/insn.rs: `crc32` encoder (two-source data-processing,
  opcode `010 C sz`; `x` forms take Xm and set sf), listed in `handwritten`.
- tests/aarch64.rs: `crc32` encodings checked against llvm-mc; three of the
  mnemonics join the random-operand fuzz list.

Pliron patch: AArch64 table covers the scalar extensions.

`tools/tables/aarch64.py` kept a scalar (GPR-only) form only when the same
mnemonic also had a SIMD/SVE form, so the atomics and pointer auth were
neither generated nor hand-written: portable-atomic's `asm!` (`ldar`,
`swpp`, `.arch_extension lse`) failed rune's build. The filter now keeps
every mnemonic the backend does not encode by hand. Regenerated with llvm-mc
22.1.8: 5879 -> 6443 forms (LSE/LSE128 atomics, acquire/release and
exclusive loads/stores, RCPC/RCPC3, pointer auth, RCW, MOPS-free scalar
extras), 0 lines in the wrong form.

Changed:
- tools/tables/aarch64.py: the post-sweep filter keeps mnemonics with no
  handwritten encoder; `crc32*`/`casp*` join HANDWRITTEN; `EXCLUDED` drops
  `ldraa`/`ldrab` (the writeback forms' split signed offset put 8 checked
  lines in the wrong form).
- src/arch/aarch64/table_data.rs, tools/mc-diff/*: regenerated.
- src/arch/aarch64/insn.rs: `casp{,a,l,al}` by hand (even/odd register pairs,
  the second register unencoded — not a relation the fitter can measure).
- src/arch/aarch64/mod.rs: `.arch_extension` accepted (every extension is on).
- tests/aarch64.rs: `casp_pairs`, `atomics_from_the_table`,
  `arch_extension_is_accepted`, against llvm-mc encodings.

Pliron patch: leading-zero local label references.

`01b` (GAS numeric local label `01`, as rust-lang/rust
tests/ui/asm/binary_asm_labels_allowed.rs writes it) was rejected as an
invalid octal literal: the local-ref path skipped runs with a leading zero.
src/lexer.rs `lex_number` now takes such runs as a reference to the label
with the run's octal value, which is how GAS reads it and how the `01:`
definition lexes.
