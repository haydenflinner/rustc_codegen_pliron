# Compatibility

Crate test suites built with the pliron backend (out-of-tree `.so` on nightly-2026-10-06; wasm via pliron-wasm-ld). Native rows are linked by wild only with `-Clinker-features=-lld -Zunstable-options`: without it stock nightly adds `-fuse-ld=lld` and rust-lld silently wins over `-Clinker=cc-wild`. "Same under LLVM" means stock rustc fails the same tests.

| target | config | result |
|---|---|---|
| rustc `tests/ui` (stage1, fork) | `./x test tests/ui --no-fail-fast` | 21,544 pass, 182 fail, 797 ignored (before LTO/EII/SIMD fixes) |
| coretests (stage1, fork) | `./x test library/coretests --skip _max_range` | 2,722 + 596 bench-tests pass, 0 fail |
| coretests `split_off*_max_range*` (6) | as above | hang: comparing `usize::MAX` ZSTs is an unremoved loop (no loop deletion yet; upstream notes the same for unoptimized rustc) |
| alloctests (stage1, fork) | `./x test library/alloctests` | 335 + 1,490 + 573 + 2 pass, 0 fail |
| std (stage1, fork) | `./x test library/std` | 2,349 pass, 0 fail, 93 ignored |
| burn-ndarray 0.22 | native, `--lib` | 41/41 |
| burn-backend-tests | native, `--features ndarray` | 2,495 pass, 0 fail |
| polars-core 0.55.1 | native, `--features object` | 107 pass, 2 fail (same under LLVM: proptest `unreachable!`) |
| naga 30 | native, `RUST_MIN_STACK=16777216` | 386 pass, 6 fail (need spirv-as/val/cross; same under LLVM) |
| naga `recursion_depth_template` | native, debug | stack overflow at 2 MB; passes with `RUST_MIN_STACK=16777216` (frames larger than LLVM's) |
| wgpu-core 30 | native | 64/64 |
| wgpu-examples 30 | wasm32-unknown-unknown, `webgpu`, wasm-bindgen 0.2.129, headless Chrome + SwiftShader | page boots, all 30 examples listed; `hello_synchronization` GPU readback identical to LLVM build |
| serde_json, regex, hashbrown, itertools, rand, memchr (git HEAD) | native, `cargo test` | 236 / 319 / 389 / 655 / 164 / 166 pass, 0 fail |
| memchr, rand, serde_json, regex (wasm32-wasip1, node 22) | `ct-wasi.sh test` (in-process panic=abort tests; doctests via `RUSTDOCFLAGS`) | memchr 107/107, rand 159/159, serde_json 160/160 (its TCP test aborts: no sockets on WASI), regex integration 62/62 (2 tests that build huge regexes segfault node on exit, same with stock LLVM) |
| burn-ndarray (wasm32-wasip1, node 22) | `ct-wasi.sh test -p burn-ndarray --lib` | 38 pass, 0 fail, 3 ignored (needed `acoshf`/`asinhf` in the pure-Rust libc) |
| uuid, anyhow, bytes (wasm32-wasip1, node 22) | `ct-wasi.sh test --no-fail-fast` | uuid 128/128; anyhow 98/98 (its trybuild `compiletest` needs to spawn cargo: no processes on WASI; std `read_dir` panicked on a misaligned `dirent` until the pure-Rust libc aligned it); bytes 803 pass, 0 fail (`test_bytes` segfaults node in `advance_bytes_mut_remaining_capacity`, same with stock LLVM) |
| parking_lot (wasm32-wasip1) | as above | 1 pass; the rest spawn threads, which wasm32-wasip1 doesn't have |
| serde, smallvec (wasm32-wasip1) | as above | not run: `serde_core` fails `deny(unused_imports)` on WASI (`OsStr`/`OsString`); smallvec's dev-dep `iai-callgrind-runner` needs serde for `OsString` |
| polars-core (wasm32-wasip1) | as above | not run: dependency `tokio` is built with features it rejects on wasm |
| bytes, smallvec, crossbeam (`--workspace`), rayon, parking_lot, anyhow (native) | `compat/pop/run2.sh` | 1305 / 82 / 1037 / 578 / 116 / 99 pass, 0 fail |
| tokio, serde, syn (`--all-features --release`), clap, uuid (native) | `compat/pop/run3.sh` | 2986 / 480 / 285 / 1787 / 181 pass, 0 fail (syn's rust-src round-trip tests download via reqwest + rustls/aws-lc-rs) |
| chrono (native) | as above | 572 pass, 3 fail: doctests using deprecated `std::i32::MAX` under `#![deny(warnings)]` |
| hashbrown, itertools (wasm32-wasip1) | as above | not run: dev-dep `criterion` refuses to build on WASI |
| wasm-bindgen 0.2.129 | minimal lib + bin | exports, strings, `console.log` from `fn main` all work |

Fixes made for these: `simd_select_bitmask` mask arg, LLVM-like constant alignment, >16-byte stack alignment, `llvm.{u,s}{add,sub}.sat`, x86 `vzeroupper`/`vzeroall`/fences, wasm custom sections + `target_features`, `\x01` verbatim symbol prefix stripped (bindgen `link_name`; aws-lc-rs), `llvm.x86.pclmulqdq{,.256,.512}` (were `ud2` stubs; crc32fast), `-O0` always-inline (wasm-bindgen describe shims), no-arg `main` → C `main(argc, argv)` wrapper in pliron-wasm-ld.

Not verified: rendered pixels in the browser — headless SwiftShader shows a blank canvas for both the pliron and the LLVM build.

Flaky: the WASI `sleep+instant` check (5 ms sleep, `elapsed() >= 5ms`) failed once under heavy load and passed on rerun.
