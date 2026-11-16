# harness/wasm — checks on the wasm stages after CLIF

## peepdiff.mjs: the byte peepholes against their own input

`wpeep` (`src/wasm.rs`) rewrites encoded function bodies. The CLIF
pipeline has `PLIRON_DIFFCHECK` and recorded corpora; the peepholes had
unit tests and a Datalog audit of one sub-pass, and four silent
miscompiles in one day (2026-11-16) were each found by bisecting a whole
program. `peepdiff.mjs` runs every function body before and after the
peepholes in V8 on the same inputs and reports the bodies that behave
differently.

    # any wasm build, with the dump knob
    PLIRON_WASM_MODDUMP=/tmp/md harness/v8/variant.sh x      # bench + std
    PLIRON_WASM_MODDUMP=/tmp/md harness/dsa/build.sh pliron x
    node harness/wasm/peepdiff.mjs /tmp/md

`PLIRON_WASM_MODDUMP=<dir>` writes `<name>.pre.wasm` and
`<name>.post.wasm` for every object the backend emits (wipe the directory
between builds: file names carry the crate hash, stale pairs stay).
Node only, no npm packages. `tests/wasm/smoke.sh` runs it on the three
smoke objects, so `wasm_smoke` (gate, quick) fails on a difference.

| option | |
|---|---|
| `--only SUBSTR` | modules whose file name contains SUBSTR |
| `--fn NAME\|INDEX` | one function |
| `--trials N` | trials per function (16) |
| `--seed S` | PRNG seed (1); a different seed is a different run |
| `--every N [--phase K]` | sample every Nth function |
| `--coverage` | share of the code the trials executed |
| `--mutate` | self-test: share of one-instruction mutants detected |
| `--strict` | also compare memory when both sides trap |
| `--trace` | with `--fn`: both traces of every trial |
| `--json OUT` | the report as JSON |
| `--pages`, `--fuel`, `--calls`, `--confirm` | memory size, budgets |

Exit status 0 clean, 1 differences, 2 a module could not be run.

### Reading a report

    rapier2d...: 2611/2611 functions (2476 changed by wpeep), 41776 trials, ... 2 differ
      DIFF [ISOLATED] func[1984] ..soft_element_linalg15symmetric_eigen
        result: trial 1 of seed 1 (1 of 16 trials differ), args (6)
        pre  returns (-37995864391680, 37995654676480, 0, 0, 0.0061.., ..)
        post returns (-5.679798517591285e-29, 1.401298464324817e-45, 0, 0, 1, 0, 0, 1)
        replay: node harness/wasm/peepdiff.mjs DIR --only rapier2d... --fn 1984 --seed 1 --trials 2 --trace
      DIFF [caller only] func[1371] ..NeoHookeanConstraint7hessian
        memory, trial 3; wrong callee: func[1984]

Internal calls go to the real callee, so a wrong body makes its callers
differ too. Each differing function is re-run with only its own body
taken from the post module:

- `ISOLATED`: wrong by itself. Start here. When the function's own trials
  agree and it was found through a caller (the caller passes it real
  data), the entry says so and the replay command names the caller.
- `caller only`: differs through a callee; the wrong callee is named. If
  none of its callees was caught directly, the changed bodies it can
  reach are bisected for the ones that make its failing trial differ.
- `not isolated`: over the `--confirm` budget.

Then `wasm-objdump -d` (or `wasm2wat`) the function in both files and
diff; `PLIRON_WASM_PEEP_MOD=<module> PLIRON_WASM_PEEP_FI=<n>
PLIRON_WASM_TRACE_BODY=<n>` shows the body after each sub-pass.

What is compared, how inputs are made and what the instrumentation adds
is in the header of `peepdiff.mjs`.

### What it found (2026-11-16)

Each of the day's three wpeep bugs put back alone (one reverted hunk),
rebuilt, dumped, and run with the defaults (16 trials, seed 1). "Changed"
is the number of bodies whose output differs between the buggy and the
fixed backend: the most that could be wrong.

| bug | corpus | changed | flagged as wrong in isolation |
|---|---|---|---|
| keep-on-stack drops a write a hoisted get reads (`b60ceb0`) | blake3 1.8.7 + std | 10 | 2: `blake3::portable::compress_in_place` (the function of the report) and `compress_xof`, the two where the dropped `local.tee` is read again. The other 8 drop a dead write (checked statically) |
| | wasm bench + std | 9 | 0: all 9 drop a dead write |
| | the same, `PLIRON_OPT_BISECT=0` | 23 | 0: 14 drop a dead write, 9 not classified |
| loop entry keeps entries of another base (`154456d`) | dsa (rapier2d, Loro, Mimas; 68,555 functions) | 8 | 1: rapier2d's `symmetric_eigen`, the function of the report. Its own trials catch it in 8 of 20 seeds; its caller `NeoHookeanConstraint::hessian` differs in every seed tried (7) and the bisection of the caller's callees names it. Of the other 7, 4 ran every changed place 400 times without a difference and 3 were not fully reached |
| | wasm bench + std; nalgebra's `symmetric_eigen` alone | 0 | |
| | wasm bench + std, `PLIRON_OPT_BISECT=0` | 14 | 0: 4 ran every changed place without a difference, 10 were not fully reached |
| a local overwritten between its get and the store (`2ba0f69`) | dsa, `PLIRON_OPT_BISECT=0` (69,461 functions) | 74 | 60, all wasm-bindgen shims of `literate_wasm`, `debugsession_compileInfo` (the function of the report: `Session::prog_diags` gets a stack address for `self`) among them; 71 with `--trials 200`. Nothing outside the changed set |
| | wasm bench + std, `PLIRON_OPT_BISECT=0` | 1 | 1 (`<&mut IsNotEmpty as FnMut>::call_mut`) |
| each of the three | the smoke objects (`tests/wasm/smoke/peep.rs`, `peep_raw.rs`) | 1, 3, 1 | all; `tests/wasm/smoke.sh` fails |

Fixed backend, no difference: the wasm bench + std (2,643 functions, 31
seeds), the same unoptimized, blake3, dsa (68,555 functions, three seeds,
one with `--strict`), dsa unoptimized (69,461 functions), the smoke
objects (100 seeds).

### How much a clean run means

Not a proof. Measured with the default 16 trials:

| corpus | functions | time | blocks executed (`--coverage`) | mutants found (`--mutate`) |
|---|---|---|---|---|
| smoke objects (64 trials) | 33 | 0.1 s | 65% | 81% |
| wasm bench + std | 2,643 | 1.3 s | 40% (46% at 64 trials) | 67% (74% at 64) |
| dsa | 68,555 | 52 s | 27% | 62% (every 40th function) |

About 0.8 ms per function on one thread (twice that on a loaded
machine), 0.6 GB for dsa. Small functions are covered well, big ones
badly: a wild pointer or a failed bounds check ends most trials of a
large function early. A wrong body that its own trials miss is still
found when a caller in the same module feeds it real data and differs
(`symmetric_eigen` above). The header of `peepdiff.mjs` lists the blind
spots; the ones that matter most:

- paths random arguments and random memory do not reach;
- no constant data (`pliron.link` is added after the peepholes): lookup
  tables, strings and vtables are random words;
- imports are stubs, and so is an indirect call through a pointer that is
  not a function of the module, so nothing is checked across crates;
- a difference that needs two pointers to alias shows only in the trials
  where the generator made them alias (half of the pointer arguments
  repeat an earlier one);
- only `wpeep` is checked. waffle and the CLIF pipeline are upstream of
  both dumps.
