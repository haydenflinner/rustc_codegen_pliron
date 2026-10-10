LLVM `-O2` pass inventory vs what `rustc_codegen_pliron` currently implements.

**Status** — `done` = we have it (our file/pass in parens); `part` = meaningful
subset; `—` = nothing. **Gain** — expected codegen win if built/finished,
informed by `/tmp/cpubench`, `/tmp/wbench`, and the stage2 regex-syntax proxy:
`high` moves measured kernels or whole-program counts, `med` plausibly moves
them, `low` is niche or already captured elsewhere. **Effort** — rough size of
the change: `S` ≤ ~150 LOC / one file, `M` ~150–600 / a pass + pipeline hook,
`L` new analysis or multi-pass machinery.

Two free riders worth remembering when reading "gain": rustc's MIR opts run
before us (copy-prop, const-prop, MIR inlining, some simplifycfg — the easy
wins are partly consumed), and Cranelift's egraph does scoped CSE/DCE/algebraic
folds at lowering time (covers a slice of instcombine/dce for free).

### Scalar (function-level)

| Pass | What it does | Status | Gain | Effort |
|---|---|---|---|---|
| `sroa` | Splits aggregates and promotes allocas to SSA values | done (`sroa.rs`) | foundational, landed | — |
| `mem2reg` | Promotes allocas to SSA registers (simpler subset of SROA) | done (`sroa.rs`) | landed | — |
| `early-cse` | Cheap common-subexpression elimination early in the pipeline | part (egraph CSE at lowering; no early pass) | low | M |
| `gvn` | Global value numbering; removes redundant loads and expressions | part (`loadfwd.rs` = load elim, no PRE, no expr GVN) | med-high (PRE is the missing half) | L |
| `newgvn` | Alternative, more complete GVN implementation | — | med-high (same as gvn) | L |
| `gvn-hoist` / `gvn-sink` | Hoists or sinks equivalent instructions across branches | — (clifpeep sinks within blocks only) | med | M |
| `instcombine` | Large peephole combiner for algebraic simplification | part (`instcombine.rs` egg ints; `clifpeep.rs`; `constload.rs`) | med (fp/vector/memory patterns open) | M |
| `aggressive-instcombine` | Costlier pattern folds that instcombine skips | — | low-med | M |
| `instsimplify` | Simplifies instructions without creating new ones | part (clifpeep + egraph) | low | S |
| `sccp` | Sparse conditional constant propagation | part (`unroll.rs` const-props through loops; `jumpthread` folds) | low-med | M |
| `correlated-propagation` | Uses value ranges and branch facts to simplify code | — | med-high (Rust is bounds-check dense) | L |
| `constraint-elimination` | Removes compares proven by dominating conditions | part (`jumpthread::fold_dominated_conds` folds dominating same/implied/range facts; offset-form `i+k<=n` measured ~0 candidates on the bc_* sweep — see native-notes) | low residual | L |
| `jump-threading` | Threads branches whose outcome is known on some paths | done (`jumpthread.rs`) | landed | — |
| `dfa-jump-threading` | Jump threading for state-machine-style switch loops | — | low | M |
| `simplifycfg` | Merges, removes, and folds basic blocks and branches | part (`unreach`, `tailmerge`, `taildup`, `switchmap`, `fusechains`, `phisimp`) | med (block merging still open) | M |
| `dce` / `adce` / `bdce` | Dead code elimination: basic, aggressive, and bit-tracking | part (egraph DCE + `unreach`; no ADCE/bdce) | low | S |
| `dse` | Dead store elimination | done (`dse.rs`: backward may-read fixpoint over `loadfwd` roots; covers same-loc overwrite + unread non-escaping slots; atomics/fences/volatile barred) | landed | — |
| `memcpyopt` | Optimizes memcpy/memset; forms them from loads and stores | part (`memcpyopt.rs` = call-slot forwarding only) | low-med | M |
| `mldst-motion` | Merges loads and stores in diamond-shaped CFGs | — (`loadfwd` is closest) | low-med | M |
| `reassociate` | Reorders commutative expressions to expose folding | part (egraph reassociates; `deflag` unblocks loopvec) | med (reduction chains, fma) | M |
| `nary-reassociate` | Reassociation aimed at CSE of n-ary expressions | — | low | M |
| `slsr` | Straight-line strength reduction | — | low (loop SR covered by loopvec/indvars) | M |
| `separate-const-offset-from-gep` | Splits GEPs to expose shared address math | — | low (our addressing is already split) | S |
| `tailcallelim` | Turns self-recursive tail calls into loops | — | low (rare in Rust; CallConv::Tail helps anyway) | M |
| `sink` | Moves instructions into the successors that use them | part (`coldargs`, clifpeep local sinks) | low-med | S |
| `speculative-execution` | Hoists cheap code out of branches (mostly for GPUs) | — | low | M |
| `callsite-splitting` | Duplicates calls so each copy gets more constant arguments | — | low-med | M |
| `consthoist` | Hoists expensive constant materialization | — | low | S |
| `div-rem-pairs` | Shares work between matching div and rem | — | low | S |
| `float2int` | Demotes float math to integer math where it is exact | — | low | S |
| `mergeicmps` | Merges chains of compares into memcmp | — | low | M |
| `partially-inline-libcalls` | Inlines a fast path for calls like sqrt | — | low | M |
| `libcalls-shrinkwrap` | Guards libcalls so the common case avoids errno work | — | low | M |
| `lower-expect` | Turns `llvm.expect` into branch weights | part (cold-region placement via `PLIRON_COLD`) | low | S |
| `lower-constant-intrinsics` | Folds `is.constant` and `objectsize` | done upstream (MIR) | — | — |
| `alignment-from-assumptions` / `infer-alignment` | Raises alignment from assumptions and analysis | — | low-med (enables pair fusion) | M |
| `vector-combine` | Scalar/vector peepholes guided by the cost model | — | med (vector epilogues, shuffles) | M |
| `scalarizer` | Splits vector operations into scalar ones | — | n/a | — |
| `reg2mem` | Demotes SSA values back to allocas (inverse of mem2reg) | — | n/a | — |

