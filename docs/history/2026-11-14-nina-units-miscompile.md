# 2026-11-14 — Nina `literate-wasm` miscompile: `Dim::powi` overflow panic

The pliron backend now compiles Nina's `literate-wasm` crate to wasm
cleanly (was: unresolved `env.*` imports, `wpeep` invalid output). The
build is real progress: `wasm-bindgen --target web` accepts the module,
`ReplSession` init/eval works, arithmetic/closures/loops/strings all
compute correctly, and signpost diagnostics render.

## The bug

Two minimal repros panic in the pliron build and work on the identical
LLVM build:

    r.eval('print(60Hz.to(1Hz))')                                  // PANIC
    r.eval('print(music::synth(0.0, 1.0, 0s, 0.02s, 1.0, 0.09s, 0Hz))')  // PANIC

    panic: /Users/wow/code/dsa/mimas/crates/shared/src/units.rs:111:13
    attempt to multiply with overflow        — `Dim::powi`, `out[i] *= n`

`Dim` is `pub struct Dim(pub [i8; N])` with `N = 33` — a 33-byte
aggregate returned by value from `const fn`s (`mul`, `div`, `powi`,
`sqrt`, `lookup` as `(Dim, f64)` tuples). The `*= n` can only overflow
if `n` or `out[i]` arrives as garbage — for `60Hz.to(1Hz)` the honest
exponent is 1, which cannot overflow i8.

Suspects: a caller computing a bad `exp` (i64→i8 truncation or sign
extension), the `(Dim, f64)` tuple/`Option<(Dim, f64)>` sret path, or
the `& 1` parity mask in the tail loop. `units.rs:107-118` is `powi`;
`dims.rs:1805` (`(D::Q(d), "to")`) is the `.to` check path;
`parser.rs:5468` is the `^exp` parser path (not implicated — no `^` in
the repro).

## What does work / did not fix

- `PLIRON_WASM_PEEP=0`, `PLIRON_WASM_BASICOPT=0`, `PLIRON_OPT_FNS=0..0`,
  `PLIRON_OPT_BISECT=0` — all still panicked (the smul bug is plain
  emit, below every optimizer).
- `PLIRON_WASM_INLINE=0` rebuild (leaf only, 58s) — still panics.
- Dim literal parsing itself works: `10px`, `3m`, `1L`, `1gal`,
  `1deg^2`, `1m/s`, `0.02s` all eval correctly; `2s.to(1ms)` = 2000,
  `500ms.to(1s)` = 0.5 — same-dim scalar conversions are fine. The
  failure needs an axis/dimension-bearing unit (`Hz`, `px↕`-class) or
  the `Secs`/`Hz` native-arg conversion (`music::synth` takes
  `attack: Secs, …, cut_hz: Hz`).
- `music::hz(60)` → `261.6 Hz` on both builds — midi→freq arithmetic
  (f64 powf) is fine.

## Repro pipeline

    cd /Users/wow/code/dsa/host
    CARGO_TARGET_DIR=/tmp/dsa-pliron-target \
    CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="\
      -Zcodegen-backend=$PWD/../purerust/rustc_codegen_pliron/target/release/librustc_codegen_pliron.dylib \
      -Clinker=pliron-wasm-ld" \
    PATH=/Users/wow/code/purerust/rustc_codegen_pliron/tools/pliron-wasm-ld/target/release:$PATH \
    cargo +nightly-2026-10-22 build --target wasm32-unknown-unknown \
      -Zbuild-std=std,panic_abort -p literate-wasm --no-default-features

    ~/Library/Caches/.wasm-pack/wasm-bindgen-cargo-install-0.2.127/wasm-bindgen \
      --target web --debug --out-dir /tmp/ppkg --out-name literate_wasm \
      /tmp/dsa-pliron-target/wasm32-unknown-unknown/debug/literate_wasm.wasm

    # then in node (needs wasm-bindgen-cli 0.2.127 matching the crate):
    import init, { ReplSession } from '/tmp/ppkg/literate_wasm.js';
    await init({ module_or_path: readFileSync('/tmp/ppkg/literate_wasm_bg.wasm') });
    new ReplSession().eval('print(60Hz.to(1Hz))');   // panics

