//! One-shot parse of the `PLIRON_*` environment.
//!
//! `std::env::var` takes a global lock per call, and passes gate
//! per-function — reads here are HashMap lookups after a single parse.
//! Semantics match the old inline reads exactly: gate vars default on
//! unless `=0`, debug flags are presence-checked, opt-ins compare `=1`.

pub mod jearena;
pub mod target;

use rustc_hash::FxHashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct PassConfig {
    /// Every `PLIRON_*` var present in the environment (raw values).
    values: FxHashMap<String, String>,
    /// `PLIRON_PIPELINE=a,b,c` — explicit CLIF-step order/selection.
    pub pipeline: Option<Vec<String>>,
}

/// Opt-in flags that are on by default: an unset variable reads as `1`.
/// `PLIRON_X=0` (any value but `1`) turns one off. Applied once, when the
/// config is parsed, so every `env_is(X, "1")` read site sees the same
/// value: the table is the single source of the default, and the kill
/// switch reaches every site by construction. Read with `env_is`/`get`,
/// never `env_set` (presence would count `=0` as on). The vendored
/// Cranelift reads `PLIRON_REMAT_SADDR`, `PLIRON_REMATCALL`,
/// `PLIRON_EDGEDEDUP`, `PLIRON_NARROWEXT`, `PLIRON_EDGEMERGE`,
/// `PLIRON_EPIMERGE`, `PLIRON_BJALIAS`, `PLIRON_TBNZ_IREDUCE`,
/// `PLIRON_EDITPAIR`, `PLIRON_MCP`, `PLIRON_MCPBB`, `PLIRON_CCMP` and
/// `PLIRON_JT_NARROW`, `PLIRON_SIBCALL`, `PLIRON_A64_HALFCLOB`,
/// `PLIRON_BUMPHOIST_INLOOP`, and the vendored waffle `PLIRON_WASM_MAXSSA_CUTALIAS`,
/// itself and
/// defaults them the same way (on unless `=0`; EDGEMERGE/EPIMERGE take
/// their mode default when unset). Validation: docs/history/2026-10-27-defaults.md,
/// 2026-10-28-overnight.md.
// r[impl par.cfg.defaults]
pub const DEFAULT_ON: &[&str] = &[
    // 2026-11-23: mintrip reads a header param's step through latch block
    // params (`clin`). blind-sig's `SmallVec::extend(repeat(x).take(n))`
    // gets its umin pre-loop and LLVM's `stp q0, q0` fill: -0.95% macOS
    // retired instructions; the other seven programs' __text is
    // byte-identical. `PLIRON_MINTRIP_CANON=0` off.
    "PLIRON_MINTRIP_CANON",
    // 2026-11-19: `call; return` as `return_call` on aarch64 (step
    // `sibcall`; the vendored Cranelift reads the same knob, on unless
    // `=0`). Exact counts: html-rewriter -0.60%, protobuf -0.40%, json
    // -0.25%, pulldown-cmark -0.20%, regex -0.15%, the rest equal
    // (`docs/history/2026-11-19-sibcall.md`). Stanza sibcall.
    // `PLIRON_SIBCALL=0` off.
    "PLIRON_SIBCALL",
    // 2026-11-20: latchspec keeps a latch whose header parameter is still
    // live into the folded branch's other successor (the fold would cost
    // moves plus the split edge's jump on the back edge). Exact counts
    // with PLIRON_RA2_POTTS_COPY: pulldown -0.78%, compression -0.57%,
    // protobuf -0.22%, html -0.13%, none worse
    // (`docs/history/2026-11-19-ra-flow.md`). `=0` off.
    "PLIRON_LATCHSPEC_LIVE",
    // 2026-11-20: slotconst forwards an integer load of a single-read slot
    // wholly covered by at most two narrower stores (brotli's
    // `Union1::shortcut` built in a stack temp and reloaded as one word).
    // Exact counts: compression -0.56%, no program worse
    // (`docs/history/2026-11-20-ivsr.md`). `PLIRON_SLOTCOMB=0` off.
    "PLIRON_SLOTCOMB",
    // 2026-11-20: brthread retargets edges into a bare `brif c` that the
    // edge decides (lol_html's `tag_is_one_of!` variant re-tests). Exact
    // counts: html -0.32%, protobuf -0.13%, json -0.06%, pulldown -0.03%,
    // none worse (`docs/history/2026-11-20-ivsr.md`). `PLIRON_BRTHREAD=0`.
    "PLIRON_BRTHREAD",
    // 2026-11-20: partial inlining of a kept callee's entry guard (enum drop
    // glue). Exact counts: regex -0.10%, html -0.045%, blind-sig -0.03%,
    // none worse (`docs/history/2026-11-20-ivsr.md`). `PLIRON_PARTINLINE=0`.
    "PLIRON_PARTINLINE",
    // 2026-11-21: mergethread copies a storing merge decided by constants
    // into each edge (lol_html's `consume_ch`). Exact counts: json -0.21%,
    // html -0.047%, none worse (`docs/history/2026-11-20-ivsr.md`).
    // `PLIRON_MERGETHREAD=0` off.
    "PLIRON_MERGETHREAD",
    // 2026-11-21: a temp only a copy of a noalias param writes reads the
    // param (memcpyopt::run_srcfwd). Exact counts: html -0.354%,
    // compression -0.174%, the rest within noise
    // (`docs/history/2026-11-20-ivsr.md`). `PLIRON_MEMCPYOPT_SRCFWD=0` off.
    "PLIRON_MEMCPYOPT_SRCFWD",
    // 2026-11-21: partinline takes niche-decoding and jump-chained drop
    // glue guards. Exact counts: pulldown -0.089%, html -0.079%, protobuf
    // -0.032%, the rest within noise (`docs/history/2026-11-20-ivsr.md`).
    // `PLIRON_PARTINLINE_WIDE=0` off.
    "PLIRON_PARTINLINE_WIDE",
    // 2026-11-22: tailmergep shares one param per distinct operand tuple
    // (a merged tail storing through one `sret` pointer seven times).
    // Exact counts: html -0.160%, pulldown -0.077%, regex -0.064%,
    // protobuf -0.018% (`docs/history/2026-11-20-ivsr.md`).
    // `PLIRON_TMPARAM_DEDUP=0` off.
    "PLIRON_TMPARAM_DEDUP",
    // 2026-11-22: wide128 splits foldable i128 block params (foldhash's
    // sponge across joins). Exact counts: html -0.060%, blind-sig +0.089%
    // (`montgomery` +8.4k, the rest noise). Gate/crates not run.
    // `PLIRON_WIDE128_PHI=0` off.
    "PLIRON_WIDE128_PHI",
    // 2026-11-22: equality chains against six or more sparse constants as
    // a balanced search tree. Exact counts: html -0.384%, the rest within
    // noise. Gate/crates not run. `PLIRON_EQTREE=0` off.
    "PLIRON_EQTREE",
    // 2026-11-22: selargs looks through `uextend` of a compare and then
    // threads edges whose arguments decide the destination's compare
    // (`Vec::with_capacity(len)` + extend: the reserve test). Exact
    // non-nop: json -0.165%, html -0.056%, blind-sig -0.041%, the rest flat.
    "PLIRON_SELARGS_EXT",
    // 2026-11-22: `bitcast.T (bitcast.U x: T)` folds, so aarch64's `shrn`
    // rule sees memchr's NEON match mask (with the vendored
    // `PLIRON_PAIRWISE_ISLE` and `PLIRON_MINMAXIMM`, flipped together):
    // html -0.312%, pulldown -0.338%, json +0.079% non-nop (`parse_str`
    // swaps an alignment nop for a callee save; same total), the rest flat.
    "PLIRON_BITCAST2",
    // 2026-11-19: deadslot deletes the stores into a slot nothing reads
    // (every address use is a plain notrap store's address), then the
    // slot. Exact counts: pulldown-cmark -0.37%, html-rewriter -0.09%, the
    // rest equal. `PLIRON_DEADSLOT_WRITEONLY=0` off.
    "PLIRON_DEADSLOT_WRITEONLY",
    // 2026-11-17, STK: memcpyopt's "tmp could promote" guard skips temps
    // larger than sroa's MAX_SIZE (512 B), which never promote:
    // driftsort_main's 4 KiB scratch is one slot again (regex-syntax both
    // monomorphs 8272 -> 4160 B, LLVM 4176; STK 1.264 -> 1.097). Held-out
    // exact counts: regex -8.9k, compression -1.8k, the rest unchanged
    // (blind-sig and rust-json vary run to run: random keys). Native
    // kernels and the wasm kernel module byte-identical. Stanza bigtmp.
    // `PLIRON_MEMCPYOPT_BIGTMP=0` off.
    "PLIRON_MEMCPYOPT_BIGTMP",
    // 2026-11-17, STK: slotmerge slides an end kill past the reads of its
    // slot a pass sank behind it in the same block (slp's paired vector
    // load), instead of dropping all the slot's markers: the seven
    // 128-byte Result slots of Parser::parse_with_comments share again
    // (1712 -> 816 B, LLVM 992; STK 1.097 -> 1.076 with BIGTMP; stale
    // slots 20 -> 8). Exact counts with BIGTMP: regex -9.2k, protobuf
    // -1.7k, compression -1.8k, none worse. Native kernels: k_hmget loses
    // a 48-byte copy, retired instructions and cycles within noise; wasm
    // module byte-identical. Poison stanzas pass. Test
    // slotmerge_killslide. `PLIRON_SM_KILLSLIDE=0` off.
    "PLIRON_SM_KILLSLIDE",
    // 2026-11-17, N7: repeated leaves of an xor tree cancel (SipHash's last
    // round: `v0` drops out of `v0 ^ v1 ^ v2 ^ (rotl(v3,21) ^ v0)`, and its
    // last `rotl`/`iadd` die). Quiet wasm kernels with --perturb=4 against
    // LLVM: hmget +1.9% -> -3.8%, hashmap +0.4% -> -2.1%, geomean 0.897 ->
    // 0.891, every other kernel inside its range. An algebraic identity;
    // native exact counts not yet taken. `PLIRON_XORCANCEL=0` off.
    "PLIRON_XORCANCEL",
    // 2026-11-17: wpeep also runs on bodies with SIMD instructions (it
    // decodes the 0xfd opcodes and treats SIMD values as opaque). Quiet
    // kernels with --perturb=4 (load 5.6): geomean 0.934 -> 0.934, no
    // kernel outside the default's range; dsa -483 KB with all 19 workloads
    // equal (fad51e94). `PLIRON_WASM_PEEP_SIMD=0` off.
    "PLIRON_WASM_PEEP_SIMD",
    // 2026-11-17, PARITY item 8: an integer load whose every use reads only
    // its low bytes loads that width (`i32.load8_u` instead of `i32.load` +
    // mask on wasm, `ldrb` on native). Native held-out exact counts:
    // pulldown-cmark -149.0k (-0.28%), rust-html-rewriter -144.8k, rust-
    // protobuf -61.2k, regex -13.0k, none worse; wasm kernels 4 functions,
    // nbody unchanged under --perturb=4. `PLIRON_LDNARROW=0` off.
    "PLIRON_LDNARROW",
    // 2026-11-17: memcpyopt redirects writers spread over branch arms that
    // together write every byte on every route into the copy. Quiet-machine
    // wasm kernels geomean 0.929 -> 0.917 against LLVM, no kernel worse than
    // the default's range; dsa +27k forwards, -184 KB, all 19 workloads
    // equal (fad51e94); xcorpus: 0 mismatches on kernels+std, the 14 dsa
    // ones explained as checker limits (docs/history/2026-11-16-xcorpus.md).
    // Native not timed.
    "PLIRON_MEMCPYOPT_SIBARMS",
    // 2026-11-17, wasm only, with `PLIRON_INLINE_ONCE_WASM=3` below
    // (docs/history/2026-11-16-licm-gte.md, -wasm-night.md): licm
    // hoists a load that runs on every iteration without a
    // dereferenceability proof, and a stack slot indexed by a loop
    // counter stays an object of its own. Alone they leave the wasm
    // kernel module byte-identical; they are what lets single-caller
    // inlining stop costing hmget. `PLIRON_LICM_GTE=0` /
    // `PLIRON_LICM_SLOTPROV=0` turn them off everywhere.
    "PLIRON_LICM_GTE_WASM",
    "PLIRON_LICM_SLOTPROV_WASM",
    // 2026-11-16 afternoon, wasm only (read in the inliner's small mode;
    // `PLIRON_INLINE_RECX=0` turns it off, `=1` turns it on for native
    // too): a site cloned out of an inlined body does not inline a member
    // of a call-graph cycle its clone chain already entered. The derived
    // `Clone` of a recursive AST is such a cycle, and callers outside it
    // unrolled it to the 20,000-op caller cap. dsa: module 49.81 -> 45.15
    // MB, code 40.1 -> 35.5 MB (`Matrix::defaulted` 193 KB -> 1.6 KB),
    // all 19 workloads equal, the 14 like-for-like rows 1.063 -> 1.061
    // against LLVM; the wasm kernel module is byte-identical.
    "PLIRON_INLINE_RECX_WASM",
    // 2026-11-16, the dsa wasm module (docs/history/2026-11-16-wasm-night.md,
    // "dsa, performance"). Gate and `harness/crates.py --tier 1` clean
    // with both on; the wasm kernels are unchanged (geomean 0.931 ->
    // 0.919 with a build running beside them).
    // memcpyopt: a sibling block is not between a writer and its copy,
    // so each `return` site's `store tmp; memcpy(sret, tmp)` forwards
    // (the interpreter's `step_one`: vm_* 1.17..1.56x LLVM -> 0.96..1.06x).
    "PLIRON_MEMCPYOPT_PATHWIN",
    // `forward_copies` again after phisimp: a constant table row
    // selected by an inlined callee's constant argument (blake3's
    // `MSG_SCHEDULE[round]` on wasm32: 4990 -> 2342 instructions).
    "PLIRON_CONSTCOPY2",
    // wasm emitter: constant copies of at most 16 bytes as loads and
    // stores instead of `memory.copy` (a C++ call in V8). On from f13cb0d,
    // off the same morning for a corrupted 2-byte copy in the
    // `PLIRON_OPT_BISECT=0` build that turned out to be waffle
    // rematerializing a load (5d703d0), on again in the afternoon: the
    // unoptimized and the optimized dsa modules return LLVM's results on
    // all 19 workloads with it, vm_mandelbrot and vm_physics are two
    // points better and the rest equal, the kernel module is identical.
    "PLIRON_WASM_SMALLCOPY",
    // Not here: `PLIRON_WASM_NOREALIGN` (N9 #9, `strip_realign`, dsa
    // -51 KB). It shifts parse_only's hot loop by 4 bytes in TurboFan's
    // code (identical otherwise) and parse_only goes from -10.3% to -4.4%
    // against LLVM, tokenize turns bimodal (one layout +5.8%), 2026-11-16
    // `--layouts=3`. Code alignment, not work, but the kernels may not lose.
    // wpeep (N9 #5): unreachable code goes ahead of block elision, so a
    // block only dead code branched to is elided too
    // (`wpeep_elide::dead_exact`). dsa: untargeted blocks 78k -> 14k in
    // non-SIMD bodies; kernels -2.8 KB, parse_only unchanged.
    "PLIRON_WASM_DEADTAIL",
    // wpeep (N9 #7): the branch-tail fold again after block elision.
    // dsa: fallthrough `br`s 37k -> 7.8k in non-SIMD bodies.
    "PLIRON_WASM_BTR2",
    // wpeep (N9 #2): `block; S; br_if 0; br K; end` -> inverted test and
    // `br_if K-1`. dsa: all 99,871 non-SIMD sites; kernels -3.7 KB.
    "PLIRON_WASM_BOB",
    // wasm emitter (N2/N9 row 1): drop sets of loop-carried block params
    // that only ever hold one outer value (`waffle::passes::maxssa::
    // remove_redundant_param_sets`). dsa -329 KB (loro's MovableListState
    // apply_diff 109 -> 67 KB), all 19 workloads equal; the kernel
    // module is byte-identical.
    "PLIRON_WASM_PARAMSCC",
    // wasm only (each is read behind an `is_like_wasm` / `Pipe::wasm`
    // test; native was not timed). 2026-11-16, `--layouts=3` against
    // the same llvm.wasm: chars_mixed +19.9% -> -1.4%, nbody +5.4% ->
    // +1.3%, module 183229 -> 180241 B
    // (docs/history/2026-11-16-wasm-night.md).
    // `assume(i < CONST)` as an `llvm.assume` marker that `lower.rs`
    // turns into a branch to the unreachable trap.
    "PLIRON_ASSUME_RANGE",
    // `vrp0` + `unreach0` ahead of jumpthread fold what such an edge
    // implies and drop it.
    "PLIRON_UNREACH_EARLY",
    // Edges that decide a test toward the unreachable trap are dropped
    // with their guard (`unwrap_unchecked` on a branchy `Option<&T>`).
    "PLIRON_UNREACH_PHI",
    // Full unroll of fixed-array pointer loops (`p == arr + N*size`).
    "PLIRON_UNROLL_SYM",
    // `binary_search` over a constant-length table as a select chain.
    "PLIRON_HALFUNROLL",
    // Equal-length reverse compares lose the second end test, and a
    // second range pass ahead of egraph2 folds what the late steps
    // built (both wasm only). strwork +10.6% -> +7.1%, parse_only
    // unchanged (`--layouts=3 --best`).
    "PLIRON_REVCMP",
    "PLIRON_VRP2",
    // Fold branches a dominating check decides before LICM looks for
    // escapes (regex's `follow_epsilons`: `assert!(len < cap)` then
    // `Vec::push`'s dead grow call, which took the set). Exact counts:
    // regex 55.09M -> 53.17M (x1.199 -> x1.157; `follow_epsilons` below
    // LLVM's), rust-protobuf, json, html-rewriter unchanged, pulldown
    // +0.03M (`scan_setext_heading` no longer unrolls).
    "PLIRON_LICM_DOMCOND",
    // Stack-slot merging (docs/history/2026-10-25-slot-coloring.md,
    // 2026-10-26-slot-interference.md).
    "PLIRON_SLOTMERGE_MARK",
    "PLIRON_SM_MARKFLOW",
    "PLIRON_SM_COALESCE",
    "PLIRON_SM_PRECISE",
    "PLIRON_SM_DROPNC",
    "PLIRON_SM_BYSIZE",
    "PLIRON_SM_FLOWIV",
    "PLIRON_SM_AFTERKILL",
    "PLIRON_SM_STARTS",
    "PLIRON_SM_STARTONLY",
    "PLIRON_SM_ESCFLOW",
    "PLIRON_SM_COALCALL",
    "PLIRON_NOCAP_FIELDS",
    "PLIRON_NOCAP_CALLEE",
    // copypair regroup + adjacent scalar-pair -> i64x2 merge
    // (docs/history/2026-11-09-copypair-scalar-merge.md); held-out A/B
    // 0.982 [0.971, 0.998] with the already-default-on AMODESHARE.
    "PLIRON_COPYPAIR",
    // sextload: uload8 -> sload8 when all uses are byte-transparent +
    // top-contiguous byte-mask compare -> signed interval compare
    // (commit d41c3ba; ~7% on the is_mostly_utf8 region, held-out A/B
    // neutral 1.004 [0.984, 1.039]).
    "PLIRON_SEXTLOAD",
    // Post-rotation dead-loop deletion: `st(f(iv))` countdown loops fold
    // to one closed-form store (montgomery's `*p = --i`; held-out A/B
    // 0.994 [0.977,1.015], html-rewriter 0.978*).
    "PLIRON_LOOPDEL",
    // Inliner (2026-10-26-inline-recursion.md).
    "PLIRON_INLINE_RESUME_DOM",
    "PLIRON_INLINE_RESUME_HOIST",
    "PLIRON_INLINE_ONCE1",
    "PLIRON_INLINE_RECSTACK",
    "PLIRON_INLINE_SCCLIMIT",
    // Price drop glue by its warm body (commit e4830a7): the EH scaffold
    // never runs on a normal drop, and charging it priced leaf glue out
    // of doomed cleanup sites LLVM inlines it into (regex-syntax
    // −1.8 KB .text; gate + corpus clean at e4830a7).
    "PLIRON_INLINE_GLUEWARM",
    // Memory (2026-10-26-memory.md, 2026-10-27-memory2.md, memory3.md).
    "PLIRON_DEADFREE",
    "PLIRON_LOWERFREE",
    "PLIRON_JEARENA",
    // Lifetime-undef on SROA-promoted locals (2026-10-27-spills.md;
    // side-table form in 922d8f7 keeps RSS flat).
    "PLIRON_LTUNDEF",
    // Cold-call args as plain copies (2026-10-27-exact-div.md).
    "PLIRON_COLDCOPY",
    // parse_only / eqbug digit-loop shape (2026-10-27-slow-kernels.md):
    // madd form, pointer step compare, select-chain peels.
    "PLIRON_SELCHAIN",
    "PLIRON_MULCARRY",
    "PLIRON_STEPCMP",
    // Wasm size (2026-10-27-wasm-size.md): refuse irreducible/recursive
    // loop-rule splices, GC dead locals, keep CLIF CFGs reducible, and
    // stop IR inlining along call-graph cycles in small/wasm mode.
    "PLIRON_WASM_NOIRRED",
    "PLIRON_WASM_NOREC",
    "PLIRON_WASM_GC",
    "PLIRON_REDUCIBLE",
    "PLIRON_INLINE_SMALLSCC",
    // Parity card B leftovers (2026-10-27-parity-card.md): jump-table
    // shape, Ordering folds through joins, drop trap after noreturn.
    "PLIRON_JT_SHAPE",
    "PLIRON_CMP3",
    "PLIRON_NORETTRAP",
    // Vector select(fcmp/icmp) fusion and inline llvm.minnum/maxnum
    // (docs/history/2026-10-29-cubecl-speed.md; semantics pinned by
    // pliron-clif-opt/tests/vecsel_native.rs). `=0` restores the scalar
    // per-lane select and the fminf/fmaxf libcalls.
    "PLIRON_VSELECT",
    "PLIRON_FMINMAX",
    // Height reassociation of straight-line chains (blake3's G:
    // 1.158 -> 1.057; docs/history/2026-10-31-tune-heldout.md).
    "PLIRON_HREASSOC",
    // Late null-test fold for cursors whose inbounds marks were lost
    // (regex's varint loop; docs/history/2026-10-31-tune-heldout.md).
    "PLIRON_NFOLD2",
    // Constant branches nfold2 decided fold; compares a join's edges all
    // decide fold (2026-11-22, docs/history/2026-11-20-ivsr.md).
    "PLIRON_NFOLD2_BR",
    "PLIRON_JOINCMP",
    // Late merge threading through conditions computed from constant edges
    // (foldhash's sponge; 2026-11-22, html -0.075%).
    "PLIRON_MERGETHREAD2",
    // Variable i128 shifts on halves: functions hashing with foldhash keep
    // no i128 and get the egraph (2026-11-22, with vendored `LOWBITS`:
    // html -0.119%).
    "PLIRON_WIDE128_VARSHIFT",
    "PLIRON_WIDE128_MULUNLOCK",
    // Dead-default switches dispatch on the raw tag; tables keep the
    // `unreachable` default and drop their bounds clamp (vendored
    // Cranelift reads PLIRON_SWITCH_KEEPDEAD itself, `!= "0"`).
    "PLIRON_SWITCH_RAW",
    "PLIRON_SWITCH_KEEPDEAD",
    // Switches of at most four cases as compare chains on the value
    // (regex-syntax's `from_encoded_range` bounds checks; bittab reads
    // the chain's offset range tests).
    "PLIRON_SWITCH_CHAIN",
    // Loop-invariant header params moved onto a preheader: regex -1.8% in
    // two layout-averaged runs (8 layouts), the rest flat
    // (docs/codegen-plan.md, inline policy search).
    "PLIRON_INVPARAM",
    // Decided Option-tag re-tests at loop exits threaded: regex -3.5%,
    // json -1.2%, geomean 0.991 (6 layouts).
    "PLIRON_JOINTHREAD",
    // bittab leaf args through single-pred params: with join threading,
    // json 1.142 -> 1.133 (serde_json's whitespace skip is one bit test).
    "PLIRON_BITTAB_CANON",
    // loadfwd ignores slot escapes in dead-end trap blocks (panic message
    // args): blind-sig 1.092 -> 1.069 (6 layouts).
    "PLIRON_ESC_TRAP",
    // (sym + n) - sym folds, so static-string copies get a constant size
    // and expand inline: json 1.118 -> 1.058, pulldown -0.9% (6 layouts).
    "PLIRON_SYMSUB",
    // (`PLIRON_LICM_INNERPRESS` is a valued default now: `DEFAULT_VALUES`.)
    // Loop-only unswitching when the loop's closed region is too big to
    // clone (docs/loop-unswitch-pressure.md section 3): regex's
    // `next_state` tests `b == EOF` once instead of per `Bytes` state.
    // Exact counts 2026-11-13: regex -0.29M, the other seven unchanged.
    "PLIRON_UNSWITCH_LOOP",
    // Static branch hints for emission-time placement (Cranelift patch 98
    // reads PLIRON_PLACE itself, on by default; docs/codegen-plan.md
    // 2026-11-02/03): held-out 1.111 -> 1.104 with the check set.
    "PLIRON_BRHINT",
    // The zip/enumerate loop chain (docs/codegen-plan.md 2026-11-03
    // afternoon): the late null fold, its brif-aware single-source
    // lookthrough, and looprot's latch sink; with `PLIRON_IVREFOLD2` and
    // `PLIRON_LOOPROT2` (pipeline gates) held-out 1.117 -> 1.099,
    // rust-compression -9%. `=0` restores each.
    "PLIRON_NULLSRC",
    "PLIRON_NULLFOLD2",
    "PLIRON_LOOPROT_SINK",
    // Bounded full unroll of `0..K` loops with LCSSA exit-cone repair:
    // postcard's varint decoders get the unrolled ldrb + const-shift orr
    // + tbz shape (varint_postcard -16%); `=0` keeps the runtime loop.
    "PLIRON_BUNROLL",
    // licm noalias provenance shelter: a writable rustc-`noalias` entry
    // param with no loop-observable escape has no in-loop value carrying
    // its provenance under another root, so stores rooted elsewhere
    // can't clobber its locs — &mut self field loads hoist past stores
    // through pointers loaded from it (compress_inner). `=0` off.
    "PLIRON_LICM_NASHEL",
    // Direct `bl`/`call` to mem*/memcmp libcalls instead of the GOT
    // indirection (lower.rs; ~2 insns per site, -1.7 KB crate-wide in
    // docs/history/2026-10-29-code-size.md). Held-out A/B with layouts
    // is noise-level (lib/pliron 1.005 vs aa 1.001; the earlier
    // html-rewriter/blind-sig regressions did not reproduce under
    // --layouts, 2026-11-09).
    "PLIRON_LIBCALL_DIRECT",
    // Edge-specialise small pure merge blocks: chars_ascii 2.473ms ->
    // 1.651ms (-33%, LLVM 1.567ms); corpus +214 insts over 12 fns, 0
    // verifier failures (docs/history/2026-11-09-esplit.md).
    "PLIRON_ESPLIT",
    // Cursor-bump hoist back into the memory block after the compile-path
    // egraph's elaboration sinks it into use blocks — exposes the
    // ldr-post-index fusion (vendored Cranelift reads it itself,
    // `!= "0"`; PLIRON_PATCH.md #116; chars_alpha 3.26ms -> ~2.1ms with
    // the AArch64 post-index path).
    "PLIRON_BUMPHOIST",
    // rustc's `nonnull` return attribute seeds nonnull call results
    // (lower.rs, extends the unconditional `st.nnret` inference to
    // `&[T]` data pointers et al) and `black_box` keeps the nonnull
    // proof — the op returns the argument's exact bits (jumpthread,
    // test black_box_keeps_nonnull). Together they fold the
    // `Option<NonNull>` niche `cbz` LLVM elides: chars_alpha 2.46 ->
    // ~2.10ms (LLVM 2.12ms); corpus replay clean with both on.
    "PLIRON_RETNN",
    "PLIRON_NNBB",
    // Affine address rebase through proven param-difference invariants:
    // `iadd X, b` with X ≡ root+K under a dominating `iadd root, b`
    // funnels onto the shared pointer (UTF-8 scan's paired index IVs,
    // unicode lookup_slow's table base). Wrapping-add reassociation
    // only; corpus -311 insts over 20 functions, one +2 outlier.
    "PLIRON_LSR",
    // Parameterized cold-tail merge: blocks identical modulo iconst
    // immediates share one parameterized body; members shrink to
    // iconsts + jump (UTF-8 Utf8Error exits, gimli match arms). Corpus
    // -194 functions touched, all shrinks (top -244/-164/-146), diff
    // clean, gate clean under the flag.
    "PLIRON_TMPARAM",
    // `bor`-tree byte assemblies of consecutive uload8s -> one wide
    // load (LLVM's CombineConsecutiveLoads). Brotli corpus -4,423
    // across 77 fns, all shrinks, diff+gate clean under the flag
    // (FindMatchLengthWithLimitMin4 872->696 emitted).
    "PLIRON_LDMERGE",
    // Adjacent `store.i8` pairs fed by adjacent `load.i8`s (scalarized
    // 2-byte aggregate copies, sort-element swaps) -> `load.i16`/
    // `store.i16` (LLVM's MergeConsecutiveStores). regex-syntax
    // drift::sort<ClassBytesRange> 963->867 insns, rlib -200, wasm
    // -573B; corpus diff clean, kernels flat-to-better (Sortw -11%).
    "PLIRON_STMERGE",
    // A load covered by `store (load s)` re-reads `s` (LLVM's
    // memcpy-forwarding): brotli sub_match buffers collapse to direct
    // slice reads after bunroll exposes the copy. Held-out geomean
    // 1.001 neutral, rust-compression 0.984; regex-syntax .text +64B.
    // `=0` off.
    "PLIRON_LOADFWD_SRCCOPY",
    // Per-callee clobber sets (aarch64, not Windows): a direct call to a
    // function compiled in the same object destroys only the registers
    // that function's code writes on a path that returns
    // (`pliron-clif-lower` `fastcc_compile`). Exact counts 2026-11-13:
    // pulldown-cmark 52.82M -> 52.53M (-0.55%), the other five held-out
    // programs within 0.01% (their hot calls leave the CGU or are
    // `try_call`s); compile CPU +0..1.5%, peak RSS +0..3% (regex,
    // rust-html-rewriter). Validation:
    // docs/history/2026-11-13-six-features.md. `=0` off.
    "PLIRON_FASTCC",
    // Cold `call f(..); trap` tails of one callee share one block per
    // function (`panicstub`). A size change only: .text -0.1..-0.2% on
    // regex and html-rewriter, less elsewhere, executed instructions
    // within +-0.05%. `=0` off.
    "PLIRON_PANICSTUB",
    // Const-only CondBr→join `Err`/`None` tails marked cold before
    // `coldedges` (`errcold`): native layout sinks them; wasm COLDBR
    // can keep the hot edge bare. `=0` off.
    "PLIRON_ERRCOLD",
    // 2026-11-17 (PARITY item 9): sroa does not write a split temporary
    // back before a call whose only pin on it is its whole `sret` argument.
    // Exact callgrind counts, aarch64-linux-musl, against the default:
    // pulldown-cmark -18.8k, rust-json -22.4k, regex -1.0k, html-rewriter
    // and blind-sig within noise, blake3/compression/protobuf unchanged;
    // native kernel `varint_postcard` -4.8% retired instructions; wasm
    // kernels: `k_sieve` loses two dead stores, timings unchanged
    // (PARITY item 7). Stanza `sretnosync`. `=0` off.
    "PLIRON_SROA_SRETNOSYNC",
    // 2026-11-17 (PARITY item 9): `unroll2`, `unroll` again after the
    // first `loadfwd`/`egraph`. Exact counts: html-rewriter -179k
    // (-0.18%), pulldown-cmark -11.9k, regex and compression within noise,
    // the others unchanged; native kernel `varint_postcard` -0.25%; the
    // wasm kernel module is byte-identical. `=0` off.
    "PLIRON_UNROLL2",
    // 2026-11-17 (PARITY item 9): `PLIRON_LICM_GTE` on native too (it was
    // on for wasm as `PLIRON_LICM_GTE_WASM`, which still works). Exact
    // counts: regex -57.2k (-0.11%), rust-json and blind-sig within noise,
    // the other five and the native kernels unchanged. `=0` off everywhere.
    "PLIRON_LICM_GTE",
    // 2026-11-17 (PARITY item 9): `punroll` does not keep a do-while's
    // counting latch test in every copy. Exact counts: regex -1.2k,
    // compression -1.7k, the others and the native kernels unchanged; the
    // wasm kernel module is byte-identical (PARITY item 5). `=0` off.
    "PLIRON_PUNROLL_DWLATCH",
    // 2026-11-17 (PARITY item 10): `slotconst`, a stack-slot load of bytes
    // that constant stores in the same block just wrote becomes the
    // constant (stdarch's NEON constants built through memory). Exact
    // counts: html-rewriter -25.9k, compression -2.0k, protobuf -1.7k,
    // rust-json -0.9k, the rest equal; native kernels unchanged; the wasm
    // kernel module changes only `k_nbody_prep` (nbody within its
    // `--perturb=4` range). Corpus replays: identical verdicts. `=0` off.
    "PLIRON_SLOTCONST",
    // 2026-11-17 (PARITY item 10): `ranges::decide` folds an equality
    // whose operands' signed intervals are disjoint (dragon's `Ordering`
    // against the -2 niche). Exact counts: blind-sig -44.2k (-0.13%), the
    // other seven unchanged (their differences, at most 0.002%, are all in
    // `RandomState` hashing and libc malloc). `=0` off.
    "PLIRON_VRP_SIGNEDEQ",
    // 2026-11-17 (PARITY item 10): `ivmerge`, lockstep loop params share
    // one register and the dropped one's end test folds. Native exact
    // counts: pulldown-cmark -0.68%, compression -0.25%, regex -4.2k,
    // blind-sig -3.7k, the other four unchanged; wasm kernel module -1.2%,
    // geomean 0.889 -> 0.884 with no kernel outside its `--perturb=4`
    // range. Corpus replays clean. `=0` off.
    "PLIRON_IVMERGE",
    // 2026-11-17: demanded bits, and the last-use scheduling refusal it
    // needs (`PLIRON_LUCARRY`, also default on in cranelift's
    // `lastuse_schedule`: that read is `std::env`, not this list).
    // Exact counts, both together: better on all eight held-out programs
    // (regex -0.34%, pulldown-cmark -0.22%, html-rewriter -0.12%,
    // compression -0.08%; `DEMANDED` alone made regex worse). html
    // `uxtb`/`uxth` 637 -> 542, wasm kernel module -444 B. Corpus
    // `--check` with `PLIRON_VERIFY=full` after the merge: smoke 2,979,
    // std every 8th, wasm-bench every 8th, no mismatch. `=0` off.
    // `docs/history/2026-11-17-demanded-bits.md`.
    "PLIRON_DEMANDED",
    "PLIRON_LUCARRY",
    // 2026-11-18: 128-bit ops on word halves (`wide128`/`wide128b`). Use
    // gate + FNMAX=512 keep montgomery out (was +2.9% blind-sig). Callgrind
    // non-nop: html −0.043%, blind-sig −0.12%, no program worse. `=0` off.
    "PLIRON_WIDE128",
];

