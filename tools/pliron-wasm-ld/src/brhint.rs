//! Emit `metadata.code.branch_hint` for V8 (`PLIRON_WASM_BRHINT=1`).
//!
//! Runs on the final linked module (function indices are stable here).
//! Scans each body for `if` whose one arm looks cold (unreachable, or a
//! short const/set/br error tail) and marks the condition. The custom
//! section is spliced in *before* the Code section.

fn leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        out.push(if v != 0 { b | 0x80 } else { b });
        if v == 0 {
            break;
        }
    }
}

fn urd(b: &[u8], p: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    let mut s = 0u32;
    loop {
        let c = *b.get(*p)?;
        *p += 1;
        if s < 32 {
            v |= ((c & 0x7f) as u32) << s;
        }
        if c & 0x80 == 0 {
            return Some(v);
        }
        s += 7;
        if s > 35 {
            return None;
        }
    }
}

fn sleb_skip(b: &[u8], p: &mut usize) -> Option<()> {
    loop {
        let c = *b.get(*p)?;
        *p += 1;
        if c & 0x80 == 0 {
            return Some(());
        }
    }
}

fn nskip(b: &[u8], p: &mut usize, n: usize) -> Option<()> {
    if b.len() - *p < n {
        return None;
    }
    *p += n;
    Some(())
}

/// Apply when `PLIRON_WASM_BRHINT=1` (or `=dbg` for a count line).
pub(crate) fn apply(bytes: &mut Vec<u8>) {
    let mode = std::env::var("PLIRON_WASM_BRHINT").ok();
    let on = matches!(mode.as_deref(), Some("1") | Some("dbg"));
    if !on {
        return;
    }
    let dbg = mode.as_deref() == Some("dbg");
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
    for (i, (s, e)) in bodies.iter().copied().enumerate() {
        let hints = scan_body(&bytes[s..e]);
        if !hints.is_empty() {
            func_hints.push((n_imports + i as u32, hints));
        }
    }
    let n_hints: usize = func_hints.iter().map(|(_, h)| h.len()).sum();
    if dbg {
        eprintln!(
            "wasm-brhint: funcs={} hinted={} hints={n_hints}",
            bodies.len(),
            func_hints.len()
        );
    }
    if func_hints.is_empty() {
        return;
    }
    let sec = encode_section(&func_hints);
    bytes.splice(code_at..code_at, sec);
}

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
                            q += 1;
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
                            q += 2;
                            skip_const_expr(bytes, &mut q)?;
                        }
                        0x04 => {
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
    loop {
        let op = *b.get(*p)?;
        *p += 1;
        match op {
            0x0b => return Some(()),
            0x23 | 0x41 | 0x42 => sleb_skip(b, p)?,
            0x43 => nskip(b, p, 4)?,
            0x44 => nskip(b, p, 8)?,
            0xd0 => sleb_skip(b, p)?,
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
            leb(&mut payload, 1);
            leb(&mut payload, val);
        }
    }
    let mut body = Vec::new();
    let name = b"metadata.code.branch_hint";
    leb(&mut body, name.len() as u32);
    body.extend_from_slice(name);
    body.extend_from_slice(&payload);
    let mut sec = Vec::new();
    sec.push(0);
    leb(&mut sec, body.len() as u32);
    sec.extend(body);
    sec
}

fn scan_body(body: &[u8]) -> Vec<(u32, u32)> {
    let mut p = 0usize;
    let Some(ng) = urd(body, &mut p) else {
        return Vec::new();
    };
    for _ in 0..ng {
        if urd(body, &mut p).is_none() {
            return Vec::new();
        }
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

fn scan_ops(b: &[u8], p: &mut usize, end_limit: usize, hints: &mut Vec<(u32, u32)>) -> bool {
    while *p < end_limit {
        let s = *p;
        let Some(&op) = b.get(*p) else {
            return false;
        };
        *p += 1;
        match op {
            0x0b | 0x05 => return true,
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
                let Some((then_end, has_else)) = split_if_arm(b, p, end_limit) else {
                    return false;
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
                if urd(b, p).is_none() {
                    return false;
                }
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
            0x41 | 0x42 | 0xd0 => {
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
                if urd(b, p).is_none() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

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
            0x41 | 0x42 | 0xd0 => sleb_skip(b, p)?,
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
                urd(b, p)?;
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
        (true, false) => Some(0),
        (false, true) => Some(1),
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
    !has_loop && has_br && ops > 0 && ops <= 10
}

fn arm_stats(arm: &[u8]) -> (usize, bool, bool, bool) {
    let mut p = 0usize;
    let mut ops = 0usize;
    let mut unreach = false;
    let mut has_loop = false;
    let mut has_br = false;
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
                let _ = sleb_skip(arm, &mut p);
            }
            0x02 | 0x04 => {
                let _ = sleb_skip(arm, &mut p);
            }
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
    }
    (ops, unreach, has_loop, has_br)
}
