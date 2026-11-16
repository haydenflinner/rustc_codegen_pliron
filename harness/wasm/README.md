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

Each of the day's three wpeep bugs reintroduced alone (one reverted hunk),
rebuilt, dumped:

| bug | corpus | bodies the bug changed | flagged |
|---|---|---|---|
| keep-on-stack drops a write a hoisted get reads (`b60ceb0`) | blake3 1.8.7 + std | 10 | `blake3::portable::compress_in_place`, `compress_xof`: the two where the dropped `local.tee` is read again. The other 8 drop a dead write (checked statically) |
| | wasm bench + std | 9 | none: all 9 drop a dead write |
| loop entry keeps entries of another base (`154456d`) | dsa (rapier2d, Loro, Mimas) | 8 | rapier2d's `symmetric_eigen`, the function of the original report (found directly in 8 of 20 seeds, through its caller `NeoHookeanConstraint::hessian` in all 7 seeds tried). The other 7 were not shown wrong or right |
| | wasm bench + std, optimized | 0 | |
| | a 4-function crate with the shape (`[b]=v; loop { acc+=[b]; [c]=acc }`) | 4 | 4 of 4 in 9 of 9 seeds |
| a local overwritten between its get and the store (`2ba0f69`) | wasm bench + std, `PLIRON_OPT_BISECT=0` | 1 | that one (`<&mut IsNotEmpty as FnMut>::call_mut`: the load after the store reads the new local) |

Fixed backend: no difference on the wasm bench + std (2,643 functions,
31 seeds), the same unoptimized, blake3, and dsa (68,555 functions, three
seeds, one of them `--strict`).

### How much a clean run means

Not a proof. Measured with the default 16 trials:

| corpus | functions | time | blocks executed (`--coverage`) | mutants found (`--mutate`) |
|---|---|---|---|---|
| smoke objects (64 trials) | 24 | 0.3 s | 61% | 80% |
| wasm bench + std | 2,643 | 1.3 s | 40% (47% at 64 trials) | 67% (74% at 64) |
| dsa | 68,555 | 52 s | 27% | not run |

About 0.8 ms per function, one thread, 0.6 GB for dsa. Small functions
are covered well, big ones badly: a wild pointer or a failed bounds check
ends most trials of a large function early. The header of `peepdiff.mjs`
lists the blind spots; the ones that matter most:

- paths random arguments and random memory do not reach;
- no constant data (`pliron.link` is added after the peepholes): lookup
  tables, strings and vtables are random words;
- imports are stubs, and so is an indirect call through a pointer that is
  not a function of the module;
- a difference that needs two pointers to alias shows only in the trials
  where the generator made them alias (half of the pointer arguments
  repeat an earlier one);
- only `wpeep` is checked. waffle and the CLIF pipeline are upstream of
  both dumps.