/// Valued defaults: an unset variable reads as the given string.
/// `PLIRON_X=0` (or any other explicit value) overrides. Used for caps
/// that are not boolean `=1` gates — `env_get` sites see the default
/// the same way `env_is` sees `DEFAULT_ON`. Kill switch for a cap is
/// usually `=0` (off).
pub const DEFAULT_VALUES: &[(&str, &str)] = &[
    // 2026-11-22: one `symbol_value` per global within a block (foldhash's
    // seed address loaded five times in `reserve_rehash`). Exact counts:
    // html -0.149%, the rest flat. `=1` also across dominator scopes,
    // `=0` off. Gate/crates not run.
    ("PLIRON_CSE_SYM", "2"),
    // 2026-11-21: the inliner charges no `call_w` for the cold calls of a
    // callee with no warm call whose ops fit the base limit
    // (`Vec::extend_trusted`). Exact counts: html -0.179%, pulldown
    // -0.030%, the rest within 0.002% (`docs/history/2026-11-20-ivsr.md`).
    // `=0` off.
    ("PLIRON_INLINE_WARMCALLW", "3"),
    // 2026-11-20: `ivsr` decided per function after allocation (aarch64):
    // the backend compiles the function with and without the pointer IV
    // and keeps the rewrite when its frequency-weighted code is smaller and
    // the once-per-call part is not larger. With `PLIRON_BUMPHOIST_INLOOP`
    // (vendored, default on): compression -1.165%, blind-sig -0.102%,
    // pulldown -0.043%, none worse (`docs/history/2026-11-20-ivsr.md`).
    // `=1` the plain pipeline step, `=0` off.
    ("PLIRON_IVSR", "2"),
    // 2026-11-20: `looprot` rotates loops with up to 4 latches (a copy of
    // the exit test in each; lol_html's `LocalNameHash::update`). With
    // regalloc2's `PLIRON_RA2_MAXSPLITS=4` (vendored default): html
    // -0.62%, compression -0.39%, json -0.11%, blake3 -0.11%, protobuf
    // -0.06%, pulldown -0.05%, regex -0.003%, blind-sig noise
    // (`docs/history/2026-11-20-ivsr.md`). `=1` one latch only.
    ("PLIRON_LOOPROT_MULTI", "4"),
    // 2026-11-17, STK: sroa's FRAGCOPY rule does not keep whole an
    // aggregate of at least 64 bytes that direct stores build (all but
    // less than a word of padding) and one copy moves out: the copy
    // becomes the stores into its destination (Box::new(PropertiesI {..}),
    // Parser::new). regex-syntax STK 1.076 -> 1.043 with BIGTMP and
    // KILLSLIDE. Exact counts: html -6.6k, the rest unchanged; at 32 or
    // with unwritten pieces allowed, regex (+0.19..0.36%) and pulldown
    // (+0.09%) lost. Native kernels within noise (iterw, tokenize
    // change), wasm module byte-identical. Stanza fragcopyout.
    // `PLIRON_SROA_FRAGCOPY_OUT=0` off.
    ("PLIRON_SROA_FRAGCOPY_OUT", "64"),
    // 2026-11-17, codegen queue (contains_rng): on wasm, counted loops of
    // unknown length unroll 4 wide (one counting test per group, with
    // `PLIRON_PUNROLL_DWLATCH`), but only in functions of at most 800
    // instructions: in dragon's format_exact (1,270-6,146) the few-limb
    // bignum loops cost strwork when unrolled. Quiet kernels with
    // --perturb=4 against LLVM: contains_rng +10.2% -> -0.6%, matmul -26.0%
    // -> -53.9%, sieve -3.9% -> -9.6%, parse_only -4.8% -> -6.1%, strwork
    // +4.2% -> +4.5% (ranges overlap), geomean 0.942 -> 0.897; every other
    // kernel inside its range. `PLIRON_PUNROLL_COUNTED_WASM=0` off.
    ("PLIRON_PUNROLL_COUNTED_WASM", "4"),
    ("PLIRON_PUNROLL_COUNTED_WASM_FNMAX", "800"),
    // 2026-11-17: wasm functions restore the shadow stack pointer once, in
    // a single exit block, instead of before every `return` (2.45
    // restores per function against LLVM's ~1). Quiet-machine kernels
    // geomean 0.927 -> 0.927 against LLVM, no kernel worse than the
    // default's own range; dsa restores 54.7k -> 32.0k, ~36 KB.
    // `PLIRON_WASM_ONEEXIT=0` off.
    ("PLIRON_WASM_ONEEXIT", "1"),
    // 2026-11-17: wasm's single-caller inlining mode `3`, one loop nest
    // per function (a callee with a loop only into a loop-free caller
    // that has taken no such callee). With the two licm rules above, on
    // a quiet machine: wasm kernels geomean 0.931 -> 0.929 against LLVM,
    // hmget +5.7% -> +1.9%, hashmap +1.2% -> -0.1%, strwork +4.4% ->
    // +2.9%, tokenize and parse_only unchanged over two 5-layout reruns
    // (-14.4/-15.6 vs -14.5/-15.9, -10.6/-11.0 vs -10.7/-10.8), every
    // other kernel within its range; dsa module 44.09 -> 42.36 MB, all
    // 19 workloads return LLVM's results, the 14 like-for-like rows
    // 1.030 -> 0.999 against LLVM. `PLIRON_INLINE_ONCE_WASM=0` restores
    // the old wasm behaviour (no single-caller bonus); native keeps `1`.
    ("PLIRON_INLINE_ONCE_WASM", "3"),
    // Outer-only loads stay out of nested-loop preheaders (`1`: geomean
    // 0.997 / 0.998 in two layout-averaged runs) unless every inner loop
    // has register room for them (`2`, docs/loop-unswitch-pressure.md
    // section 2). `2` lost 1.07M instructions on regex until `loadfwd`
    // stopped threading the hoisted loads through the nest as block
    // params (`PLIRON_LF_TRIVPHI`); with that, exact counts 2026-11-13:
    // regex -1.21M (`next_state`'s decode loop keeps the `SparseSet`
    // fields in registers), pulldown-cmark -0.18M, rust-protobuf and
    // blind-sig -0.04M, the rest within 0.03M. `=1` restores, `=0` off.
    ("PLIRON_LICM_INNERPRESS", "2"),
    // At most 8 CGUs past the start of stage A (2026-10-27-memory2.md).
    // Tuned on a 14-core Mac; `=0` disables the cap.
    ("PLIRON_CGU_ACAP", "8"),
    // At most 4 CGUs submitted and unfinished: the main thread parks in
    // `submit` instead of queueing more unoptimized Contexts. syn peak
    // RSS 1.07->0.75 GB (1.70->1.22x LLVM), regex-syntax 0.55->0.47 GB,
    // no measurable wall cost (2026-11-06; `=0` disables).
    ("PLIRON_CGU_INFLIGHT", "4"),
    // Waffle loop-rule leaf filter (`1` ⇒ ≤2 calls in the callee).
    // 5.73x → 2.17x on regex-wasm CODE; hash_one-style leaves still inline.
    ("PLIRON_WASM_HOTLEAF", "1"),
    // Edge/epilogue merge modes (vendored Cranelift `pliron_mode` also
    // defaults unset the same way; `=0` off, `=1` placement-aware merge,
    // `=2` merge-everything, EDGEMERGE `=3` merge-everything-outside-loops).
    ("PLIRON_EDGEMERGE", "3"),
    ("PLIRON_EPIMERGE", "2"),
    // Cap inlining of high-fan-in callees (2026-10-27-ir-size.md). lockf
    // ctime: FANIN=8 cut syn text 1.94→1.46x / RSS 1.81→1.52x. `=0` off.
    ("PLIRON_INLINE_FANIN", "8"),
    // Drop-glue call-site threshold cap (bfc18cc): glue lowers ~1 op:1
    // insn, so mid-size glue slips under the 320-op limit where LLVM
    // declines it, cloning dealloc+landing-pad paths into every site.
    // regex-syntax .text 356,016->346,064 (-2.8%), wasm obj -1.0%,
    // kernels flat. `=0` off.
    ("PLIRON_INLINE_DROPLIM", "64"),
    // Waffle single-callsite inline cap (src/wasm.rs). 15: slight win
    // over 25 on regex-wasm post-WTREEREMAT; 10 is a wash. Prior 100.
    ("PLIRON_WASM_SOLO", "15"),
    // Always-inline waffle bodies (src/wasm.rs). 6: −~0.4 KiB regex-wasm
    // vs default 12 with no observed runtime hit on hash kernels.
    ("PLIRON_WASM_TINY", "6"),
    // Duplicate-less copies: every shared `inlined` mono item keeps one
    // owner definition; the other partitions' copies convert to imports
    // after inlining, as does every `extra` copy whose home is linkable
    // (2026-11-06-xcgu-dedup.md: syn SIZ 1.75->1.20, CMP 1.31->1.13,
    // MEM 1.84->1.69, runtime neutral). `=1` degraded copies only, `=all`
    // copies only (no inlined-item election), `=0` off.
    ("PLIRON_XCGU_HOME", "mono"),
    // Promote single-home Internal items some CGU copies (and `inlined`
    // items' owners under HOME=mono) hidden-external so the duplicates
    // can bind them. `=cold` copies only, `=1` every uncopyable dep,
    // `=0` none.
    ("PLIRON_XCGU_PROMOTE", "copyable"),
    // Descend into external deps when closing over copy candidates, so a
    // consumer CGU's copy inlines as deeply as the home CGU's (2026-11-04:
    // bench/sieve -21%, varint_postcard -5%, rest within noise; ~+30%
    // emitted copies, ~7% codegen on the bench crate). `=0` off.
    ("PLIRON_XCGU_XDEPS", "1"),
    // `inlined=true` deps may exceed the size cap: the partitioner already
    // expects them duplicated per CGU (SipHash `d_rounds`, size 127, in the
    // hashmap probe loop). Only items that actually fail the normal limit
    // are exempted from cap accounting (2026-11-05: hashmap -5%, iterw and
    // the rest within noise, no codegen-time delta; needed the loopidiom
    // forwarder-param escape fix for the wasm leg). `=0` off.
    ("PLIRON_XCGU_INLINED", "1"),
    // 2026-11-18: leaf callees (calls only cold / intrinsics / other small
    // leaves) copyable up to 100 MIR stmts, with LASTCALL=20 so a single-
    // user dep over that size stays Internal and is not promoted (the
    // finish_grow / grow_amortized trap). Retimed on top of RA2_REGION:
    // callgrind non-nop vs default — pulldown -0.40%, json -1.23%, html
    // -0.42%, regex -0.08%, protobuf -0.07%, blind-sig -0.02%, blake3 flat;
    // rust-compression +0.03% (HistogramSelfAddHistogram RA residual, was
    // +0.23% before REGION). Plain SIZE=100 still costs protobuf +1.18%.
    // `PLIRON_XCGU_LEAF=0` / `PLIRON_XCGU_LASTCALL=0` off.
    ("PLIRON_XCGU_LEAF", "100"),
    ("PLIRON_XCGU_LASTCALL", "20"),
    // 2026-11-22: the leaf walk goes 12 levels deep (was 8): a by-value
    // `Vec::push` reaches the allocator through ten levels of unwind
    // drop glue, so lol_html's `LimitedVec::push` (10.4k calls from
    // another CGU) stayed a call. Exact non-nop: html -0.094%, pulldown
    // -0.057%, blind-sig -0.06% (noise), protobuf +0.037% (prost's
    // `merge_repeated` inlining reshuffle), the rest flat. `=8` the old walk.
    ("PLIRON_XCGU_LEAF_DEPTH", "12"),
    // Native enum matches of 5+ cases as clustered trees (2026-11-22:
    // regex -4.97%; `=2` the old wasm-only clustering).
    ("PLIRON_SWITCH_GCC", "3"),
    // 2026-11-22: runtime-size copies in functions of at most 1024 IR ops
    // handle sizes below 16 inline, as loops already do (`MEMFAST_SMALL`'s
    // `loop` mode). Exact non-nop: protobuf -0.83%, json -0.41%, html
    // -0.29%, pulldown -0.18%, the rest flat; .text +0.04% (protobuf) to
    // +1.44% (regex). `MEMFAST_SMALL=1` (every copy): json -1.99% but
    // .text up to +2.17%. `=0` off.
    ("PLIRON_MEMFAST_SMALLFN", "1024"),
    // 2026-11-18: wide128 skips functions over this many CLIF instructions
    // (montgomery ~1.7k). `=0` no size cap. Foldhash `build_state` is a
    // few hundred.
    ("PLIRON_WIDE128_FNMAX", "512"),
    // Single-site free inline threshold, x the inline limit (2026-11-02:
    // pulldown's `parse_line` closure, 4,594 ops, inlines; pulldown -2%).
    ("PLIRON_INLINE_FREEMUL", "12"),
];

