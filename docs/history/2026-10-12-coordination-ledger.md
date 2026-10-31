
## Parallel prototyping via subagent worktrees (2026-10-11)

Workflow: each prototype gets `git worktree add ../rcp-<name> -b feat/<name>`
own target dir, agent self-validates `./test.sh` in its worktree,
orchestrator re-runs the suite + merges serially and routes failures
back. Pass convention: `src/<name>.rs` `run(ctx, st)` gated on
`PLIRON_<NAME>` + `domcheck` after; regression under `tests/<name>/`.

Landed today (all validated: worktree test.sh + post-merge smoke,
full ./test.sh green with all features on):

- 159a09c sroa Kind::Pin — non-copyable call args with known pointee
  extent (`CallInfo::arg_extent` from fn_abi: Direct &T = pointee,
  sized Indirect = arg layout, raw ptrs excluded) pin only that range
  into a shared `pin_a` alloca; the rest still promotes. Fixed the
  dup-op self-copy guard so two pinned args on one call work
  (restricted to Kind::Copy). PLIRON_SROA_PIN; tests/sroa.
- f09c807 loadfwd partial-availability PRE (edge_pre) — compensating
  loads on missing pred edges, merged via block params; fixed a
  latent kill_st hole on select-pointer stores. PLIRON_LOADPRE;
  tests/loadpre.
- d786a19 inline callsite cost model — cost (ops + CALL_OP_W/nested
  call, x2 cold) <= threshold (base x10 free/devirt/fold bonuses +
  FOLD_USE/MEM_USE use credits + loop bonus, capped 32x); growth
  budget per caller. PLIRON_INLINE_MODEL=0 selects legacy; `small`
  forces legacy. PLIRON_INLINE_VERBOSE logs verdicts. tests/inlinecm.
- aefe797 IPO return attrs — nnret (nonnull-return inference feeding
  jumpthread via FnLower::nonnull; inbounds-gep requires nonnull
  base, weak/declared syms excluded) and retarg (returned(k)
  least-fixpoint + RAUW of call results, type-equality guard).
  PLIRON_NNRET / PLIRON_RETARG; tests/retattr.

Feature branches kept: feat/load-pre, feat/inline-cost, feat/ipo-attrs
(worktrees removed after merge). test.sh is the merge hotspot — every
prototype adds a stanza; resolve by keeping all.

## Stage2 miscompilations #1/#2: emitted-code correctness (2026-10-11)

After the stage2 compiler build finished, `./x build --stage 2 library`
exposed two miscompiled-codegen bugs (correct MIR in, wrong code out):

- 6239c9b bittab: sparse-membership fold tested `ushr(mask, d) != 0`
  instead of bit 0 — accepted every x <= max, so `kw::Fn` matched
  {Move,Static,Use} and `pub const fn` mis-parsed. Regression:
  tests/bittab.
- fe3fd90 ifconv: conv_table deduped `br_table` arms on the raw
  final-edge args — the shared last hop's own params, identical for
  every arm — so the whole {3,4,9..=15} ItemKind switch returned the
  first arm's constant (Forbidden) and stage2 rustc rejected all
  generic parameter defaults (param_default_policy). Args now resolved
  through the chain's param substitutions before dedup. Regression:
  ifconv::tests::table_arms_dedup_on_resolved_args + tests/polsw.

Bootstrap note: `PLIRON_<PASS>=0` disables a buggy pass at runtime while
the still-loaded old dylib compiles its replacement — the same
self-poisoning shield used for BITTAB now applied to IFCONV.

## Stage2 miscompilation #3: sret2reg desynced CallInfo arg arrays (2026-10-12)

SIGSEGV in pliron-built stage2 rustc compiling core:
`SmallVec::<[Ty;2]>::extend` wrote through a null heap pointer —
the vec's `capacity` field read uninitialized garbage > 2, so
`spilled()` took the heap path on an inline vec.

