# Hot reload for Rust applications (`PLIRON_HOT`)

Subsecond-style hot reload without a linker in the loop: edit a file, the
crate is recompiled to one relocatable object, and the running process
maps it in place. Functions that changed are repointed; the ones that did
not keep running; statics keep their live state. Works on x86_64 Linux
(ELF) and arm64 macOS (Mach-O).

## Getting started

1. Build the backend (`cargo build` at the repo root produces
   `target/debug/librustc_codegen_pliron.{so,dylib}`).

2. Add the runtime helper to your crate:

   ```toml
   [dependencies]
   pliron_hot = { path = ".../examples/pliron-hot" }
   ```

   ```rust
   fn main() {
       pliron_hot::start(); // watches $PLIRON_HOT_DIR for patch objects
       // ... your application as usual
   }
   ```

3. Run the dev loop from your crate's directory:

   ```bash
   path/to/examples/hot-dev.sh . -- your-app-args
   ```

   It builds the crate with `PLIRON_HOT` + `-Ccodegen-units=1`, writes
   `target/hot/patches/base.ref` (the fingerprint reference), starts your
   app with `PLIRON_HOT_DIR` pointing at that directory, and rebuilds a
   patch object on every change under `src/`. Each patch lands in the
   running process in ~1 ms once built.

## How it works

With `PLIRON_HOT=<crate>` the backend emits every function of that crate
as a body `<sym>.hot` plus a stable thunk `<sym>` that jumps through a
writable `__hot_slot.<sym>` pointer (`pliron-clif-lower/src/hot.rs`).
Calls, function pointers and vtables all go through the thunk, so the
loader only ever repoints one cell per function.

A patch is the same crate recompiled normally (`--emit=obj`). The
`pliron_hot` runtime (`examples/pliron-hot`) watches `PLIRON_HOT_DIR`,
fingerprints each new object against `base.ref` / the previous patch,
maps it near the executable, resolves undefined symbols against the
executable's symbol table (then `dlsym`), binds writable statics to the
live copies so state survives, and repoints the slots of the functions
whose code changed.

On macOS the patch is mapped with `MAP_JIT`, written while
`pthread_jit_write_protect_np` is off, made executable again, and the
instruction cache is invalidated before any slot is repointed.

## Manual invocation (what the script does)

```bash
export RUSTFLAGS="-Zcodegen-backend=.../librustc_codegen_pliron.dylib -Ccodegen-units=1"
PLIRON_HOT=<crate_name> cargo build --bin <crate_name>   # thunked base
cargo rustc --bin <crate_name> -- --emit=obj=target/hot/patches/base.ref
PLIRON_HOT_DIR=target/hot/patches ./target/debug/<crate_name>
# after each edit:
cargo rustc --bin <crate_name> -- --emit=obj=target/hot/p.o
mv target/hot/p.o target/hot/patches/0001.o   # name-sorted apply order
```

`PLIRON_HOT` takes the crate name (underscores, not hyphens).
`-Ccodegen-units=1` is required so the crate is one object. Emit the
patch outside the patch directory and move it in atomically — a watcher
can pick up a partially written object otherwise. Files matching
`*.rcgu.o` (rustc intermediates) are ignored.

## Environment variables

- `PLIRON_HOT=<crate>` — emit the named crate's functions patchably.
- `PLIRON_HOT_DIR=<dir>` — runtime: directory watched for `*.o` patches
  (plus `base.ref` fingerprints). Unset = hot reload off, thunks cost one
  indirect jump.
- `PLIRON_HOT_LAYOUT=1` — compile time, for `base.ref` and every patch
  (`hot-dev.sh` sets it): each object carries a table of the crate's
  struct/enum/union layouts (`src/hot_layout.rs`). The loader compares a
  patch's table with `base.ref`'s and refuses the patch, naming the
  types, when one changed.
- `PLIRON_HOT_DIRECT=1` — runtime, arm64 macOS: after a patch, each
  patched function's thunk is rewritten from the slot load to a direct
  `b <new body>` (see "Direct thunks" below).
