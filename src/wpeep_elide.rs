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