Cold build ~4m31s; leaf-only rebuild ~58s. A `[i8; 33]` + tuple-return
standalone repro (`/tmp/dimrepro.rs`) PASSES at `-O`, so the bug needs
the real units machinery — likely shape involves the typechecker-side
`solve::dims` path or `MimasType` arg conversion, not plain const-fn
aggregate returns.

## Root cause (resolved) — two bugs

The report turned out to be two independent bugs, one per target:

1. **`webmerge` phase-D unsound merge (native + wasm).** A relational
   merge in `units::parse` rebound the char cursor to `index + base`,
   so the scan dereferenced the counter. This is the `0x1` segfault in
   the native unit tests.
2. **`smul_overflow` on i8/i16 emitted a wrong flag (wasm only).** The
   overflow check compared `hi` (normed to `w` bits, `{0, 0xFF}` for
   i8) against `prod >>s (w-1)` (`{0, 1}` — the sign *bit*, not the
   sign *byte*). Every negative i8 product reported overflow, so
   `Dim::powi`'s `out[i] *= n` panicked for any unit with a negative
   exponent — `Hz`, `s`, `px↕`-class dims. This is the
   `units.rs:111` panic in the repros. Native was never affected:
   cranelift emits the op itself; only the waffle lowering had the
   width mismatch.

### Bug 1 — webmerge phase D

Native oracle: `mimas-shared`'s `units::parse`/`units::lookup` under
the backend segfault when the unit tests run together —
`angle_units_carry_the_angle_dimension` dereferences `0x1` inside the
`^`-exponent char scan of `parse("deg^2")`. `PLIRON_OPT_BISECT` plus
per-merge `wmA`/`wmB`/`wmC`/`wmD` bisect gates named the culprit: a
`wmD` relation merge at `parse`'s char-decode loop header
(`block1138`, renumbered `block546` in a full build; slot 0, param
`v11745`, the cursor pointer).

The header is a cursor/index pair `(cur, aux, idx)`. Its back edge
carries `new_idx = old_idx + (new_cur - old_cur)`, so
`arg0 - arg2 = p0 - p2` — a genuinely preserved diff. `diff_b` proves
that and returns `Diff::Head`. Phase D's oracle then called the edge
"proven", and `defers` returned true for any `proven` edge without
rechecking the relation.

The seeded relation came from the *entry* edge: the loop's initial
cursor is `select(sep == 0, base, sep)` ("start of string if no `-`
was found"), and `paramize` rewrote the literal `0` in `icmp` to
`P(2)` — because slot 2's entry arg is the constant `0`. The lifted
relation `p0 = p2 + SEL` therefore means `cur = idx + base` only while
`P(2)` reads `0`; once the index steps, `P(2)` materializes to the
running index. With no `-` in the string (`sep = 0`), `SEL` reduces
to `0` for `idx > 0`, so the cursor becomes `idx` itself: `0x1` is
the counter dereferenced as a pointer.

`Head` proves only that the edge preserves the pair's diff — it says
nothing about `D`'s own slot references, which the materialization
replays at the *new* head. Two edges of the merge were unsound in
exactly that way, and `proven.contains(&e)` skipped the check that
would have caught it.

## Bug 1 — the fix

`clifpeep.rs` `defers` now checks provenance:

- `Head`-deferred edges are no longer counted as `proven`; they go to
  a separate `heads` set. `defers` accepts such an edge only when
  every slot the relation's `D` references either echoes
  (`pk_of(e,k)` peels to `Q(k)` with offset 0) or steps under a
  cancelling affine term (`Σ slope_k·o_k = 0`) — the same account the
  structural path uses, applied to `D` rather than to `r`'s whole
  reference set.
- Verbatim (`kj == r`) and `key_sub`-proven edges get the same
  treatment for `r`'s `Q` leaves: a `Q(k)` bound `p_k` at the edge
  source but materializes as the incoming `arg_k`, so every such slot
  must echo (new `proots::qset` helper — `P` leaves are fine; they
  name the incoming arg on both sides).

Cycle resolution then sees `hold=false` on both slot 0 and slot 2 and
keeps both params. `keep_count`'s counter-first ordering is what made
the merge pick the pointer to drop under `sameargs2`; under the buggy
`proven` both slots reported `hold=true`.

Regression test: `pliron-clif-opt/tests/scan_loops.rs`,
`webmerge_head_deferral_must_echo_its_slots`. It builds the same
shape — a `select(sep == 0, base, sep)` entry, a forwarding block so
the back edge's args are predecessor params (`diff_b` `Head`, not
`key_sub`), and `idx += cur_delta` — and asserts the byte load still
reads the cursor param. It fails on the old code (the merge fires,
`cur` becomes `idx + SEL`) and passes on the fix.

