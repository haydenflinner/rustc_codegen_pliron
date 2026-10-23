# Bounded full unroll: is it worth building? (2026-10-22)

The question: LLVM fully unrolls loops whose trip count is a runtime
value with a small constant bound (`parse_only`'s ≤8-digit loop). Our
`unroll.rs` needs a constant-decided trip, and `punroll.rs` needs a body
with no mid-loop exits, so neither does this. Does LLVM win big on such
loops because unrolling cascades (load combining, bounds-check folding,
SROA)? Three new kernels test it (`d4e809f`): `varint`,
`varint_postcard` and `arrayvec` (see the doc comments in
`bench/benches/varint.rs` and `arrayvec.rs`).

**Verdict: no, not as a parity item.** LLVM's wins on these loops are
small, and the cascades mostly don't happen. The pliron gap that does
show up (`arrayvec` +16%) has a different cause: our loop vectorizer and
partial unroller fire on loops that can run at most 8 times. With
`PLIRON_VEC=0 PLIRON_PUNROLL=0` that gap drops to +1%. The cheap fix
is a max-trip-count fact from the dominating guard (`len <= 8`) that
keeps `loopvec` and `punroll` off such loops. Full unroll is still
worth something on top of that, mostly in places where LLVM's default
doesn't unroll either (varint, −17%), so it would beat LLVM there rather
than catch up to it.

## Numbers

Criterion, `bench/run.sh 'varint|arrayvec'`, pliron vs the stock
baseline (+ = pliron slower), two rounds:

| kernel | LLVM | round 1 | round 2 | `PLIRON_VEC=0 PLIRON_PUNROLL=0` |
|---|---|---|---|---|
| arrayvec | 5.5 ms | +15.6% | +17.3% | +0.9% (2nd round; 1st had a noisy baseline) |
| varint | 6.5 ms | +1.4% | −1.8% | — |
| varint_postcard | 6.0 ms | −0.4% | +1.1% | — |

Per function, standalone (`rustc -O -C codegen-units=1`, kernel files
included with `#[path]`, postcard/serde from the bench build's rlibs).
Each cell is the minimum over 3 alternating rounds of 41 runs. The `_u`
rows are scratch variants with the loop unrolled by hand in the source:
8 copies, each with its own `if n <= k { break }`. They stand in for what
a bounded full unroll would hand the rest of our pipeline.

| function | LLVM ms | pliron | pliron vec+punroll off | LLVM's strategy |
|---|---|---|---|---|
| varint `sum_shift` | 1.99 | −6.5% | +7% | rolled (11 insns/byte) |
| varint `sum_bounded` | 1.90 | −1% | +23% | rolled |
| varint `sum_const` | 2.04 | +1.6% | +14% | rolled |
| `sum_postcard` | 5.53 | −6% | −15% | `try_take_varint_u64` fully unrolled but **not inlined**; u32/u16 unrolled + inlined |
| arrayvec `sum_all` | 1.44 | **+28%** | +11% | fully unrolled (upper bound 8) |
| arrayvec `find_all` | 1.41 | **+48%** | +14% | fully unrolled (upper bound 8) |
| arrayvec `push_iter` | 2.19 | −3% | −15% | both loops fully unrolled, array **not** SROA'd |
| `sum_all_u` (hand-unrolled) | 1.46 | −8% | | |
| `find_all_u` (hand-unrolled) | 1.44 | +10% | | |
| `push_iter_u` (hand-unrolled) | 1.29 | +35% | | LLVM SROA'd the whole array |

(The vec+punroll-off column is a global switch, so the varint rows show
that `punroll` *helps* there today. Read that column for arrayvec only.)

## LLVM's own ablation

The cleanest measure of what upper-bound unrolling buys is to flip it in
LLVM (`-C llvm-args=-unroll-max-upperbound=N`, default 8). Same harness,
two rounds, both agree:

| function | default (8) | `=0` (no bounded unroll) | `=16` |
|---|---|---|---|
| `sum_all` | 1.43 | 1.52–1.55 (+6–8%) | same as default |
| `find_all` | 1.41–1.45 | 1.61–1.62 (+12–14%) | same |
| `push_iter` | 2.20–2.28 | **1.81 (−18%)** | same |
| `sum_shift` | 1.96–1.98 | same | **1.61 (−19%)** |
| `sum_const` | 2.05–2.08 | same | **1.70 (−17%)** |
| `sum_bounded` | 1.90 | same | same (still rolled) |

So on its own target shapes (trip ≤ 8), bounded unroll is worth 6–14%
to LLVM, and on `push_iter` it is an 18% *loss*. Unrolling there turns
one well-predicted loop-exit branch per loop into eight data-dependent
branch sites. The `n` here is random in 0..=8, so the kernel is bound
by branch mispredicts. The varint loops (bound 10) gain 17–19% when
LLVM is allowed to unroll them, but by default it doesn't.

## Mechanism, per kernel

### arrayvec `sum_all` / `find_all`: our vectorizer, not missing unroll

LLVM unrolls fully, at 4–5 instructions per element and no setup:

```
ldr  w1, [x8, #0x10]      ; len
cmp  w1, #0x9 ; b.hs panic ; the as_slice bounds check (the guard)
cbz  w1, next
ldur w10, [x8, #-0x10] ; add x0, x0, x10 ; cmp w1, #0x1 ; b.eq next
ldur w10, [x8, #-0xc]  ; add x0, x0, x10 ; cmp w1, #0x2 ; b.eq next
...
```

We emit a 16-lane `ldp q,q` vector loop (`uaddw`/`cmeq`), an 8-way
`punroll` body and a 1-element remainder. For every ArrayVec we also run
~20 instructions of dispatch (`cset`/`and`/`csel` legality and remainder
math), and the vector and ×8 bodies never run because `len <= 8`. Every
element goes through the remainder loop after that overhead. Of 131/256
instructions (sum/find), most is this dead machinery.

- With `PLIRON_VEC=0 PLIRON_PUNROLL=0`, `sum_all` becomes a 4-insn rolled
  loop (`ldr [x2],#4 ; add ; cmp ; b.ne`, the same count as LLVM's
  unrolled body), and the criterion `arrayvec` gap goes from +16% to +1%.
- Hand-unrolled, our `sum_all_u` beats LLVM by 8%.
- `find_all` keeps a gap either way: +14% rolled and +10% hand-unrolled.
  Rolled, our IV code recomputes the element address every trip
  (`sub x7,x10,#0x24 ; add x7,x7,x6,lsl #2` next to the increments, 9
  insns/elem against LLVM's 5). Hand-unrolled, every element still
  recomputes the base (`sub x5,x2,#0x24`), and the common
  "not found, keep going" path is a **taken** branch (`b.ne next`).
  LLVM's layout makes that path fall through. That is block layout and
  CSE, not unrolling.

No load combining or `ldp`: the per-element exits sit between the loads
on both sides. No bounds checks to fold: the single `len <= 8` check
before the loop is shared by all versions.

### arrayvec `push_iter`: the SROA cascade, which LLVM doesn't get either

LLVM fully unrolls both the push loop and the iterate loop. The array
still lives on the stack, though. Every push reloads and stores `len`
(`ldr w11,[sp,#0x20] ; add ; str`), keeps the `assert!(len < CAP)`
(`cmp w11,#0x7 ; b.hi`) and stores through a variable index
(`str w13,[x10,w11,uxtw #2]`). The full unroll runs after the
pipeline's SROA/GVN, `len` is never forwarded, and the index never
becomes constant. Unrolled, LLVM is 3% *slower* than us, and 18% slower
than its own `-unroll-max-upperbound=0` build.

The cascade the hypothesis describes does exist. LLVM gets it only when
the source is already unrolled (`push_iter_u`). That function has no
memory ops at all, and it runs in 1.29 ms against 2.19 ms (−41%). We
don't get it even then: `push_iter_u` keeps 36 stack accesses.
`loadfwd` forwards `len` through the first push (`stur w6,[sp,#0x20]`
with a constant 1), then reloads it after the next block boundary
(`ldur w12,[sp,#0x20]`), and every later index is variable. For pliron
to reach the −41%, a bounded unroll would need cross-block store→load
forwarding of the `len` field after it. That would put us ahead of
LLVM, not level with it.

### varint (handwritten): LLVM doesn't unroll; we already partially do

- `sum_shift` (`shift += 7; if shift >= 70 break`): LLVM leaves it rolled
  (11 insns/byte, variable `lsl x16,x16,x12`). Our `punroll` already
  unrolls it ×4 behind a `shift < 28` guard, and we are 6.5% faster. The
  unrolled copies still carry `and w4,w13,#0x3f ; lsl` on a shift that is
  known 0 at loop entry. Constant-folding the shift is the cascade left
  on the table.
- `sum_const` (`for i in 0..10 { buf.get(pos)?; ... }`, postcard's shape
  inlined): LLVM's default does *not* unroll it after inlining. At
  `-unroll-max-upperbound=16` it does, and the cascade is visible: 7
  insns/byte, with `orr x9,x9,x13,lsl #14` taking the place of the
  variable shift, the counter gone, and the bounds check left as one
  `cmp/b.hs` per byte. That is −17%. We are +1.6% against default LLVM
  with a rolled loop. Its extras: `uxtb` on loaded bytes, `and #0x3f`
  shift masks, a `min(len, 2)` `csel` recomputed per varint.
- `sum_bounded` (`for i in 0..len.min(10)`): rolled on both sides, at any
  LLVM setting. Even.

### varint_postcard: LLVM unrolls, then loses on inlining

LLVM fully unrolls postcard's `for i in 0..varint_max::<T>()` loops
(constant trip, two exits per byte). The u32/u16 decoders are inlined
and use `bfi w20,w8,#7,#7`, but `try_take_varint_u64`, which the record
uses twice, stays an out-of-line call. The cursor round-trips through
the stack at each call (`str x11,[x1]` after every byte inside it, and
`ldp x8,x9,[sp]` after). We inline everything, keep the cursor in
registers and leave the loops rolled (variable shift, `msub` for `7*i`).
Standalone we are 6% faster; criterion has it even. The unroll is
present but doesn't carry the result.

## What to build instead (ranked)

1. **Max-trip-count from the dominating guard, used as a cost input to
   `loopvec` and `punroll`.** Don't vectorize or partially unroll when
   the bound is below the vector factor or unroll factor. That is
   `arrayvec` +16% → +1% (measured with the global switches), with no
   new transform. It also stops a code-size waste on every small
   inline-array loop (`sum_all` is 131 insns, against 47 without those
   steps).
2. **Bounded full unroll, if it is built.** It should target bounds up to
   ~10 (varint) as well as ≤8, keep each copy's exit, and fold the
   per-copy constants: shift amounts, `7*i`, IV offsets. The measured
   upside is −17–19% on varint loops (from LLVM's ablation) and
   −8–16% on `sum_all` over a plain rolled loop. Add a guard against
   unpredictable trip counts (`push_iter` shows unrolling can lose
   18%). Without PGO there is no good signal for that, so the safe
   default is the LLVM-like ≤8, and varint-style loops would need a
   larger budget than that default.
3. **Cross-block `len` forwarding (loadfwd/slotprom) after unroll**: the
   only route to the SROA cascade (−41% on `push_iter_u`). It beats
   LLVM; it doesn't catch up to it.
4. Small, unroll-independent issues found along the way: the `find_all`
   IV recompute and taken-branch layout; `uxtb`/`and #0x3f` noise and
   the per-varint `min` recompute in `sum_const`/`sum_bounded`.

## Reproducing

- Kernels: `bench/run.sh 'varint|arrayvec'`.
- Per-function harness: a `main.rs` that pulls in
  `bench/benches/{varint,arrayvec}.rs` with `#[path]` and times each
  `pub fn` (min of 41 runs). Build it with `rustc +nightly-2026-10-06 -O
  --edition 2024 -C codegen-units=1`. Cargo's new build-dir layout puts
  the dependency rlibs under
  `target/release/build/<crate>/<hash>/out/`, and they are built with
  `-Zembed-metadata=no`. So pass `--extern postcard=` and
  `--extern serde=` **twice**, once for the `.rlib` and once for the
  `.rmeta`, plus `-L dependency=` for every dependency out dir (take
  them from `cargo rustc -p pliron-bench --bench kernels --profile bench
  --message-format json`). `target/release/deps` holds stale rlibs from
  an older toolchain, which fail with E0514.
- LLVM ablation: `-C llvm-args=-unroll-max-upperbound=0` and `=16`.
- The wasm leg (`bench/wasm/lib.rs`) does not include the new kernels.
  It lists kernels explicitly, and varint_postcard would need serde and
  postcard in that crate.
