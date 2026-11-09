# Instruction-count bloat: a map (utf8_valid_up_to, montgomery, decompress)

Held-out census work, fresh post-BJALIAS/narrowext-32/bfuse-fix binaries.
Three gap archetypes, in order of how much they seem to matter:

## 1. Edge-block proliferation → regalloc boundary moves

Our pipeline keeps fine-grained CFGs (`edgespec`, `coldedges`,
`edgesink`, `splink` all mint dedicated edge blocks). Every block arg
materializes as regalloc moves at the boundary — the
"scalar_iterate_special_bytes" class. Biggest lever seen:
`splink` added +708 CLIF insts inside `miniz_oxide::decompress` alone.

`decompress`: 3357 final-CLIF insts → 5446 machine insns (1.62x
expansion); 1831 vs 378 `[sp]`/`[x29]` refs vs LLVM. The explicit_slot
count is a red herring — they're dead size-0 decls; the stack traffic
is regalloc2 spills on a function 2.5x LLVM's size.

## 2. Loop-variant duplication (punroll + late looprot + edgesink)

`utf8_valid_up_to`: 686 vs 220 machine insns.
`PLIRON_PUNROLL=0` → 311. Growth timeline: punroll 68→129 blocks,
late looprot 104→139, edgesink →181. The punrolled loop is the scalar
"skip ASCII" scan — its data-dependent `tbnz` mid-exit stays per-copy,
so unrolling amortizes only the `pos<len` latch while cloning ~8 exit
cones. `montgomery`: 864 vs 596 (punoff) vs 308 (LLVM); inner mul
loop itself matches LLVM 11-insn carry chain — the 2x is variant
copies and glue.

Held-out A/B `punoff/pliron` = **0.997 [0.971,1.026]** — net wash:
helps compression (0.942)/blake3 (0.958), hurts blind-sig
(**1.052***)/regex (1.055). Punroll stays for now; the cost model has
no mid-exit term — `unroll_width` looks only at body insts. If it is
revisited, count exit-cone clone insts against latch savings.

## 3. Cross-CGU inlining attribution (pliron-only symbols)

`Vec<u8>::extend_trusted` (+7 samples, 172-insn standalone vs inlined
by LLVM), `RawVecInner::try_allocate_in` (+5, 41-insn shim),
`DispatcherDelegate` (+7, vtable-attributed). `XCGU_SIZE=50` MIR-stmt
cap blocks `has_body=false` callers; a prior `=150` probe inlined the
encoding_rs copy but timing was noise-level.

## What is not the gap

- `utf8_valid_up_to` bounds checks: final CLIF is `load.i8 notrap` —
  the checks are all eliminated already; LLVM wins the same way.
- `utf8` SWAR inner loop + montgomery mul chain: instruction-for-
  instruction equivalent to LLVM (11 insns/iter, same carry chain).
- narrow `uxt` waste: fixed by NARROWEXT bit 32 (`orr w,w,w,lsr #k`
  fusions live, c1fc7d8).

## Numbers

Held-out geomean 1.054 (3 rounds, 11-10). Per-program: html-rewriter
1.137, compression 1.116, cmark 1.078, blind-sig 1.049, blake3 1.024,
regex/json/protobuf ~1.01.