## Bug 2 — the fix

`src/wasm.rs` `overflow`, `UmulOverflow | P::SmulOverflow` arm. For the
signed case the check is `hi != sext(prod)`. `hi` comes back normed to
`w` bits (`umulhi` masks bits `[w, 2w)` of the widened product), and
`prod` is the canonical zero-extended rep (`ibin` norms narrow
results). `prod >>s (w-1)` on `0xFF` gives `1`, not `0xFF` — a bit,
not a byte. The fix sign-extends `prod` to the word width first
(`sext(prod, w)`), shifts, then `norm`s back to `w` bits so `e` is in
`hi`'s rep. For `w >= 32` `sext`/`norm` are no-ops — i32/i64 emission
is unchanged. The `sadd`/`ssub` arms already did exactly this
(`sext(m, w)`); the mul arm missed it.

Regression test: `tests/wasm/lib.rs` `smul_i8`/`smul_i16`
(`i8::checked_mul`/`i16::checked_mul`, which emit `smul_overflow` even
under `--release`), asserted in `tests/wasm/run.mjs`. Without the fix:
`smul_i8(-1,1) = -1000` (want `-1`), `smul_i16(-1000,30) = -1000`
(want `-30000`); real-overflow and positive cases still pass. With the
fix all checks pass.

## Verification

- `mimas-shared` native pair (`a_unit_times_itself…`,
  `angle_units_carry_the_angle_dimension`): 2 passed; full
  `mimas-shared` suite 19/19; `mimas-library` units 34/34 — all
  native, all under pliron.
- `cargo test -p pliron-clif-opt`: green (9 tests incl. the webmerge
  regression).
- `tests/wasm/run.mjs` smoke: all checks green incl. `smul_i8`/`smul_i16`
  — and FAIL on exactly those checks under the unfixed backend.
- `harness/quick.py`: green; `harness/gate.py`: 105 jobs green (all
  stanzas, poison, corpora, `wasm_smoke`); `gate.py --only wasm` for
  the wasm sysroot legs.
- wasm end-to-end: `ReplSession.eval` on the pliron-built module
  (`PLIRON_WASM_PEEP` default-on, all optimizers on):

      print(60Hz)              => "60.0 Hz\n"
      print(60Hz.to(1Hz))      => "60.0\n"
      print(1Hz)               => "1.0 Hz\n"
      print(90deg.to(1deg))    => "90.0\n"
      print(1m/s^2)            => "1.0 m/s^2\n"
      print(music::synth(...)) => Voice { ... }

Note: `wasm-bindgen` must be 0.2.127 to match the crate
(`~/Library/Caches/.wasm-pack/wasm-bindgen-cargo-install-0.2.127/wasm-bindgen`);
`~/.cargo/bin/wasm-bindgen` is 0.2.130 and silently leaves a stale
`/tmp/ppkg` — that is what made the post-fix eval look red once.
