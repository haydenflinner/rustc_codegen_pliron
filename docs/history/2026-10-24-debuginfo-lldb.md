# 2026-10-24: lldb on pliron builds — measured, and most gaps closed

Follow-up to 5161aad (DWARF line tables, inlined frames, O0 variables).
Goal: lldb as good on pliron-built code as on stock rustc+LLVM, first of
all for locals in optimized builds.

## Measurement

Two runners, both stock vs pliron, `-g`, Apple lldb (`lldb-2100`; it
renders stock's DWARF cleanly — Homebrew's lldb 20 misrenders some of
stock's base types, e.g. `-1 {}`):

- `harness/dbgsuite.py`: rustc's own tests/debuginfo suite (lldb-command /
  lldb-check, `#break`, revisions, aux-build) through the suite's
  `src/etc/debugger_tester`, as compiletest runs it. Its checks are
  O0-shaped; `--opt 2` adds -Copt-level=2 to both sides.
- `harness/dbgcorpus.py` + `harness/dbgcorpus/*.rs`: 8 programs, a
  breakpoint per `// BREAK` line (an opaque call), comparing every
  variable `frame variable` shows (rust-lldb formatters loaded) against
  stock. The O2 measure.

| | before (5161aad) | after |
|---|---|---|
| suite O0, tests passed (stock 75/76) | 51 | 69 |
| suite O0, checks found (stock 659/660) | 518 | 623 |
| suite O2, checks found (stock 60/660) | 53 | 132 |
| corpus O0, variables matching stock (of 123) | 97 | 118 |
| corpus O0, variables shown out of scope | 34 | 0 |
| corpus O2, variables matching stock (of 59) | 1 | 29 |
| corpus O2, wrong values | 8 | 3 |

(The suite's "after" O0 row predates the usize-typedef commit: three
repr-checked tests — basic-types, pretty-std, vec_visualizer — failed on
`usize` showing as `unsigned long`; they could not be re-run, see below.
One of the three O2 "wrong" values is stock's: `steps` inside `if steps
== 5` shows 4 in stock, 5 in pliron.)

## What changed (commits)

1. Harness (above).
2. CLIF passes keep srclocs on copied instructions (30 sites): a
   duplicated tail had no line rows, and line breakpoints resolved to
   the copy that wasn't run.
3. Types: pattern types are their base type; tuple fields `__N`.
4. Optimized locals: spill stores' values carry variable pieces
   (`Context::dbg_notes`, moved by RAUW, copied by the inliner) →
   Cranelift value labels (phis, slotprom-promoted fields, promoted
   allocas' stores) → regalloc ranges → location lists; constants as
   DW_OP_constu/stack_value; whole-variable-only locations.
5. Data-carrying enums (DW_TAG_variant_part), template type parameters,
   lexical blocks.
6. Cranelift patch 60: a dead value's location stays valid until
   overwritten (LiveDebugValues-lite).
7. Statics; subprograms in namespaces (`b top` works), `[opt]`, `dyn`
   data pointers; constants take their first user's srcloc (O0
   stepping); usize/isize typedefs.

Compile time, regex-syntax release dylib, median of 5 on a loaded box:
-O3 -g 1.61 -> 1.66 s (-O3 1.50 s), -O0 -g 0.84 -> 0.85 s.

## Still missing, by how much it hurts

1. O2: variables of memory locals SROA splits (pliron's sroa drops the
   alloca's decls when it splits it: `pair`, `s`, `i` in the corpus show
   nothing) and of by-reference arguments whose register is reused
   (`self` in `grow`). Stock recovers some through DW_OP_entry_value.
2. `-g` changes optimized code when rustc's MIR differs: MIR SROA is off
   at full debuginfo, and our passes don't fully recover
   `closures.rs`'s `largest<f64>` iterator (pre-existing; 7 of 8 corpus
   programs and test.sh's are byte-identical).
3. Stepping at function ends: `next` from a closing `}` stops twice
   (drop-locations); dummy_span's `return Ok(())` has no row of its own
   (its code is a constant and a jump).
4. A line breakpoint inside duplicated code lands on one copy only (lldb
   keeps one location per function).
5. x86_64: not run. An x86-capable dylib builds in ~2.5 min (+2.9 GB):
   `llvm-dwarfdump --verify` is clean and text is identical with and
   without -g for x86_64-apple-darwin objects, location lists name x86
   registers; but running under Rosetta hung, and lldb on a Rosetta
   process wedged this machine's debugserver (task_for_pid hangs for
   every later launch, arm64 included) — a reboot clears it. ELF output
   was only exercised by 5161aad's sightglass run.
6. Not evaluated: gdb; vtable DIEs (`create_vtable_debuginfo` is still a
   no-op, so lldb can't show a `dyn` object's concrete type).
