//! Emit `metadata.code.branch_hint` for V8 (`PLIRON_WASM_BRHINT=1`).
//!
//! After peep, scan each body for `if`/`br_if` whose one arm looks cold
//! (unreachable, or a short const/set/br error tail) and mark the
//! condition accordingly. Offsets are post-peep so they stay valid.
//! The custom section is spliced in *before* the Code section, as the
//! branch-hinting proposal requires.
//!
//! parse_only's residual (PARITY): without `PLIRON_WASM_COLDBR`, short
//! `Err` tails are inlined into the `if` arm so block-arg moves sit on
//! the hot edge; a hint can still ask V8 to treat that arm as cold.
//! Prefer COLDBR (bare labeled cold edge) when measuring — hints are a
//! fallback, not the shape fix.

use super::{leb, nskip, sleb_skip, urd};

/// Apply when `PLIRON_WASM_BRHINT=1` (or `=dbg` for a count line).
/// No-op if unset / `=0` or the module is malformed.
pub(super) fn apply(bytes: &mut Vec<u8>) {
    let mode = pliron_cfg::env_get("PLIRON_WASM_BRHINT");
    let on = matches!(mode, Some("1") | Some("dbg"));
    if !on {
        return;
    }
    let dbg = mode == Some("dbg");
    if bytes.len() < 8 || &bytes[..8] != b"\0asm\x01\0\0\0" {
        return;
    }
    let Some((n_imports, code_at, bodies)) = locate_code(bytes) else {
        if dbg {
            eprintln!("wasm-brhint: locate_code failed");
        }
        return;
    };
    let mut func_hints: Vec<(u32, Vec<(u32, u32)>)> = Vec::new();
    let mut scanned = 0u32;
    for (i, (s, e)) in bodies.iter().copied().enumerate() {
        let hints = scan_body(&bytes[s..e]);
        scanned += 1;
        if !hints.is_empty() {
            func_hints.push((n_imports + i as u32, hints));
        }
    }
    let n_hints: usize = func_hints.iter().map(|(_, h)| h.len()).sum();
    if dbg {
        eprintln!(
            "wasm-brhint: funcs={scanned} hinted={} hints={n_hints}",
            func_hints.len()
        );
    }
    if func_hints.is_empty() {
        return;
    }
    let sec = encode_section(&func_hints);
    bytes.splice(code_at..code_at, sec);
}

/// `(import_func_count, code_section_id_offset, [(body_start, body_end)])`.
fn locate_code(bytes: &[u8]) -> Option<(u32, usize, Vec<(usize, usize)>)> {
    let mut p = 8usize;
    let mut n_imports = 0u32;
    while p < bytes.len() {
        let id_at = p;
        let id = *bytes.get(p)?;
        p += 1;
        let sz = urd(bytes, &mut p)? as usize;
        let (s, e) = (p, p.saturating_add(sz));
        if e > bytes.len() {
            return None;
        }
        match id {
            2 => {
                let mut q = s;
                let mut ni = urd(bytes, &mut q)?;
                while ni > 0 {
                    ni -= 1;
                    for _ in 0..2 {
                        let nl = urd(bytes, &mut q)? as usize;
                        nskip(bytes, &mut q, nl)?;
                    }
                    let kind = *bytes.get(q)?;
                    q += 1;
                    match kind {
                        0x00 => {
                            urd(bytes, &mut q)?;
                            n_imports += 1;
                        }
                        0x01 => {
                            q += 1; // reftype
                            let fl = *bytes.get(q)?;
                            q += 1;
                            urd(bytes, &mut q)?;
                            if fl & 1 != 0 {
                                urd(bytes, &mut q)?;
                            }
                        }
                        0x02 => {
                            let fl = *bytes.get(q)?;
                            q += 1;
                            urd(bytes, &mut q)?;
                            if fl & 1 != 0 {
                                urd(bytes, &mut q)?;
                            }
                        }
                        0x03 => {
                            q += 2; // valtype + mut
                            skip_const_expr(bytes, &mut q)?;
                        }
                        0x04 => {
                            // tag: attribute + typeidx
                            q += 1;
                            urd(bytes, &mut q)?;
                        }
                        _ => return None,
                    }
                }
            }
            10 => {
                let mut q = s;
                let nf = urd(bytes, &mut q)?;
                let mut bodies = Vec::with_capacity(nf as usize);
                for _ in 0..nf {
                    let bsz = urd(bytes, &mut q)? as usize;
                    let be = q + bsz;
                    if be > e {
                        return None;
                    }
                    bodies.push((q, be));
                    q = be;
                }
                return Some((n_imports, id_at, bodies));
            }
            _ => {}
        }
        p = e;
    }
    None
}

