# 2026-11-16: five corpus mismatches of the CLIF checker

`clif-check report --check` flagged four functions of the dsa corpus
(`target/corpus/wasm-dsa-eval`, 69,698 functions) and one of clcg
(every 4th of 38,045). None is a wrong CLIF pass. Three were the
checker; two are a miscompile of the IR pass `memcpyopt` that was fixed
after the corpus was recorded.

| function | verdict | cause |
|---|---|---|
| `robust::incircleadapt` (dsa) | checker | a slot over 4 KiB overlapped the next slots in the sandbox |
| `FlattenCompat::next` (dsa) | checker | never-written bytes copied out; the rerun that should say so did not return |
| aarch64 `emit` (clcg) | checker | the same |
| bincode `next_element_seed` (dsa) | stale corpus | `memcpyopt` left `lifetime.start(dst)` behind a forwarded write; fixed in `7ee65e8` |
| `DragSession::drop` (dsa) | stale corpus | the same |

## What each one was

**incircleadapt.** "before returned 0.0, after trapped `user2`".
`PLIRON_DIFFCHECK=step` names slotmerge. The block paths of the two
runs part at a compare of `load.f64 ss52`: 0.0 in the original, NaN in
the rewritten function (trial 0 has an infinite argument, so the NaN is
what the program computes and stores there). Watching the byte: in the
original, `memset(ss51, 0, 9216)` writes it. `ss50` and `ss51` are
`[f64; 1152]`, and the sandbox gave every slot one 4 KiB page, so the
memset ran over `ss52` and `ss53`. slotmerge had folded `ss52` onto
`ss50`, out of the memset's reach. The original's run was the wrong
one. The NaN canonicalization of `9f9fba1` is not involved: the report
is the same, word for word, with the checker of `9f9fba1^`. Fix: each
slot gets whole pages for its
size; the layout is computed per pair from the larger size of each slot
index, so both runs still put slot `k` at the same place.

**FlattenCompat::next, emit.** 128 and 11 bytes differ in memory the
function writes through a pointer. Watching one of them back: a copy of
a whole slot (364 and 48 bytes) of which the function stored only the
tag (`self.frontiter = None`; an i32 `2`), so the original copies the
frame's zero fill and the rewritten function whatever slotmerge put
under the slot. The checker tells such bytes by running the original
again under another fill (0xa5) and comparing; in `emit` that rerun
does store `0xa5a5a5a5_00000002` where the first stored `2`. But both
reruns then leave the path of the first run (they test a byte a stub
left unfilled, a stub fills 64 bytes of an out-slot) and load through a
pointer made of fill bytes, with `notrap`: an interpreter panic,
"inconclusive", and an inconclusive rerun kept the report. Fix: a rerun
that does not return, for any reason, means the original's behaviour
depends on bytes it never wrote, and the trial counts as inconclusive.

`emit` mismatches with the checker of `9f9fba1^` too (same trial, same
bytes): not from today's checker changes.

**next_element_seed, DragSession::drop.** 12 and 4 determined bytes of
the returned value differ; slotmerge is the step. In both the recorded
function writes a slot ahead of that slot's `lifetime.start` and reads
it after (the `; diffcheck-lifetimes:` header read against the code:
in the bincode function the start of `ss3` sits after the stores to
`ss3+240..264` and the 240-byte copy into it, right before the copy
out; in `drop` the start of `ss10` heads the block that reads the 12
bytes another block stored). slotmerge
reads the marker as "dead before here" and reuses the bytes. That is
the `install_assoc` bug of the morning (`2026-11-16-wasm-night.md`,
"dsa, 06:20"), in two more functions: the corpus files are dated
04:42-04:46, the fix `7ee65e8` 06:18. A real miscompile in the build
that recorded the corpus, already fixed, and the reports are right
about the functions as recorded. Not added to
`harness/expectations/corpus-known.txt`: re-record the corpus instead.
`memcpyopt` gained a test with the writer in an earlier block
(`dst_marker_heading_the_copy_block_dropped`).

**Not verified:** that today's `memcpyopt` leaves no such marker in
these two functions. That needs the backend and a dsa build to record
the corpus again; neither was built here. The new unit test covers the
shape, not the functions.

## Why `clif-check pipeline` passed all of them

It builds a `Pipe` per step and read the lifetime markers from the
file's header each time; the header places a marker by instruction
ordinal in the function as recorded, so from the second step on they
pointed at other instructions and slotmerge merged differently. It
carries the markers now. `PLIRON_DIFFCHECK=step clif-check report` was
right all along (one `Pipe`). `pair` did not set the 32-bit pointer
mode (`heap_oob` at the first `stack_addr.i32`); fixed.

## Calls that could be argued

- **The rerun rule forgives more.** Before, a rerun that trapped
  dropped the trial and one that was inconclusive kept the report. Now
  both drop it. A wrong transform in a function whose original also
  branches on never-written bytes is no longer reported from that trial
  (it has to show on another one, or after the corpus is re-recorded
  with other seeds). The alternative that would lose nothing is a
  definedness bit per stack byte and per value in the interpreter
  (MemorySanitizer's model): then a copied-out undefined byte is exempt
  by itself and a branch on one ends the original's run as undefined
  behaviour, with no rerun. Not built.
- **Stubs fill 64 bytes.** Both reruns went astray on a byte of a
  callee's out-slot past the 64 the stub writes. A stub that filled the
  whole extent would make those bytes determined, and send the original
  down paths with made-up pointers more often. Left as is.
- **Lifetime markers are not modelled by the checker.** If they were
  (a slot's bytes become the fill at its `lifetime.start`), the two
  stale-marker functions would pass, and the checker would stop seeing
  the one kind of IR-pass bug it found here. Left unmodelled on purpose.

## Numbers

`PLIRON_VERIFY=full clif-check report <corpus> out.json --check`, at
`92d914a` and with these changes:

| corpus | before | after |
|---|---|---|
| std (4,925) | 4,240 ok, 685 inconclusive | the same, no function changed status |
| clcg, every 4th (9,512) | 8,645 ok, 866 inconclusive, 1 mismatch | 8,646 ok, 866 inconclusive |
| wasm-dsa-eval (69,698) | 56,709 ok, 12,985 inconclusive, 4 mismatches | 56,710 ok, 12,986 inconclusive, 2 mismatches |

On dsa exactly two functions changed status: `incircleadapt` to ok,
`FlattenCompat::next` to inconclusive (no trial of it is comparable:
each one either stops in the original or depends on never-written
bytes). The two left are the stale-marker functions. The slot layout
changed no other verdict in any of the three corpora.
`cargo test -p pliron-clif-opt`: green.
