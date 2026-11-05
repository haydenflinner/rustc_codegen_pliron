# 2026-11-05: loopidiom forwarder-param escapes, `PLIRON_XCGU_INLINED` on

Two commits: `fd68351` (miscompile fix) and `ee464fd` (default-on flip).

## The wasm ICE under `PLIRON_XCGU_INLINED=1`

The wasm sysroot leg with the inlined-dep exemption ICE'd in
`pipeline.rs`'s `eliminate_unreachable_code` verifier on std's
`PathBuf::__push`: an `isub` read `block20`'s param from `block268`,
which a new edge reached directly, past `block20`.

`plan_dead` walks lone-jump forwarder blocks on a dead loop's exit
path and binds their params in `fwd` (`loopidiom.rs:2471`), then
emits a fast edge straight to the final dest. The escape scan only
tracked loop-internal values, so a forwarder param read under the
dest stayed dangling on the bypass edge. `apply_dead` also only
remapped uses inside `exit_dest`; the `PathBuf` case reads the param
two blocks deeper (`call fn2(v0, v1432, …)` in `block281`).

Fix (`fd68351`): treat `fwd` keys as escapes (they may be read at
any depth since the forwarder's lone `jump` makes `dest` dominate
every legal use), append a param to `exit_dest` per escape, bind the
original value on the old edges and the closed form on the fast edge,
and remap uses function-wide *before* the old edges get the original
values appended (so the appended args keep the raw spelling).

Regression test `pliron-clif-opt/tests/loopidiom_dead_forwarder.rs`
(3 cases: param read at dest, read two blocks down, full transform +
diffcheck). Verified red without the fix (`uses value arg from
non-dominating block2`), green with it.

## `PLIRON_XCGU_INLINED` on by default (`ee464fd`)

`inlined=true` deps may exceed the copy size cap; only items that
actually fail the normal limit skip cap accounting (`src/xcgu.rs`).
That covers the SipHash `d_rounds` chain (size 127 vs the native 50)
so `hash_one` inlines into the probe loop.

Same-power-state medians, native:

| kernel | before | after | note |
|---|---|---|---|
| hashmap | 62.4 ms | 59.3 ms | `bl hash_one` x2/iter gone |
| sieve | 12.8 ms | 12.8 ms | XDEPS win kept |
| iterw | 14.4 ms | 14.4 ms | strength-reduced loop intact |
| varint_postcard | 7.16 ms | 7.16 ms | cap eviction fixed earlier |

Codegen time on the bench crate: no measurable delta (1.19 s vs
1.21 s; the earlier +28% was the rejected `CAP=400` experiment).

Validation: `harness/quick.py` 12 steps, `harness/gate.py` 93 jobs,
wasm sysroot leg identical to baseline.

## Pre-existing wasm std failures (not caused by these changes)

`run_std.mjs` fails two tests identically with and without
`PLIRON_XCGU_INLINED`, and both predated this work (`hm_u64` was
already failing with the phisimp change reverted; `float_fmt`
appeared under `PHISIMP=0` before the loopidiom fix existed):

- `float_fmt` = trap: unreachable (want 19)
- `hm_u64(100)` = 100005 (want 100000): a few lookups miss inserted
  keys in hashbrown

They need their own triage (`docs/miscompile-triage.md`); likely
candidates are `fmt` float paths and hashbrown probing on wasm32.
