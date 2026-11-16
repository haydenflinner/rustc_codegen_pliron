# The keyed `-Zcodegen-backend` path

Two caches sit between a script and a rustc that loads this backend, and
neither looks inside it:

- **cargo**'s fingerprint holds RUSTFLAGS as text. The dylib behind
  `-Zcodegen-backend=PATH` can be rebuilt, and any `PLIRON_*` variable can
  change, and the build is still "Fresh".
- **sccache** (a `rustc-wrapper` in `~/.cargo/config.toml` on the
  development machine) keys a rustc call on its arguments as text, the
  source files, the `--extern` files, `CARGO_*` variables and the cwd. A
  target dir that was wiped and rebuilt gets the objects of whichever
  backend filled the cache first.

`harness/backend_path.py` makes the path itself say what both need to know:

    target/backends/<key16>/librustc_codegen_pliron.dylib

`key16` is a hash of the dylib's sha256, the sorted code-affecting
`PLIRON_*` environment, the contents of the files that knobs name
(`PLIRON_PGO_USE`, `PLIRON_FIELD_ORDER`, any `@file` value) and an optional
`--salt`. The entry is a private copy (a copy-on-write clone on APFS; hard
links between entries of one dylib), so its content never changes: the
backend can be rebuilt while a long build still loads its entry. The path is
an argument, so the fingerprint and the cache key change exactly when the
backend or a knob does.

    RUSTFLAGS="-Zcodegen-backend=$(harness/backend_path.py --gc "$CARGO_TARGET_DIR" PLIRON_X=0)" \
      PLIRON_X=0 cargo build ...

The helper must see the `PLIRON_*` environment the rustc processes will get:
pass per-build knobs as `K=V` (or run it under the same `env ...`).
`--explain` prints the key's inputs, `--list` the entries.

## What is not in the key

