//! Datalog audit for `wpeep`'s copy-forward rewrite (`PLIRON_WASM_AUDIT`).
//!
//! # The idea: code and assumptions in one stream of facts
//!
//! A peephole is a claim — "this program means the same as that one" —
//! plus a side condition the pass believes makes it true. Bugs live in
//! the gap between the side condition the pass *checked* and the one the
//! rewrite *needs*. The copy-forward miscompile (dab149b) was exactly
//! that: the pass checked that the same *set* of definitions reaches
//! every read before and after, while the rewrite needed the same
//! definition on every *path*. Both programs passed the set check; one
//! path out of an `if` arm saw a different value.
//!
//! The audit doesn't re-run the pass's reasoning. It states the program
//! and the rewrite's assumptions as plain facts in one database and lets
//! a small, obviously-correct Datalog program decide:
//!
//! - **the code**, both versions at once: control-flow edges (`succ`,
//!   shared — the rewrite only renames locals and deletes copy pairs, so
//!   both programs have the same instruction positions), and per version
//!   the writes (`odef`/`ndef`, with the *value source* each write
//!   stores) and reads (`oread`/`nread`);
//! - **the assumptions**, emitted by the rewrite as it commits each
//!   candidate: "the copy-set carries the producer's value" (the source
//!   normalization below) and "the copy-get reads exactly the producer"
//!   (`assume_copy`).
//!
//! Then one rule family walks both programs down the *same* path in
//! lockstep (`flow`: at position `j`, local `lo` in the old program last
//! held source `o` while local `ln` in the new one held `n`) and flags
//! any read where they disagree. That product construction is the
//! path-sensitive check the pass approximated with sets, in four rules.
//! Assumptions are checked by the same machinery (`oflow`), so a wrong
//! assumption is as visible as a wrong rewrite.
//!
//! Why this shape is worth having:
//!
//! - **One checker, many rewrites.** Any rewrite that renames locals,
//!   deletes copies or moves writes between locals is audited by
//!   emitting its facts; nothing about copy-forward is hard-coded in the
//!   rules. A new peephole adds facts, not a new checker.
//! - **The assumptions become first-class.** A pass's private belief
//!   ("t is only read by the copy") is a row you can print, count and
//!   check — not a comment. When one fails, the report names it.
//! - **The checker is simple enough to trust.** Reaching definitions
//!   over a product state is textbook Datalog; it shares no code with
//!   the pass it audits, so a bug in the pass's analysis can't hide
//!   itself.
//! - **It runs as a guard.** On a violation the round is reverted (the
//!   body keeps the pre-round instructions), so debug builds become
//!   "verified peephole" builds rather than just noisy ones.
//!
//! Cost: one fixpoint per rewrite round over the tracked locals only
//! (`track`: pairs touched by the round). The product state can grow as
//! positions × (old sources × new sources) per pair, so like every other
//! fixpoint here it has a budget (`BUDGET`): a round whose bound exceeds
//! it is left unaudited and counted, never hung on (proc_macro's bridge
//! bodies took one rustc 28+ minutes before the budget existed).
//! Scope: local-variable dataflow. The operand stack isn't modelled —
//! copy-forward deletes `get t; set x` pairs, which are stack-neutral.

use super::WPeek;
use rustc_hash::FxHashSet;

/// Value source for a local's initial (param / zero) value.
const ENTRY: u32 = u32::MAX;

/// Upper bound on `flow` rows a round may produce before it is skipped:
/// positions × Σ over tracked pairs of (old sources + 1) × (new sources + 1).
const BUDGET: u64 = 4_000_000;