fn skip_const_expr(b: &[u8], p: &mut usize) -> Option<()> {
    // init_expr: instructions ending in `end` (0x0b). Only the forms we
    // emit (global.get / i32.const / …) — bail on anything odd.
    loop {
        let op = *b.get(*p)?;
        *p += 1;
        match op {
            0x0b => return Some(()),
            0x23 | 0x41 | 0x42 => {
                sleb_skip(b, p)?;
            }
            0x43 => nskip(b, p, 4)?,
            0x44 => nskip(b, p, 8)?,
            0xd0 => {
                sleb_skip(b, p)?;
            }
            _ => return None,
        }
    }
}

fn encode_section(funcs: &[(u32, Vec<(u32, u32)>)]) -> Vec<u8> {
    let mut payload = Vec::new();
    leb(&mut payload, funcs.len() as u32);
    for &(fi, ref hints) in funcs {
        leb(&mut payload, fi);
        leb(&mut payload, hints.len() as u32);
        for &(off, val) in hints {
            leb(&mut payload, off);
            leb(&mut payload, 1); // size of hint payload
            leb(&mut payload, val);
        }
    }
    let mut body = Vec::new();
    let name = b"metadata.code.branch_hint";
    leb(&mut body, name.len() as u32);
    body.extend_from_slice(name);
    body.extend_from_slice(&payload);
    let mut sec = Vec::new();
    sec.push(0); // custom
    leb(&mut sec, body.len() as u32);
    sec.extend(body);
    sec
}

/// Hints as `(func_body_offset, value)` with value 0 = likely false /
/// not-taken, 1 = likely true / taken.
fn scan_body(body: &[u8]) -> Vec<(u32, u32)> {
    let mut p = 0usize;
    let Some(ng) = urd(body, &mut p) else {
        return Vec::new();
    };
    for _ in 0..ng {
        if urd(body, &mut p).is_none() {
            return Vec::new();
        }
        // valtype byte (or typed-ref prefix — skip one; typed refs need
        // more, but our bodies use numeric locals).
        if p >= body.len() {
            return Vec::new();
        }
        let t = body[p];
        p += 1;
        if t == 0x63 || t == 0x64 {
            if sleb_skip(body, &mut p).is_none() {
                return Vec::new();
            }
        }
    }
    let mut hints = Vec::new();
    let _ = scan_ops(body, &mut p, body.len(), &mut hints);
    hints
}