Variables on the `IGNORED` list in the script: the harness's own
(`PLIRON_BACKEND`, `PLIRON_WASM_LD`, `PLIRON_PROFILE`) and reporting-only
knobs whose read sites gate no transform (`PLIRON_TRACE`,
`PLIRON_TRACE_FILE`, `PLIRON_STATS`, `PLIRON_TIMING`, `PLIRON_VERSION`,
`PLIRON_MCPROF`, `PLIRON_CACHE_LOG`, `PLIRON_RA_EXPLAIN`,
`PLIRON_OPT_BISECT_DEBUG`, `PLIRON_OPT_FNS_LOG`, `PLIRON_MEM_MARK`). Setting
one of these does not rerun rustc on a warm cache, as before; use `--salt`,
a fresh target dir or touch the sources. Everything else is in the key,
including checkers (`PLIRON_VERIFY`, `PLIRON_DIFFCHECK*`, `PLIRON_RA_CHECK`,
`PLIRON_WASM_AUDIT`: a check skipped because the build was fresh would read
as a pass) and dumps (`PLIRON_CLIF`, `PLIRON_DUMP`, `PLIRON_CORPUS`, ...).
An explicit `PLIRON_X=1` for a default-on knob is a different key from the
unset variable: a spurious rebuild, never a stale one. The same goes for
the dylib: `build.rs` stamps it with `harness/src-rev.sh` (`git describe
--dirty` plus a hash of the uncommitted diff), so after a commit or an edit
to any tracked file the next `cargo build` produces other bytes and a new
key although no backend source changed, and two builds of one revision are
not byte-identical either. To hold a backend still across such edits, point
`--dylib` (the scripts' `PLIRON_BACKEND`) at a copy.

## What sccache keys on (0.19.0, `src/compiler/rust.rs`)

`generate_hash_key` hashes, in order: the digests of the dylibs in the
toolchain's `lib/` directory (not the backend); the command line as text,
minus `--extern`, `-L`, `--check-cfg`, `--out-dir`, `--diagnostic-width`
(the `-Z` parser only looks for `-Z profile`; `-Zcodegen-backend=PATH` is
hashed as that string and the file is not opened); the source files; the
`--extern` rlibs and static libraries; the target JSON; the variables the
dep-info lists as `env-dep` plus every `CARGO_*` variable except
`CARGO_MAKEFLAGS`, `CARGO_REGISTRIES_*`, `CARGO_BUILD_JOBS`,
`CARGO_ENCODED_RUSTFLAGS`; the cwd; the rustc version. No `PLIRON_*`
variable is in it. Only `rlib`/`staticlib` crates with `--emit` within
`link,metadata,dep-info` are cached: a `bin` or `cdylib` crate, an
`--emit obj` call and anything incremental always compile.

`CARGO_TARGET_DIR` set in the environment is one of the hashed `CARGO_*`
variables, so two target dirs do not share entries; the stale hits come
from rebuilding into a target dir path used before (measured below).

## Measurements (2026-11-16, nightly-2026-10-22, aarch64-apple-darwin)

Dylib A is this tree; dylib B is A with `PLIRON_PUNROLL` forced to `0` in
`pliron-cfg` (so its output is known: A under `PLIRON_PUNROLL=0`). `text` is
the machine-code size of a four-function test crate's rlib.

sccache, its own server and empty cache, target dir wiped before each
build (`RUSTC_WRAPPER=sccache CARGO_TARGET_DIR=T RUSTFLAGS=-Zcodegen-backend=P
cargo build --release`; "truth" is the same build with `RUSTC_WRAPPER=`):

| build | fixed path: sccache | text | truth | keyed path: sccache | text | truth |
|---|---|---|---|---|---|---|
| A | miss | 1428 | 1428 | miss | 1428 | 1428 |
| A, `PLIRON_OPT_BISECT=0` | **hit** | **1428** | 1256 | miss | 1256 | 1256 |
| B | **hit** | **1428** | 1172 | miss | 1172 | 1172 |
| A again | | | | hit | 1428 | 1428 |
| A, `PLIRON_OPT_BISECT=0` again | | | | hit | 1256 | 1256 |
| B again | | | | hit | 1172 | 1172 |

With the fixed path both later builds are served the first build's rlib
(same sha256). With the keyed path every rlib has the sha256 of its
no-wrapper build, and coming back to an earlier backend is a hit. A symlink
whose target moved from A to B is a hit on A's rlib too: sccache does not
resolve the path, so a symlink named by the key would work for it, but a
symlink to the build output would not be immutable.

The same with a `-Zbuild-std` wasm build (`RUSTC_WRAPPER=sccache
harness/v8/variant.sh NAME`, a fresh target dir each time; 23 cacheable
crates, 11 calls sccache never caches): cold 33 s, 0 hits / 23 misses; again
3 s, 23 / 0, same wasm; `PLIRON_PUNROLL=0` 33 s, 0 / 23, and the wasm is the
no-wrapper `PLIRON_PUNROLL=0` one; again 4 s, 23 / 0. Under another NAME
(another `CARGO_TARGET_DIR`) the first build is 0 / 23 again.

cargo, no wrapper, target dir kept (`cargo build --release -v`, counting
`Running` lines; 2 = the lib and the bin):

| build | fixed path: rustc runs | text | keyed path: rustc runs | text |
|---|---|---|---|---|
| A | 2 | 1428 | 2 | 1428 |
| A again | 0 | 1428 | 0 | 1428 |
| A, `PLIRON_OPT_BISECT=0` | **0** | **1428** | 2 | 1256 |
| same again | | | 0 | 1256 |
| same + `PLIRON_VERSION=1` (ignored) | | | 0 | 1256 |
| B | **0** | **1428** | 2 | 1172 |
| back to A | | | 0 | 1428 |

`harness/v8/variant.sh` (`bench/wasm`, `-Zbuild-std`, 28 units) with
`VARIANT_KEEP=1`, and `harness/dsa/build.sh pliron` on a stand-in workspace
(a `literate-wasm` cdylib with one path dependency, 29 units, stub
wasm-bindgen), each in one kept target dir:

| build | variant.sh | units new / removed | dsa/build.sh `cargo_secs` | units new / removed |
|---|---|---|---|---|
| cold | 33 s | 28 / 0 | 56 | 29 / 0 |
| nothing changed | 0 s | 0 / 0 | 1 | 0 / 0 |
| `PLIRON_PUNROLL=0` | 30 s | 28 / 28 | 57 | 29 / 29 |
| same again | 0 s | 0 / 0 | 0 | 0 / 0 |
| knob removed | 30 s | 28 / 28 | 35 | 29 / 29 |
| dylib B | 31 s | 28 / 28 | 30 | 29 / 29 |
| dylib B again | 1 s | 0 / 0 | | |

The wasm of "dylib B" is byte-identical to the one of A under
`PLIRON_PUNROLL=0`, and "knob removed" to "cold": the output does not
depend on the path's spelling. In `dsa/build.sh`, `DSA_WIPE=1` with nothing
changed recompiles all 29 (43 s), and setting `DSA_PLIRON_FALLBACK` gives
another key (29 new) that a rerun reuses. The real dsa build (5 minutes,
4-7 GB) was not rerun for this: the machine was at load 20-70.

`harness/crates.py --only itoa --profile debug`: under `--deps stock` a
rerun compiles nothing and `PLIRON_PUNROLL=0` recompiles `itoa` alone; under
`--deps pliron` the knob recompiles all 44 crates and the target dir stays
at one set (82 MB). `bench/run.sh`: nothing changed, 0 compiles of the bench
crate (2 on every run before); a knob, 2.

## Side effects

- **One set of units per path.** On this toolchain cargo puts RUSTFLAGS in
  a unit's file-name hash (`-C extra-filename`, the `build/<pkg>/<hash>/`
  directory) and not in `-C metadata`: the `-C metadata` value and the
  mangled symbols are the same under every path, and units of different
  paths sit side by side (going back to an earlier path is "Fresh").
  Without help a target dir grows by one artifact set per backend revision
  and knob set. `backend_path.py --gc TARGET_DIR` removes the units whose
  fingerprint names another `-Zcodegen-backend=` than the path it prints
  (85 MB per switch for `bench/wasm`, about 650 MB for the dsa eval set).
  It reads cargo's new build-dir layout only. `cargo rustc -- -Z
  codegen-backend=...` (`bench/run.sh`) keeps the flag out of the
  fingerprint's `rustflags`, so that script cleans its one crate when the
  path moved.
- **Crates of two paths do not mix.** The flag's value is in rustc's crate
  hash (SVH): the same crate compiled under two paths has the same
  StableCrateId and symbol names and two different hashes. Within one cargo
  invocation every crate gets the same path. By hand: `mid` compiled against
  `tiny` under path 1, then linked with `tiny` compiled under path 2, fails
  with E0460 ("found possibly newer version of crate `tiny` which `mid`
  depends on"). A binary compiled under path 2 directly on an rlib made
  under path 1 links and runs.
- **Disk.** An entry costs the dylib's size (64 MB debug) once the build
  output it was cloned from is replaced. Each call keeps the 8 most recently
  used entries and those used in the last 2 hours.

## Who uses it

On the keyed path: `harness/dsa/build.sh` (no wipe; `DSA_WIPE=1` restores
it; `DSA_RUSTC_WRAPPER=sccache` is now safe), `harness/v8/variant.sh`
(`VARIANT_KEEP=1` reuses the target dir), `bench/wasm/run.sh` (no wipe),
`bench/run.sh` (no `cargo clean -p` unless the path moved),
`tests/wasm/smoke.sh`, `harness/crates.py` (both `--deps` modes; knob
changes now rebuild the pliron side). The shell scripts fall back to the raw
dylib and their old wipe where the helper cannot run.

Not converted, each with its own answer to the same problem: `test.sh` (plain
rustc; its wasm PGO leg and `tests/wasm/run.sh` use a target dir named by
the dylib's checksum), `tests/wasm/{real,fuzz,bench,simd}.sh`,
`tests/hot/run.sh`, `bench/hot/run.sh`, `harness/corpus.sh` (fresh target
dir per run), `bench/heldout/ab.py` (a stamp of environment, dylib mtime
and profile per side) and the other held-out tools (the target dir is
removed before a build), `harness/ctime.py` (plain rustc into a temp dir;
it times the compile). `harness/gate.py` and `harness/quick.py` run no
cargo build through the backend themselves.

The repo's `.cargo/config.toml` (`rustc-wrapper = ""`) stays until the
scripts in the second list pass the keyed path too: they run cargo inside
the repo with the raw dylib path and rely on it.
