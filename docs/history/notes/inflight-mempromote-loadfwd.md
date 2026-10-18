# In-flight: scoped-root normalization + `pre` iso fix (hashmap get loop)

Uncommitted work parked as `inflight-mempromote-loadfwd.patch`
(`src/loadfwd.rs`, `src/licm.rs`; `git apply` restores it). Written
mid-investigation into `hmget`/`hashmap` get-path codegen; not yet
validated with the full suite, so it is a patch, not a commit.

## Context

Benchmark trail (Windows, `-O`, min-of-several runs):

- `hmhit`/`hmget`: `total += is_some` round-tripped a stack slot every
  iteration because `&total` escapes to `println!` after the loop.
  Landed fix: `90d3eac` — `loadfwd::isolated_escapes` + loop-local
  isolation in `licm` promote it to a header param. `hashmap` 1.28x →
  ~1.12x vs LLVM.
- `bench/probes/hmg2.rs` (untracked probe, all-miss `HashMap::get`):
  ~10.2ms vs LLVM ~9.0ms, i.e. still ~1.15x.

## What the patch does

1. `loadfwd::concrete` — `root()` without `noalias`/operand-frozen stops:
   peels scope names to the concrete object underneath.
2. `loadfwd::norm_root` — `root()` plus a `concrete` peel for `Root::V`
   scope names; resolves `&local`-shaped scope roots to `Root::S(slot)`
   so disjointness/deref checks see the real object. Unresolved `V`
   roots stay may-alias (safe).
3. `licm` uses `norm_root` for store-clobber roots, hoisted-load locs,
   store-sink locs, and promote-group locs, and the `safe`
   dereferenceability check tests the normalized root. Result: the
   `HashMap` field loads (`v19+0/8/24/32/40` — base, mask, hash keys)
   hoist out of the get loop; `hmg2` ~1.19x → ~1.15x.
4. `pre` correctness fix: `run` previously called `pre` with no `iso`
   set, so `pre`'s loc-set dataflow ran `kill` with an empty isolation
   set — the `_ => iso.contains(&r) || iso.contains(&r2)` retain arm
   dropped every loc at the first store to a different root. The patch
   computes `iso` before `pre` and threads it through (`pre` signature
   gains `iso: &FxHashSet<Root>`).

## What is still wrong (next step)

`store v594 -> v9` (the `&k` temp) followed by `v1168 = load v9` in the
same iteration still doesn't forward. The loc dies at the CFG join
before `block457`: on the entry edge the loc's `Avail` value is `v594`,
on the probe-loop backedge it is `v1168` (the load's own result from
last iteration) — `meet` requires identical values and drops the loc.

`pre`'s `St::In` + block-param machinery is designed for exactly this
("same loc, different values per pred → param"), but in the hmg2 main
the loc never reaches `ins[block451]` in `pre`'s loc-set dataflow:
`out[block420]` loses it even though `out[block19]` has it and
`ins[block420]` has it — `had=false` from block420's first inst in the
`pre` iteration, suggesting either another killing store in the
block420-630 chain (likely a `store` to an escaped/non-iso root, e.g.
`S(ss58)`-adjacent stores, or a `nowrite=none` call retain) or a meet
issue further upstream. Needs one more trace pass to name the exact
killer; the `pre` side already had its `iso` fix.

Also open: whether `MAX_ENTRIES=64` saturates in large CGUs (hmg2 main
has 100+ tracked locs; raising to 512 did not fix the forward, so it is
not the blocker for this case).

## Scratch artifacts in the worktree (untracked, safe to delete)

`clif_fwd.txt`, `clif_g3f.txt`, `clif_g_final.txt`, `get_llvm.asm`,
`get_plir.asm`, `nul.*` — disasm/CLIF dumps from this investigation.
`bench/probes/` — benchmark probes (hmg2.rs is the hashmap-miss probe;
sortdesc/sortrev/scan probes from the earlier `ldrot` work).
