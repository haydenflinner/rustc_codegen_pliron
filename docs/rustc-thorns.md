# rustc thorns

Places where upstream rustc's semantics, emission, or behavior diverge from
what a backend would like — things that bit us or that we verified ourselves.
Each entry: what it is, where we hit it, upstream references, consequences.

Detection-verified on aarch64-apple-darwin, nightly-2026-10-06 + rustc 1.97.1.

## 1. `'a: 'static` declared-bound hole (unsound, accepted today)

```rust
use std::time::Duration;
use std::thread;

struct Q<'a: 'static>(Option<&'a String>);

impl<'a: 'static> Drop for Q<'a> {
    fn drop(&mut self) {
        let wrong: &'static String = self.0.unwrap();  // coercion legal: 'a: 'static assumed in-body
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            println!("{}", wrong);                     // UAF read
        });
    }
}

fn main() {
    {
        let l = String::from("abc");
        let mut q = Q(None);       // 'a inferred...
        q.0 = Some(&l);            // ...to l's short lifetime. Bound never enforced.
    }
    thread::sleep(Duration::from_secs(2));
}
```

Mechanism, narrowed down:

- `Q(&l)` and `Q(Some(&l))` are correctly rejected (E0597 — the declared
  `'a: 'static` bound IS enforced when 'a is pinned at construction).
- `Q(None)` + `q.0 = Some(&l)` is **accepted**: `'a` is inferred to the local
  lifetime and the declared bound never fires as a requirement. Inside `drop`
  the same bound is an ambient assumption, so `&'a String → &'static String`
  is legal there.

Zero diagnostics on stable 1.97.1 and nightly-2026-10-06. Both stock- and
pliron-built binaries print garbage, exit 0.

Related upstream: rust-lang/rust#90838 ("Unsound drop due to imperfect
lifetime checks" — impl-side stricter `Drop` where-clauses, fixed by #90840 /
E0367 `explicit-implied-outlives`). This variant uses the ADT's *own* declared
bound — part of the still-open implied-bounds hole family (#25860 and
descendants; #160491 tracks moving implied-bounds computation out of
borrowck).

Runtime detection results:

| Tool | Verdict |
|---|---|
| stock rustc `-Zsanitizer=address` | `stack-use-after-scope` reading `l`'s dead `String` header in instrumented `String::fmt` (no `-Zbuild-std` needed — the monomorphization is in the user CGU) |
| pliron `-Zsanitizer=address` | `heap-use-after-free` via intercepted `memcpy` of the freed 3-byte buffer — runtime links, **no instrumentation** (see §4) |
| `cargo +nightly-2026-10-06 miri run` | `dangling reference (use-after-free)` at closure *construction*, before the read — earliest + deterministic |

Nothing at codegen level can reject this; it's a borrowck hole in shared
frontend code. Differential harness can't catch it (both sides emit
identically-broken binaries); rustlantis can't generate it (UB-free by
construction).

## 2. `StorageDead`: dead-dead on paper, looser in emission

