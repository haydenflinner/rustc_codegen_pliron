# History

Dated session logs, superseded reports and parked work. Nothing here is
current guidance; read `AGENTS.md` and `docs/PARITY.md` first. These are
kept for archaeology: when a pass misbehaves, the log entry that
introduced or last fixed it often explains why it is shaped the way it
is (search by pass name).

| file | what it is | period |
|---|---|---|
| `coordination-ledger.md` | multi-agent coordination log: merges, stage2 self-host miscompiles #1–#4, bisects, profiling rounds | Oct 10–16 |
| `native-notes.md` | aarch64 optimization session log (loopvec, bcheck, DSE, RMW loop deletion) | Oct 8–10 |
| `x64-notes.md` | x86-64 session log (target-cpu ISA flags, unroll, strided gathers) | Oct 8–10 |
| `codegen-improvements.md` | early -O pass notes + microbenchmarks; wild vs mold | Oct 8–9 |
| `llvm-passes.md` | LLVM -O2 pass inventory vs ours (status as of Oct 10 — many `part`/`—` rows have since landed) | Oct 10 |
| `DEBUG-LOG.md` | Windows overnight: WindowsFastcall two-float-return bug, MSVC-free linking | Oct 11 |
| `QUESTIONS.md` | questions an unattended session left for the user | Oct 16 |
| `PERF-REPORT.md`, `perf-data-2026-10-16.md` | Windows i7-12700K parity report + raw data (native, wasm, cubecl JIT) | Oct 16 |
| `notes/perf-report-windows.md` | earlier Windows parity report | Oct 14 |
| `dod-steps-4-5.md` | data-oriented refactor plan, steps 4–5 (step 5 landed in 62eafb7) | Oct 16 |
| `notes/inflight-mempromote-loadfwd.{md,patch}` | parked loadfwd/licm scoped-root patch (hmget get loop), never validated | Oct 12 |
| `wip/STATUS-stage2-miscompile4.md`, `wip/lowerfold-gate.patch` | stage2 miscompile #4 bisect drop point | Oct 12 |
| `2026-10-12-coordination-ledger.md` | parallel-prototyping ledger (subagent worktrees), kept outside the repo until 2026-10-30 | Oct 11–12 |
| `2026-10-13-perf-report/` | the generated `wasm-and-native-performance.md` report (branch `wasm-simd`), its raw tables, charts, `report.html` and the scripts that build them (formerly `docs/perf/`) | Oct 13–15 |
| `2026-10-16-narrow-vec-gaps.md` | diagnosis of sub-128-bit vector gaps in the CubeCL path; superseded by d5d4d01 (f32x2/f16x4 leaves) | Oct 16 |
| `2026-10-18-test-harness-findings.md` | findings and early plan from building `harness/run.py` (moved out of `docs/test-harness.md`) | Oct 18 on |
| `2026-10-20-wasm-open.md` | wasm parity working ledger: open and undecided items | Oct 19–20 |
| `2026-10-20-compile-time-worklist.md`, `2026-10-21-wasm-runtime.md`, `2026-10-27-native-runtime.md` | former sections 1–3 of `docs/PARITY.md` | Oct 20–27 |
| `2026-10-21-wasm-experiments.md` | wasm runtime experiment ledger: tried, landed, rejected (formerly `docs/experiments.md`) | Oct 21 |
| `2026-10-22-parity-scoreboard.md`, `2026-10-28-parity-card-snapshot.md` | former `docs/PARITY.md` scoreboards, incl. the Windows i7 re-measure | Oct 18–29 |
| `2026-10-23-seh-handoff.md` | pick-up notes for real SEH catch (design: `../seh-design.md`) | Oct 23 |

Other `YYYY-MM-DD-<topic>.md` files are single-session logs; the name
says what and when.