ascent::ascent! {
    struct Audit;
    relation entry(u32);
    relation succ(u32, u32);
    /// Local pairs (old local, new local) read at a common position.
    relation track(u32, u32);
    /// Old/new program writes `local` at `pos` with value source `src`.
    relation odef(u32, u32, u32);
    relation ndef(u32, u32, u32);
    relation okill(u32, u32);
    relation nkill(u32, u32);
    relation oread(u32, u32);
    relation nread(u32, u32);
    /// Locals whose old-program reaching sources are needed for
    /// assumption checks.
    relation ofocus(u32);
    /// Assumption: at `pos` the copy-get reads `local`, whose only
    /// reaching source is `src` (the producer).
    relation assume_copy(u32, u32, u32);

    okill(p, l) <-- odef(p, l, _);
    nkill(p, l) <-- ndef(p, l, _);

    /// Before `pos`, on some path: old `lo` holds `o`, new `ln` holds `n`.
    relation flow(u32, u32, u32, u32, u32);
    flow(e, lo, ln, ENTRY, ENTRY) <-- entry(e), track(lo, ln);
    flow(k, lo, ln, o2, n2) <-- flow(j, lo, ln, _o, _n), succ(j, k),
        odef(j, lo, o2), ndef(j, ln, n2);
    flow(k, lo, ln, o2, n) <-- flow(j, lo, ln, _o, n), succ(j, k),
        odef(j, lo, o2), !nkill(j, ln);
    flow(k, lo, ln, o, n2) <-- flow(j, lo, ln, o, _n), succ(j, k),
        !okill(j, lo), ndef(j, ln, n2);
    flow(k, lo, ln, o, n) <-- flow(j, lo, ln, o, n), succ(j, k),
        !okill(j, lo), !nkill(j, ln);

    /// A read that sees different value sources on the same path.
    relation bad_read(u32, u32, u32, u32, u32);
    bad_read(u, lo, ln, o, n) <-- oread(u, lo), nread(u, ln),
        flow(u, lo, ln, o, n), if o != n;

    /// Old-program reaching sources, for checking assumptions.
    relation oflow(u32, u32, u32);
    oflow(e, l, ENTRY) <-- entry(e), ofocus(l);
    oflow(k, l, s) <-- oflow(j, l, _s0), succ(j, k), odef(j, l, s);
    oflow(k, l, s) <-- oflow(j, l, s), succ(j, k), !okill(j, l);

    relation bad_assume(u32, u32, u32);
    bad_assume(p, l, s) <-- assume_copy(p, l, want), oflow(p, l, s), if s != want;
}

/// Control-flow successors over instruction positions of a decoded body.
/// Structured wasm: `br L` targets the L-th enclosing construct — a
/// `loop` re-enters at its head, `block`/`if` continue after `end`; an
/// `if` either falls into its then-arm or jumps past its `else`; the
/// `else` marker is only reached by falling off the then-arm and jumps
/// to the `end`; `return`/`unreachable` and branches past the body
/// leave the function.
pub(super) fn cfg(b: &[u8], ins: &[(WPeek, usize, usize, usize)]) -> Option<Vec<(u32, u32)>> {
    let op = |i: usize| b[ins[i].1];
    let ctl = |i: usize| matches!(ins[i].0, WPeek::Ctl);
    // Match constructs: for each opener, its `else` (if) and `end`.
    let n = ins.len();
    let mut end_of = vec![usize::MAX; n];
    let mut else_of = vec![usize::MAX; n];
    let mut st: Vec<usize> = Vec::new();
    for i in 0..n {
        if !ctl(i) {
            continue;
        }
        match op(i) {
            0x02 | 0x03 | 0x04 => st.push(i),
            0x05 => *else_of.get_mut(*st.last()?)? = i,
            0x0b => {
                if let Some(o) = st.pop() {
                    end_of[o] = i;
                    if else_of[o] != usize::MAX {
                        end_of[else_of[o]] = i;
                    }
                }
            }
            _ => {}
        }
    }
    let mut edges = Vec::new();
    let mut st: Vec<usize> = Vec::new();
    // Branch target for label depth `l` given the open-construct stack.
    let target = |st: &[usize], l: usize| -> Option<usize> {
        let o = *st.get(st.len().checked_sub(1 + l)?)?;
        Some(if op(o) == 0x03 { o } else { end_of[o] })
    };
    let lbl = |i: usize| -> Option<usize> {
        let mut q = ins[i].1 + 1;
        super::urd(b, &mut q).map(|l| l as usize)
    };
    for i in 0..n {
        let next = (i + 1 < n).then_some(i + 1);
        let mut push = |t: Option<usize>| {
            if let Some(t) = t {
                edges.push((i as u32, t as u32));
            }
        };
        if !ctl(i) {
            push(next);
            continue;
        }
        match op(i) {
            0x02 | 0x03 => {
                st.push(i);
                push(next);
            }
            0x04 => {
                st.push(i);
                push(next);
                push(Some(if else_of[i] != usize::MAX { else_of[i] + 1 } else { end_of[i] }));
            }
            0x05 => push(Some(end_of[i])),
            0x0b => {
                st.pop();
                push(next);
            }
            0x0c => push(target(&st, lbl(i)?)),
            0x0d => {
                push(target(&st, lbl(i)?));
                push(next);
            }
            0x0e => {
                let mut q = ins[i].1 + 1;
                let k = super::urd(b, &mut q)?;
                for _ in 0..=k {
                    let l = super::urd(b, &mut q)? as usize;
                    push(target(&st, l));
                }
            }
            0x00 | 0x0f => {}
            _ => push(next), // calls fall through
        }
    }
    Some(edges)
}

