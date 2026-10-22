# Real-crate test suites under pliron (2026-10-21/22)

`harness/crates.py` runs pinned crates' own test suites under stock rustc
and pliron and reports pliron-only failures. The tiers:

- **1:** 20 core crates.
- **2:** 11 more: crossbeam, parking_lot, rayon, tokio, serde_json, toml.
- **3:** 6 applications cloned from local checkouts at a pinned commit:
  uiua, nushell, rapier, prql, rune, makepad.

The table lists each bug found and its fix. Every fix has a regression
test that fails without it.

| found by | symptom | pass / cause | commit |
|---|---|---|---|
| sha2 | SIGTRAP | missing `llvm.aarch64.crypto.*` bodies | e803048 |
| smallvec | double drop | loadfwd/licm: region-frozen loc immune to a same-root store | 8205c7f |
| regex, regex-automata | null deref | `Option<&T>` (`dereferenceable_or_null`) treated as dereferenceable | 06cf9b1 |
| rayon `execute_strings_split` | `split_ascii_whitespace` skipped 64-byte groups | loopvec: early-exit *data* cond normalized with `bnot` (0/1 lanes stay nonzero) | a6f134c |
| rayon `collect_drop_on_unwind_zst` | 0 drops | loopidiom deleted a cleanup loop: every block of a loop whose exit leads to resume is a `deadend` | 3b1ea4a |
| serde_json `test_retain` | SIGILL (unwrap on None) | `slot_loads` ignored i64 loads of a stored pointer's bytes → `extract_if` nocapture(self) → stale map length via xfacts | 05bc487 |
| tokio | ICE (cranelift egraph) | dem64 demoted a `select` cond as a web operand and removed its def | 29cdc47 |
| uiua (zlib-rs) | `asm!` unknown `crc32x` | rsasm had no CRC32 | 6751ea4 |
| nushell (zopfli) | regalloc EntryLivein | bcheck clone re-entered the original region through a shared inner loop | 1aad18a |
| rune (portable-atomic) | `asm!` unknown `ldar`/`swpp`, `.arch_extension` | rsasm's table generator dropped every scalar extension | 1974223 |
| nushell (scraper) | ICE, CLIF use of a non-dominating param | domcheck/phisimp looked up invoke pads on the terminator only (since f9b9ef7, 2026-10-15) → sroa-fwd forwarded into cleanup code | 22eb8fe |
| prql | `lto = true` rejected | no LTO; fat LTO now = `cc -r` of every object, rlibs carry a postcard payload | dc49339 |
| `ctor`-style statics | constructors never ran | `__DATA,__mod_init_func` without a section type was `S_REGULAR` | ed986ce |
| prql `test_append_select_multiple` | extra SQL column | slotmerge: reads through a container (closure env) didn't keep the slot live across the loop; self-loops weren't backedges | 8f9c780 |
| nushell (nu_command `copy_on_thread`) | ICE, cranelift egraph | ops after a mid-block invoke (inliner, sroa reloads, sret2reg) were credited to the pad by block dominance → sroa-fwd into cleanup | fa11870 |
| uiua `Shape::extend_from_slice` | stale SmallVec capacity | CLIF block merges kept an inlined `&self` helper's region tag; regions now derive from instructions | c035f52 |
| uiua `derive_sortedness` | `Ordering::Less` folded away | ovkill bounds took the `min` over a pred's edges to one block (`brif c, b(0), b(x)`) | 914fb36 |
| nushell `nu-cmd-lang` tests | undefined `usize::rotate_left` at link | xcgu exempted every fn *named* `rotate_left` from needing a body | 8f7845c |
| uiua `prim_docs` (xlsx) | `free`: pointer not allocated | xcgu copied `extern "C"` allocator callbacks that zlib-rs compares by address | b850c03 |

## Status (2026-10-22, 13:56)

- Tiers 1 and 2: 31 crates, all match stock (2026-10-21 run). A rerun against
  the 2026-10-22 fixes was in progress at this point.
- Tier 3: all six apps match stock.

| app | result |
|---|---|
| uiua | 22/0 |
| nushell | 3504/4 |
| rapier | 155/0 |
| prql | 543/15, with its own `lto = true` |
| rune | 607/2 |
| makepad | 21/14 |

## Lessons for the next hunt

These are also in the agent memory notes.

- **fnbisect names carry the crate hash.** Rebuild with exactly the
  probe's cargo environment, or `PLIRON_OPT_FNS=@file` matches nothing.
- **Fact-driven passes need their facts.** A `clif-check step` replay
  without the facts header loses them; slotmerge only merged slots
  in-pipeline.
- **"First CLIF step broke the function" means the bug is earlier.**
  Verify the pre-pipeline CLIF: twice tonight it was already invalid
  coming out of IR-opt/lowering.
- **A silent SIGABRT in a test can hide its message.** The test harness
  swallows the malloc/abort text, so rerun with `--nocapture`, or break on
  `abort` in lldb and walk the frame-pointer chain (lldb's own unwinder
  stops at `__pthread_kill`). This beat a 1-hour copy bisect by about
  50 minutes.
- **`PLIRON_XCGU=0` fixing a bug that `PLIRON_OPT_FNS=0..0` doesn't means a
  cross-CGU copy is at fault, not an optimization.**
  `PLIRON_XCGU_LOG`/`PLIRON_XCGU_ONLY` let fnbisect delta-debug the copies.
- **Avoid running two heavy probes at once.** On this machine earlyoom
  kills one, and a bisect then counts the kill as a failure.