### Loops

| Pass | What it does | Status | Gain | Effort |
|---|---|---|---|---|
| `loop-simplify` / `lcssa` | Canonical loop form (preheader, dedicated exits, LCSSA) | part (passes assume rotated canonical form) | enables everything | M |
| `loop-rotate` | Converts loops to do-while form | done (`looprot.rs`) | landed | — |
| `licm` | Hoists and sinks loop-invariant code | done (`licm.rs`, incl. notrap-load guard) | landed | — |
| `simple-loop-unswitch` | Moves invariant conditions out of loops | — | med | M |
| `indvars` | Canonicalizes and simplifies induction variables | done (`indvars.rs` ×2, + count-down IV, + stream-base SR) | landed | — |
| `loop-idiom` | Recognizes loops that are memset/memcpy, popcount, etc. | part (`loopidiom.rs` = memset/memcpy) | low-med (popcount/ctz open) | S |
| `loop-deletion` | Removes loops with no side effects | done (`loopdel.rs`) | landed | — |
| `loop-instsimplify` / `loop-simplifycfg` | Loop-aware cleanup | part (via pipeline order) | low | S |
| `loop-unroll` / `loop-unroll-full` | Partial, runtime, and full unrolling | done (`punroll.rs` side-exits; `unroll.rs` const-trip) | landed | — |
| `loop-unroll-and-jam` | Unrolls an outer loop and fuses the inner copies | — | high (matmul register tiling, biggest known gap) | L |
| `loop-reduce` | Loop strength reduction (LSR), tuned to the target | part (loopvec stream bases, count-down IV) | med (scalar-pointer loops remain) | M |
| `loop-distribute` | Splits a loop to isolate parts that can be vectorized | — | med | M |
| `loop-fusion` | Fuses adjacent compatible loops | — | med | L |
| `loop-interchange` | Swaps loop nest order for locality | — | med-high (matmul, column access) | L |
| `loop-flatten` | Collapses a loop nest into one loop | — | low-med | M |
| `loop-load-elim` | Forwards stores to loads across iterations | — | med (same-address RMW loops: hist) | M |
| `loop-versioning` / `loop-versioning-licm` | Versions loops behind runtime alias checks | done (`bcheck.rs`, incl. <2-fold skip) | landed | — |
| `loop-predication` / `guard-widening` | Hoists and widens guards and checks | part (`bcheck` widening, loopvec early-exit guards) | med | M |
| `irce` | Inductive range check elimination | part (`bcheck` covers counted-loop checks only) | med-high (general version kills residual checks) | L |
| `loop-bound-split` | Splits a loop at a condition on the induction variable | — | low-med | M |
| `loop-data-prefetch` | Inserts software prefetches | — | low (M3/x64 prefetchers cover streams) | S |
| `loop-sink` | Sinks preheader code back into cold loop bodies | — | low | S |

