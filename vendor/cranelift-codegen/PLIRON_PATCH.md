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

4. aarch64 `udot` (FEAT_DotProd): new `VecALUModOp::Udot` variant (same
   encoding as `sdot` plus the unsigned bit), `udot` helper in
   `isa/aarch64/inst.isle`, and a `lower.isle` rule folding the
   `iadd_pairwise(uwiden* lo) + iadd_pairwise(uwiden* hi) + acc` tree —
   the shape our loopvec emits for `u8 * u8 → i32` widening dot products.
5. aarch64 `mla`/`mls`: new `VecALUModOp::{Mla,Mls}` variants (same
   `vec_rrr_mod` encoding shape as `sdot` but with element-size bits; `.2d`
   excluded — integer `mla`/`mls` have no i64-lane form), `mla`/`mls` helpers
   in `inst.isle`, and `lower.isle` rules folding vector `iadd(acc, imul a, b)`
   in either operand order plus `isub(acc, imul a, b)` (non-commutative, one
   order only). The two `iadd` orders sit at distinct priorities (10/11) so
   the ISLE overlap checker can prove them disjoint from each other and from
   the scalar `madd`/`dot` folds; `iadd(imul, imul)` matches only the first,
   leaving the second product as the accumulator operand.
6. aarch64 `smlal`/`umlal` widening MAC: new `VecRRRLongModOp::{Smlal8,16,32}`
   variants (`umlal` already existed for 8/16/32), helpers, and `lower.isle`
   rules folding `iadd(acc, iadd(imul(widen_lo a, widen_lo b),
   imul(widen_hi a, widen_hi b)))` into `smlal`+`smlal2` at priorities
   12-15. Lane-exact because the inner `iadd` is elementwise; loopvec emits
   this shape for widening dot products at i8→i16/i16→i32/i32→i64, i.e. the
   widths with no `sdot`-family instruction.
7. aarch64 `shuffle` rotation → `ext`: `lower.isle` rule (priority 7) plus
   the `vec_rotate_imm4_from_immediate` extractor in
   `isa/aarch64/lower/isle.rs` — a mask `bytes[i] = (n+i) mod 16` rotates a
   single vector by `n` bytes, which is `ext a, a, n`. All mask bytes are
   < 16 so the second operand is never referenced. Turns the loopvec
   descending-stream lane reversal for two-lane vectors (e.g. `u64x2`) into
   one instruction instead of a mask load + `tbl`.
8. aarch64 `shuffle` full lane reversal → `rev64`+`ext`: `lower.isle` rules
   (priority 8) for the i8x16/i16x8/i32x4 reversal masks. `rev64` flips the
   element order within each 64-bit group and `ext #8` swaps the halves —
   two instructions, no literal-pool mask and no `tbl` table register
   dependency. Used by loopvec's descending streams; identical to what LLVM
   emits for `shufflevector` reversals.

## x64 PIC calls use PLT32

`CallKnown`/`ReturnCallKnown` emit `R_X86_64_PLT32` instead of `R_X86_64_PC32`
when `is_pic` is set (`isa/x64/inst/emit.rs`). Together with the backend marking
direct-call `FuncRef`s colocated (`PLIRON_PLTCALL`), calls to symbols from other
CGUs become `call rel32` (the linker resolves PLT32 directly when the definition
is local, else through the PLT) instead of a GOT load plus `call *reg`.