// r[impl par.cfg.once]
pub fn config() -> &'static PassConfig {
    static CONFIG: LazyLock<PassConfig> = LazyLock::new(PassConfig::parse);
    &CONFIG
}

impl PassConfig {
    fn parse() -> Self {
        let mut values = FxHashMap::default();
        for (k, v) in std::env::vars_os() {
            if let Some(k) = k.to_str()
                && k.starts_with("PLIRON_")
            {
                values.insert(k.to_string(), v.to_string_lossy().into_owned());
            }
        }
        for k in DEFAULT_ON {
            values.entry(k.to_string()).or_insert_with(|| "1".to_string());
        }
        for &(k, v) in DEFAULT_VALUES {
            values.entry(k.to_string()).or_insert_with(|| v.to_string());
        }
        let pipeline = values.get("PLIRON_PIPELINE").map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        });
        Self { values, pipeline }
    }

    /// `PLIRON_X` unset or `!= "0"` — the default-on pass gate.
    pub fn enabled(&self, var: &str) -> bool {
        record(var, self.values.get(var).map(String::as_str));
        self.values.get(var).map_or(true, |v| v != "0")
    }

    /// `PLIRON_X` present at all — presence-flag debug knobs.
    pub fn set(&self, var: &str) -> bool {
        record(var, self.values.get(var).map(String::as_str));
        self.values.contains_key(var)
    }

    /// `PLIRON_X == v` — opt-in gates (`=1` and model selects).
    pub fn is(&self, var: &str, v: &str) -> bool {
        record(var, self.values.get(var).map(String::as_str));
        self.values.get(var).is_some_and(|x| x == v)
    }

    pub fn get(&self, var: &str) -> Option<&str> {
        record(var, self.values.get(var).map(String::as_str));
        self.values.get(var).map(String::as_str)
    }

    /// One-line summary for `PLIRON_STATS` headers: explicit pipeline or
    /// the default, plus every gate forced off.
    pub fn describe(&self) -> String {
        let mut off: Vec<&str> = self
            .values
            .iter()
            .filter(|(_, v)| v.as_str() == "0")
            .map(|(k, _)| k.as_str())
            .collect();
        off.sort();
        match &self.pipeline {
            Some(p) => format!("custom [{}], off: {}", p.join(","), off.join(",")),
            None => format!("default, off: {}", off.join(",")),
        }
    }
}