- `PLIRON_HOT_REACH=1` — compile time: every thunk first marks its
  function entered; `pliron_hot::reach` lists the marked functions and
  `PLIRON_HOT_REACH_OUT=<file>` writes the list at exit (see "Tests on
  save").
- `PLIRON_HOT_DEBUG=1` — print per-phase timings during a patch apply.
- `PLIRON_HOT_FREE=protect` — like `=1` below, but a retired mapping is
  made inaccessible instead of unmapped: its addresses cannot be reused,
  and a thread that was still running the old code stops at once with
  `[hot] a thread was still running code of a patch that has been
  retired` instead of executing whatever was mapped there next.
- `PLIRON_HOT_FREE=1` — `munmap` superseded patch mappings once every
  function they supplied has been repointed again, after a grace window
  of `PLIRON_HOT_GRACE` generations (default 3). Off by default: an
  unmap could race a thread still executing old code.

## Direct thunks (`PLIRON_HOT_DIRECT=1`)

A thunk costs a load and an indirect branch per call. With the knob, the
loader rewrites the thunk of each function a patch replaced into a direct
`b` to the new body (the slot is updated as well, for a thread already
past the first instruction). Apple Silicon has no writable+executable
pages, so a thunk page is briefly read-write and nothing on it can run:
thunks therefore sit in their own section (`__TEXT,__hot_thunks`, padded
to whole 16 KiB pages by the backend) and every other thread is suspended
for the flip. A body out of `b` range (±128 MiB) keeps the slot load.
arm64 macOS only; elsewhere the knob does nothing. Measured on the smoke
test: 3 thunks rewritten, apply 14 ms instead of 0.4 ms (the thread
suspension), so it is off by default — turn it on when call overhead in
the patched code matters more than apply time.

## Tests on save (`examples/hot-test.sh`)

```bash
path/to/examples/hot-test.sh .     # in a crate with #[test]s
```

Reruns, after each save, only the tests that entered a function whose
code changed. The crate is built with `PLIRON_HOT` and
`PLIRON_HOT_REACH=1`; each test runs in its own process and leaves the
functions it entered; after a save `hotdiff old.o new.o`
(`examples/pliron-hot/src/bin/hotdiff.rs`) names the functions whose
fingerprint changed, and a test reruns when its list meets that set, when
it failed last time, or when it is new. The crate needs `pliron_hot` as a
dev-dependency and `#[cfg(test)] use pliron_hot as _;`. Edits in
dependencies are not seen. `tests/hot/reach.sh` is the smoke test (three
tests, one edit, two rerun).

## Scaling

Apply cost grows with the size of the *edit*, not the crate. The loader
copies only changed function ranges plus the data sections reachable
from them; references to unchanged patch functions resolve to the live
executable's thunk, so their code is never mapped or relocated.

`bench/hot/run.sh [--fns N] [--rounds K] [--free]` generates a leaf
crate of N functions behind a small app binary and repeats this
edit→compile→drop loop, reporting patch build time, apply time and app
RSS. On an 8000-function leaf editing one function:

- patch build ~1.2 s (rustc dominates — apply is ~2% of the loop)
- apply ~25 ms at 8000 functions, ~60 ms at 20000 (~3 ms/1000), of
  which nearly all is parsing + fingerprinting the object; the actual
  copy/relocate/slot work is <1 ms
- RSS plateaus (old mappings are a few pages each; `PLIRON_HOT_FREE`
  reclaims them if needed)

## Limits

- x86_64 ELF and arm64 Mach-O only (the thunk assembly and the
  relocations are per-platform; other targets need a port).
- `main` itself is not patchable (the C entry point is excluded).
- Patches that define or use thread-locals are rejected — restart.
- Changing a struct/enum layout that live state depends on needs a
  restart. With `PLIRON_HOT_LAYOUT=1` the loader detects it and leaves
  the old code running (`[hot] ...: not applied: the layout of `State`
  (8 -> 40 bytes ...) changed ...; restart`); without it new code sees
  old-layout values. The check covers types defined in the patched
  crate that take no type or const parameters, and treats a renamed
  field as a change.
- New dependencies of the patch that are not already in the process
  (new extern symbols) fail the apply with a "symbols not in the running
  binary" diagnostic.
- Mach-O compact unwind cannot be registered post-load: panics
  unwinding through patched frames do not get unwind metadata (the patch
  code still runs correctly; `.eh_frame` registration is automatic on
  ELF).
- On macOS the base executable needs a symbol table (the default —
  do not pass `-Wl,-x` / strip it).
- The per-function compact mapping is Mach-O only for now; on ELF the
  whole patch object is mapped and relocated, so apply time and memory
  scale with the crate, not the edit.
- Section-target relocations into text sections are rejected on Mach-O
  (rare — the linker normally emits named `ltmp` symbols).

## Smoke test

`tests/hot/run.sh` exercises the whole path on both platforms: run the
base, edit `value()` to read a static, drop the patch in and check the
process prints the live-static result (~80 ms patch build, ~1 ms apply).
