# 2026-11-16: memcpyopt rebased a temporary that a loop reads again

The dsa wasm build under `PLIRON_INLINE_RECX=1 PLIRON_INLINE_ONCE=3`
died in six workloads (compile_page, dims_page, checks_page, live_page,
game_tetris, game_asteroids) with
`dlmalloc.rs:1263: assertion failed: psize <= size + max_overhead`:
`realloc` was handed an old size that did not match the block. The
inliner modes were innocent. The bug is in `memcpyopt::try_forward`, it
is older than both knobs, and a 12-line program aborted the same way on
the default native configuration (`tests/mcrdwrap`).

## The rewrite

`mimas-parse`, `<parse::laws::Gen>::emit_shrink` (`laws.rs:391`), the
`Adt::Struct` arm:

```rust
let mut body = String::new();
for (ix, f) in fields.iter().enumerate() {
    let name = f.name.to_string();      // to_string: `let mut buf = String::new()`
    ...
    body.push_str(&format!(..));
}
```

rustc's MIR GVN builds the empty `Vec<u8>` once and copies it into both
strings (`_2 = String { vec: copy _22 }` ... `_6 = String { vec: copy
_22 }` in the test program's MIR), so the pliron IR going into
memcpyopt is

```
pre:   store tmp+0 <- 0 ; store tmp+4 <- 1 ; store tmp+8 <- 0     three 4-byte writers
       memcpy(body, tmp, 12)                                      m
loop:  memcpy(buf, tmp, 12)                                       r, a post-copy reader of tmp
       ...
       body.push_str(..)                                          changes body, after r
       br loop
```

and after it

```
pre:   store body+0 <- 0 ; store body+4 <- 1 ; store body+8 <- 0
loop:  memcpy(buf, body, 12)
       ...
```

From the second iteration on `to_string`'s buffer starts as a copy of
`body`: two `String`s own one heap block. The formatter appends to it,
`name` is dropped (the block is freed or reallocated under `body`), and
the next `body.push_str` reallocates a block whose header no longer
matches `body`'s capacity. In the wasm (`emit_shrink`, function 624 of
the failing module) the loop head reads `i64.load 24 / i64.store 176`,
`body`'s header copied into the formatter's buffer.

## The rule it breaks

A reader of `tmp` after the copy reads `tmp`'s final bytes; rebased it
reads `dst`, so `dst`'s range must not change between the copy and
*every* execution of the reader. The partial-writer path checked that
with `any_between(r, m, touches)`, a dominance window: touches in blocks
the reader's block dominates, and touches after the reader in its own
block, count as "after the reader". In a loop they are also ahead of
its next execution. The writer side had the same hole and got a wrap
scan for regex-syntax's `alternate_literals` (a copy in a loop whose
writer sits outside); the reader side did not. The full-writer path
(`full`) walks routes and was already right.

Fix: for each post-copy reader, walk backward from `r` over all routes,
cut at `m`, refuse at a `Write`/`Other` touch of the reader's range
("dst changed on a route from the copy to a post-copy read"), 8192
steps of budget. No `tmp` writer can lie on such a route (writers
dominate `m`, `m` dominates `r`), so `tmp` itself is constant there.
`PLIRON_MEMCPYOPT_RDWRAP=0` turns the scan off, for bisecting only.

## Why only this configuration showed it in dsa

Established: with mimas-parse compiled under `ONCE=2` the same copy is
refused by the old window check (the trace of `emit_shrink` has two
"dst touched between copy and post-copy read" lines, one under
`ONCE=3`), and `PLIRON_MEMCPYOPT_PATHWIN=0` also hides the failure.
Under `ONCE=2` `drop_glue::<String>(body)` is inlined into
`emit_shrink`, under `ONCE=3` it is a call; which block that puts into
the window was not looked at. Inferred: the refusal under `ONCE=2` is
for a `body` touch in a block the reader's block does not dominate, an
accident of the block layout, not a proof that the forward was wrong.
The unit test needs no such block and no path window: the hole is in
the dominance rule itself. `PATHWIN` (default on since this morning)
widened it by no longer refusing for touches that are on no path from
the copy to the reader.

## How it was found

1. Stack (`node --stack-trace-limit=60`): `finish_grow` under
   `emit_shrink`.
2. Crate swap. A clone of the failing target dir (`cp -Rcp`, free on
   APFS; cargo accepts it when the build passes the same
   `-Zcodegen-backend` path string) with `mimas-parse` and its nine
   dependents rebuilt under `ONCE=2` was good. Then no cargo at all:
   the final `literate-wasm` rustc command (from `cargo -vv`) relinks a
   tree assembled from the two builds' unit directories in 70 s.
   `mimas-parse` alone from the bad build fails.
3. One crate: cargo's own rustc command for `mimas-parse`, rerun under
   knobs, then the relink. Two traps: the `--error-format=json
   --json=...` flags must stay (they are tracked options: without them
   the crate hash changes and the dependents report E0463), and so must
   the backend path (same reason; for another dylib, compile into a
   scratch directory and replace only the object in the tree's rlib
   with `llvm-ar r`).
4. Stage knobs on that crate: `PLIRON_WASM_PEEP=0`,
   `PLIRON_WASM_INLINE=0`, `PLIRON_WTREEREMAT=0` and
   `PLIRON_INLINE_RECX=0` still fail; `PLIRON_OPT_BISECT=0`,
   `PLIRON_MEMCPYOPT=0` and `PLIRON_MEMCPYOPT_PATHWIN=0` pass.
   `PLIRON_DIFFCHECK=1` and `peepdiff` were clean, as they must be:
   both start below the pliron IR.
5. The wasm body at the trap named the `String` at frame offset 24 and
   the copy at the loop head; `PLIRON_CLIF` showed the copy in the
   lowered CLIF already; `PLIRON_DUMPFN` (now also after inline,
   instcombine, memcpyopt, constcopy and sroa-fwd) showed it appear in
   memcpyopt.

## Result

- `cargo test -p pliron-ir-opt --lib memcpyopt`: 20 pass;
  `post_copy_reader_in_loop_with_dst_write_refused` fails under
  `PLIRON_MEMCPYOPT_RDWRAP=0`.
- `tests/mcrdwrap` (stanza `mcrdwrap`), native, defaults: correct
  output with the scan, exit 133 (abort in the allocator) with
  `PLIRON_MEMCPYOPT_RDWRAP=0`.
- mimas-parse alone recompiled with the fixed backend under `RECX=1
  ONCE=3`, relinked into the otherwise-`ONCE=2` tree: the four probe
  workloads equal; the same dylib with `PLIRON_MEMCPYOPT_RDWRAP=0`
  fails as before.
- The whole dsa build under `PLIRON_INLINE_RECX=1 PLIRON_INLINE_ONCE=3`
  with the fixed backend (`/tmp/dsa-bench/pkg-pliron-hx-fix`): all 19
  workloads return LLVM's results.

Not done: how many forwards the scan now refuses across the dsa build
or the held-out programs (code size and speed of the default
configuration were not measured), and whether any default-configuration
dsa crate contained a wrong forward of this kind that no workload runs.
