//! Fact-audited unused-`block`/`loop` elision for `wpeep`.
//!
//! # Claim
//!
//! A `block`/`loop` + `end` pair whose label is never a branch target
//! may be deleted; every `br`/`br_if` inside whose label pointed *past*
//! the construct (label > depth) becomes `label - 1`. Fallthrough and
//! every other label still name the same construct. An untargeted
//! `loop` is just a `block` nobody continues into — same delete.
//!
//! # Assumptions (emitted as facts, checked before commit)
//!
//! - `assume_untargeted(bi)`: no `br`/`br_if`/`br_table` in `(bi, ei)`
//!   has label equal to its depth relative to `bi` (wasm: that would
//!   target this block's `end` / this loop's head).
//! - `assume_empty_type(bi)`: the construct's blocktype is empty
//!   (`0x40`) — result-typed frames are left alone (eliding them would
//!   drop values).
//! - `assume_wellformed(after)`: after the delete + label fix, the
//!   control stack still parses (no `else` outside `if`, no extra
//!   `end`, depth never negative). This is what a earlier un-audited
//!   cut violated.
//!
//! The checker shares no analysis with the candidate picker beyond the
//! raw instruction bytes: untargeted is re-derived from the bytes, and
//! wellformedness is a separate walk. On any failed fact the body is
//! restored (guard, not just an alarm).

use super::WPeek;

/// One elision the rewrite wants to commit.
#[derive(Clone, Debug)]
pub(super) struct ElideClaim {
    pub bi: usize,
    pub ei: usize,
}

/// Control-stack wellformedness of a decoded body (skips `Del`/`Nop`).
///
/// A wasm function body is `locals*; expr; end` — the trailing `end`
/// closes an implicit function frame, not a `block`/`loop`/`if`. We
/// seed the stack with that frame so the final `end` is expected.
pub(super) fn wellformed(b: &[u8], ins: &[(WPeek, usize, usize, usize)]) -> Result<(), String> {
    #[derive(Clone, Copy)]
    enum Frame {
        Func,
        Block,
        Loop,
        If { seen_else: bool },
    }
    let mut st: Vec<Frame> = vec![Frame::Func];
    for &(k, s, _, _) in ins {
        if matches!(k, WPeek::Del | WPeek::Nop(_)) {
            continue;
        }
        // Only `Ctl` opens/closes frames. Synthesized `Br`/`BrIf` keep a
        // byte span that may still point at the old `if`/`block` they
        // replaced — reading `b[s]` for those would double-count.
        if !matches!(k, WPeek::Ctl) {
            continue;
        }
        match b.get(s).copied() {
            Some(0x02) => st.push(Frame::Block),
            Some(0x03) => st.push(Frame::Loop),
            Some(0x04) => st.push(Frame::If { seen_else: false }),
            Some(0x05) => match st.last_mut() {
                Some(Frame::If { seen_else }) if !*seen_else => *seen_else = true,
                Some(Frame::If { .. }) => {
                    return Err("second `else` in an `if`".into());
                }
                _ => return Err("`else` found outside `if`".into()),
            },
            Some(0x0b) => {
                if st.pop().is_none() {
                    return Err("extra `end`".into());
                }
            }
            _ => {}
        }
    }
    if !st.is_empty() {
        return Err(format!("{} unclosed construct(s)", st.len()));
    }
    Ok(())
}

