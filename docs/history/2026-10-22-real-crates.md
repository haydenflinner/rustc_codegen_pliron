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

## Status at the end of the run

- Tiers 1 and 2: all 31 crates match stock (2026-10-21 run).
- Tier 3:
  - rapier 155/155 and rune 607/2 match stock.
  - prql 543/15 matches stock, built with its own `lto = true`.
- Open:
  - **uiua:** `tests/optimized.ua` fails. The only culprit is
    `Shape::extend_from_slice` (`SmallVec<[usize; 2]>`), and a standalone
    smallvec 1.15 program does not reproduce it.
  - **nushell:** ICE in cranelift's egraph ("elaborating effectful
    instructions") on `std::thread` spawn closures for
    `nu_command::filters::tee::copy_on_thread`. The CLIF that lowering
    produces is already invalid (a use of a non-dominating value), so this
    is another IR-level bug.
  - **makepad:** the package list needed pruning; not yet run green.

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
- **Avoid running two heavy probes at once.** On this machine earlyoom
  kills one, and a bisect then counts the kill as a failure.
