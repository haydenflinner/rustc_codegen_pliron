# Round-trip (incremental edit → build → link) parity + pliron-wasm-ld speedup

Motivation: the SubSecond-style question — "edit one widget in an egui
app over a deep dep tree; how fast is the compile/codegen/link/run
loop, and is the backend the bottleneck?"

## Benchmark

`/tmp/bevyrtt`: a thin bin (`main.rs`, ~20 lines touching
egui/wgpu/winit) plus a generated 12 kLOC bin (`large.rs`, 1200
functions, all kept live) over **281 crates** — egui 0.36 + wgpu 30 +
winit 0.30, the real GUI substrate (bevy's full tree didn't resolve
offline). Dev profile, medians of 5–6 interleaved reps,
aarch64-apple-darwin, nightly-2026-10-22, release dylib.

## Results

| round trip | stock | pliron | ratio |
|---|---|---|---|
| cold, whole tree, native | 23.3 s | 23.1 s | 0.99 |
| thin leaf, native (22 M Mach-O) | 0.508 s | 0.510 s | 1.00 |
| large leaf, native (1.1 M) | 0.876 s | 0.876 s | 1.00 |
| thin leaf, wasm (before ld work) | 0.659 s | 1.383 s | 2.10 |
| thin leaf, wasm (after ld work) | 0.659 s | **0.65 s** | ~0.98 |

Every crate in the tree (wgpu, wgpu-hal, naga, metal, egui, winit,
accesskit…) compiles through pliron cleanly on both targets — a
useful correctness datapoint by itself.

## Where the time goes (native, thin leaf)

| step | stock | pliron |
|---|---|---|
| rustc total | 0.524 s | 0.335 s |
| run_linker (Apple ld) | 0.196 s | 0.211 s |
| LLVM IR emit + passes on reused CGUs | ~0.39 s | ~0.02 s |
| cargo overhead (fingerprint/graph) | ~0.18 s | ~0.18 s |

Pliron is ~36% faster *inside rustc* on the thin incremental rebuild:
incremental CGU reuse skips LLVM's re-emission entirely (stock
re-runs `codegen_to_LLVM_IR` + `LLVM_passes` for every CGU every
round). The win is masked end-to-end by two costs no backend owns:
the system link (~0.2 s) and cargo's own bookkeeping (~0.18 s).

## The link is the floor — and Apple ld is already good

Same inputs, same args, standalone:

- `cc` → Apple ld64: **~0.23 s**
- `cc -fuse-ld=lld` → ld64.lld: **~0.52 s** (2.3× *slower*)

ld64.lld's Mach-O backend is not competitive with Apple's linker on
this box. `wild` is installed but its Mach-O support is experimental
behind a `--features macho` build flag. For a genuinely sub-100 ms
round trip the lever is not a faster general linker — it is a
specialised path (incremental/patch link reusing the prior image, or
dodging the link via dynamic loading), which is what SubSecond-class
stacks do.

## pliron-wasm-ld: 1.01 s → 0.40 s on the same link

The wasm leg *does* have a lever we own: `tools/pliron-wasm-ld`
(pure Rust, replaces wasm-ld). Profile of the 21 M egui-app link:
16.7% SipHash, ~10% memcmp from O(n²) dedup scans, ~30% repeated
full-body operator decodes. Fixes (all mechanical, output
byte-identical):

- `std::collections` → `rustc_hash::FxHashMap/FxHashSet`
  (~16% SipHash removed).
- Host-import dedup `Vec::any`/`position` per import →
  `HashMap<(module, name), u32>` (was O(imports²); wasm-bindgen
  modules have thousands).
- Type dedup `types.iter().position` per type → arity-bucketed
  `HashMap` (was O(types²); exact same output).
- GC reachability `frames.iter().any()` per instruction → running
  dead-frame counter (was O(body × block-depth)).
- `Obj::local` linear `data` scan per symbol lookup → per-object
  name→index map.
- The GC scan now records each live body's blocktype/`call_indirect`
  type references as it decodes, removing a third full decode of
  every live body (dedup losers are filtered at consumption, so
  output is unchanged).

Result: standalone link 1.01 → 0.40 s (wasm-ld does 0.30 s on a 5×
bigger module — the residual gap is the full decode+reencode of every
live body for index remapping; a byte-level selective rewrite is the
next lever). End-to-end wasm incremental round trip 1.38 → 0.65 s —
parity with stock. wasm test suite + `gate --only wasm` green.

## Open gaps vs a SubSecond-class loop

- Native link has no owned fast path: pliron-ld would need to exist
  (wild-fork or a Mach-O writer on top of our own object knowledge),
  or a patch/incremental-link mode. Apple ld at 0.2 s already beats
  lld.
- Cargo + rustc frontend overhead (~0.28 s combined) is identical for
  both backends; below ~0.3 s total needs bypassing cargo, not the
  backend.
- The `ctime.py` card measures cold single-crate compiles only; no
  incremental round-trip input exists yet. This experiment is the
  prototype for one (thin + large leaf over a vendored GUI stack,
  alternating stock/pliron).