/// One committed copy-forward candidate, as the rewrite saw it.
pub(super) struct Claim {
    pub p1: usize,
    pub gi: usize,
    pub si: usize,
    pub t: u32,
    pub x: u32,
}

/// Check one rewrite round: `old` and `new` are the instruction streams
/// before and after it (same positions; deleted entries are `Del`).
/// Returns human-readable violations (empty: the round preserved every
/// read's value on every path and every assumption held), or `None` when
/// the round is over `BUDGET` and was not audited.
pub(super) fn audit(
    b: &[u8],
    old: &[(WPeek, usize, usize, usize)],
    new: &[(WPeek, usize, usize, usize)],
    claims: &[Claim],
) -> Option<Vec<String>> {
    let Some(edges) = cfg(b, old) else {
        return Some(vec!["audit: could not build the CFG".into()]);
    };
    let mut a = Audit::default();
    a.entry.push((0,));
    a.succ = edges.into_iter().collect();
    // Assumption: each copy-set stores the producer's value — both
    // programs name it by the producer's position.
    let norm: rustc_hash::FxHashMap<usize, usize> =
        claims.iter().map(|c| (c.si, c.p1)).collect();
    let focus: FxHashSet<u32> = claims.iter().flat_map(|c| [c.t, c.x]).collect();
    let write = |k: WPeek| match k {
        WPeek::Set(l) | WPeek::Tee(l) => Some(l),
        _ => None,
    };
    let read = |k: WPeek| match k {
        WPeek::Get(l) | WPeek::Nop(l) => Some(l),
        _ => None,
    };
    for (i, (o, n)) in old.iter().zip(new).enumerate() {
        let src = *norm.get(&i).unwrap_or(&i) as u32;
        if let Some(l) = write(o.0) {
            a.odef.push((i as u32, l, src));
        }
        if let Some(l) = write(n.0) {
            a.ndef.push((i as u32, l, i as u32));
        }
        if let (Some(lo), Some(ln)) = (read(o.0), read(n.0)) {
            a.oread.push((i as u32, lo));
            a.nread.push((i as u32, ln));
            if focus.contains(&lo) || focus.contains(&ln) {
                a.track.push((lo, ln));
            }
        }
    }
    for c in claims {
        a.ofocus.push((c.t,));
        a.assume_copy.push((c.gi as u32, c.t, c.p1 as u32));
    }
    let mut srcs: rustc_hash::FxHashMap<(bool, u32), u64> = Default::default();
    for &(_, l, _) in &a.odef {
        *srcs.entry((false, l)).or_default() += 1;
    }
    for &(_, l, _) in &a.ndef {
        *srcs.entry((true, l)).or_default() += 1;
    }
    let pairs: FxHashSet<(u32, u32)> = a.track.iter().copied().collect();
    let per_pos: u64 = pairs
        .iter()
        .map(|&(lo, ln)| {
            (srcs.get(&(false, lo)).copied().unwrap_or(0) + 1)
                * (srcs.get(&(true, ln)).copied().unwrap_or(0) + 1)
        })
        .sum();
    if per_pos.saturating_mul(old.len() as u64) > BUDGET {
        return None;
    }
    a.run();
    let at = |p: u32| old.get(p as usize).map_or(0, |e| e.1);
    let src = |s: u32| if s == ENTRY { "entry".to_string() } else { format!("@{:#x}", at(s)) };
    let mut out: Vec<String> = a
        .bad_read
        .iter()
        .map(|&(u, lo, ln, o, n)| {
            format!(
                "read @{:#x}: old local {lo} sees {} but new local {ln} sees {} on the same path",
                at(u),
                src(o),
                src(n)
            )
        })
        .collect();
    out.extend(a.bad_assume.iter().map(|&(p, l, s)| {
        format!("assumption broken @{:#x}: copy-get of local {l} also sees {}", at(p), src(s))
    }));
    out.sort();
    out.dedup();
    Some(out)
}