// Cache read tracking: a cached entry's validity depends only on the
// `PLIRON_*` vars the producing run actually consulted, so flipping a
// var invalidates only entries that read it. `READS` is a stack of
// windows: a read lands in every open frame, so an outer (pipeline-
// level) window accumulates the union of its inner (per-step) windows.
// Reads outside a tracking window cost a TLS borrow-check.
thread_local! {
    static READS: std::cell::RefCell<Vec<rustc_hash::FxHashMap<String, Option<String>>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Read windows open on THIS thread. A global flag would gate on
    /// other threads' windows: while any worker is inside a pipeline
    /// frame, every `config()` read on every other thread (lowering,
    /// emission) falls out of the no-window fast path and pays a TLS
    /// borrow + frame walk — measured at ~1 s per million reads.
    static ARMED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// True while a read-recording window is open on this thread. Callers
/// memoizing an env-derived value must take the real `config()` path
/// under this so the reads still land in the open windows.
#[inline]
pub fn reads_open() -> bool {
    ARMED.with(|a| a.get() != 0)
}

/// Open a read-recording frame. Always succeeds — frames nest, and each
/// frame sees only the reads that happen inside it.
pub fn reads_begin() -> bool {
    ARMED.with(|a| a.set(a.get() + 1));
    READS.with(|r| {
        r.borrow_mut().push(Default::default());
        true
    })
}

/// Close the innermost frame and return its `(var, value)` pairs,
/// sorted so equal read-sets compare equal.
pub fn reads_take() -> std::collections::BTreeMap<String, Option<String>> {
    if mcprof() {
        // Merge this thread's per-key counts — a window close is the
        // natural batch point, so the global map sees one lock per
        // window instead of one per recorded read.
        KEY_COUNTS.with(|m| {
            let mut m = m.borrow_mut();
            if !m.is_empty() {
                let mut g = RECORD_KEYS.lock().unwrap_or_else(|e| e.into_inner());
                for (k, n) in m.drain() {
                    *g.entry(k).or_default() += n;
                }
            }
        });
    }
    let m = READS.with(|r| r.borrow_mut().pop());
    if m.is_some() {
        ARMED.with(|a| a.set(a.get() - 1));
    }
    m.unwrap_or_default().into_iter().collect()
}

pub static RECORD_COUNT: AtomicU64 = AtomicU64::new(0);
static RECORD_KEYS: std::sync::LazyLock<
    std::sync::Mutex<rustc_hash::FxHashMap<String, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(rustc_hash::FxHashMap::default()));
