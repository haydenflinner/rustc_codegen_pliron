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

22. `black_box` instruction (`cranelift-codegen-meta` shared
    `instructions.rs` + a `lower.isle` rule per ISA): a unary op over any
    scalar/vector type marked `can_store` + `other_side_effects`, so no
    optimization may fold `a` back to `x`, eliminate it, or move memory
    ops across it — the CLIF equivalent of LLVM's
    `asm("" : "r,~{memory}" :: v)` used for `core::hint::black_box`.
    Each ISA lowers `(black_box x)` to `x`, aliasing the result vreg to
    the input's: zero machine instructions emitted. The vendored
    interpreter evaluates it as the identity. pliron's `black_box`
    intrinsic lowering emits `call __pliron_bb*`/`__pliron_bbm` ops that
    `src/lower.rs` rewrites into this instruction (the call shape keeps
    pliron's own IR opts opaque); wasm keeps a volatile load fallback.

23. `VCode::emit` `preg_ops` table hoisted out of the per-block loop
    (`machinst/vcode.rs`): it was allocated `insts.len()`-sized per
    block, making emit O(blocks × insts) memset on many-block
    functions (16k-block match dispatch: 6.3s in `VCode emission`
    → ~0.1s). Entries for a block's insts are repopulated by the
    eager `apply_allocs` pass before hazard checks read them, so
    cross-block leftovers are unreachable. Also adds the
    `PLIRON_EPROF=1` per-function emit-phase timers
    (`eprof frame=/loop=/items=/apply=/fuse=/inst=/allocs=/island=/obr=/fin=`),
    gated so no `Instant` calls run when unset, and a `self_times()`
    accessor on `timing::PassTimes` plus the `timing` feature enabled
    in `Cargo.toml` so `src/lower.rs` can report `emit:<pass>`
    pseudo-steps under `PLIRON_TIMING`/`PLIRON_TRACE_FILE`.
||||||| dd0fb97
24. x64 wide-vector boolean ops (`vendor/cranelift-assembler-x64-meta/
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

25. x64 wide-operand types (`src/isa/x64/inst/args.rs`,
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

26. x64 wide-vector full op coverage (`src/isa/x64/lower.isle`,
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

27. x64 wide lane ops and widening (`src/isa/x64/lower.isle`,
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

28. x64 dot-product opcodes (`src/isa/x64/lower.isle`,
    `src/isa/aarch64/inst.isle`, `src/isa/aarch64/lower.isle`):
    new CLIF opcodes `sdot` (signed i16xN pairwise dot -> i32xN/2,
    pmaddwd), `udot` (unsigned x times signed y pairwise -> i16xN/2
    saturating, pmaddubsw — x is the unsigned operand, matching
    `udot x y` -> `vpmaddubsw x y`) and `udot_acc` (four-wide
    u8x4N x i8x4N accumulate into i32xN, vpdpbusd). The opcode
    definitions live in `vendor/cranelift-codegen-meta/src/shared/
    instructions.rs` — see that crate's PLIRON_PATCH.md. `udot_acc`'s
    byte operands are free typevars (the 4x lane relation to the
    i32 accumulator is not expressible one derived level down), so
    the lowering rules match concrete input types. aarch64's
    internal `sdot`/`udot` ISLE terms were renamed `a64_sdot`/
    `a64_udot` to free the names for the CLIF opcodes. VNNI gating:
    `udot_acc` prefers the VEX AVX-VNNI form, falls back to the EVEX
    AVX512-VNNI(+VL) forms; byte/word 512-bit forms gate on
    `has_avx512bw`.

29. x64 runtime-index swizzles (`src/isa/x64/lower.isle`): new CLIF
    opcodes `vswizzle` (`a[i] = x[idx[i] mod N]` — a register-indexed
    crossbar `shuffle`, sidestepping `shuffle`'s fixed `uimm128` mask)
    and `vswizzle2` (`a[i] = (x++y)[idx[i] mod 2N]`, x86 `vpermt2*`
    semantics — the index operand is the read-write register).
    Definitions in `vendor/cranelift-codegen-meta` patch 2; index
    operand is a free typevar (idx lane count/width relation to the
    data vector is enforced by the concrete-type lowering rules).
    Dword forms: `vpermd`/`vpermps` Y (avx2), X/Z (avx512f, +vl at
    128); qword reg-index permutes are EVEX-only: `vpermq`/`vpermpd`
    X/H/Z (avx512f, +vl at 128/256). `vswizzle2` X/H/Z `vpermt2d`/
    `vpermt2q`/`vpermt2ps`/`vpermt2pd` (avx512f, +vl at 128/256).

30. x64 VSIB gather/scatter (new CLIF opcodes `vgather`/`vscatter`;
    definitions in cranelift-codegen-meta patch 3, parser arm in
    vendored cranelift-reader, interpreter arms in
    vendor/cranelift-interpreter). `src/isa/x64/lower.isle`: VEX
    `vpgatherdd/dq/qq/qd` + `vgatherdps/dpd/qpd` at 128/256 (avx2)
    fed a fresh all-ones mask (`vpcmpeqb`); EVEX `vpgather*`/
    `vgather*` at 256/512 (avx512f) unmasked. `vscatter` is
    EVEX-only (`vpscatterdd`/`vscatterdps`/`vpscatterqd`/
    `vpscatterdq`/`vscatterqps`/`vscatterqpd`/`vscatterqpd` H/Z;
    avx512f, +avx512vl for ymm-index). `src/verifier/mod.rs` +
    `src/write.rs`: `Scatter` instruction-data arms. Trap to note:
    the generated operand collector ties a `rw` write to use-index
    0 (`read_write_xmm`), so the VEX gather's mask operand must be
    declared first; the dst result comes out as `value_regs_get`
    index 1.

31. Wide vector emit support for load/store + sized type extractors:
    `src/prelude.isle`/`src/isle_prelude.rs` add `ty_vec256`/`ty_vec512`
    extern extractors (`ty_bits_u64` is a converter — it cannot appear
    in rule patterns). `src/isa/x64/lower.isle`: generic `load`/`store`
    rules for all 256-bit types at pri 5 → `x64_vmovups_ya`/`_yb_mem`
    (avx2), all 512-bit types at pri 7 → `x64_vmovups_za`/`_zb_mem`
    (avx512f); pri 7 keeps them disjoint from the pri-4 128-bit
    concrete-type rules (same-priority rules with disjoint extractors
    still collide in this ISLE).

32. x64 wide-vector register-class gate (`src/isa/x64/inst/mod.rs`):
    `rc_for_type` now rejects a vector type wider than
    `WIDE_VECTOR_BITS` with `CodegenError::Unsupported`. Previously a
    >128-bit value reaching codegen with the atomic at its 128 default
    compiled "successfully": the ISLE wide rules emit ymm/zmm ops at the
    operand's full width, but the vreg's Float class stayed xmm-canonical,
    so class moves (`gen_move`) and spill slots kept only the low 128
    bits — a silent low-lane-only miscompile for `bnot`/`bitselect`/
    `band`/`bor`/`bxor` et al. Now the unsupported combination fails at
    vreg allocation like any other unexpected SSA type. `src/lib.rs`
    raises the ceiling in step with `X64_WIDE_VECTOR_BITS` when the
    target has AVX2/AVX512F, so real compiles are unaffected; the gate
    catches stray users that emit wide CLIF without the flag (test
    harnesses, the `clif-check` replayer, or an out-of-sync mirror).
    Regression test: `pliron-clif-opt/tests/widevec_gate.rs` asserts the
    loud error at ceiling 128 and (on non-x86 hosts, where no wide ISA
    exists) that wide `bnot`/`bitselect` never compile silently.

33. aarch64 dead extension after `cset` (`src/isa/aarch64/lower.isle`):
    `uextend`/`sextend` of an `icmp`/`fcmp` result is a no-op — the
    `cset`/`fcset` materialization already writes a clean full-width 0/1.
    New rules return `put_in_reg` of the compare instead of emitting
    `MInst.Extend`. Visible as `cset; uxtb` pairs on every
    bool-as-integer idiom (`(x == y) as i64`, `select c, 1, 0`).

34. DFG alias epoch + write list (`src/ir/dfg.rs`): `alias_epoch`
    counter bumped on every alias write (`change_to_alias`,
    `replace_with_aliases`, `make_value_alias_for_serialization`);
    tombstone writes and same-root retargets (`compact_aliases`,
    `resolve_all_aliases`) don't bump since they create no new
    resolvable pairs. `alias_dirty` records each write's `dest` in
    order (duplicates kept), so `carry_aliases` scans only the
    watermark backlog and `compact_aliases` walks writes instead of
    the whole `values` table — the epoch early-out and
    whole-`values()` scans were ~1.5% of bench pipeline time and the
    backlog scan was ~2% of regex-syntax phaseB CPU.

35. `imul.i8x16` x64 lowering (`src/isa/x64/lower.isle`, `inst.isle`,
    `lower/isle.rs`): upstream leaves i8x16 multiplication unimplemented
    (no `pmullb` exists). Lowered as even/odd byte split — `pand`/`psrlw`
    into 16-bit lanes, `pmullw`, keep each product's low byte with a
    pooled 0x00FF mask (`mask_00ff`), weave back with `psllw`+`por`.
    Wider i8 lanes (i8x32/i8x64) still unsupported — no VEX/EVEX
    `vpmullw` forms exist yet.

36. aarch64 narrow-int compare/extend lowering (`src/isa/aarch64/inst.isle`,
    `src/isa/aarch64/lower.isle`, `src/isa/aarch64/lower/isle.rs`):

    - `imul_pow2p1_shamt`/`imul_pow2m1_shamt` externs + rules: `imul x, 2^n+1`
      lowers to `add x, x, lsl #n`; `imul x, 2^n-1` to `lsl` + `sub`. Kills
      the `movz` constant materialization and `madd`'s 3-cycle latency.
    - `val_already_extended`: partial bool oracle answering "does this
      value's register already hold the `ArgumentExtension`-extended form".
      Covers 32/64-bit values, `uextend`/`sextend`, `icmp`/`fcmp` (cset
      writes full-width 0/1), `uload8`/`uload16`/`sload8`/`sload16`,
      narrow-typed `load` (`ldrb`/`ldrh` zero-extend — `Uext`-clean),
      narrow `ireduce` (see below), and narrow `black_box`. NB: `(if (oracle))` in this ISLE only tests
      that the partial call returned `Some` — a `Some(false)` result still
      passes. Guards that need the bool value must use `if-let true` /
      `if-let false`.
    - `emit_icmp_narrow` + `put_in_reg_ext32_maybe`: for i8/i16 compares,
      when `rm` is clean and `rn` is dirty, swap operands (and `intcc_swap
      _args` the cond) so the dirty operand rides `cmp`'s fused extend slot
      and no standalone `uxtb`/`sxtb` is emitted; the imm-compare path also
      elides a redundant ext on a clean operand. Bumps the string-compare
      kernels; LLVM gets the same operand cleanliness for free via `ldrb`
      /`ldrh` black-box traffic.
    - `lower (ireduce (fits_in_16 ..))` now materializes `uxtb`/`uxth`
      instead of the wide passthrough, producing the clean narrow regs the
      oracle advertises.
    - `lower (black_box i8/i16)` emits `uxtb`/`uxth` when the operand's
      high bits are dirty (oracle says not clean); clean inputs still pass
      through with no inst.

37. aarch64 `CSInc` MInst + adjacent/zero const select lowering
    (`src/isa/aarch64/inst.isle`, `src/isa/aarch64/inst/emit.rs`,
    `src/isa/aarch64/inst/mod.rs`):

    - New `MInst.CSInc { rd, cond, rn, rm }` — the `(op,o2)=(0,1)` csel-family
      encoding (`csinc rd, rn, rm, cc` = `rn` if cc else `rm + 1`), with emit,
      reg-collect, and display arms, and a `csinc` ISLE helper.
    - `lower_select_cond` rules: `select c, K+1, K` /
      `select c, K, K+1` lower to one `csinc` off the shared materialized
      base register (LLVM's `cinc` idiom) instead of two movs + `csel`;
      `select c, 1, 0` / `select c, 0, 1` use `xzr` as the base so they
      stay a single inst like `cset`; `select c, t, 0` / `select c, 0, e`
      use `xzr` for the zero arm, dropping a `mov w_, 0` per select.
      Priorities are staggered (adjacent 5/6 < zero-arm 7/8 < 0/1 9) so the
      more-specific const pairs win; `u64_gt 0` keeps the adjacent rules off
      the zero shapes.

38. Egraph remat: no `iconst` marking, no remat into deeper loops
    (`src/opts/remat.isle`, `src/egraph/elaborate.rs`):

    - `remat.isle` no longer marks `iconst` (the `imm64_remat_cheap`
      rule is gone). The elaborator clones a marked def into every
      use block, so a deduped constant (e.g. `iconst 10` feeding a
      digit-parse `imul`/`icmp` chain inside a loop) became a `movz`
      per use block per iteration — strictly more dynamic work than
      keeping the vreg live; under real pressure regalloc spills
      either way. op-with-imm remat stays: it re-executes the op but
      folds the immediate into the encoding.
    - `maybe_remat_arg` takes `&LoopAnalysis` and refuses to remat
      when `insert_block` sits in a deeper loop than the arg's block
      — that trades the def's single execution for per-iteration
      work, which even a one-inst materialization loses.

39. aarch64 i128 add/sub with provably-zero high half rides `xzr`
    (`src/isa/aarch64/lower.isle`):

    `iadd.i128`/`isub.i128` against `uextend` or `iconcat .., iconst 0`
    now lower the high half to `zero_reg` (`adc`/`sbc`/`ngc` with `xzr`)
    instead of materializing `mov xN, #0` per use. Priority 40/41 keeps
    them above the generic i64-pair rules; zero-hi-right outranks
    zero-hi-left so both-extended inputs resolve deterministically.

    The `uextend` variants take the operand via `put_in_reg_zext64`, not
    `put_in_reg`: narrow values live in X registers whose bits above the
    value width are garbage (a 32-bit `lsl`/`eor` leaves them set), so
    the raw register is not the zero-extension the `uextend` promised
    (rustlantis seed 24 miscompiled `-65535` as `-5439487`).

40. `#[inline]` on hot DFG/Layout readers (`src/ir/dfg.rs`,
    `src/ir/layout.rs`): dev-profile builds have no LTO, so every
    `value_def`/`resolve_aliases`/`inst_args`/`Insts::next` call from
    pliron-clif-opt was a real cross-crate function call.
    `ValueData::from` alone measured ~6% leaf samples in the opt
    pipeline. Annotated the small leaf readers + the two `ValueData`
    packed-conversion `From` impls + `Values`/`Blocks`/`Insts`
    iterator `next`/`next_back`. No behavior change.

41. Fused resolve+def accessors (`src/ir/dfg.rs`):
    `resolved_value_def`, `try_resolved_value_def`, `resolve_and_def`,
    all built on `resolve_to_data` — the alias walk hands back the
    resolved entry's decoded `ValueData`, so the ubiquitous
    `value_def(resolve_aliases(v))` pattern costs one map hit + one
    decode instead of two. ~95 opt-crate call sites converted.
    Panic/Option semantics identical to the unfused pair.

42. More `#[inline]` on hot cross-crate readers (`src/ir/dfg.rs`,
    `src/ir/instructions.rs`, `src/ir/types.rs`, `src/ir/function.rs`):
    `Insts`/`Blocks` `Index`/`IndexMut`, free `maybe_resolve_aliases`/
    `resolve_aliases`, `BlockCall::block`, `BlockArg::decode_from_value`,
    `branch_destination`/`_mut`, `Type::index`/`from_repr`,
    `Function` `Deref`/`DerefMut`. A fresh profile of
    `cargo rustc -p pliron-clif-opt` through the backend put these at
    ~11k/233k leaf samples combined. Same no-LTO rationale as #40.

43. `#[inline]` on generated `InstructionData` methods
    (`cranelift-codegen-meta/src/gen_inst.rs`): `opcode`, `arguments`,
    `arguments_mut`, `typevar_operand` — the generated code is all
    non-generic, so every `inst.opcode()`/arg read from the opt crates
    was an outlined call.

44. aarch64 packed-f16 support (`src/isa/aarch64/lower.isle`,
    `inst.isle`, `inst/emit.rs`, `inst/mod.rs`): NEON fp16 is universal
    on Apple Silicon but upstream had no lowering for it. Adds scalar
    `fpromote.f32`/`fdemote.f16` (`fcvt s,h`/`fcvt h,s` via new
    `FPUOp1::Cvt16To32`/`Cvt32To16`), fp16 three-same encodings for
    `VecALUOp` fadd/fsub/fmul/fdiv/fmax/fmin and `VecALUModOp`
    fmla/fmls on `f16xN`, `FpuMoveFromVec` Size16 (`dup h, v.h[i]`),
    an `fcvtn2` ISLE helper, and rules for the vendored packed-half
    casts (`fvpromote_f16`/`_lo`/`_hi` -> `fcvtl`/`fcvtl2`,
    `fvdemote_f16_pair` -> `fcvtn` + `fcvtn2`). Used by the pliron
    lowerer's CubeCL path when `types::NATIVE_F16`/`F16C` are set.

45. Exact `size_hint` on the layout iterators (`src/ir/layout.rs`):
    `BlockNode` gains a per-block inst `count` and `Layout` gains a
    `num_blocks` counter (maintained in append/insert/remove/split),
    feeding `Blocks`/`Insts` `size_hint`/`ExactSizeIterator`. Passes
    that snapshot `layout.block_insts(b).collect()`/`blocks().collect()`
    were hitting `spec_from_iter_nested` growth — a ~1% leaf —
    because the linked-list iterators reported `(0, None)`. The serde
    round-trip rebuilds counts through the public insert path.

46. `LoopAnalysis::is_in_loop` O(1) (`src/loop_analysis.rs`): `compute`
    now also assigns each loop an Euler-tour interval `[tin, tout)` over
    the loop forest (`assign_loop_intervals`); `is_in_loop` tests
    `tin[lp] <= tin[innermost(block)] < tout[lp]` instead of finger-
    walking the parent chain. The opt pipeline calls it per
    (loop, block) pair inside `layout.blocks()` filters — O(loops ×
    blocks × depth) per pass — ~40k/2.65M leaf samples in the rustc
    profile.

47. Seedable `cfg`/`domtree` in `Context::optimize` (`src/context.rs`):
    the initial `compute_cfg`/`compute_domtree` pair is now guarded by
    `is_valid()` — the pliron pipeline moves its own exact analyses
    (built by `Analysis` on the final IR) into the context before
    `compile`, so emit no longer rebuilds them per function. Paths
    that mutate the function after seeding (the narrow-vector
    `veclegalize` retry in pliron-clif-lower) must `clear()` both.

48. Sub-128-bit vector ops in wide xmm regs (`src/isa/x64/lower.isle`):
    an F32X2/F16X4 value lives in the low 64 bits of an xmm and only its
    real lanes are observable, so the packed 128-bit op is used and the
    high lanes carry unobservable garbage. Adds F32X2
    fadd/fsub/fmul/fdiv/fsqrt/fneg/fabs/fmin/fmax rules, an `x64_round`
    F32X2 arm (ceil/floor/nearest/trunc), F32X2 arms on all eight
    vfmadd/vfnmadd/vfmsub/vfnmsub 213/132 decls (fma.f32x2 arrives via
    the type-polymorphic fmadd rules), f32x2 pshufd extractlane +
    f32x4_insertlane reuse, xmm `fvpromote_f16`/`fvdemote_f16` arms for
    <4 x f16> <-> <4 x f32> (F16C), f16x4 pshuflw/pinsrw lane ops, and
    F16X4 vaddph-family arms for AVX512-FP16 hosts. Lets
    pliron-clif-lower's `simd_leaves` admit these 64-bit leaves; the
    fmin/fmax helpers copy the F32X4 NaN-safe sequences verbatim.

49. Per-function wide-xmm gating (`src/machinst/abi.rs`,
    `src/isa/x64/abi.rs`): `Callee` records `has_wide_vector` (any CLIF
    value whose vector type is >256 bits) computed in `new` from
    `f.dfg.values()`, and `machine_env()` calls a new defaulted
    `ABIMachineSpec::get_machine_env_for(has_wide, flags, call_conv)`.
    On x64 it forces the hi=false env for functions with no >256-bit
    values: when `wide_vector_bits() >= 512` offered all 32 zmm regs to
    every function, regalloc parked narrow (<=256-bit) vregs in
    zmm16-31 under pressure — those functions never need the regs for
    their own ops — and the parked values read back corrupted
    (observed: lanes 12-15 of each 64-byte region returned the
    slot's chunk-0 bytes). Functions that actually manipulate 512-bit
    data keep the wide env, so zmm codegen stays profitable where it
    is used. The `high_xmm_regs()`/`WIDE_VECTOR_BITS` knobs are
    unchanged — they cap the maximum, this patch gates it per fn.

50. `src/machinst/blockorder.rs`: the chained-order successor pick prefers
    a block that is itself a loop latch (its terminator reaches back to a
    dominator — `is_latch`) over a non-latch of the same rank; in-count
    breaks remaining ties as before. Placing the latch right after the
    loop head puts the hot continuation on the not-taken edge of the
    head's `brif`, so each iteration pays only the latch's own back-edge
    branch instead of an extra taken branch to reach the arm block
    (LLVM's latch-before-head shape; ~25% on a per-element
    `filter().count()` loop). `PLIRON_BCHAIN_DEBUG` prints pick
    decisions.

51. OnceLock-cached `PLIRON_*` env reads (`src/context.rs`,
    `src/machinst/{vcode,lower,compile,abi,blockorder}.rs`,
    `src/isa/aarch64/lower.rs`, `src/isa/x64/abi.rs`): debug/gate
    env vars that were re-read via `std::env::var{,_os}` per inst,
    per block, or per function are now read once into `OnceLock`
    statics (`PLIRON_POSTIDX_DEBUG`, `PLIRON_COMPILE_EGRAPH`,
    `PLIRON_POSTOPT`, `PLIRON_LUDEBUG`, `PLIRON_REMATSINK_DEBUG`,
    `PLIRON_STATS`, `PLIRON_PRECOMPILE`, `PLIRON_ALIGN`,
    `PLIRON_LOWER_DEBUG`, `PLIRON_FN_DUMP`, `PLIRON_FP_FNS`,
    `PLIRON_FP_RANGE`, `PLIRON_PREVCODE`, `PLIRON_RA2_VERBOSE`,
    `PLIRON_RA2_EDITS`, `PLIRON_BCHAIN_DEBUG`, `PLIRON_NO_FREE_RBP`,
    `PLIRON_HI_ALWAYS`). `std::env::var` takes the process env lock
    per call — a backend-codegen profile showed ~11% of samples in
    `getenv`/lock waits even after the workspace-side reads were
    cached.

52. Conditional domtree recompute after the egraph (`src/context.rs`,
    `src/egraph/mod.rs`): `EgraphPass::run` now returns
    `remove_pure_and_optimize`'s `cfg_maybe_changed`. The pass already
    recomputes `cfg` itself when it rewrites a terminator and
    `eliminate_unreachable_code` maintains it through block removal, so
    `Context::egraph_pass` no longer recomputes `cfg` unconditionally
    and recomputes `domtree` only when edges actually moved — about
    1.4s of analysis rebuilds per bench build was unconditional.

53. Flat CSR `ControlFlowGraph` (`src/flowgraph.rs`): the per-block
    `bforest::Map`/`Set` edge storage is replaced by packed arrays
    (`edges` + `succ_off`/`succs` + `pred_off`/`preds`) built by two
    sort+dedupe passes. Iteration order is unchanged — successors in
    block order, predecessors in branch-inst order. `compute`/`clear`
    retain Vec capacity, so the tens of CFG rebuilds per function in
    pliron's pipeline stop regrowing bforest node pools (the single
    largest allocator leaf in the pipeline profile, ~3.5% of samples
    via `visit_block_succs`/`add_edge`). `recompute_block` patches the
    edge list and rebuilds (still O(E); nothing calls it hot).

54. `ControlFlowGraph::build` counting-sorts (`src/flowgraph.rs`): the
    two whole-edge-list `sort_unstable` calls (by `(from, inst, to)`
    then `(to, inst)`) are replaced by O(E + B) counting scatters keyed
    on block index — histogram + prefix-sum offsets + scatter into
    `scratch`/`edges`, then per-block segment sorts by `(inst, to)` /
    `inst` (a block names at most a handful of destinations, so these
    stay tiny). Output order is unchanged; `edges` still ends grouped
    by destination. CFG build time on the corpus roughly halved.

55. Multi-use `iadd` amode sharing (`src/isa/aarch64/inst.isle`,
    `src/isa/aarch64/lower/isle.rs`, `src/prelude_lower.isle`,
    `src/machinst/isle.rs`, `src/machinst/lower.rs`): new amode rules
    keep a multi-use `iadd` load/store base whole (`UnsignedOffset` /
    `Unscaled` on its own register) instead of decomposing it into a
    reg+reg amode — the sum is materialized for its other uses anyway,
    so decomposing only kept the operands' registers live as well and
    emitted a redundant address computation per load. Enables the
    generic reg+imm path (and `ldp`/`stp` pairing) for shared
    `base + index` addresses. `PLIRON_AMODESHARE=1` enables
    (`Lower.value_has_multiple_uses` is exposed to ISLE for the guard;
    the gate lives in `amode_share_ok` on the aarch64 isle context).

56. Liveness- and pressure-aware amode sharing gate
    (`src/machinst/lower.rs`, `src/isa/aarch64/lower/isle.rs`): the
    multi-use-`iadd` amode rules from #55 now require
    `Lower::amode_shareable` — share when the sum is genuinely needed
    as a value (a use that cannot fold into an addressing mode;
    `compute_use_states` builds `value_non_amode_use` in the same
    inst scan, `arg_is_mem_addr` identifies the address arg of plain
    loads/stores), or when the all-address fanout is ≤3 AND the
    block's live-in count is ≤ `PLIRON_AMODESHARE_PRESS` (default 5).
    The pressure proxy counts cross-block arg uses per block and
    rolls the max up per value (`value_max_pressure`) — a kept base
    would hold a register across those blocks, and measured
    regressions (sortw +5%, hashmap +2.6% unbounded) live in
    high-pressure blocks while the win (sieve −5.5%) lives in
    low-pressure ones. Default on (PRESS=5): paired medians give
    sieve −5.5% / parse_only −3% wins with sortw/hashmap hot fns
    byte-identical to OFF; `PLIRON_AMODESHARE=0` disables.
57. `DominatorTree::try_dominates` is false across blocks unless both
    are reachable in the tree (`src/dominator_tree.rs`). Unreachable
    and newer-than-the-tree blocks keep default preorder numbers, and
    `block_dominates` answered yes for any two of them: a dead loop's
    latch def "dominated" its header, `remove_trivial_params` aliased
    the header param to `iadd param, 16`, and the def cycle hung
    `clifpeep::lin_flat` (rustc UI `test-case-on-use-item-129765.rs`
    with `--test`). Callers only lose transforms on dead code.
58. `iconst_bounds` reports a bad controlling type instead of
    `unreachable!()` (`src/verifier/mod.rs`): upstream assumes every
    `iconst` is i8..i64 and panics on anything else, which aborted the
    per-step verifier before it could name the pass that emitted the
    bad inst (rustlantis seeds 65/89 — `iconst.i128` out of lcse/bandk).
59. `DataFlowGraph::resolve_all_aliases` moves value labels from an alias
    onto the value it resolves to instead of dropping them
    (`src/ir/dfg.rs`). Our CLIF passes replace values by aliasing them, so
    upstream's rule lost most variables' labels (`-g` at `-O`, see
    `pliron-clif-lower/src/dwarf.rs`). Labels never affect code.
60. `VCode::extend_debug_locations` (`src/machinst/vcode.rs`): value-label
    ranges continue past the end of the vreg's liveness until the location
    is overwritten (a def, a call clobber or a regalloc move into it), the
    label's next assignment, or the block end; a block whose predecessors
    all precede it in layout starts with the locations they agree on.
    LLVM's LiveDebugValues, roughly: a dead value's register still holds
    it. Debug info only. `PLIRON_DBGEXT=0` restores upstream ranges.
61. Cold-arg rebind copy (`src/isa/aarch64/lower.isle`
    `iadd_coldcopy_*`, `umulhi_zero`, `src/isa/aarch64/inst/emit.rs`):
    `iadd x, (umulhi 0, 1)` lowers to a 64-bit `Inst::Mov` into a fresh
    vreg, a bare `umulhi 0, 1` to `mov #0`, and emitting a 64-bit `Mov`
    with `rd == rm` writes no bytes. pliron's
    `coldargs` emits that form under `PLIRON_COLDCOPY=1`, replacing the
    `umulh` + `add` per cold call arg with a `mov` that regalloc often
    makes a self-move.

62. aarch64 clean narrow ints (`PLIRON_NARROWEXT=1`, default off;
    `src/isa/aarch64/{inst.isle,lower.isle,lower.rs,lower/isle.rs}`,
    `src/machinst/{lower.rs,isle.rs,abi.rs}`). `narrow_uext_clean` (Rust,
    budget 16) extends the `val_already_extended` oracle (patch 36) to
    canonical narrow `iconst`s, `band` with one clean operand, and
    `bor`/`bxor` of two clean operands (all exact 32-bit `and`/`orr`/`eor`;
    the `bic`/`orn`/`eon` forms need a `bnot` operand, never clean). Users:
    - `Lower::gen_return` asks the new `LowerBackend::narrow_ret_uext_clean`
      for each i8/i16 `uext` return and passes `skip_uext` to
      `gen_copy_regs_to_retval`, dropping the ABI `uxtb`/`uxth` (309 of
      381 on regex-syntax were after `and #k`, `mov #k` or `cset`);
    - `uextend_clean` (priority -2; the general `uextend` rule moved to -3):
      `uextend` of a clean narrow value is a passthrough;
    - `is_nonzero` rule 4: a clean i8/i16 tests with `cbnz w` instead of
      `tst #0xff` / `uxth`;
    - `band_ireduce_imm` (priority 30): `band (ireduce x) k`, canonical `k`,
      masks `x`'s register directly, so the `ireduce`'s `uxtb` (patch 36)
      is not materialized.

63. Shared `br_table` split-edge blocks (`PLIRON_EDGEDEDUP=1`, default
    off; `src/machinst/blockorder.rs`, `src/machinst/lower.rs`). Table
    entries passing the same block call (target and args) map to one
    `CriticalEdge` lowered block instead of one identical `mov*; b` block
    each. `lower_branch_blockparam_args` skips the repeated successor so
    regalloc2 sees each CFG edge once (a repeat would read as a critical
    edge); repeated successors carry no args. The jump table itself keeps
    one entry per case, now pointing at the shared label.

Fix: `iadd_ushr_right`/`iadd_ushr_left` (the add-with-`lsr` folds from
a47132a) matched any scalar width up to 64 bits. An i8/i16 operand's
register has unspecified bits above its width, which `lsr #k` shifts into
the sum; plain `ushr` lowering zero-extends first. They now require
`ty_32_or_64`, like the `band`/`bxor` shifted-operand rules (fuzz seeds
1939 and 3441; tests/narrowlsr).

- `src/machinst/abi.rs`: `pliron_last_frame()` returns the frame breakdown
  (stack slots, spill slots, clobber saves, outgoing args) of the last
  function the thread compiled, for `PLIRON_FRAME_INFO`. Debug aid only.
- `src/opts/remat.isle` (+ `pliron_remat_saddr` in `prelude_opt.isle` and
  `opts.rs`): rematerializes `stack_addr` per use (default on since
  2026-10-27; `PLIRON_REMAT_SADDR=0` turns it off)
  block, as LLVM's RA does for frame indices, instead of keeping one
  GVN'd frame address live across calls.
- `PLIRON_REMATCALL=1` (default off; `src/egraph/elaborate.rs`,
  `src/opts/remat.isle`, `pliron_remat_call` in `prelude_opt.isle` and
  `opts.rs`; `egraph::elaborate` made `pub(crate)` for the switch): marks
  one-instruction `iconst`s (`imm64_remat_cheap`) for remat again, and the
  elaborator's two loop rules in `maybe_remat_arg` (never into a deeper
  loop, never into another block of the def's loop) give way when the
  use block's innermost loop contains a call (`Elaborator::call_loops`,
  one pass over the blocks). A constant hoisted above such a loop
  otherwise lives across its calls in a callee-saved register or a spill
  slot: regex-syntax reloaded spilled 0/1/-1/element sizes 778 times (with
  `PLIRON_COLDCOPY=1`; 1,319 without).
  Call-free hot loops keep the old rules. docs/history/2026-10-27-spills.md.

64. Deferred narrow jump tables (`PLIRON_JT_NARROW=1`, default off;
    docs/history/2026-10-27-jump-tables.md). `src/machinst/buffer.rs`:
    `MachDeferredJumpTable` records, `can_defer_jump_table`,
    `defer_jump_table`, and `emit_deferred_jump_tables`, which `finish`
    runs after the last island — every label is bound and nothing can move
    — to write each table at the end of the function with 1-, 2- or 4-byte
    unsigned entries `(target - lowest target) / unit`, the narrowest that
    fits, then patch the dispatch's table address, base address and entry
    load. Labels go through `resolve_label_offset`, so aliased (jump-only)
    targets resolve to their final block. `src/machinst/vcode.rs`: per
    block in emission order, a sound bound on everything still to come
    (every inst and regalloc edit at worst-case size plus a veneer,
    constant re-emission and island jump; epilogues; 4 bytes per CFG edge;
    padding), so deferral happens only where both placeholders provably
    stay in pc-relative range; otherwise the stock inline table is used.
    `isa/aarch64/inst/emit.rs` (`JTSequence`): `adr; ldr{b,h,} [uxtw];
    adr; add lsl #2; br`, one instruction more than the stock sequence,
    table out of `__text`'s hot path. `isa/x64/inst/emit.rs`
    (`JmpTableSeq`): `lea; movz{bl,wl}/movl (NOP-padded); lea; add; jmp`,
    unit 1, the default as the last entry as before. `PLIRON_JT_WIDTH=2|4`
    forces a minimum width (tests). Tests: `pliron-clif-opt/tests/jt_narrow*.rs`
    (run the machine code; x86-64 under Rosetta with
    `--target x86_64-apple-darwin`).
