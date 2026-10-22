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
