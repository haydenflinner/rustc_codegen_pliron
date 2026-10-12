# WIP: stage2 self-hosting miscompile #4 — bisect in progress

State as of pause (2026-10-12 ~11:00). Drop point for an agent picking this up.

## The bug

A pliron-built **stage2** rustc completes its full build cleanly, but using it
to compile a real crate corrupts MIR: `body.basic_blocks.ptr` ends up pointing
into a heap chunk full of **unrelocated emitted AArch64 code** (`bl #0`,
`adrp x0,0` GOT placeholders — a live or stale Cranelift `MachBuffer`).

Observed crash (deterministic, same addresses every run):

- `rustc_mir_transform::remove_noop_landing_pads::find_noop_landing_pads + 328`
- `ldrb w14, [x3]`, `x3 = 0x390683e094000000` (packed insn words)
- Callers: `find_noop_landing_pads_for_instance` → `codegen_mir` → pliron backend
- `body` itself is genuine (Arc at +0x18 has strong=1/weak=1); only
  `basic_blocks.ptr` is corrupt. Other MIR Vec fields verified plausible.
- `postorder(body)` walks the same Vec earlier in the function and survives.

Field offsets verified against LLVM-built stage1 dylib: Vec = {cap@0, ptr@8,
len@16}; `BasicBlockData.statements` at +0, stride 0x90. Drop-glue code and the
crashing reader are emitted **correctly** — corruption is upstream (bad store
or use-after-free), not a reader miscompile.

Leading mechanism: a store pass wrongly deletes/redirects the `Vec.ptr` store
in `Vec::reserve`/`set_ptr` — Vec keeps a stale ptr to a freed buffer which a
MachBuffer then claims. Or an in-place MIR Vec replacement frees the buffer.

## Reliable reproducer (tiny.rs does NOT discriminate — passes everywhere)

    RUSTC=/Users/wow/code/purerust/rust-sh/build/aarch64-apple-darwin/stage2/bin/rustc
    SRC=/Users/wow/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rustc-demangle-0.1.28/src/lib.rs
    SYS=/Users/wow/code/purerust/rust-sh/build/aarch64-apple-darwin/stage2/lib/rustlib/aarch64-apple-darwin/lib
    $RUSTC --crate-type rlib --crate-name rustc_demangle --edition 2015 -O \
        --emit=link --sysroot /dev/null \
        --extern core=$SYS/libcore-*.rmeta \
        --extern compiler_builtins=$SYS/libcompiler_builtins-*.rmeta \
        $SRC -o /tmp/demangle.rlib
    # → SIGSEGV/SIGTRAP inside librustc_driver find_noop_landing_pads

lldb repro script preserved at /tmp/lldb_demangle.sh.

## Exonerated

- **AArch64 pair fusion + post-index sweep**: full stage2 rebuild with
  `PLIRON_NO_PAIRSTFUSE=1` exported to every rustc invocation still crashes,
  same site. (`/tmp/stage2-allnofuse.log`; no-fuse dylib also at
  `/tmp/pliron-dylib-nofuse.dylib`.) All fusion internals audited clean:
  `uoff()` formulas match `mem_finalize`, `UImm12Scaled::value()` is raw
  bytes, gap-scan breaks on `Edit::Move`, preg_ops union correct.
- **egraph remat**: pass is opt-in (`PLIRON_EGRAPH=1`), never runs in stage2.
- **preg_ops union fix (75cfe85)**: code only runs inside fusion paths.
- **memoff/lcse/vecslot/devirt-inline/selfarg/negfold**: earlier partial
  passes-off build still crashed (but that build moved the crash site into
  the backend dylib → ambiguous; recheck inside full sweep).
- Ungated `lower.isle` additions audited semantically correct: band/bxor
  shift+rot fusion (`09f5dc1`), `iadd+ushr → add lsr` (a47132a).

## Suspect list (in priority order)

1. **Store/alias passes merged from remote, never tested off**: `dse2`,
   `ovkill`, `sroa` family (`PLIRON_SROA_*`), `loadfwd`, enum-K `lcse`.
2. **lower-fold** (`506876d`/`06fecc4`): was ungated — `wip/lowerfold-gate.patch`
   adds `PLIRON_LOWERFOLD` so it joins the sweep.
3. **Merge-resolution artifacts** from the 25-commit rebase (clifpeep had
   three-way lcse splices — compiles+tests green but a subtly dropped helper
   would run unconditionally).
4. Ungated lowering/emit/regalloc paths; objmerge; `CallConv::Tail` handling.

## In-flight discriminator

Full stage2 rebuild with **all ~138 `PLIRON_*` pass gates = 0**, full crate
invalidation. Log: `/tmp/stage2-alloff.log` in rust-sh
(`/Users/wow/code/purerust/rust-sh`). Was compiling stage2-std when paused
(near done). Gate list extracted via `rg 'pass_enabled\("([A-Z_0-9]+)"' -or '$1'`.

- Clean → culprit is a gated pass: binary-search gate groups, starting with
  store/alias set above, then `PLIRON_OPT_BISECT=N` for finer narrowing.
- Still crashes → ungated path: lldb watchpoint on `basic_blocks.ptr` field
  from body construction (break in `codegen_mir` when body is sane, watch
  `*(x21+8)`, continue → catches the bad write).

## Env / misc notes

- rust-sh backend source was synced to match the main repo before the sweep
  build (including the LOWERFOLD gate). `selfhost/setup.sh` re-syncs.
- `pass_enabled` reads `PLIRON_*` env at emit time; `!= "0"` ⇒ on.
- Global application cap: `PLIRON_OPT_BISECT=N`, debug `..._DEBUG=1`.
- `PLIRON_CLIF` dumps pre-define CLIF; `PLIRON_PRECOMPILE` dumps
  post-optimize CLIF (vendored cranelift); `PLIRON_VERIFY` verifies input.
- Earlier wrong-backend trap: `--sysroot /dev/null` must pair with explicit
  `-Zcodegen-backend=` pointing at the *stage2* dylib, else it loads stage1's.

## After root cause

Rebuild stage2 → rerun repro → `selfhost/verify.sh` → `./test.sh` →
harness tiers → UI run-pass → add regression test for the faulty transform →
commit+push → then revisit hashmap residual gap (~1.10x vs LLVM).
