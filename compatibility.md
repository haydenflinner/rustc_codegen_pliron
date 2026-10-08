# Compatibility

Crate test suites built with the pliron backend (out-of-tree `.so` on nightly-2026-10-06; wasm via pliron-wasm-ld). Native rows are linked by wild only with `-Clinker-features=-lld -Zunstable-options`: without it stock nightly adds `-fuse-ld=lld` and rust-lld silently wins over `-Clinker=cc-wild`. "Same under LLVM" means stock rustc fails the same tests.

| target | config | result |
|---|---|---|
| rustc `tests/ui` (stage1, fork) | `./x test tests/ui --no-fail-fast` | 21,544 pass, 182 fail, 797 ignored (before LTO/EII/SIMD fixes) |
| coretests (stage1, fork) | `./x test library/coretests --skip _max_range` | 2,722 + 596 bench-tests pass, 0 fail |
| coretests `split_off*_max_range*` (6) | `./x test library/coretests -- max_range` | 6/6 pass since loop deletion (`PLIRON_LOOPDEL`) removes the `usize::MAX`-long ZST compare loop; they hung before |
| alloctests (stage1, fork) | `./x test library/alloctests` | 335 + 1,490 + 573 + 2 pass, 0 fail |
| std (stage1, fork) | `./x test library/std` | 2,349 pass, 0 fail, 93 ignored |
| burn-ndarray 0.22 | native, `--lib` | 41/41 |
| burn-backend-tests | native, `--features ndarray` | 2,495 pass, 0 fail |
| polars-core 0.55.1 | native, `--features object` | 107 pass, 2 fail (same under LLVM: proptest `unreachable!`) |
| naga 30 | native, `RUST_MIN_STACK=16777216` | 386 pass, 6 fail (need spirv-as/val/cross; same under LLVM) |
| naga `recursion_depth_template` | native, debug | stack overflow at 2 MB; passes with `RUST_MIN_STACK=16777216` (frames larger than LLVM's) |
| wgpu-core 30 | native | 64/64 |
| rapier2d + rapier3d (Wild) | `ct-oot.sh test -p rapier3d -p rapier2d --no-fail-fast` | 659 pass, 0 fail; before c832d82, 72 test binaries died with SIGILL in the `ud2` stubs for `llvm.prefetch` and `llvm.x86.sse.min.ps` (now a no-op and a real lowering, checked in `tests/asm`) |
| wgpu-examples 30 | wasm32-unknown-unknown, `webgpu`, wasm-bindgen 0.2.129, headless Chrome + SwiftShader | page boots, all 30 examples listed; `hello_synchronization` GPU readback identical to LLVM build |
| serde_json, regex, hashbrown, itertools, rand, memchr (git HEAD) | native, `cargo test` | 236 / 319 / 389 / 655 / 164 / 166 pass, 0 fail |
| memchr, rand, serde_json, regex (wasm32-wasip1, node 22) | `ct-wasi.sh test` (in-process panic=abort tests; doctests via `RUSTDOCFLAGS`) | memchr 107/107, rand 159/159, serde_json 160/160 (its TCP test aborts: no sockets on WASI), regex integration 62/62 (2 tests that build huge regexes segfault node on exit, same with stock LLVM) |
| burn-ndarray (wasm32-wasip1, node 22) | `ct-wasi.sh test -p burn-ndarray --lib` | 38 pass, 0 fail, 3 ignored (needed `acoshf`/`asinhf` in the pure-Rust libc) |
| uuid, anyhow, bytes (wasm32-wasip1, node 22) | `ct-wasi.sh test --no-fail-fast` | uuid 128/128; anyhow 98/98 (its trybuild `compiletest` needs to spawn cargo: no processes on WASI; std `read_dir` panicked on a misaligned `dirent` until the pure-Rust libc aligned it); bytes 803 pass, 0 fail (`test_bytes` segfaults node in `advance_bytes_mut_remaining_capacity`, same with stock LLVM) |
| parking_lot (wasm32-wasip1) | as above | 1 pass; the rest spawn threads, which wasm32-wasip1 doesn't have |
| serde, smallvec (wasm32-wasip1) | as above | not run: `serde_core` fails `deny(unused_imports)` on WASI (`OsStr`/`OsString`); smallvec's dev-dep `iai-callgrind-runner` needs serde for `OsString` |
| itoa, ryu, semver, byteorder (wasm32-wasip1) | `ct-wasi.sh test --no-fail-fast` | 13 / 50 / 38 / 514 pass, 0 fail |
| indexmap, bitflags (wasm32-wasip1) | as above | indexmap 170 pass. bitflags 78 pass; its trybuild `compile` test needs to spawn cargo |
| indexmap `tests/quick.rs` (wasm32-wasip1, node 22) | `ct-wasi-unwind.sh test --test quick` (panic=unwind) | 39/39 pass; under panic=abort it trapped at the first property expected to panic (stock LLVM same), now works via emulated EH |
| wasm emulated EH (wasm32-wasip1, node 22) | `tests/wasm/unwind` crate in `tests/wasm/run.sh` | `catch_unwind` Ok/Err, panic payloads (&str/String/formatted), drop-guard order across nested frames, `resume_unwind`, `panic_unwind`/`unwind` `_Unwind_*` from pliron-wasi-libc over the linker's `__pliron_eh` flag+exn words |
| bevy 0.17 (pliron_bevy_game) | wasm32-unknown-unknown, `RUSTFLAGS=--cfg=web_sys_unstable_apis`, `ct-wasm.sh check + build` | full bevy tree (bevy_ecs, bevy_render, wgpu, winit, naga) checks and links into a 190 MB wasm; wasm-bindgen 0.2.129 processes its custom sections; the breakout game runs in desktop Chrome (canvas renders, autoplay scores) |
| rapier2d + rapier3d (wasm32-wasip1, node 22, panic=unwind) | `ct-wasi-unwind.sh test --release -p rapier2d -p rapier3d` | 566 pass, 0 fail. `spinner_containment` core-dumps node (same for the stock-LLVM wasip1 build — node-side). rapier3d's 92 doctests fail E0152 `duplicate lang item core` — rustdoc resolves stock core alongside the -Zbuild-std core, a build-std quirk not codegen |
| base64, unicode-segmentation (wasm32-wasip1) | as above | not run: dev-deps `criterion` (Rayon) and `wait-timeout` do not build for WASI |
| polars-core (wasm32-wasip1, panic=unwind) | `ct-wasi-unwind.sh test --features object` | 85 pass, 23 fail: all failures are `rayon-core: GlobalPoolAlreadyInitialized` (no threads on wasip1 — stock LLVM identical) plus 2 proptest path/env issues. Needs local cfg patches dropping tokio `net`/`rt-multi-thread` features and `block_in_place` on wasm |
| bytes, smallvec, crossbeam (`--workspace`), rayon, parking_lot, anyhow (native) | `compat/pop/run2.sh` | 1305 / 82 / 1037 / 578 / 116 / 99 pass, 0 fail |
| tokio, serde, syn (`--all-features --release`), clap, uuid (native) | `compat/pop/run3.sh` | 2986 / 480 / 285 / 1787 / 181 pass, 0 fail (syn's rust-src round-trip tests download via reqwest + rustls/aws-lc-rs) |
| chrono (native) | as above | 572 pass, 3 fail: doctests using deprecated `std::i32::MAX` under `#![deny(warnings)]` |
| hashbrown, itertools (wasm32-wasip1) | as above | not run: dev-dep `criterion` refuses to build on WASI |
| tokio (wasm32-wasip1) | `compat/tokio-wasi` smoke, `features=[rt,sync,macros,time,io-util]`, `current_thread` flavor | spawn/join, mpsc, oneshot, `time::sleep` all pass under node. `net` is compile_error'd by tokio itself on wasm |
| rapier3d + wgpu/WebGL2 (wasm32-unknown-unknown, browser) | `compat/rapier-browser`: `PhysicsPipeline` stepping 55 dynamic cuboids + ground, `DebugRenderPipeline` (COLLIDER_SHAPES) → wgpu line-list over `wgpu-hal` `gles` backend | renders and animates in headless Chrome (SwiftShader WebGL2): pyramid settles on the ground plane. Needed `Backends::GL` (navigator.gpu exists but has no adapters — BROWSER_WEBGPU gets picked first and fails) and `Limits::downlevel_webgl2_defaults()` |
| wasm-bindgen 0.2.129 | minimal lib + bin | exports, strings, `console.log` from `fn main` all work |

Fixes made for these: `simd_select_bitmask` mask arg, LLVM-like constant alignment, >16-byte stack alignment, `llvm.{u,s}{add,sub}.sat`, x86 `vzeroupper`/`vzeroall`/fences, wasm custom sections + `target_features`, `\x01` verbatim symbol prefix stripped (bindgen `link_name`; aws-lc-rs), `llvm.x86.pclmulqdq{,.256,.512}` (were `ud2` stubs; crc32fast), `-O0` always-inline (wasm-bindgen describe shims), no-arg `main` → C `main(argc, argv)` wrapper in pliron-wasm-ld.

Rendered pixels in the browser are now verified by the rapier-browser demo (headless Chrome + SwiftShader WebGL2).

Flaky: the WASI `sleep+instant` check (5 ms sleep, `elapsed() >= 5ms`) failed once under heavy load and passed on rerun.
