#import "../../pliron-spec.typ": *

== Words from Verification

#gl("test-stanza", "Stanza", qual: "test.sh stanza", see: "verify.stanza.layout", src: "test.sh")[
  One named end-to-end test in `test.sh`: a shell function `t_<name>`, its
  program in `tests/<dir>/`, and one dispatch line
  `want <name> && t_<name>`. `./test.sh --only=<name>` runs it alone.
]

#gl("quick-check", "Quick check", qual: "harness/quick.py", see: "verify.quick.tiers", src: "harness/quick.py")[
  The diff-driven inner loop. It reads `git diff`, picks the unit tests,
  corpora, stanzas and fuzz seeds the changed paths select, runs them, and
  prints what it skipped. A green quick check is not a licence to merge.
]

#gl("oracle", "Oracle", qual: "check script", see: "verify.triage.order", src: "docs/miscompile-triage.md")[
  A command that rebuilds the suspect code with the backend and exits
  nonzero exactly when the symptom reproduces. Every bisector drives one.
]

#gl("env-status", "env status", qual: "env-*", see: "verify.diff.stock", src: "harness/run.py")[
  A differential result whose name starts with `env-`: stock rustc fails
  too, or its own reference is unstable, so the case says nothing about
  this backend. It is never a regression.
]

#gl("expectations", "Expectations", qual: "ratchet", see: "verify.diff.ratchet", src: "harness/expectations/")[
  The checked-in `{test: status}` file per suite and host. A run fails when
  a test expected to pass stops passing, or a new test fails; improvements
  are recorded only with `--accept`.
]

#gl("poison-run", "Poison run", qual: "PLIRON_SM_POISON", see: "verify.poison", src: "pliron-clif-opt/src/slotmerge.rs")[
  A `test.sh` stanza run with `PLIRON_SM_POISON=1`: slotmerge merges
  nothing and overwrites every slot it would consider with `0xA5` where it
  believes the slot dead. A wrong live range then fails at its first stale
  read.
]

#gl("rustlantis", "rustlantis", see: "verify.fuzz.seeds", src: "harness/run.py, ../rustlantis")[
  An external generator of terminating, UB-free, deterministic Rust
  programs in custom MIR, each printing a checksum of its state. Built from
  a sibling checkout; not part of this repository.
]

#gl("fuzz-seed", "Fuzz seed", see: "verify.fuzz.seeds", src: "harness/run.py")[
  The integer that makes #term("rustlantis") produce one program. A
  #dfn[regression seed] is one that once exposed a real bug; the bank is
  `FUZZ_REGRESS` in `harness/gate.py`.
]

#gl("known-failure", "Known corpus failure", see: "verify.corpus.known", src: "harness/expectations/corpus-known.txt")[
  A corpus function whose replay failure is accepted checker noise, listed
  as `<set>/<file> # reason`. The gate and the quick check ignore it.
]

#gl("pliron-only", "Pliron-only failure", see: "verify.crates.pliron-only", src: "harness/crates.py")[
  A test that fails when a crate's suite is built by this backend and
  passes when the same suite is built by stock rustc. Crashed test binaries
  count, as `<signal: N>` entries.
]

#gl("stage2-only", "Stage-2-only failure", see: "verify.selfhost.stage2", src: "selfhost/verify.sh")[
  A compiletest failure under the pliron-built stage-2 rustc that the
  LLVM-built stage-1 rustc does not show. A candidate miscompile of rustc
  itself.
]
