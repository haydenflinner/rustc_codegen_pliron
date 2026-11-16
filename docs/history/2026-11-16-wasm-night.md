# 2026-11-16 overnight wasm parity session

Running log. Newest decisions that want a second look are under
"Review these".

## Review these

1. **sccache made rebuilds stale (found 02:05).** `~/.cargo/config.toml`
   got `build.rustc-wrapper = "sccache"` on 2026-11-14 22:30. sccache
   keys on rustc's arguments and inputs; the backend dylib behind
   `-Zcodegen-backend` and every `PLIRON_*` variable are not in the key.
   A rebuild into a target dir path that was used before returned the
   objects of the earlier build. Seen directly: `variant.sh uear` gave
   the same module before and after a backend change, `variant.sh uear2`
   (new path) gave the new one. `bench/wasm/run.sh` always builds into
   `target/wasm-bench`, so every run.sh after 11-14 22:30 could have
   timed an old backend. Variant names that were reused have the same
   problem (`half` built with `PLIRON_HALFUNROLL=1` was byte-for-byte
   the default body of `lookup_slow`). Numbers recorded between
   11-14 22:30 and this note should be re-measured before they are
   trusted. Fix: `.cargo/config.toml` in the repo sets
   `rustc-wrapper = ""` (verified: no `sccache` in `cargo build -v`).
   Builds outside the repo (`~/code/dsa`) need `RUSTC_WRAPPER=` in the
   environment.
2. **I wiped `target/` by accident (02:07).** A scratch `cargo init`
   under `target/` joined the workspace, and `cargo clean` in it removed
   the workspace target dir. Build artifacts only: backend builds,
   recorded corpora (`target/corpus/*`), held-out build caches, earlier
   agents' `target/v8prof` variants and `target/wasm-ab-*` dirs. The
   `Cargo.toml`/`Cargo.lock` edit `cargo init` made was reverted.
   Corpora were re-recorded (see below).