thread_local! {
    static KEY_COUNTS: std::cell::RefCell<rustc_hash::FxHashMap<String, u64>> =
        Default::default();
}
// std::env, not `config()` — the recorder must not re-enter `config()`.
fn mcprof() -> bool {
    static PROF: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("PLIRON_MCPROF").is_some());
    *PROF
}

fn record(k: &str, v: Option<&str>) {
    // Cache-machinery knobs are inputs to the cache itself, not to the
    // computation — recording them would tie variants to the cache dir
    // or profiling flags rather than to the work performed.
    if matches!(
        k,
        "PLIRON_STEPCACHE" | "PLIRON_CACHE_DIR" | "PLIRON_CACHE_LOG" | "PLIRON_MCPROF"
    ) {
        return;
    }
    if ARMED.with(|a| a.get() == 0) {
        return;
    }
    if mcprof() {
        RECORD_COUNT.fetch_add(1, Ordering::Relaxed);
        KEY_COUNTS.with(|m| *m.borrow_mut().entry(k.to_string()).or_default() += 1);
    }
    READS.with(|r| {
        for m in r.borrow_mut().iter_mut() {
            // Most calls re-record a key this frame already has — hash
            // once and skip the String allocs.
            if !m.contains_key(k) {
                m.insert(k.to_string(), v.map(str::to_string));
            }
        }
    });
}