/// Walk ops until `end_limit`. Returns false on decode failure.
fn scan_ops(b: &[u8], p: &mut usize, end_limit: usize, hints: &mut Vec<(u32, u32)>) -> bool {
    while *p < end_limit {
        let s = *p;
        let Some(&op) = b.get(*p) else {
            return false;
        };
        *p += 1;
        match op {
            0x0b => return true, // end — caller handles
            0x05 => return true, // else — caller handles
            0x02 | 0x03 => {
                if sleb_skip(b, p).is_none() {
                    return false;
                }
                if !scan_ops(b, p, end_limit, hints) {
                    return false;
                }
            }
            0x04 => {
                if sleb_skip(b, p).is_none() {
                    return false;
                }
                let then_start = *p;
                // then arm: until else or end at this level
                let (then_end, has_else) = match split_if_arm(b, p, end_limit) {
                    Some(x) => x,
                    None => return false,
                };
                let else_start = *p;
                let else_end = if has_else {
                    match split_if_arm(b, p, end_limit) {
                        Some((e, _)) => e,
                        None => return false,
                    }
                } else {
                    else_start
                };
                let then_arm = &b[then_start..then_end];
                let else_arm = &b[else_start..else_end];
                if let Some(val) = hint_for_if(then_arm, else_arm) {
                    hints.push((s as u32, val));
                }
                // Nested ifs: offsets from `scan_ops` on an arm slice are
                // arm-relative — rebase to the function body.
                let mut nested = Vec::new();
                let mut tp = 0usize;
                let _ = scan_ops(then_arm, &mut tp, then_arm.len(), &mut nested);
                for (off, val) in nested.drain(..) {
                    hints.push((then_start as u32 + off, val));
                }
                if has_else {
                    let mut ep = 0usize;
                    let _ = scan_ops(else_arm, &mut ep, else_arm.len(), &mut nested);
                    for (off, val) in nested {
                        hints.push((else_start as u32 + off, val));
                    }
                }
            }
            0x0d => {
                // br_if depth
                if urd(b, p).is_none() {
                    return false;
                }
                // Without arm bodies we can't score; leave unhinted.
            }
            0x00 | 0x01 | 0x0f | 0x1a | 0x1b | 0xd1 => {}
            0x0c | 0x10 | 0x12 | 0x14 | 0x20..=0x26 | 0x3f | 0x40 | 0xd2 => {
                if urd(b, p).is_none() {
                    return false;
                }
            }
            0x11 | 0x13 => {
                if urd(b, p).is_none() || urd(b, p).is_none() {
                    return false;
                }
            }
            0x0e => {
                let Some(n) = urd(b, p) else {
                    return false;
                };
                for _ in 0..=n {
                    if urd(b, p).is_none() {
                        return false;
                    }
                }
            }
            0x28..=0x3e => {
                if urd(b, p).is_none() || urd(b, p).is_none() {
                    return false;
                }
            }
            0x41 | 0x42 => {
                if sleb_skip(b, p).is_none() {
                    return false;
                }
            }
            0x43 => {
                if nskip(b, p, 4).is_none() {
                    return false;
                }
            }
            0x44 => {
                if nskip(b, p, 8).is_none() {
                    return false;
                }
            }
            0x45..=0xc4 => {}
            0xd0 => {
                if sleb_skip(b, p).is_none() {
                    return false;
                }
            }
            0xfc => {
                let Some(sub) = urd(b, p) else {
                    return false;
                };
                match sub {
                    0..=7 => {}
                    8 | 10 | 12 | 14 => {
                        if urd(b, p).is_none() || urd(b, p).is_none() {
                            return false;
                        }
                    }
                    9 | 11 | 13 | 15 | 16 | 17 => {
                        if urd(b, p).is_none() {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            0xfd => {
                // SIMD: skip subopcode LEB; leave lane/memarg forms to the
                // arm splitter's fail-soft (body keeps hints found so far).
                if urd(b, p).is_none() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Consume one if-arm; returns `(arm_end_offset, saw_else)` where
/// `arm_end` is the index of the else/end opcode (not included in arm).
fn split_if_arm(b: &[u8], p: &mut usize, end_limit: usize) -> Option<(usize, bool)> {
    let mut depth = 0i32;
    while *p < end_limit {
        let at = *p;
        let op = *b.get(*p)?;
        *p += 1;
        match op {
            0x02 | 0x03 | 0x04 => {
                depth += 1;
                sleb_skip(b, p)?;
            }
            0x05 => {
                if depth == 0 {
                    return Some((at, true));
                }
            }
            0x0b => {
                if depth == 0 {
                    return Some((at, false));
                }
                depth -= 1;
            }
            0x00 | 0x01 | 0x0f | 0x1a | 0x1b | 0xd1 => {}
            0x0c | 0x0d | 0x10 | 0x12 | 0x14 | 0x20..=0x26 | 0x3f | 0x40 | 0xd2 => {
                urd(b, p)?;
            }
            0x11 | 0x13 => {
                urd(b, p)?;
                urd(b, p)?;
            }
            0x0e => {
                let n = urd(b, p)?;
                for _ in 0..=n {
                    urd(b, p)?;
                }
            }
            0x28..=0x3e => {
                urd(b, p)?;
                urd(b, p)?;
            }
            0x41 | 0x42 | 0xd0 => {
                sleb_skip(b, p)?;
            }
            0x43 => nskip(b, p, 4)?,
            0x44 => nskip(b, p, 8)?,
            0x45..=0xc4 => {}
            0xfc => {
                let sub = urd(b, p)?;
                match sub {
                    0..=7 => {}
                    8 | 10 | 12 | 14 => {
                        urd(b, p)?;
                        urd(b, p)?;
                    }
                    9 | 11 | 13 | 15 | 16 | 17 => {
                        urd(b, p)?;
                    }
                    _ => return None,
                }
            }
            0xfd => {
                urd(b, p)?; // subopcode; memarg/lane may desync — fail soft
            }
            _ => return None,
        }
    }
    None
}

fn hint_for_if(then_arm: &[u8], else_arm: &[u8]) -> Option<u32> {
    let tc = arm_cold(then_arm);
    let ec = arm_cold(else_arm);
    match (tc, ec) {
        (true, false) => Some(0), // condition likely false → take else (hot)
        (false, true) => Some(1), // condition likely true → take then (hot)
        _ => None,
    }
}

fn arm_cold(arm: &[u8]) -> bool {
    if arm.is_empty() {
        return false;
    }
    let (ops, unreach, has_loop, has_br) = arm_stats(arm);
    if unreach {
        return true;
    }
    // Short forward error tails: a few consts/sets and a br out, no loop.
    // parse_only's `len == 0`/`1` arms land here; the digit loop does not.
    !has_loop && has_br && ops > 0 && ops <= 10
}

fn arm_stats(arm: &[u8]) -> (usize, bool, bool, bool) {
    let mut p = 0usize;
    let mut ops = 0usize;
    let mut unreach = false;
    let mut has_loop = false;
    let mut has_br = false;
    let mut depth = 0i32;
    while p < arm.len() {
        let op = match arm.get(p).copied() {
            Some(o) => o,
            None => break,
        };
        p += 1;
        ops += 1;
        match op {
            0x00 => unreach = true,
            0x0c | 0x0d => {
                has_br = true;
                let _ = urd(arm, &mut p);
            }
            0x03 => {
                has_loop = true;
                depth += 1;
                let _ = sleb_skip(arm, &mut p);
            }
            0x02 | 0x04 => {
                depth += 1;
                let _ = sleb_skip(arm, &mut p);
            }
            0x0b => depth -= 1,
            0x05 => {}
            0x10 | 0x12 | 0x14 | 0x20..=0x26 | 0x3f | 0x40 | 0xd2 => {
                let _ = urd(arm, &mut p);
            }
            0x11 | 0x13 => {
                let _ = urd(arm, &mut p);
                let _ = urd(arm, &mut p);
            }
            0x0e => {
                if let Some(n) = urd(arm, &mut p) {
                    for _ in 0..=n {
                        let _ = urd(arm, &mut p);
                    }
                }
            }
            0x28..=0x3e => {
                let _ = urd(arm, &mut p);
                let _ = urd(arm, &mut p);
            }
            0x41 | 0x42 | 0xd0 => {
                let _ = sleb_skip(arm, &mut p);
            }
            0x43 => {
                let _ = nskip(arm, &mut p, 4);
            }
            0x44 => {
                let _ = nskip(arm, &mut p, 8);
            }
            0xfc => {
                if let Some(sub) = urd(arm, &mut p) {
                    match sub {
                        8 | 10 | 12 | 14 => {
                            let _ = urd(arm, &mut p);
                            let _ = urd(arm, &mut p);
                        }
                        9 | 11 | 13 | 15 | 16 | 17 => {
                            let _ = urd(arm, &mut p);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let _ = depth;
    }
    (ops, unreach, has_loop, has_br)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_then_hot_else_hints_false() {
        // then: unreachable; else: long nop run → then cold only
        assert_eq!(hint_for_if(&[0x00], &[0x01; 20]), Some(0));
        assert_eq!(hint_for_if(&[0x01; 20], &[0x00]), Some(1));
        assert_eq!(hint_for_if(&[0x00], &[0x00]), None);
    }

    #[test]
    fn short_br_tail_is_cold() {
        // i32.const 0; local.set 0; br 1  ≈ cold error tail
        let arm = vec![0x41, 0x00, 0x21, 0x00, 0x0c, 0x01];
        assert!(arm_cold(&arm));
        assert!(!arm_cold(&[0x01; 3])); // short but no br / unreachable
        assert!(!arm_cold(&[0x03, 0x40, 0x0c, 0x00, 0x0b])); // br inside loop
    }
}
