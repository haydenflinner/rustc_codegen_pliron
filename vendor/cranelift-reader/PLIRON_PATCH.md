# Vendored cranelift-reader 0.135.5

Local changes vs the crates.io release, all marked `PLIRON:`:

- `src/parser.rs`: a `Scatter` instruction-format arm for the `vscatter`
  CLIF opcode (`vscatter flags x, p, idx+off` — three SSA values plus an
  offset32). The parser matches exhaustively on `InstructionFormat`, so
  the new variant added for `vscatter`/`vgather` in
  `vendor/cranelift-codegen-meta` (patch 3) needs this crate patched.

Drop this copy once the opcodes are upstream or no longer vendored.