/// PLIRON_MCPROF: top recorded keys (debug only).
pub fn record_keys_top() -> Vec<(String, u64)> {
    let mut v: Vec<_> = RECORD_KEYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, n)| (k.clone(), *n))
        .collect();
    v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    v
}

/// `PLIRON_<PASS>=0` disables a pass at runtime (default on).
// r[impl par.cfg.accessors]
pub fn pass_enabled(var: &str) -> bool {
    config().enabled(var)
}

/// Whether any tracing subscriber is live. The `tracing` macros pay a
/// `get_default` TLS access + callsite-registry interest check per
/// invocation even with no dispatcher installed — measured at ~4% of
/// codegen samples under `finish_module`. Guarding per-fn/per-step span
/// construction with this is one relaxed atomic load. `tracing_on()` is
/// true once the backend's `tracing_init` installs its subscriber, or
/// when any caller (e.g. a test harness) installed a dispatcher itself.
static TRACING_ON: AtomicBool = AtomicBool::new(false);

/// Mark that a subscriber was installed. Called from the backend's
/// `tracing_init`; test harnesses that call `fmt::init()`/`set_default`
/// are caught by the `has_been_set` arm below.
pub fn mark_tracing_on() {
    TRACING_ON.store(true, Ordering::Relaxed);
}

