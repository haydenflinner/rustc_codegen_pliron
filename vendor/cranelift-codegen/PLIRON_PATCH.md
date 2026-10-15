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
7. aarch64 logical-op shifted-operand fusion: `lower.isle` rules folding
   `band`/`bxor` of i32/i64 with an `ishl`/`ushr`/`sshr`/`rotr`/`rotl`
   immediate operand into `and`/`eor` with a shifted register operand
   (`eor x, y, z, ror #k`, as LLVM emits for SipHash rounds). New
   `rotr_from_u64` extern in `lower/isle.rs` + decl in `inst.isle`.
   `rotl x, k` lowers as `ror x, bits-k`. `bor` deliberately skipped: ten
   priorities can't fit below `extr_32_or_64`'s 5 without shadowing it.
   ROR is only valid in the logical (shifted register) encoding — never
   extend this to `iadd`/`isub` (their shifted-register form has no ROR).
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
9. aarch64 widening adds: new `VecRRRLongOp::{Saddw,Uaddw,Saddl,Uaddl}8/16/32`
   variants (`enc_vec_rrr_long` generalized from a `bit14` flag to the full
   six-bit opcode field — `uaddw`/`uaddl` live at opcodes `0b000100`/`0b000000`,
   next to `umlal`/`umull`). `lower.isle` rules fold `iadd(acc, widen(x))` →
   `uaddw`/`saddw` (+`2` for the high half) in both operand orders,
   `iadd(widen a, widen b)` of equal halves → `uaddl`/`saddl`, and
   `iadd(acc, iadd(widen_lo x, widen_hi x))` → `uaddw`+`uaddw2` chained on
   `acc`, at priorities 20-24 (above the `smlal`/`umlal` block; each operand
   order needs its own priority since the orders overlap on
   `iadd(uwiden,uwiden)`). loopvec's widening reductions emit
   acc-chained `iadd(acc, widen half)` trees into per-half accumulators —
   the exact shape LLVM produces for `sum += (x as u64)*(x as u64)`.

10. aarch64 `ldp`/`stp` fusion at emission: new `MachInstEmit::fuse_with_next`
    hook (default `None`), called from `VCode::emit` when the next block item
    is the next instruction — no intervening regalloc edit, whose moves could
    clobber either inst's registers. The AArch64 impl folds two adjacent
    `UnsignedOffset` loads/stores of equal size and flags on the same base
    register into `LoadP64`/`StoreP64`/`FpuLoadP64`/`FpuStoreP64`/
    `FpuLoadP128`/`FpuStoreP128` with a `PairAMode::SignedOffset` amode
    (offsets may be in either order; `rt` takes the lower slot). The second
    inst is squashed to `Nop0`, keeping inst indices stable for srclocs and
    regalloc edits; its operands are pre-applied so the fused inst carries
    physical registers. Loads require distinct destination regs (`ldp` with
    `rt == rt2` is unpredictable).

11. aarch64 pair fusion across pure-ALU gaps: a second
    `MachInstEmit::pair_fusion_crossable` hook whitelists pure
    register-dataflow insts (no memory, control flow, traps, calls or hidden
    state). `VCode::emit` scans up to 8 insts ahead for a fusable partner
    (regalloc edits still stop the scan). Post-alloc use/def regs are
    recorded per inst during allocation application — `get_operands` skips
    already-physical regs, so they cannot be collected after the fact. The
    fused pair emits at one of two slots: early (partner moves up — unsafe
    when a gap inst writes a reg the partner touches or reads one it
    defines, or when the partner reads this inst's defs since pair uses
    precede defs) or late (this inst moves down — symmetric hazard set on
    this inst's regs). The `fmul; str` interleave produced by
    demand-driven lowering fuses via the late placement, yielding
    `ldp/fmul/stp` blocks for vectorized map loops.

12. x64 `psadbw`: new CLIF op `x86_psadbw` (`i8x16,i8x16 -> i64x2`, added in
    `vendor/cranelift-codegen-meta/src/shared/instructions.rs`), plus
    `x64_psadbw` in `isa/x64/inst.isle` and a `lower.isle` rule (the splat-0
    operand folds to `xmm_zero`). The encoding itself is added in vendored
    `cranelift-assembler-x64-meta` (`instructions/avg.rs`); codegen-meta's
    assembler.isle generator emits `x64_psadbw_a_or_avx` automatically.
    loopvec emits `iadd(acc, x86_psadbw(chunk, splat 0))` for unsigned
    u8→u64 widening sums — one instruction per 16 bytes instead of a
    14-unpack widen tree.

13. direct-use side-effect sinking (`src/machinst/lower.rs`):
    `compute_use_states` additionally returns `value_direct_uses` — raw
    argument-position use counts, *not* coarsened by the transitive
    `Multiple` propagation. The side-effect branch of
    `get_value_as_source_or_const` accepts a def as `UniqueUse` when either
    upstream's `value_ir_uses == Once` holds, or the value has exactly one
    direct use, that use is `cur_inst` itself, and nothing has already
    materialized the value (`value_lowered_uses == 0`). Upstream's `Once`
    is poisoned to `Multiple` whenever a consumer's result fans out (e.g.
    a scalar `fadd` accumulator feeding both backedge and exit args), so
    `load; fadd` could never fuse into `addss (mem), %xmm` in a reduction
    loop. The two extra tests are load-bearing: `cur_inst`-is-user rejects
    probes that reach the value through a still-live pure sub-pattern
    (`(store (iadd (load l) k) addr)` matching `l`), and the
    `lowered_uses` test rejects sinking after an earlier-lowered inst
    legitimately put the value in a register — fusing then would leave
    that register's definition unemitted.