Spec (`rustc_middle/mir/syntax.rs`, written by #99050): `StorageDead` *frees*
the local's storage; using a local while unallocated is UB; `drop` on a local
implicitly ends its storage (#61015 semantics decision). On paper it is the
"can't-be-dereferenced" marker.

Emission is demonstrably looser:

- **Our own hit**: slotmerge treated `llvm.lifetime.*` markers as hard validity
  bounds and exempted escaped slots from end-of-function liveness —
  stage2 `lower_to_hir` read a corrupted `DefIndex`. Landed guard: unioned/
  phi-derived marker operands mark nothing (`pliron-clif-opt/src/slotmerge.rs`,
  `PLIRON_SLOTMERGE_MARK` re-enables for experiments). Either rustc emitted a
  `StorageDead` while a derived pointer was still legitimately readable
  (a rustc bug in the #98896 class) or the marker→slot resolution was too
  ambiguous to trust — both readings say the same thing: **don't consume the
  marker as a hard bound without the non-escaped/unambiguous guard.**
- **Upstream evidence it isn't airtight**: #98896 (`StorageDead` not dominated
  by `StorageLive` — found by a verifier on a trivial `#[derive(PartialEq)]`);
  #42371 (unbalanced markers in loops); #61015 still open (drop↔StorageDead↔
  unwind interaction unresolved; adding explicit StorageDeads regressed perf
  so `drop` carries the semantics instead).
- **Design context**: born in #35409 purely to feed `llvm.lifetime.*` and
  shrink stack frames; #68622 (eddyb) argues they're possibly unnecessary in
  MIR entirely — ideal placement computable from dominators/post-dominators.

Consequence for us: markers are liveness *hints* that need the
escape/ambiguity guard, not semantic walls. A poison-on-`StorageDead`
sanitizer would need the same guard — or accept violations as found rustc
bugs (on-brand for this project, but they'd be noise for users).

## 3. The weak-marker → strong-intrinsic handoff

`StorageDead` → `llvm.lifetime.end` is a direct translation:

```
statement.rs:80  StorageDead → cg_place.storage_dead(bx)
place.rs:320     storage_dead → bx.lifetime_end(llval, layout.size)
builder.rs:1464  → call @llvm.lifetime.end.p0(i64 size, ptr %alloca)
```

LLVM's `lifetime.end` is a *hard* semantic bound (deref past it = UB).
rustc hands LLVM a strict intrinsic derived from a marker its own MIR doesn't
always uphold (§2) — a latent unsoundness handoff that only doesn't bite
because LLVM doesn't exploit `lifetime.end` aggressively today. Meanwhile
`borrowed_locals.rs` dataflow already assumes "any pointers to this memory
are now invalid" at `StorageDead` — rustc internally treats it strictly
where convenient.

Gate detail (`session.rs:889` `emit_lifetime_markers`): LLVM only receives
the markers at `-O`, under `-Zsanitizer` (ADDRESS|KASAN|MEMORY|HWADDRESS),
or `-Zcodegen-emit-retag`. At `-O0` upstream emits nothing. **Our builder
emits marker ops unconditionally** — the pliron pipeline sees StorageLive/Dead
even where LLVM wouldn't.

LLVM-side fragility: LLVM 22 requires the intrinsic directly on the alloca
(`LLVMRustStripPointerCasts`, `builder.rs:1904`); LLVM 23's MemCpyOpt merges
allocas and *deletes* markers → 36–71% stack-frame regressions
(rust-lang/rust#164228, stable-to-stable). The intrinsics are fighting the
opt pipeline upstream; ours are first-class ops consumed by passes we control.

## 4. `-Zsanitizer` under pliron: half-on, silently

`rustc +nightly-2026-10-06 -Zcodegen-backend=...dylib -Zsanitizer=address`
produces a binary that:

- links `librustc-nightly_rt.asan.dylib` (shared rustc_ssa link code) —
  malloc/free/memcpy/pthread interceptors live, caught the §1 heap-UAF;
- has **zero instrumentation** — nothing in `src/` reads the sanitizer
  setting, `nm` shows no asan symbols; direct-load UAF passes silently;
- prints **no warning** that the flag is only half-implemented.

Harness state: every `sanitizer/*` UI test is `"skipped"` in
`harness/expectations/ui.*.json`.

Recorded options (not done): `__asan_load/store` emission pass in
pliron-ir-opt against the already-linked runtime (full ASan reports, no C++
in tree); pure-Rust `pliron-rt` shadow/registry runtime reached by rewriting
`__rust_alloc`/`__rust_dealloc` call targets at lowering (same pattern as the
`__udivti3` libcall adaptation — cleaner than compiler-rt's malloc
interposition); un-skip the interceptor-passable UI tests (`badfree`, `leak`);
stock-ASan/Miri as UB oracle when a run-pass diff needs classifying
"UB in source" vs "pliron miscompile"; at minimum warn when `-Zsanitizer` is
requested.

## 5. Older ones (already recorded elsewhere)

- f128 on aarch64-darwin: LLVM lowers to `long double` libcalls but
  `long double` is 64-bit there — stock rustc returns wrong values, pliron
  is *more* correct (`accepted` harness status; see
  `history/2026-10-18-test-harness-findings.md`).
- rustlantis-found upstream bugs: dozens in rustc, LLVM, Cranelift
  (see `testing-overview.md` "What the tests found", a note kept
  outside this repo).
- Backend-visible `captures(none)` / `nonnull` arg attrs from rustc feed
  jumpthread/slotmerge — useful hints, but deduced at fn-ABI granularity,
  so call rewrites (sret2reg, dead_args) must re-index them or they
  desync (stage2 miscompilation #3, docs/history/coordination-ledger.md).
