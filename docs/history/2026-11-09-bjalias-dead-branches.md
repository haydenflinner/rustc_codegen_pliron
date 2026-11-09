# PLIRON_BJALIAS: eager bare-jump label aliasing

2026-11-09. c33c56a (default off) + afcc31b (flip on).

## The symptom

A whole-binary disassembly census of rust-html-rewriter showed **616
unconditional `b` instructions whose target is the immediately following
address** (`b .+4`, word `0x14000001`), vs ~27 in LLVM's build. In
`SelectorMatchingVm`-family functions this read as `b.eq far; b next`
— the not-taken edge jumping to the next block instead of falling
through, 2-4x LLVM's branch count on every lol_html function.

## Why MachBuffer didn't fold them

`MachBuffer::optimize_branches` ("purgatory") truncates a tail branch
whose target resolves to the current offset — but only while the branch
is still `latest_branches.last()` and only when some `bind_label` runs
with the branch at the tail.

The dead `b`s were never that. Trace (`PLIRON_DEADB` diagnostics,
removed): at `bind_label(L_next)` the tail `b` targets `L_edge`,
**unbound and unaliased**, so it survives; its target binds nowhere near.
`L_edge` is a *bare-jump block* — a single `b L_next` edge-split block
placed later in the emission order. When block `edge` finally emits,
`optimize_branches` aliases `L_edge -> L_next` (the "a bare jump emits
nothing" path) — but by then the predecessor's `b` is long out of
purgatory, and its fixup resolves through the alias to `L_next` = its
own address +4. Same shape for `try_call` continuations
(`blr x2; b L_edge` where `edge` is `b cont`).

So the information needed to fold — "L_edge means L_next" — exists only
*after* the branch can still be removed. LLVM never has the problem:
its edge-split blocks don't exist as machine-level labels.

## The fix

`PLIRON_BJALIAS` (vendored `vcode.rs::emit`, default on; `=0` off):
before emission, compute the bare-jump blocks — one inst,
`is_term() == Branch`, no regalloc edits or allocs, one block succ —
resolve chains to the final non-bare successor (cycles `b .` / mutual
loops stay emitted), call `buffer.alias_label` eagerly, and mark them
`merged` so they emit nothing, like an edge-merge dup. Every branch to
such a block then resolves to the real target while it can still fold
at the tail — `b.c far; b edge; next:` becomes `b.c far; fallthrough`.

Excluded: the entry block, `noret_dead` blocks (which bind a label with
no code), `machine_code_cfg_info` compiles (needs every block start),
out-of-range succs. The skipped jump keeps `NO_INST_OFFSET` like a
merged dup, so `-g` changes nothing semantically.

## Numbers

rust-html-rewriter binary:

| | insns | `b` | dead `b .+4` |
|---|---|---|---|
| before | 219,324 | 13,817 | 616 |
| after | 218,300 | 12,687 | 7 |

(The surviving 7 are landing-pad/unwind edge pairs, not the
bare-jump chain.)

Held-out A/B (`--variant bja:PLIRON_BJALIAS=1`, interleaved):

- `bja/pliron` geomean **0.985** [0.928, 1.003]
- regex 0.952*, rust-json 0.956*, pulldown 0.967, compression 0.959,
  html-rewriter 1.009, protobuf 1.061 (noise)

Validation: gate 94/94, fuzz 400/400, quick green both ways. Regression
check folded into `t_edgemerge`: counts `14000001` (`b .+4`) words in
the object — 7 with `PLIRON_BJALIAS=0`, 0 on.

## Remaining

The 7 residual sites are `b` after `brk`/landing-pad edges and an
edge-block chain whose intermediate block isn't bare (carries real
code). Not worth chasing — the binary-wide branch count is now within
~8% of LLVM's shape rather than 3-4x.