## x64 PIC calls use PLT32

`CallKnown`/`ReturnCallKnown` emit `R_X86_64_PLT32` instead of `R_X86_64_PC32`
when `is_pic` is set (`isa/x64/inst/emit.rs`). Together with the backend marking
direct-call `FuncRef`s colocated (`PLIRON_PLTCALL`), calls to symbols from other
CGUs become `call rel32` (the linker resolves PLT32 directly when the definition
is local, else through the PLT) instead of a GOT load plus `call *reg`.

14. aarch64 `add`/`lsr` fusion (`src/isa/aarch64/lower.isle`): `iadd` with
    a logical-right-shift immediate operand lowers to `add x, y, z, lsr #k`
    (priorities 20/21, above the `iadd_imul` folds at 7 but below the
    imm12 rules). Hashbrown's probe-index `off + (hash >> 3)` pattern.

15. `PLIRON_PRECOMPILE` debug hook (`src/context.rs`): dumps each func's
    CLIF inside `compile_stencil` after `optimize()` — the exact form
    `compile_function`/isel consumes, unlike the backend-side `PLIRON_CLIF`
    `final` dump which precedes `remove_constant_phis`/egraph.

16. x64 loop-header 16-byte alignment (`src/machinst/vcode.rs`): blocks
    targeted by back-edges in the final emission order get `gen_nop`
    padding before their label binds, like LLVM's `.p2align 4` loop tops.
    `PLIRON_ALIGN=0` disables. A companion fix lives in vendored
    `cranelift-object` (`backend.rs`): `define_function_inner` maxes the
    ISA's `function_alignment().preferred` (32 on x64) into symbol
    alignment — the buffer minimum is usually 1, so without it functions
    land at arbitrary offsets and buffer-relative 16-alignment means
    nothing in the final image.

17. x64 `icmp` memory-operand compares (`src/isa/x64/inst.isle`): a
    sinkable `load` on the LHS of `icmp` is swapped to the RHS (`cmp reg,
    [mem]` + `intcc_swap_args`); against a constant it instead lowers to
    `cmp r/m, imm` via new `x64_cmp_mem` helper; `is_nonzero` of a sinkable
    load (or `uextend` of one) lowers to `cmp $0, mem` instead of
    materializing for `test reg, reg`. Both forms use
    `sinkable_load_exact` (narrow loads read at their own width) so byte
    scans like `is[i]`/`cmpb $0, mem; jne` fold to a single memory compare.

18. Alias-chain hardening (`src/ir/dfg.rs`): `compact_aliases` rewrites
    each alias to point at its fully-resolved terminal (skipping chains
    that dead-end at reserved/tombstoned targets) so per-use
    `resolve_aliases`/`value_def` calls stay O(1) after merge-heavy
    passes. `try_value_def` returns the shallow def without chasing
    aliases and `None` for invalid/reserved entries; `try_resolve_aliases`
    returns `None` on invalid, tombstone-ended, or cyclic chains instead
    of panicking — pipeline carry logic iterates all values and must not
    panic on stale pairs.