### Vectorization

| Pass | What it does | Status | Gain | Effort |
|---|---|---|---|---|
| `loop-vectorize` | Vectorizes and interleaves inner loops | done (`loopvec.rs`: 128-bit, UNROLL 4, widening/dot/minmax/ordered-fp/early-exit, x64 psadbw) | landed — our biggest win | — |
| `slp-vectorizer` | Vectorizes straight-line code (superword parallelism) | done (`slp.rs`) | landed | — |
| `load-store-vectorizer` | Merges adjacent loads and stores (mainly GPUs) | part (aarch64 ldp/stp fusion in emit; not a vectorizer) | low | M |

### Interprocedural (module and CGSCC)

| Pass | What it does | Status | Gain | Effort |
|---|---|---|---|---|
| `inline` / `always-inline` | Cost-based inlining and forced inlining | part (`inline.rs` = CGU-local small callees; MIR inliner ran already) | med (cross-CGU absent) | L |
| `partial-inliner` | Inlines only the early-exit part of a callee | — | low | L |
| `ipsccp` | Interprocedural SCCP, including function specialization | part (`spec.rs` = same-constant-arg specialization) | med | M |
| `globalopt` | Optimizes globals: constant-folds, localizes, shrinks | part (`constload.rs` folds global-init loads) | low | M |
| `globaldce` | Removes unreachable globals and functions | part (linker GC covers most) | low | S |
| `constmerge` | Merges duplicate constant globals | — | low (codesize) | S |
| `deadargelim` | Removes dead arguments and return values | — | low-med | M |
| `argpromotion` | Turns pointer arguments into by-value arguments | — | med (feeds sroa/alias) | M |
| `function-attrs` / `rpo-function-attrs` | Infers attributes such as readnone and nocapture | part (`nounwind.rs`, `nowrite.rs` — feeds loadfwd/licm) | med | M |
| `inferattrs` | Adds known attributes to library functions | — | low | S |
| `attributor` / `attributor-cgscc` | Fixpoint framework that deduces attributes | — | med-high but very large | L |
| `mergefunc` | Merges identical functions | — | low (codesize) | M |
| `hotcoldsplit` | Outlines cold regions into separate functions | part (`PLIRON_COLD` placement; no outlining) | low-med | M |
| `iroutliner` | Outlines repeated code sequences to reduce size | — | low | L |
| `called-value-propagation` | Narrows targets of indirect calls | — | low-med | M |
| `elim-avail-extern` | Drops `available_externally` bodies after inlining | n/a | — | — |
| `strip-dead-prototypes` | Removes unused declarations | — | low | S |
| `globalsplit` | Splits globals so unused parts can be removed | — | low | M |
| `wholeprogramdevirt` | Devirtualizes calls under LTO | n/a (Rust dyn dispatch is rare; MIR devirtualizes some) | — | — |
| `openmp-opt` | OpenMP-specific optimizations | n/a | — | — |
| `coro-early` / `coro-split` / `coro-elide` / `coro-cleanup` | Lowers and optimizes coroutines | n/a (rustc lowers coroutines upstream) | — | — |
| `rel-lookup-table-converter` | Converts lookup tables to relative offsets (PIC) | — | low | S |

Not listed: analysis passes, sanitizer and PGO instrumentation passes (`asan`,
`pgo-instr-gen`, etc.), and the backend MachineFunction passes (register
allocation, scheduling, machine-licm, etc.). Those are a separate pipeline in
llc — our analogs live in vendored Cranelift/regalloc2 (see
`vendor/*/PLIRON_PATCH.md` for patches already applied there, e.g. the
direct-use sinking rule and the regalloc2 spill-priority change).

**Highest-value missing pieces by measured evidence** (see
`coordination-ledger.md` and `native-notes.md`): `loop-unroll-and-jam` /
register tiling (matmul is the biggest single gap on all targets),
`correlated-propagation`/`constraint-elimination`/full `irce` (bounds-check
density is the main scalar-side whole-program cost), `dse`+`loop-load-elim`
(store-side work we never eliminate), and `reassociate` strength (reduction
chain shapes). Everything already landed is measured in the ledger.