fn branch_label(b: &[u8], k: WPeek, s: usize) -> Option<(u32, bool)> {
    match k {
        WPeek::Br(d) => Some((d, false)),
        WPeek::BrIf(d) => Some((d, true)),
        WPeek::Ctl => match b.get(s).copied()? {
            0x0c | 0x0d => {
                let mut p = s + 1;
                let d = super::urd(b, &mut p)?;
                Some((d, b[s] == 0x0d))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Re-derive `assume_untargeted`: no branch in `(bi, ei)` targets depth 0
/// relative to the block body.
pub(super) fn untargeted(
    b: &[u8],
    ins: &[(WPeek, usize, usize, usize)],
    bi: usize,
    ei: usize,
) -> Result<(), String> {
    let mut depth: i32 = 0;
    let mut i = bi + 1;
    while i < ei {
        let (k, s, _, _) = ins[i];
        if matches!(k, WPeek::Del | WPeek::Nop(_)) {
            i += 1;
            continue;
        }
        // Kind first: synthesized Br/BrIf may still span an old `if`/`block`
        // byte. Control depth only moves on live `Ctl`.
        match k {
            WPeek::Br(lab) | WPeek::BrIf(lab) => {
                if lab as i32 == depth {
                    return Err(format!(
                        "assume_untargeted broken: br {lab} at depth {depth} targets the block"
                    ));
                }
            }
            WPeek::Ctl => match b.get(s).copied().unwrap_or(0) {
                0x02 | 0x03 | 0x04 => depth += 1,
                0x0b => depth -= 1,
                0x0c | 0x0d => {
                    let (lab, _) = branch_label(b, k, s)
                        .ok_or_else(|| format!("br @{s:#x}: bad label"))?;
                    if lab as i32 == depth {
                        return Err(format!(
                            "assume_untargeted broken: br {lab} at depth {depth} targets the block"
                        ));
                    }
                }
                0x0e => {
                    return Err("assume_untargeted: br_table in body (not rewritten)".into());
                }
                _ => {}
            },
            _ => {}
        }
        if depth < 0 {
            return Err("assume_untargeted: depth went negative (bad pair)".into());
        }
        i += 1;
    }
    if depth != 0 {
        return Err(format!(
            "assume_untargeted: depth {depth} at end (block/end mismatch)"
        ));
    }
    Ok(())
}

fn empty_blocktype(b: &[u8], s: usize, e: usize) -> bool {
    // `block`/`loop` + blocktype; empty is the single byte `0x40`.
    e > s + 1 && matches!(b[s], 0x02 | 0x03) && b[s + 1] == 0x40 && e == s + 2
}

/// Check every claim's assumptions against `ins` (pre- or post- fixup
/// for untargeted; post for wellformed).
pub(super) fn audit_claims(
    b: &[u8],
    before: &[(WPeek, usize, usize, usize)],
    after: &[(WPeek, usize, usize, usize)],
    claims: &[ElideClaim],
) -> Vec<String> {
    let mut bad = Vec::new();
    for c in claims {
        if c.bi >= before.len() || c.ei >= before.len() || c.bi >= c.ei {
            bad.push(format!("claim ({}, {}): out of range", c.bi, c.ei));
            continue;
        }
        if !empty_blocktype(b, before[c.bi].1, before[c.bi].2) {
            bad.push(format!(
                "assume_empty_type broken @{:#x}",
                before[c.bi].1
            ));
        }
        if let Err(e) = untargeted(b, before, c.bi, c.ei) {
            bad.push(e);
        }
    }
    if let Err(e) = wellformed(b, after) {
        bad.push(format!("assume_wellformed broken: {e}"));
    }
    // Pre-state must have been wellformed too — otherwise we shouldn't
    // have tried (guards against auditing garbage).
    if let Err(e) = wellformed(b, before) {
        bad.push(format!("pre-state not wellformed: {e}"));
    }
    bad
}

/// Pick elidable `block`/`end` pairs using a structured control stack
/// (same matching rules as `wpeep_audit::cfg`).
pub(super) fn candidates(
    b: &[u8],
    ins: &[(WPeek, usize, usize, usize)],
) -> Vec<ElideClaim> {
    let n = ins.len();
    let mut end_of = vec![usize::MAX; n];
    let mut st: Vec<usize> = Vec::new();
    for i in 0..n {
        if matches!(ins[i].0, WPeek::Del | WPeek::Nop(_)) {
            continue;
        }
        if !matches!(ins[i].0, WPeek::Ctl) {
            continue;
        }
        match b.get(ins[i].1).copied() {
            Some(0x02) | Some(0x03) | Some(0x04) => st.push(i),
            Some(0x05) => {}
            Some(0x0b) => {
                if let Some(o) = st.pop() {
                    end_of[o] = i;
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for bi in 0..n {
        if !matches!(ins[bi].0, WPeek::Ctl) {
            continue;
        }
        if !matches!(b.get(ins[bi].1), Some(0x02 | 0x03)) {
            continue;
        }
        let ei = end_of[bi];
        if ei == usize::MAX || ei <= bi {
            continue;
        }
        if !empty_blocktype(b, ins[bi].1, ins[bi].2) {
            continue;
        }
        if untargeted(b, ins, bi, ei).is_ok() {
            out.push(ElideClaim { bi, ei });
        }
    }
    out
}

/// Apply label fixups for one elided block; does not delete yet.
fn retarget_past(
    b: &[u8],
    ins: &mut [(WPeek, usize, usize, usize)],
    bi: usize,
    ei: usize,
) {
    let mut depth: i32 = 0;
    let mut j = bi + 1;
    while j < ei {
        let (k, s, _, _) = ins[j];
        if matches!(k, WPeek::Del | WPeek::Nop(_)) {
            j += 1;
            continue;
        }
        match k {
            WPeek::Br(lab) | WPeek::BrIf(lab) => {
                if lab as i32 > depth {
                    let nd = (lab - 1) as u32;
                    ins[j].0 = if matches!(k, WPeek::BrIf(_)) {
                        WPeek::BrIf(nd)
                    } else {
                        WPeek::Br(nd)
                    };
                }
            }
            WPeek::Ctl => match b.get(s).copied().unwrap_or(0) {
                0x02 | 0x03 | 0x04 => depth += 1,
                0x0b => depth -= 1,
                0x0c | 0x0d => {
                    if let Some((lab, is_br_if)) = branch_label(b, k, s) {
                        if lab as i32 > depth {
                            let nd = (lab - 1) as u32;
                            ins[j].0 = if is_br_if {
                                WPeek::BrIf(nd)
                            } else {
                                WPeek::Br(nd)
                            };
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        j += 1;
    }
}

/// Run one audited elision round. Returns how many pairs were removed.
/// On audit failure the round is reverted (`ins` restored).
// r[impl wasm.audit.elide]
pub(super) fn elide_round(
    b: &[u8],
    ins: &mut Vec<(WPeek, usize, usize, usize)>,
    audit_panic: bool,
    tag: &str,
) -> u32 {
    let claims = candidates(b, ins);
    if claims.is_empty() {
        return 0;
    }
    // Innermost first so indices of outer pairs stay valid after Del
    // (we don't remove until the end of the round; overlapping pairs
    // share ends only when nested — process inner→outer by ei-bi span).
    let mut claims = claims;
    claims.sort_by_key(|c| c.ei - c.bi);

    let before = ins.clone();
    let mut applied: Vec<ElideClaim> = Vec::new();
    for c in &claims {
        // Skip if an earlier (inner) claim already deleted this frame.
        if matches!(ins[c.bi].0, WPeek::Del) || matches!(ins[c.ei].0, WPeek::Del) {
            continue;
        }
        // Re-check against the *current* stream (inner elisions may
        // have changed depths — only apply if still untargeted).
        if untargeted(b, ins, c.bi, c.ei).is_err() {
            continue;
        }
        retarget_past(b, &mut ins[..], c.bi, c.ei);
        ins[c.bi].0 = WPeek::Del;
        ins[c.ei].0 = WPeek::Del;
        applied.push(c.clone());
    }
    if applied.is_empty() {
        return 0;
    }
    let bad = audit_claims(b, &before, ins, &applied);
    if !bad.is_empty() {
        if audit_panic {
            panic!(
                "wpeep elide audit: rejected in {tag}\n  {}",
                bad.join("\n  ")
            );
        }
        tracing::debug!(
            target: "pliron::wasm",
            "wpeep elide audit: reverted {} claim(s) in {tag}: {}",
            applied.len(),
            bad.join("; ")
        );
        *ins = before;
        return 0;
    }
    let n = applied.len() as u32;
    ins.retain(|&(k, _, _, _)| !matches!(k, WPeek::Del));
    n
}

/// Delete unreachable code in one forward pass, the companion of
/// `elide_round` (`PLIRON_WASM_DEADTAIL=1`).
///
/// Why it matters (N9 #5): `elide_round` deletes an untargeted block whose
/// body always branches away, and the instruction after its old `end`
/// (waffle emits a `br` there) is dead code that still names an outer
/// block as its target. That block then looks targeted and is kept;
/// wasm-bindgen's re-encoding drops the dead `br` and leaves the block
/// untargeted in the shipped module (67k such blocks in dsa). In a nest
/// each level hides the next, so alternating the two rounds would need
/// one round per level.
///
/// The pass tracks reachability with a control stack. Code is dead after
/// an unconditional `br`, `br_table`, `return`, `unreachable` or tail call,
/// and stays dead through the `end` of a construct `elide_round` will
/// delete ("transparent": a `block`/`loop` with the empty blocktype, no
/// branch from live code to it and no `br_table` inside) when the body's
/// end is dead. At any other `end`/`else` of a live construct it is live
/// again, as the validator sees it. Branches in dead code target nothing.
/// Dead constructs go whole.
///
/// Typing: once every transparent construct is elided, each deleted
/// instruction follows an unconditional transfer in the same construct
/// with only deleted code between, where wasm validates against a
/// polymorphic stack and an empty sequence is valid for any block type.
/// So the caller must check that `elide_round` removed every opener this
/// returns (their byte offsets) and revert the whole body otherwise. With
/// `transparent_ok` false nothing is transparent and the cut is valid on
/// its own.
///
/// `None` (nothing changed): a branch label out of range, or a deleted
/// instruction anchors a `local.get` prelude (`is_anchor`).
pub(super) fn dead_exact(
    b: &[u8],
    ins: &mut Vec<(WPeek, usize, usize, usize)>,
    is_anchor: impl Fn(usize) -> bool,
    transparent_ok: bool,
) -> Option<(u32, Vec<usize>)> {
    struct Fr {
        kind: u8,
        empty: bool,
        tgt: bool,
        bt: bool,
        dead_open: bool,
        s: usize,
    }
    let live = |k: WPeek| !matches!(k, WPeek::Del | WPeek::Nop(_));
    let byte = |k: WPeek, s: usize| -> Option<u8> {
        matches!(k, WPeek::Ctl).then(|| b.get(s).copied()).flatten()
    };
    let mut st: Vec<Fr> = vec![Fr {
        kind: 0,
        empty: false,
        tgt: false,
        bt: false,
        dead_open: false,
        s: usize::MAX,
    }];
    let mut dead = false;
    let mut del: Vec<usize> = Vec::new();
    let mut must: Vec<usize> = Vec::new();
    for i in 0..ins.len() {
        let (k, s, e, _) = ins[i];
        if !live(k) {
            continue;
        }
        let op = byte(k, s);
        match op {
            Some(0x02 | 0x03 | 0x04) => {
                if dead {
                    del.push(i);
                }
                st.push(Fr {
                    kind: b[s],
                    empty: empty_blocktype(b, s, e),
                    tgt: false,
                    bt: false,
                    dead_open: dead,
                    s,
                });
                continue;
            }
            Some(0x05) => {
                if st.last()?.dead_open {
                    del.push(i);
                } else {
                    dead = false;
                }
                continue;
            }
            Some(0x0b) => {
                if st.len() == 1 {
                    break; // the body's own `end`
                }
                let fr = st.pop()?;
                if fr.dead_open {
                    del.push(i);
                    continue;
                }
                if fr.bt {
                    st.last_mut()?.bt = true;
                }
                let transparent = transparent_ok
                    && matches!(fr.kind, 0x02 | 0x03)
                    && fr.empty
                    && !fr.tgt
                    && !fr.bt;
                if transparent {
                    if dead {
                        must.push(fr.s);
                    }
                } else {
                    dead = false;
                }
                continue;
            }
            _ => {}
        }
        if dead {
            del.push(i);
            continue;
        }
        let n = st.len();
        let mark = |d: u32, st: &mut Vec<Fr>| -> Option<()> {
            let d = d as usize;
            if d >= n {
                return None;
            }
            st[n - 1 - d].tgt = true;
            Some(())
        };
        match (k, op) {
            (WPeek::Br(d), _) => {
                mark(d, &mut st)?;
                dead = true;
            }
            (WPeek::BrIf(d), _) => mark(d, &mut st)?,
            (_, Some(0x0c | 0x0d)) => {
                let (d, is_if) = branch_label(b, k, s)?;
                mark(d, &mut st)?;
                dead = !is_if;
            }
            (_, Some(0x0e)) => {
                let mut p = s + 1;
                let cnt = super::urd(b, &mut p)?;
                for _ in 0..=cnt {
                    let d = super::urd(b, &mut p)?;
                    mark(d, &mut st)?;
                }
                st.last_mut()?.bt = true;
                dead = true;
            }
            (_, Some(0x00 | 0x0f | 0x12 | 0x13)) => dead = true,
            _ => {}
        }
    }
    if del.is_empty() {
        return Some((0, Vec::new()));
    }
    if del.iter().any(|&i| is_anchor(ins[i].1)) {
        return None;
    }
    for &i in &del {
        ins[i].0 = WPeek::Del;
    }
    ins.retain(|&(k, _, _, _)| !matches!(k, WPeek::Del));
    Some((del.len() as u32, must))
}

/// (pops, pushes) of one straight-line instruction, `None` for control
/// flow and anything this table does not size.
fn straight_eff(
    b: &[u8],
    k: WPeek,
    s: usize,
    fsig: &[(u32, u32)],
    tsig: &[(u32, u32)],
) -> Option<(u32, u32)> {
    Some(match k {
        WPeek::Del | WPeek::Nop(_) => (0, 0),
        WPeek::Get(_) => (0, 1),
        WPeek::Set(_) | WPeek::Drop => (1, 0),
        WPeek::Tee(_) | WPeek::Eqz => (1, 1),
        WPeek::Op1(_) => (2, 1),
        WPeek::Br(_) | WPeek::BrIf(_) => return None,
        WPeek::Ctl => match *b.get(s)? {
            0x10 => {
                let mut p = s + 1;
                *fsig.get(super::urd(b, &mut p)? as usize)?
            }
            0x11 => {
                let mut p = s + 1;
                let (np, nr) = *tsig.get(super::urd(b, &mut p)? as usize)?;
                (np + 1, nr)
            }
            _ => return None,
        },
        WPeek::Other => match *b.get(s)? {
            0x01 => (0, 0),
            0x1a => (1, 0),
            0x1b | 0x1c => (3, 1),
            0x23 => (0, 1),
            0x24 => (1, 0),
            0x25 => (1, 1),
            0x26 => (2, 0),
            0x28..=0x35 => (1, 1),
            0x36..=0x3e => (2, 0),
            0x3f => (0, 1),
            0x40 => (1, 1),
            0x41..=0x44 => (0, 1),
            0x45 | 0x50 => (1, 1),
            0x46..=0x4f | 0x51..=0x66 => (2, 1),
            0x67..=0x69 | 0x79..=0x7b | 0x8b..=0x91 | 0x99..=0x9f => (1, 1),
            0x6a..=0x78 | 0x7c..=0x8a | 0x92..=0x98 | 0xa0..=0xa6 => (2, 1),
            0xa7..=0xc4 => (1, 1),
            0xd0 | 0xd2 => (0, 1),
            0xd1 => (1, 1),
            0xfc => match *b.get(s + 1)? {
                0..=7 => (1, 1),
                8 | 10 | 11 | 12 | 14 | 17 => (3, 0),
                9 | 13 => (0, 0),
                15 => (2, 1),
                16 => (0, 1),
                _ => return None,
            },
            0xfd => super::simd_eff(b, s)?,
            _ => return None,
        },
    })
}

/// The integer compare with the opposite result (`eq`/`ne`, `lt`/`ge`,
/// `gt`/`le`, signed and unsigned, i32 and i64). Float compares have none
/// (NaN), and `eqz` is handled by its caller.
fn invert_icmp(op: u8) -> Option<u8> {
    Some(match op {
        0x46 => 0x47,
        0x47 => 0x46,
        0x48 => 0x4e,
        0x4e => 0x48,
        0x49 => 0x4f,
        0x4f => 0x49,
        0x4a => 0x4c,
        0x4c => 0x4a,
        0x4b => 0x4d,
        0x4d => 0x4b,
        0x51 => 0x52,
        0x52 => 0x51,
        0x53 => 0x59,
        0x59 => 0x53,
        0x54 => 0x5a,
        0x5a => 0x54,
        0x55 => 0x57,
        0x57 => 0x55,
        0x56 => 0x58,
        0x58 => 0x56,
        _ => return None,
    })
}

/// Branch over branch (N9 #2, `PLIRON_WASM_BOB=1`):
/// `block; S; br_if 0; br K; end` with `S` straight-line becomes
/// `S'; br_if K-1`, where `S'` ends in the inverted test: an integer
/// compare flips (`lt_u` to `ge_u`), an `i32.eqz` goes, anything else
/// gets an `i32.eqz`. Saves the block (3 bytes), the `br` (2) and, for the
/// compare and `eqz` cases, nothing is added.
///
/// Conditions: the empty blocktype; `S` sized by `straight_eff` with the
/// operand depth (relative to the block) never below zero and exactly 1
/// (the condition) at the `br_if`, so `br K` passes no values and
/// `S'` runs in the outer frame with the same net effect; `K >= 1`
/// (`br 0` would name the block itself); no `local.get` prelude anchored
/// on an instruction this deletes. `S` holds no branch, so no label
/// inside needs a fixup. Returns the number of folds.
pub(super) fn bob_round(
    b: &[u8],
    ins: &mut Vec<(WPeek, usize, usize, usize)>,
    fsig: &[(u32, u32)],
    tsig: &[(u32, u32)],
    is_anchor: impl Fn(usize) -> bool,
) -> u32 {
    let live = |k: WPeek| !matches!(k, WPeek::Del | WPeek::Nop(_));
    let label = |k: WPeek, s: usize, want: u8| -> Option<u32> {
        match (k, want) {
            (WPeek::BrIf(d), 0x0d) | (WPeek::Br(d), 0x0c) => Some(d),
            (WPeek::Ctl, _) if b.get(s) == Some(&want) => {
                let mut p = s + 1;
                super::urd(b, &mut p)
            }
            _ => None,
        }
    };
    // Each fold: (block opener, br_if, br, end, the condition's producer,
    // what happens to it).
    enum Cond {
        Flip(u8),
        DropEqz,
        AddEqz,
        /// `{i32,i64}.const 0; {eq,ne}` (the emitter would fuse it to
        /// `eqz`): the const goes and the test becomes the value itself,
        /// `i32.eqz`, `i64.eqz` or `i64.eqz; i32.eqz`.
        Zero(u8, usize),
    }
    let mut folds: Vec<(usize, usize, usize, usize, usize, Cond)> = Vec::new();
    let mut i = 0;
    'scan: while i < ins.len() {
        let (k, s, e, _) = ins[i];
        if !(matches!(k, WPeek::Ctl) && empty_blocktype(b, s, e)) || b[s] != 0x02 {
            i += 1;
            continue;
        }
        let mut d: i64 = 0;
        let mut last: Option<usize> = None;
        let mut j = i + 1;
        while j < ins.len() {
            let (k2, s2, ..) = ins[j];
            if label(k2, s2, 0x0d).is_some() {
                break;
            }
            let Some((p, u)) = straight_eff(b, k2, s2, fsig, tsig) else {
                i = j.max(i + 1);
                continue 'scan;
            };
            d -= p as i64;
            if d < 0 {
                i = j;
                continue 'scan;
            }
            d += u as i64;
            if live(k2) {
                last = Some(j);
            }
            j += 1;
        }
        let jb = j;
        let next_live = |from: usize| (from..ins.len()).find(|&x| live(ins[x].0));
        let (Some(jr), true) = (next_live(jb + 1), d == 1) else {
            i += 1;
            continue;
        };
        let Some(je) = next_live(jr + 1) else { break };
        let ok = jb < ins.len()
            && label(ins[jb].0, ins[jb].1, 0x0d) == Some(0)
            && label(ins[jr].0, ins[jr].1, 0x0c).is_some_and(|kk| kk >= 1)
            && matches!(ins[je].0, WPeek::Ctl)
            && b[ins[je].1] == 0x0b;
        let Some(c) = last.filter(|_| ok) else {
            i += 1;
            continue;
        };
        let prev = (i + 1..c).rev().find(|&x| live(ins[x].0));
        let zero_before = |want: u8| {
            prev.filter(|&x| {
                let (k3, s3, e3, _) = ins[x];
                matches!(k3, WPeek::Other) && e3 == s3 + 2 && b[s3] == want && b[s3 + 1] == 0
            })
        };
        let cond = match ins[c].0 {
            WPeek::Other => match b[ins[c].1] {
                0x45 => Cond::DropEqz,
                op @ (0x46 | 0x47) if zero_before(0x41).is_some() => {
                    Cond::Zero(op, zero_before(0x41).unwrap())
                }
                op @ (0x51 | 0x52) if zero_before(0x42).is_some() => {
                    Cond::Zero(op, zero_before(0x42).unwrap())
                }
                op => invert_icmp(op).map_or(Cond::AddEqz, Cond::Flip),
            },
            WPeek::Eqz => Cond::DropEqz,
            _ => Cond::AddEqz,
        };
        let mut gone = vec![i, jb, jr, je];
        match cond {
            Cond::DropEqz => gone.push(c),
            Cond::Zero(_, pc) => gone.extend([c, pc]),
            _ => {}
        }
        if gone.iter().any(|&x| is_anchor(ins[x].1)) {
            i = je + 1;
            continue;
        }
        folds.push((i, jb, jr, je, c, cond));
        i = je + 1;
    }
    if folds.is_empty() {
        return 0;
    }
    let n = folds.len() as u32;
    let mut add_eqz: Vec<usize> = Vec::new();
    for (bi, jb, jr, je, c, cond) in folds {
        let kk = label(ins[jr].0, ins[jr].1, 0x0c).unwrap_or(1);
        ins[bi].0 = WPeek::Del;
        ins[jr].0 = WPeek::Del;
        ins[je].0 = WPeek::Del;
        ins[jb].0 = WPeek::BrIf(kk - 1);
        match cond {
            Cond::Flip(op) => ins[c].0 = WPeek::Op1(op),
            Cond::DropEqz => ins[c].0 = WPeek::Del,
            Cond::AddEqz => add_eqz.push(jb),
            Cond::Zero(op, pc) => {
                ins[pc].0 = WPeek::Del;
                match op {
                    0x46 => ins[c].0 = WPeek::Del,
                    0x47 => ins[c].0 = WPeek::Eqz,
                    0x51 => {
                        ins[c].0 = WPeek::Op1(0x50);
                        add_eqz.push(jb);
                    }
                    _ => ins[c].0 = WPeek::Op1(0x50),
                }
            }
        }
    }
    let mut out = Vec::with_capacity(ins.len() + add_eqz.len());
    let mut a = 0;
    for (x, &it) in ins.iter().enumerate() {
        if a < add_eqz.len() && add_eqz[a] == x {
            out.push((WPeek::Eqz, it.1, it.1, it.3));
            a += 1;
        }
        if !matches!(it.0, WPeek::Del) {
            out.push(it);
        }
    }
    *ins = out;
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-decode a tiny body into `ins` slots: each entry is one
    /// opcode at byte offset == index (empty blocktypes for block/if/loop).
    fn body(ops: &[u8]) -> (Vec<u8>, Vec<(WPeek, usize, usize, usize)>) {
        let mut b = Vec::new();
        let mut ins = Vec::new();
        for &op in ops {
            let s = b.len();
            b.push(op);
            let e = match op {
                0x02 | 0x03 | 0x04 => {
                    b.push(0x40);
                    b.len()
                }
                0x0c | 0x0d => {
                    b.push(0x00); // label 0
                    b.len()
                }
                _ => b.len(),
            };
            let k = match op {
                0x0c => WPeek::Br(0),
                0x0d => WPeek::BrIf(0),
                _ => WPeek::Ctl,
            };
            // Fix Br label byte if we need non-zero — tests set manually.
            ins.push((k, s, e, 0));
        }
        (b, ins)
    }

    #[test]
    fn wellformed_rejects_else_outside_if() {
        // block; else; end; end(func)
        let (b, ins) = body(&[0x02, 0x05, 0x0b, 0x0b]);
        assert!(wellformed(&b, &ins).is_err());
    }

    #[test]
    fn wellformed_accepts_if_else() {
        // if; else; end; end(func)
        let (b, ins) = body(&[0x04, 0x05, 0x0b, 0x0b]);
        assert!(wellformed(&b, &ins).is_ok());
    }

    #[test]
    fn wellformed_accepts_bare_func_end() {
        let (b, ins) = body(&[0x0b]);
        assert!(wellformed(&b, &ins).is_ok());
    }

    #[test]
    fn untargeted_empty_block() {
        let b = vec![0x02, 0x40, 0x01, 0x0b];
        let ins = vec![
            (WPeek::Ctl, 0, 2, 0),
            (WPeek::Other, 2, 3, 0),
            (WPeek::Ctl, 3, 4, 0),
        ];
        assert!(untargeted(&b, &ins, 0, 2).is_ok());
    }

    #[test]
    fn targeted_by_br0() {
        // block; br 0; end
        let b = vec![0x02, 0x40, 0x0c, 0x00, 0x0b];
        let ins = vec![
            (WPeek::Ctl, 0, 2, 0),
            (WPeek::Br(0), 2, 4, 0),
            (WPeek::Ctl, 4, 5, 0),
        ];
        assert!(untargeted(&b, &ins, 0, 2).is_err());
    }

    #[test]
    fn elide_removes_untargeted_block() {
        let b = vec![0x02, 0x40, 0x01, 0x0b, 0x0b];
        let mut ins = vec![
            (WPeek::Ctl, 0, 2, 0),
            (WPeek::Other, 2, 3, 0),
            (WPeek::Ctl, 3, 4, 0),
            (WPeek::Ctl, 4, 5, 0),
        ];
        let n = elide_round(&b, &mut ins, true, "test");
        assert_eq!(n, 1);
        assert_eq!(ins.len(), 2);
        assert!(matches!(ins[0].0, WPeek::Other));
        assert!(matches!(ins[1].0, WPeek::Ctl));
    }

    // r[verify wasm.audit.elide]
    #[test]
    fn elide_keeps_targeted_block() {
        let b = vec![0x02, 0x40, 0x0c, 0x00, 0x0b, 0x0b];
        let mut ins = vec![
            (WPeek::Ctl, 0, 2, 0),
            (WPeek::Br(0), 2, 4, 0),
            (WPeek::Ctl, 4, 5, 0),
            (WPeek::Ctl, 5, 6, 0),
        ];
        let n = elide_round(&b, &mut ins, true, "test");
        assert_eq!(n, 0);
        assert_eq!(ins.len(), 4);
    }

    #[test]
    fn wellformed_ignores_synth_br_span() {
        // Synthesized `br_if` still spans the old `if` byte at offset 0.
        // Must not count as an open `if`. Trailing func `end` expected.
        let b = vec![0x04, 0x40, 0x0b];
        let ins = vec![
            (WPeek::BrIf(0), 0, 0, 0),
            (WPeek::Ctl, 2, 3, 0),
        ];
        assert!(wellformed(&b, &ins).is_ok());
        let ins2 = vec![(WPeek::BrIf(0), 0, 0, 0)];
        assert!(wellformed(&b, &ins2).is_err()); // missing func end
    }

    #[test]
    fn elide_retargets_synth_br_past() {
        // block; br 1 (synth); end; end(func) — br targets past the block.
        let b = vec![0x02, 0x40, 0xff, 0x0b, 0x0b];
        let mut ins = vec![
            (WPeek::Ctl, 0, 2, 0),
            (WPeek::Br(1), 2, 2, 0),
            (WPeek::Ctl, 3, 4, 0),
            (WPeek::Ctl, 4, 5, 0),
        ];
        let n = elide_round(&b, &mut ins, true, "test");
        assert_eq!(n, 1);
        assert_eq!(ins.len(), 2);
        assert!(matches!(ins[0].0, WPeek::Br(0)));
        assert!(matches!(ins[1].0, WPeek::Ctl));
    }

    #[test]
    fn elide_removes_untargeted_loop() {
        // loop; nop; end; end(func) — nothing continues to the head.
        let b = vec![0x03, 0x40, 0x01, 0x0b, 0x0b];
        let mut ins = vec![
            (WPeek::Ctl, 0, 2, 0),
            (WPeek::Other, 2, 3, 0),
            (WPeek::Ctl, 3, 4, 0),
            (WPeek::Ctl, 4, 5, 0),
        ];
        let n = elide_round(&b, &mut ins, true, "test");
        assert_eq!(n, 1);
        assert!(matches!(ins[0].0, WPeek::Other));
    }

    #[test]
    fn elide_keeps_continued_loop() {
        // loop; br 0; end; end(func)
        let b = vec![0x03, 0x40, 0x0c, 0x00, 0x0b, 0x0b];
        let mut ins = vec![
            (WPeek::Ctl, 0, 2, 0),
            (WPeek::Br(0), 2, 4, 0),
            (WPeek::Ctl, 4, 5, 0),
            (WPeek::Ctl, 5, 6, 0),
        ];
        assert_eq!(elide_round(&b, &mut ins, true, "test"), 0);
        assert_eq!(ins.len(), 4);
    }
}
