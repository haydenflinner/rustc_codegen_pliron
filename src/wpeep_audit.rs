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
//!   the writes (`owr`/`nwr`, with the *value source* each write
//!   stores) and the reads (`sread`: same local in both; `rread`: a read
//!   the rewrite redirected to another local);
//! - **the assumptions**, emitted by the rewrite as it commits each
//!   candidate: "the copy-set carries the producer's value" (the source
//!   normalization below) and "the copy-get reads exactly the producer"
//!   (`assume_copy`).
//!
//! Then the rules walk both programs down the *same* path in lockstep and
//! flag any read where they disagree — the path-sensitive check the pass
//! approximated with sets. Assumptions are checked by the same machinery
//! (`cb` → `bad_assume`), so a wrong assumption is as visible as a wrong
//! rewrite.
//!
//! # Evaluation: only where the programs can differ
//!
//! The two programs are identical except at the rewritten instructions,
//! so almost every (position, local) pair trivially agrees. The rules
//! store only what can differ and only compute what a check asks for:
//!
//! - **Disagreements only** (`dis`). Agreement is the implicit default
//!   for a local the rewrite touched; a `dis(pos, l, o, n)` row exists
//!   only on paths where old `l` holds source `o` and new `l` holds `n`
//!   with `o != n`. Rows start at rewritten writes and die at the next
//!   write both programs make identically.
//! - **Demand-driven agreement** (`dem` → `agree_in`). Where only one
//!   program writes `l`, the result depends on whether the two agreed on
//!   the way in. That is asked by a backward walk from the write that
//!   stops at the nearest identical write — local, not whole-function.
//! - **Renamed reads** (old reads `t`, new reads `x`) are backward
//!   queries too (`rb`): from the read to the nearest write of either.
//! - **Block at a time.** The CFG is contracted to nodes that matter:
//!   control instructions and instructions touching a tracked local.
//!   Straight-line runs that touch nothing tracked collapse away.
//!
//! Sources are static positions, so a value from an earlier loop
//! iteration would look like a fresh one from the same producer. A
//! held source becomes `STALE` when its producer runs again, and markers
//! (`STALE`, `PREV` — "the value both agreed on before this write")
//! never compare equal to anything.
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
//! Cost: proportional to the regions around the rewrites, not to the
//! function (the first version tracked every source pair from the entry
//! and held a rustc 28+ minutes on a 1M-instruction proc_macro body).
//! A budget on the contracted graph (`BUDGET`) stays as a safety net.
//! Scope: local-variable dataflow. The operand stack isn't modelled —
//! copy-forward deletes `get t; set x` pairs, which are stack-neutral.

use super::WPeek;
use rustc_hash::FxHashSet;

/// Marker sources (never equal to anything, including themselves).
const STALE: u32 = u32::MAX - 1;
const PREV: u32 = u32::MAX - 2;

/// Skip rounds whose contracted graph × tracked locals exceeds this.
const BUDGET: u64 = 50_000_000;

fn stale(v: u32, j: u32) -> u32 {
    if v == j { STALE } else { v }
}