#[inline]
pub fn tracing_on() -> bool {
    TRACING_ON.load(Ordering::Relaxed) || tracing::dispatcher::has_been_set()
}

/// `run_step`'s per-step `Instant::now()`s are only observable through
/// `pliron::timing` logs or `PLIRON_TRACE_FILE` lines — harnesses that
/// consume `Stats::timing` directly (clif-check bench) mark this.
static STEP_TIMING: AtomicBool = AtomicBool::new(false);

pub fn mark_step_timing() {
    STEP_TIMING.store(true, Ordering::Relaxed);
}

/// Whether per-step wall-clock reads are worth paying. Off in a plain
/// rustc compile (no subscriber path can print `pliron::timing` and no
/// trace file is open): ~2 clock reads per step per function saved.
#[inline]
pub fn step_timing_on() -> bool {
    static DERIVED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        env_get("PLIRON_TRACE_FILE").is_some()
            || (tracing_on()
                && tracing::enabled!(target: "pliron::timing", tracing::Level::INFO))
    });
    STEP_TIMING.load(Ordering::Relaxed) || *DERIVED
}

/// `PLIRON_<X>` present — presence-flag debug knobs.
pub fn env_set(var: &str) -> bool {
    config().set(var)
}

/// `PLIRON_<X> == v` — opt-in gates.
pub fn env_is(var: &str, v: &str) -> bool {
    config().is(var, v)
}

/// `PLIRON_<X>` raw value.
pub fn env_get(var: &str) -> Option<&'static str> {
    config().get(var)
}

/// A profile-use knob's value: what the environment says, else `bundle`
/// when a profile is in use (`PLIRON_PGO_USE`) and the bundle is not
/// switched off (`PLIRON_PGO_BUNDLE=0`). The bundle is the set of
/// consumers that measurably pay on the held-out programs
/// (docs/pgo.md): hot-call-site inlining, hot callees copied across
/// codegen units, small inline copies at hot sites, measured block
/// placement.
pub fn pgo_knob(var: &str, bundle: &'static str) -> Option<&'static str> {
    env_get(var).or_else(|| {
        (env_get("PLIRON_PGO_USE").is_some() && !env_is("PLIRON_PGO_BUNDLE", "0")).then_some(bundle)
    })
}

/// `PLIRON_TRACE_FILE=<path>`: append one timing line per pipeline step (and per
/// `*pipeline*`/`*compile*` CGU summary) the moment it is measured, so a
/// watcher can read live progress instead of waiting for the process to
/// exit. Line: `trace <pid> <step> <ms> <name>`.
pub fn trace(step: &str, ms: f64, name: &str) {
    let Some(path) = env_get("PLIRON_TRACE_FILE") else {
        return;
    };
    use std::io::Write;
    let line = format!("trace {} {step} {ms:.3} {name}\n", std::process::id());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `PLIRON_TRACE_FILE=<path>`: append `mark <pid> <t_ms> <label>` — a
/// wall-clock point event (ms since process start), for reconstructing
/// pipeline timelines that `trace`'s durations can't express.
/// The process's current physical footprint in MiB (macOS; 0 elsewhere).
pub fn footprint_mib() -> u64 {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
        }
        // rusage_info_v0: 16-byte uuid, then u64s; phys_footprint is the
        // eighth (user, system, pkg_idle, interrupt, pageins, wired,
        // resident, phys_footprint, ...).
        let mut buf = [0u64; 2 + 11];
        // SAFETY: a buffer as large as rusage_info_v0, flavor 0 (V0).
        if unsafe { proc_pid_rusage(std::process::id() as i32, 0, buf.as_mut_ptr()) } == 0 {
            return buf[2 + 7] >> 20;
        }
    }
    0
}

pub fn mark(label: &str) {
    let Some(path) = env_get("PLIRON_TRACE_FILE") else {
        return;
    };
    use std::io::Write;
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let t = T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1e3;
    // `PLIRON_MEM_MARK=1`: the process's current memory footprint too.
    let line = if env_set("PLIRON_MEM_MARK") {
        format!("mark {} {t:.3} {label} mem={}MiB\n", std::process::id(), footprint_mib())
    } else {
        format!("mark {} {t:.3} {label}\n", std::process::id())
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `PLIRON_REMARKS=<path>`: optimization remarks, one tab-separated line
/// per decision *not* to optimize (`<pass>\t<function>\t<what>\t<why>`),
/// appended to `<path>.<pid>`. Unlike the `PLIRON_TRACE` debug lines these
/// have one fixed shape, cost nothing when the knob is unset, and are
/// meant to be joined with a profile: `harness/remarks.py` answers "why
/// is this call still a call" / "why did this loop not vectorize" for the
/// functions a profile names (after relax's `remarks <fn>`).
pub fn remarks_on() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| env_get("PLIRON_REMARKS").is_some());
    *ON
}