Root cause: `CallInfo::arg_extent`/`nocap` are indexed by call operand
position (sret pointer at index 0). `abi::rewrite` (sret2reg) rebuilds
internal calls as `call(cargs[1..])` dropping the sret operand, but
carried the arrays over unshifted — so at sroa time `init`'s pin_extent
lookup (operand 1, was 2) read `iter`'s 16-byte pointee extent instead
of its own 24-byte Indirect extent. `pin_a` shrank to 16 B, the
`capacity` slice at +0x10 promoted out to a dead var, and `try_fold`
read 24 bytes from a 16-byte pinned object. `dead_args` had the same
latent bug (filters operands by `live`).

- 230d7d3 abi: re-index nocap/arg_extent on call rewrites — drop
  element 0 for sret2reg, filter by `live` for dead_args, clear the
  array if its length doesn't match (stale hint worse than none).
  Regression: tests/sroa sret_caller (rd takes `&[u64;2]` then a 24B
  by-value `Big` returning an sret `Ret`; fails 17 vs 0xDEAD unfixed).
  Verified: svtest segfault gone, stage2 rustc_middle init emits the
  +0x10 store, full `./x build --stage 2 compiler library` (17m54s)
  completes — the step that SIGSEGV'd.

## Stage2 miscompilation #4: MIR arena drop SIGSEGV (2026-10-12, open)

Stage2 build completes cleanly (34m), but the pliron-built stage2
rustc SIGSEGVs on teardown compiling ANY crate that allocates MIR
bodies (`--emit=metadata` clean; `--emit=link` crashes). Crash:
`drop_glue<Statement>` does `ldrb w0,[x0]` on
`0xa8c163f7a8c15bf5` — a pointer made of two packed `ldp`-family
instruction words. Stack: Vec<Statement> inner loop <-
`Vec<BasicBlockData>::drop` <- `drop_glue<Body>` <-
`TypedArena<Body>::drop` <- `create_and_enter_global_ctxt`.

Disasm shows the drop code itself is CORRECT (Vec layout
{cap@0,ptr@8,len@16} verified against LLVM-built deallocate; outer
reads ptr@+8/len@+16, inner reads statements ptr@+8/len@+16, stride
0x28 = sizeof(Statement), arena stride 0x110). A BasicBlockData
element's `statements.ptr` field contained code bytes — corruption
upstream (arena write path or an earlier miscompiled function).

