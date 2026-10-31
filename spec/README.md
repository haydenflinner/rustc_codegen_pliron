# rustc_codegen_pliron specification

A formal, cross-linked specification of what this backend promises, how each
stage keeps the promise, where it differs from stock rustc + LLVM, and which
check holds each claim to account. Written in [Typst](https://typst.app) and
traced with [tracey](https://github.com/bearcove/tracey); the machinery is
adapted from dsa's Nina spec (`../dsa/spec`). SLCR-style: sliceable, linked,
checkable requirements. Tracey is the discovery and coverage half,
`pliron-spec.typ` the rendering and linking half.

    ./build.sh            # PDF in build/pliron-spec.pdf (must build with no warnings)
    ./build.sh html png   # experimental HTML, PNG page previews
    ./build.sh trace      # tracey status + validate (tracey on PATH)

Tracey needs its Typst backend, which is on upstream main but not yet in a
crates.io release: `cargo install --git https://github.com/bearcove/tracey tracey`.

Fonts: Libertinus Serif (built into Typst), Atkinson Hyperlegible Next and
Mono (bundled in `fonts/`, SIL OFL; copied from dsa). The build ignores
system fonts, so the PDF is the same on every machine.

## Layout

    main.typ              document root: cover, contents, chapters, back matter
    pliron-spec.typ       theme + requirement machinery (import this)
    chapters/*.typ        one chapter per file; tracey reads these
    ../.config/tracey/config.styx

| Chapter | Covers |
|---|---|
| 00-vocabulary | one entry per word (`#gl`) |
| 01-model | L0: the contract and the properties (roots of the trace) |
| 02-frontend | `src/` builder, ABI, intrinsics, SIMD, asm: MIR → LLVM dialect |
| 03-iropt | `pliron-ir-opt/`: inlining, SROA, IPO summaries |
| 04-clif | `pliron-clif-lower/`, `pliron-clif-opt/`: lowering and the CLIF pipeline |
| 05-native | objects, unwinding, asm splicing, debug info, LTO, hot reload, Windows |
| 06-wasm | waffle emission, wasm objects, pliron-wasm-ld, emulated EH, wpeep |
| 07-parallel | CGU pool, cross-CGU copies and facts, caches, determinism rules |
| 08-verification | gate, corpus, diffcheck, crates, UI, self-host, fuzz, parity card |

## Levels

| Level | Meaning |
|---|---|
| L0 Model | the contract: inputs, outputs, properties; roots of the trace |
| L1 Behaviour | what each stage does, in L0 words |
| L2 Reference | formats, symbol names, section layouts, knob names, step lists |

`chapter("id", N)` sets the default level of the requirements that follow; a
single requirement overrides it with `tier: N`.

## Writing a requirement

```typst
#r("clif.budget", title: "Fixpoints are budgeted", level: "must",
   refines: "model.bail", depends: "clif.analysis", see: "iropt.budget",
   checked: "harness/gate.py corpus")[
  Every fixpoint in a CLIF step MUST stop after ...
]
```

* **id**: `area.topic[.detail]`, lowercase, dots and hyphens. Stable forever.
  Unique (a duplicate fails the build). The area is the chapter id
  (`front`, `iropt`, `clif`, `native`, `wasm`, `par`, `verify`).
* **level**: `"must"`, `"should"` or `"may"`. Omit for pure definitions. Write
  MUST/SHOULD/MAY in capitals in the body.
* **refines**: ids on a *lower level number* this requirement makes concrete.
  The build fails otherwise. Every L1/L2 requirement should refine
  something; the Level map lists the ones that don't.
* **depends**: ids that must hold first. **see**: loose links.
* **checked**: the check that holds the claim to account: a gate leg, harness
  script, `test.sh` stanza, unit test. Listed in *Properties and checks*.
* **status**: `"draft"` marks a claim checked against docs but not yet
  against code or a test.
* One atomic, testable claim per requirement. Where docs and code disagree,
  follow the code and add a `#drift[..]` note.

Tracey reads `level`, `status`, `since`, `until`; the other arguments are ours.

### Differences from LLVM

```typst
#llvm(kind: "gap", summary: [One line for the back-matter table.])[
  The full explanation, shown in place.
]
```

`kind` is `"gap"` (LLVM does it, we don't yet), `"choice"` (different on
purpose) or `"extra"` (we do something LLVM doesn't). Put it right after (or
inside) the requirement it qualifies; *Where we differ from LLVM* at the back
collects every note with a link back.

### Other helpers

`#refr("a.b")` (clickable id; unknown ids fail the build), `#term("id")` /
`#term("id", [shown words])` (glossary link; unknown terms fail the build),
`#dfn[..]`, `#note[..]`, `#example[..]`, `#drift[..]`, `#resolved[..]`.

## Tracing code

In source: `// r[impl clif.budget]`; in tests `// r[verify clif.budget]`.
Bump `r[..+2]` when a requirement's meaning changes (tracey then flags stale
references). `vendor/` is not scanned; vendored patches are described from
their `PLIRON_PATCH.md`.

## Known limits

* Tracey's dashboard renders chapters through its own Typst pipeline, which
  replaces `#r`; the layout machinery in `pliron-spec.typ` is for the PDF.
* Typst HTML export is experimental; the PDF is the reference rendering.
* Numbers (ratios, test counts) are deliberately left out of requirements:
  they live in `docs/PARITY.md` and move daily.