/// Record one remark (see [`remarks_on`]; callers check it first so the
/// arguments are not formatted when off). `what` names the subject (a
/// callee, a loop header), `why` the reason with its numbers.
pub fn remark(pass: &str, func: &str, what: &dyn std::fmt::Display, why: &dyn std::fmt::Display) {
    use std::io::Write;
    static SINK: std::sync::LazyLock<Option<std::sync::Mutex<std::io::LineWriter<std::fs::File>>>> =
        std::sync::LazyLock::new(|| {
            let path = format!("{}.{}", env_get("PLIRON_REMARKS")?, std::process::id());
            let f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
            Some(std::sync::Mutex::new(std::io::LineWriter::new(f)))
        });
    if let Some(m) = SINK.as_ref()
        && let Ok(mut w) = m.lock()
    {
        let _ = writeln!(w, "{pass}\t{func}\t{what}\t{why}");
    }
}

/// `remark!(pass, func, what; "why {}", args)`: a remark when
/// `PLIRON_REMARKS` is set, nothing otherwise.
#[macro_export]
macro_rules! remark {
    ($pass:expr, $func:expr, $what:expr; $($t:tt)*) => {
        if $crate::remarks_on() {
            $crate::remark($pass, &*$func, &$what, &format_args!($($t)*));
        }
    };
}

/// `PLIRON_OPT_BISECT=N`: allow only the first N optimization-pass
/// applications (across all passes and functions), like LLVM's
/// `-opt-bisect-limit`. Each gate call consumes one unit; with no env var
/// the budget is unlimited. `PLIRON_OPT_BISECT_DEBUG=1` logs each gate.
static BISECT_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn bisect(name: &str) -> bool {
    bisect_in(name, "")
}

// r[impl verify.bisect.unit]
/// [`bisect`] for a unit inside function `func`, which the debug log names
/// (`bisect <n> <pass> run|skip <func>`) so `harness/bisect.py` can say
/// which function the first bad unit touched.
// r[impl par.cfg.bisect]
pub fn bisect_in(name: &str, func: &str) -> bool {
    let Some(limit) = bisect_limit() else {
        return true;
    };
    let n = BISECT_COUNT.fetch_add(1, Ordering::Relaxed);
    let ok = n < limit;
    if env_is("PLIRON_OPT_BISECT_DEBUG", "1")
        && tracing::enabled!(target: "pliron::cfg", tracing::Level::DEBUG)
    {
        let rss = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1).map(|x| x.to_string()))
            .unwrap_or_default();
        tracing::debug!(target: "pliron::cfg", "bisect {} {name} {} {func} rss={}",
            n + 1,
            if ok { "run" } else { "skip" },
            rss
        );
    }
    ok
}

/// The `PLIRON_OPT_BISECT` budget, parsed once. When `Some`, pass
/// applications must run in a deterministic order — the counter numbers
/// each `bisect` call, which is what makes bisection reproducible.
// r[impl par.det.numbering]
pub fn bisect_limit() -> Option<u64> {
    static LIMIT: LazyLock<Option<u64>> =
        LazyLock::new(|| env_get("PLIRON_OPT_BISECT").and_then(|v| v.parse().ok()));
    *LIMIT
}

/// Current bisect position and a way to replay skipped draws. A
/// whole-pipeline cache hit bypasses the per-step `bisect` calls, so it
/// advances the counter by the recorded amount to keep downstream
/// numbering identical to an uncached run.
pub fn bisect_count() -> u64 {
    BISECT_COUNT.load(Ordering::Relaxed)
}

pub fn bisect_skip(n: u64) {
    BISECT_COUNT.fetch_add(n, Ordering::Relaxed);
}

/// Stable 32-bit id of a symbol for `PLIRON_OPT_FNS` ranges: the high
/// half of its 64-bit FNV-1a hash. Unlike the `PLIRON_OPT_BISECT`
/// counter it depends on nothing but the name, so it agrees across
/// threads, CGUs, rustc processes and rebuilds — a whole bootstrap can
/// be bisected by function.
pub fn fn_hash(name: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h >> 32) as u32
}

enum FnSel {
    /// `lo..hi` over `fn_hash` (`hi` exclusive, `u32::MAX + 1` allowed).
    Range(u64, u64),
    /// `~substr`: names containing it.
    Sub(String),
    /// `@file`: exact names, one per line (`#` comments; a leading
    /// `<hash>\t` column, as `PLIRON_OPT_FNS_LOG` writes, is skipped).
    Names(rustc_hash::FxHashSet<String>),
}

fn parse_num(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

fn parse_fn_sel(spec: &str) -> Option<(bool, FnSel)> {
    let (neg, s) = match spec.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, spec),
    };
    let sel = if let Some(sub) = s.strip_prefix('~') {
        FnSel::Sub(sub.to_string())
    } else if let Some(path) = s.strip_prefix('@') {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("PLIRON_OPT_FNS=@{path}: {e}"));
        FnSel::Names(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.rsplit('\t').next().unwrap_or(l).to_string())
                .collect(),
        )
    } else {
        let (lo, hi) = s.split_once("..")?;
        FnSel::Range(parse_num(lo)?, parse_num(hi)?)
    };
    Some((neg, sel))
}

// r[impl verify.bisect.fn]
/// `PLIRON_OPT_FNS=<sel>`: optimize only the selected functions — the
/// rest keep their unoptimized body through the per-function pliron-IR
/// transforms and skip the CLIF pipeline. Interprocedural rewrites
/// (signature changes, dead-fn sweeps) still see every function, so a
/// mixed build stays consistent. `<sel>` is `lo..hi` over [`fn_hash`]
/// (decimal or 0x hex, `hi` exclusive; `0..0x100000000` is everything),
/// `~substr`, or `@file`; a leading `!` inverts. Unset selects all.
///
/// `PLIRON_OPT_FNS_LOG=<path>` appends `<hash>\t<name>` for every
/// selected function — `harness/fnbisect.py` reads it to name the
/// culprits once the range is narrow.
// r[impl par.cfg.opt-fns]
pub fn fn_opt_selected(name: &str) -> bool {
    static SEL: LazyLock<Option<(bool, FnSel)>> = LazyLock::new(|| {
        // An instrumented build (`PLIRON_PGO`) only has to count: its
        // counters sit on the pre-optimization CFG, so optimizing it buys
        // nothing but a faster profiling run, and costs a third of its
        // build time. `PLIRON_PGO_OPT=1` optimizes it anyway (a long
        // profiling workload).
        let spec = env_get("PLIRON_OPT_FNS").or_else(|| {
            (env_get("PLIRON_PGO").is_some() && !env_is("PLIRON_PGO_OPT", "1")).then_some("0..0")
        })?;
        Some(
            parse_fn_sel(spec)
                .unwrap_or_else(|| panic!("PLIRON_OPT_FNS: bad selector `{spec}`")),
        )
    });
    // Record the read so step-cache variants key on the selector.
    if reads_open() {
        let _ = env_get("PLIRON_OPT_FNS");
    }
    let Some((neg, sel)) = &*SEL else {
        return true;
    };
    let h = fn_hash(name);
    let hit = match sel {
        FnSel::Range(lo, hi) => (*lo..*hi).contains(&(h as u64)),
        FnSel::Sub(s) => name.contains(s.as_str()),
        FnSel::Names(set) => set.contains(name),
    };
    let on = hit != *neg;
    if on && let Some(path) = env_get("PLIRON_OPT_FNS_LOG") {
        use std::io::Write;
        // Every gated pass asks again; log each name once per process.
        static LOGGED: std::sync::Mutex<Option<rustc_hash::FxHashSet<String>>> =
            std::sync::Mutex::new(None);
        if !LOGGED
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .insert(name.to_string())
        {
            return on;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(format!("{h:08x}\t{name}\n").as_bytes());
        }
    }
    on
}

#[cfg(test)]
mod fn_sel_tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        // Pinned: bisect logs from older builds must stay comparable.
        assert_eq!(fn_hash(""), 0xcbf29ce4);
        assert_eq!(fn_hash("main"), fn_hash("main"));
        assert_ne!(fn_hash("main"), fn_hash("mains"));
    }

    // r[verify par.cfg.opt-fns]
    #[test]
    fn selectors_parse() {
        assert!(matches!(parse_fn_sel("0..0x100000000"), Some((false, FnSel::Range(0, 0x1_0000_0000)))));
        assert!(matches!(parse_fn_sel("!~drop"), Some((true, FnSel::Sub(s))) if s == "drop"));
        assert!(parse_fn_sel("12").is_none());
    }
}