Regression window (rust-sh had commits through c21231b; previous
working stage2 predates all of today's):
- 548ab6a lcse memoff in-place iadd arg mutation (+ untrack fix),
  neg/imul peel mutation, ireduce(uextend), add-lsr
- d512f21 pairfusion: union partner preg_ops uses/defs into fused
  inst (unconditional — not env-gated!), post-index head check
- 5601908 vecslot (NOTE: rust-sh ran WITHOUT the 9f29e04
  attached-result bail guard — the ICE-prone version)
- c21231b devirt-inline rescan + indirect-callee resolution

Bisect running: full stage2 rebuild with
PLIRON_{LCSE,VECSLOT,DEVIRT_INLINE,SELFARG,MEMOFF,NEGFOLD}=0
(touched rustc_driver/src/lib.rs so all crates rebuild).
Crash gone -> culprit in env-gated set, binary-search re-enables.
Crash persists -> d512f21 pairfusion union or an ungated change.
Repro: `rust-sh/build/.../stage2/bin/rustc --emit=link /tmp/tiny.rs`
with stage2-std core rmeta on --extern (see /tmp/lldb_demangle.sh).
NOTE: rust-sh lags primary by 3 commits — sync 9f29e04, 40e6c8d,
506876d before next stage2 build.

Update (2026-10-12, ~09:45): bisect landed on PAIR FUSION.
- Pass-gate build (LCSE/VECSLOT/DEVIRT_INLINE/SELFARG/MEMOFF/NEGFOLD=0)
  still crashed — but MOVED: SIGSEGV in driver drop became SIGTRAP
  inside the backend dylib itself (`ControlFlowGraph::with_function`
  -> `try_reserve` -> xzone realloc freelist check = heap corruption).
- Rebuilt ONLY the backend dylib with PLIRON_NO_PAIRSTFUSE=1 (same
  source, same passes-off driver) -> stage2 rustc compiles tiny.rs
  cleanly, no teardown crash. Single-variable discriminator:
  pair-fused code inside the backend dylib corrupts heap during
  codegen. d512f21's preg_ops union was necessary but NOT sufficient.
- Vecslot lanes-bug found during rebase testing: uniform check used
  byte count as lane index -> shr overflow ICE on multi-byte lanes
  (u16x8/f64x2). Fixed (93cdcc0, pushed).
- Rebase onto origin/main done (25 commits): remote had richer
  enum-K lcse + ovkill + bsub_iv/Step::V bcheck + shared-exit
  looprot — ported memoff/negfold/untrack/extred/ident folds onto
  remote's lcse; kept late vecslot->lcse slot after remote's dse2.
- Fusion-on stage2 rebuild in flight to confirm crash returns
  (/tmp/stage2-fusion-on.log).

Update (2026-10-12, ~10:30): fusion bisect was MISLED by underpowered repro.
- tiny.rs compiles cleanly under BOTH fusion-on and no-fusion backend dylibs —
  it never discriminated. The real repro (rustc-demangle lib crate) crashes
  under BOTH dylibs too => pair-fusion dylib swap proved nothing.
- Reliable repro: stage2/bin/rustc --crate-type rlib --emit=link on
  rustc-demangle-0.1.28 lib.rs with stage2-std core+compiler_builtins rmeta
  --extern, --sysroot /dev/null. Deterministic: same fault addr every run.
- lldb: crash in librustc_driver find_noop_landing_pads +328 `ldrb [x3]`.
  body.basic_blocks={cap:13,ptr:0x9ffc32800,len:13} where ptr lands in a
  rw- heap region full of UNRELOCATED emitted aarch64 code (bl #0 relocs) —
  a live/stale MachBuffer. Vec buffer was either freed+reused (UAF) or its
  ptr field overwritten with an emit-buffer pointer. postorder() on the same
  Vec ran fine earlier in the same function.
- Reader code verified correct (field offsets match LLVM stage1 ground truth).
- Remat exonerated: egraph pass is opt-in (PLIRON_EGRAPH=1), never runs here.
- Running: PLIRON_NO_PAIRSTFUSE=1 full stage2 rebuild (env reaches every
  rustc invocation) — the REAL fusion discriminator, /tmp/stage2-allnofuse.log.
- If still crashes: suspect set = lower-fold (ungated), remote merge
  (ovkill/dse2/bsub_iv/dead_end/looprot shared-exit/loadfwd/licm-frozen/
  inline-provenance — all gated but untested-off), or merge resolution.
  Full gate list extracted: 133 PLIRON_* pass_enabled vars.

Update (backend-work agent): versioned analysis cache landed + pushed
(`c39c02f`, on top of `ee7ed4d` pliron-ir-opt split). Pipe steps now share
a version-keyed cfg/domtree/loop cache; mutating passes call
`an.invalidate()` and a shape-fingerprint debug_assert catches stale
materialization (dev dylib only — bootstrap.toml has
debug-assertions=false, so stage2 codegen is unaffected). It already
caught one real stale read: s_loopvec ran indvars2 on a post-loopvec
cache. test.sh + `cargo test -p pliron-clif-opt` green.
NOTE for stage2-bisect agent: rust-sh/compiler/rustc_codegen_pliron was
rsynced to `c39c02f` — your next stage2 build includes the analysis
cache + ir-opt split. Semantics preserved (same pass order/results,
analyses are deterministic functions of the IR) but the instrument
changed; sync back to `ee7ed4d` if you need the old baseline.
Full stage2 rebuild + verify.sh deferred — rust-sh is your instrument.
