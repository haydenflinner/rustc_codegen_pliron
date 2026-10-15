# Why cranelift-codegen-meta is vendored

Vendored from crates.io cranelift-codegen-meta 0.135.5 so new CLIF
opcodes can be declared. Changes, all in `src/shared/instructions.rs`:

1. `sdot`, `udot`, `udot_acc` pairwise dot-product opcodes (marked
   `PLIRON:`). `sdot`: signed i16xN pairwise dot -> i32xN/2 (x86
   pmaddwd). `udot`: unsigned x * signed y -> i16xN/2 saturating
   (x86 pmaddubsw). `udot_acc`: four-wide u8x4N * i8x4N accumulate
   into i32xN (x86 vpdpbusd); its byte operands are free typevars
   since the 4x lane relation is not expressible with one derived
   typevar level. See cranelift-codegen patch 26 for the x64 rules.

2. `vswizzle`, `vswizzle2` runtime-index swizzle opcodes (marked
   `PLIRON:`). `vswizzle x idx` = `a[i] = x[idx[i] mod N]`;
   `vswizzle2 x y idx` = `a[i] = (x++y)[idx[i] mod 2N]`. The index
   operand is a free typevar (`IdxVec`) — the lane-count/width
   relation to the data vector is enforced in lowering. See
   cranelift-codegen patch 27 for the x64 rules.