ascent::ascent! {
    struct Audit;
    relation entry(u32);
    relation succ(u32, u32);
    /// Old / new program writes local `l` at node `j` with source `s`.
    relation owr(u32, u32, u32);
    relation nwr(u32, u32, u32);
    relation o_any(u32, u32);
    relation n_any(u32, u32);
    /// Read of the same tracked local in both programs.
    relation sread(u32, u32);
    /// Read renamed by the rewrite: old reads `lo`, new reads `ln`.
    relation rread(u32, u32, u32);
    /// Assumption: the copy-get at `g` reads `t`, whose only reaching
    /// write is the producer `p`.
    relation assume_copy(u32, u32, u32);

    o_any(j, l) <-- owr(j, l, _);
    n_any(j, l) <-- nwr(j, l, _);

    relation both_eq(u32, u32);
    both_eq(j, l) <-- owr(j, l, s), nwr(j, l, s2), if s == s2;
    relation oonly(u32, u32, u32);
    oonly(j, l, s) <-- owr(j, l, s), !n_any(j, l);
    relation nonly(u32, u32, u32);
    nonly(j, l, s) <-- nwr(j, l, s), !o_any(j, l);

    // ---- same-local disagreements (forward, only where they exist) ----
    relation dis(u32, u32, u32, u32);
    dis(k, l, so, sn) <-- owr(j, l, so), nwr(j, l, sn), if so != sn, succ(j, k);
    dis(k, l, so, n2) <-- oonly(j, l, so), dis(j, l, _o, n), let n2 = stale(*n, *j),
        if *so != n2, succ(j, k);
    dis(k, l, o2, sn) <-- nonly(j, l, sn), dis(j, l, o, _n), let o2 = stale(*o, *j),
        if o2 != *sn, succ(j, k);
    dis(k, l, so, PREV) <-- oonly(j, l, so), agree_in(j, l), succ(j, k);
    dis(k, l, PREV, sn) <-- nonly(j, l, sn), agree_in(j, l), succ(j, k);
    dis(k, l, o2, n2) <-- dis(j, l, o, n), !o_any(j, l), !n_any(j, l), succ(j, k),
        let o2 = stale(*o, *j), let n2 = stale(*n, *j);

    /// A one-sided write that makes a disagreement agree again.
    relation reconv(u32, u32);
    reconv(j, l) <-- oonly(j, l, so), dis(j, l, _o, n), if stale(*n, *j) == *so;
    reconv(j, l) <-- nonly(j, l, sn), dis(j, l, o, _n), if stale(*o, *j) == *sn;

    // ---- demand-driven agreement before a one-sided write ----
    relation dem(u32, u32);
    dem(j, l) <-- oonly(j, l, _);
    dem(j, l) <-- nonly(j, l, _);
    dem(p, l) <-- dem(j, l), succ(p, j), !o_any(p, l), !n_any(p, l);
    relation agree_in(u32, u32);
    agree_in(j, l) <-- dem(j, l), entry(j);
    agree_in(j, l) <-- dem(j, l), succ(p, j), both_eq(p, l);
    agree_in(j, l) <-- dem(j, l), succ(p, j), reconv(p, l);
    agree_in(j, l) <-- dem(j, l), succ(p, j), !o_any(p, l), !n_any(p, l), agree_in(p, l);

    relation bad_read(u32, u32, u32, u32);
    bad_read(u, l, o, n) <-- sread(u, l), dis(u, l, o, n);

    // ---- renamed reads: backward to the nearest write of either ----
    relation agen(u32, u32, u32);
    agen(j, lo, ln) <-- rread(_u, lo, ln), owr(j, lo, s), nwr(j, ln, s2), if s == s2;
    relation dgen(u32, u32, u32);
    dgen(j, lo, ln) <-- rread(_u, lo, ln), owr(j, lo, _), !agen(j, lo, ln);
    dgen(j, lo, ln) <-- rread(_u, lo, ln), nwr(j, ln, _), !agen(j, lo, ln);
    relation rb(u32, u32, u32, u32);
    rb(p, u, lo, ln) <-- rread(u, lo, ln), succ(p, u);
    rb(q, u, lo, ln) <-- rb(p, u, lo, ln), !agen(p, lo, ln), !dgen(p, lo, ln), succ(q, p);
    relation bad_rread(u32, u32, u32, u32);
    bad_rread(u, lo, ln, p) <-- rb(p, u, lo, ln), dgen(p, lo, ln);
    bad_rread(u, lo, ln, p) <-- rb(p, u, lo, ln), entry(p), !agen(p, lo, ln), !dgen(p, lo, ln);
    bad_rread(u, lo, ln, u) <-- rread(u, lo, ln), entry(u);

    // ---- assumption: the copy-get reads exactly the producer ----
    relation cb(u32, u32, u32);
    cb(p, g, t) <-- assume_copy(g, t, _), succ(p, g);
    cb(q, g, t) <-- cb(p, g, t), !o_any(p, t), succ(q, p);
    relation bad_assume(u32, u32, u32);
    bad_assume(g, t, s) <-- cb(p, g, t), owr(p, t, s), assume_copy(g, t, want), if s != want;
    bad_assume(g, t, PREV) <-- cb(p, g, t), entry(p), !o_any(p, t);
    bad_assume(g, t, PREV) <-- assume_copy(g, t, _), entry(g);
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
    let focus: FxHashSet<u32> = claims.iter().flat_map(|c| [c.t, c.x]).collect();
    // The copy-set stores the producer's value: name it by the producer.
    let norm: rustc_hash::FxHashMap<usize, usize> =
        claims.iter().map(|c| (c.si, c.p1)).collect();
    let write = |k: WPeek| match k {
        WPeek::Set(l) | WPeek::Tee(l) => Some(l),
        _ => None,
    };
    let read = |k: WPeek| match k {
        WPeek::Get(l) | WPeek::Nop(l) => Some(l),
        _ => None,
    };
    let touches = |k: WPeek| write(k).or(read(k)).is_some_and(|l| focus.contains(&l));
    let structural = |i: usize| {
        matches!(old[i].0, WPeek::Ctl)
            && matches!(b[old[i].1], 0x00 | 0x02 | 0x03 | 0x04 | 0x05 | 0x0b | 0x0c | 0x0d | 0x0e | 0x0f)
    };
    let n = old.len();
    // Contract: every position maps to the next node that matters.
    let node = |i: usize| structural(i) || touches(old[i].0) || touches(new[i].0);
    let mut nxt = vec![usize::MAX; n + 1];
    for i in (0..n).rev() {
        nxt[i] = if node(i) { i } else { nxt[i + 1] };
    }
    let mut a = Audit::default();
    let mut nodes = 0u64;
    for &(f, t) in &edges {
        let (f, t) = (f as usize, t as usize);
        if node(f) && nxt[t] != usize::MAX {
            a.succ.push((f as u32, nxt[t] as u32));
        }
    }
    if nxt[0] != usize::MAX {
        a.entry.push((nxt[0] as u32,));
    }
    for i in (0..n).filter(|&i| node(i)) {
        nodes += 1;
        let (o, w) = (old[i].0, new[i].0);
        if let Some(l) = write(o).filter(|l| focus.contains(l)) {
            a.owr.push((i as u32, l, *norm.get(&i).unwrap_or(&i) as u32));
        }
        if let Some(l) = write(w).filter(|l| focus.contains(l)) {
            a.nwr.push((i as u32, l, i as u32));
        }
        if let (Some(lo), Some(ln)) = (read(o), read(w))
            && (focus.contains(&lo) || focus.contains(&ln))
        {
            if lo == ln {
                a.sread.push((i as u32, lo));
            } else {
                a.rread.push((i as u32, lo, ln));
            }
        }
    }
    if nodes.saturating_mul(focus.len() as u64 + 1) > BUDGET {
        return None;
    }
    for c in claims {
        a.assume_copy.push((c.gi as u32, c.t, c.p1 as u32));
    }
    let t0 = std::time::Instant::now();
    a.run();
    if n > 100_000 && pliron_cfg::tracing_on() {
        tracing::debug!(target: "pliron::wasm",
            "wpeep audit: {n} insts, {nodes} nodes, {} dis rows, {:.1?}", a.dis.len(), t0.elapsed());
    }
    let at = |p: u32| old.get(p as usize).map_or(0, |e| e.1);
    let src = |s: u32| match s {
        STALE => "a stale (earlier-iteration) value".to_string(),
        PREV => "the value from before the rewrite".to_string(),
        s => format!("the write @{:#x}", at(s)),
    };
    let mut out: Vec<String> = a
        .bad_read
        .iter()
        .map(|&(u, l, o, nn)| {
            format!("read of local {l} @{:#x}: old program sees {}, new sees {}", at(u), src(o), src(nn))
        })
        .collect();
    out.extend(a.bad_rread.iter().map(|&(u, lo, ln, p)| {
        format!(
            "renamed read @{:#x} (old local {lo}, new local {ln}): a path from @{:#x} reaches it with the two disagreeing",
            at(u),
            at(p)
        )
    }));
    out.extend(a.bad_assume.iter().map(|&(g, l, s)| {
        format!("assumption broken @{:#x}: copy-get of local {l} can see {}", at(g), src(s))
    }));
    out.sort();
    out.dedup();
    Some(out)
}