19. `Context::egraph_safe` (`src/context.rs`): the ISLE prelude's
    `ty_smin`/`ty_smax`/`ty_mask`/`ty_lane_mask` `expect` ≤64 — any
    >64-bit scalar (rustc's routine i128) or >64-lane vector panics the
    whole egraph pass. `optimize` and the pipeline's opt-in egraph step
    skip functions containing such values. `PLIRON_COMPILE_EGRAPH=0`
    disables the compile-path egraph entirely for A/B measurement —
    it still earns ~7-13% runtime on hash/string kernels as of the
    last benchmark, so it stays on by default.

20. Encoding-aware ALU-immediate remat (`src/opts/remat.isle`,
    `src/opts.rs`, `src/prelude_opt.isle`): the ALU-op-with-`iconst`
    remat rules fired unconditionally, marking e.g. `bxor v, 0x7465...`
    for re-emission at every use block. A remat only pays when the
    immediate folds into the target operand — otherwise the constant
    still needs a register *and* the op re-executes per use (hmget's
    lookup loop re-xored a hasher key with a full-width constant every
    iteration). `imm64_addsub_imm` gates iadd/isub on the 12-bit
    (optionally shifted) add/sub immediate form; `imm64_logic_imm`
    gates band/bor/bxor on logical-immediate encodability (replicated
    element pattern that is one cyclic run of set bits). Conservative
    on targets with wider immediate fields — only a neutral remat is
    lost.

21. `PLIRON_STATS=1` also prints per-function `EgraphPass` stats in
    `egraph_pass` (`src/context.rs`; previously only reachable via
    `log::debug!`). Used to size egraph's contribution: scoped-GVN hits,
    alias-eliminated loads (~0 — nothing emits `alias_region` metadata),
    licm hoists, and `elaborate_remat` clone counts.

    A loop-aware variant of `maybe_remat_arg` (pin outer-loop constants /
    redirect in-loop constant clones to the preheader) was implemented
    and benchmarked, then reverted: the eliminated per-iteration `movz`s
    were ~free while the pinned live ranges spilled measurably worse
    (hashmap `main`: 96 → 121-178 stack ops; upstream won or tied every
    A/B). Upstream's per-use-block cloning is already the pressure-
    optimal placement; remat wins have to come from *what* gets marked
    (patch 20's encodability gate), not *where* clones land.

22. x64 wide-vector boolean ops (`vendor/cranelift-assembler-x64-meta/
    src/instructions/wide.rs`, `src/isa/x64/lower.isle`): the lane-generic
    `sse_and`/`sse_and_not`/`sse_or`/`x64_xor_vector`/`bnot`/`bitselect`
    rules match 256/512-bit vectors but emit xmm-width instructions —
    `Xmm` wraps an allocated ymm/zmm reg unchecked, so the high lanes
    were silently dropped. New `vpand`/`vpandn`/`vpor`/`vpxor` ymm and
    `vpandd`/`vpandnd`/`vpord`/`vpxord` zmm forms plus `vpcmpeqb` ymm /
    `vpternlogd` zmm wide all-ones; `lower.isle` rules at priorities
    20/21 emit them for `band`/`bor`/`bxor`/`bnot`/`bitselect` on
    256/512-bit vectors. The lane-generic `sse_*`/`x64_xor_vector`/
    `bnot`/`bitselect`/`pblendvb` fallbacks are guarded to ≤128-bit
    types so any other wide use fails to lower instead of truncating.
    `ty_bits` (`src/isle_prelude.rs`) saturates at `u8::MAX` for
    >255-bit types instead of panicking. Covered by
    `pliron-clif-opt/tests/widevec_lower.rs`.

23. x64 wide-operand types (`src/isa/x64/inst/args.rs`,
    `src/isa/x64/inst.isle`, `src/isa/x64/lower/isle.rs`,
    `src/isa/x64/lower.isle`,
    `cranelift-assembler-x64-meta/src/dsl/format.rs`,
    `cranelift-assembler-x64-meta/src/generate/operand.rs`,
    `cranelift-assembler-x64-meta/src/instructions/wide.rs`):
    new `xmm_m256`/`xmm_m512` DSL locations generating `XmmMem256`/
    `XmmMem512` ISLE operand types (`size:256`/`512` in
    `newtype_of_reg!`), with `xmm_to_xmm_mem_*`,
    `synthetic_amode_to_xmm_mem_*`, `writable_xmm_to_xmm_mem_*` and
    `xmm_mem_*_to_xmm_mem` converters. Every wide instruction in
    `wide.rs` now takes an operand of its true access width — ymm
    instructions take `xmm_m256`, zmm `xmm_m512` — while 128-bit
    lane-chunk ops (`vextractf128`/`vinsertf128`/`vextractf32x4`/
    `vinsertf32x4`, `vcvtph2ps`/`vcvtps2ph`/`vpmovzxwd` at Y, the
    `*ph` X forms) keep `xmm_m128`. This closes the latent hole where
    a 128-bit `RegMem` operand could be passed to a ymm/zmm
    instruction and read out of bounds. All wide `lower.isle` rules
    use the matching-width converter. `widevec_lower.rs`
    `float_ops_*` covers the sibling's vmovups/fadd/fmul/fma path.
    `put_in_xmm_mem_256`/`_512` (`src/isa/x64/lower/isle.rs`) enable
    load folding into wide operands: they delegate to
    `put_in_reg_mem` (sinkable load → amode) but skip
    `put_in_xmm_mem`'s u128 const-pool path, which can't
    materialize a >128-bit constant; bound-Value operands in the
    wide `lower.isle` rules are passed bare so the fold fires.
    `fold_load_256` checks a single-use load folds into `vaddps`'s
    memory operand.

24. x64 wide-vector full op coverage (`src/isa/x64/lower.isle`,
    `src/isa/x64/lower/isle.rs`, `src/isa/x64/inst.isle`,
    `src/isa/x64/inst/mod.rs`, `src/prelude.isle`,
    `vendor/cranelift-codegen-meta/src/isa/x86.rs`,
    `cranelift-assembler-x64-meta/src/dsl/features.rs`): ISLE
    lowerings for the full op set at 256/512 bits — int
    add/sub/mul/min-max/sat-add/avg/mulhi/shifts/abs/neg for all
    lane widths, `vpmullq` (avx512dq) for i64x8; f64x4/x8 and the
    missing f32 wide float ops (min/max/sqrt/div/neg/abs/round);
    `icmp`/`fcmp` masks (512-bit compares split into two 256-bit
    halves — the DSL has no k-register location — reassembled via
    `vinsertf64x4`); `select`, `splat` and `vconst` for all wide
    types; `iconcat`/`isplit`. Byte/word ops at 512 are gated on
    the new `has_avx512bw` ISA flag (CPUID.07H:EBX[30], added to
    skylake-avx512/znver4/x86-64-v4 presets and `Feature::avx512bw`)
    so they fall through to loud Unsupported instead of emitting
    an encoding the host lacks. Helpers: `wide_lane_mask_amode`
    (Generated const-pool mask vectors of arbitrary width),
    `wide_shift_amt_128` (mask scalar count then movq to xmm),
    `put_low128_in_xmm_mem` (sinkable-load folding of the low half).
    Int `splat` uses `bitcast_gpr_to_xmm` — scalar ints live in
    Int-class regs, not Xmm. Tests: `pliron-clif-opt/tests/
    widevec_lower.rs` byte-checks each family at both widths.

25. x64 wide lane ops and widening (`src/isa/x64/lower.isle`,
    `src/isa/x64/lower/isle.rs`): `extractlane`/`insertlane` for
    all wide int and f64 types — pull the 128-bit chunk containing
    the lane down via `vextractf128`/`vextractf32x4` (chunk index
    from the new `wide_lane_chunk128`/`wide_lane_sub128` helpers,
    generalizing the f32-only `wide_lane_chunk`), run the existing
    `pextr*`/`pinsr*` lane op, reinsert the chunk. f64 uses
    `shufpd` for the in-chunk half-lane select (`wide_f64_sub_lane`
    merges scalars by parity). `swiden_low/high` and
    `uwiden_low/high` for `i16x16 -> i32x8` via `vpmovsxwd`/
    `vpmovzxwd` ymm — the only wide widen CLIF can express
    (inputs are capped at 16 lanes and outputs exclude i64, so
    i8x32->i16x16, i32x8->i64x4 and any 512-bit widen are
    unrepresentable and loud-fail on the verifier). `shuffle` is
    i8x16-only and `gather`/`scatter` do not exist in this CLIF —
    nothing to lower there.
