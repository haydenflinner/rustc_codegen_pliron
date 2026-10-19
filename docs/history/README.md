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

`../perf/` (the generated `wasm-and-native-performance.md` + its chart
scripts) is the Oct 15 Windows snapshot and is historical too.
