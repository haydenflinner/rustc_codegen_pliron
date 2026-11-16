//! Fold a constant address into a load/store memarg offset.
//!
//! The linker rewrites a GOT `global.get` into `i32.const TABLE` once
//! layout is known. Uses of that constant then look like
//! `local.get base; i32.add; i32.load offset=0`. The add is on the
//! address path of every probe. `i32.load offset=TABLE` is the same
//! wrap-around address `waddr` already folds when the constant is
//! still an `i32.add` operand, which it is not after local allocation.
//!
//! `PLIRON_WASM_ADDROFF=1` folds. `=dbg` also prints a count. Default
//! is off. A layouts=3 run with it on moved chars_mixed from +10.4%
//! to +11.4% against LLVM and left chars_ascii at −25%: the table
//! address in the memarg is a 20-bit immediate V8 rematerializes,
//! which did not beat a hoisted local.

use rustc_hash::FxHashMap;

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

fn sleb(b: &[u8], p: &mut usize) -> Option<i64> {
    let mut v = 0i64;
    let mut s = 0u32;
    loop {
        let c = *b.get(*p)?;
        *p += 1;
        if s < 63 {
            v |= ((c & 0x7f) as i64) << s;
        }
        if c & 0x80 == 0 {
            if s < 63 && c & 0x40 != 0 {
                v |= !0 << (s + 7).min(63);
            }
            return Some(v);
        }
        s += 7;
        if s > 70 {
            return None;
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

#[derive(Clone, Copy)]
enum Op {
    I32Const(i64),
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    I32Add,
    /// `opcode`, alignment, offset. The offset is the second uleb.
    Mem(u8, u32, u32),
    Block,
    Loop,
    If,
    Else,
    End,
    Br(u32),
    BrIf(u32),
    BrTable,
    Stop,
    Other,
}

struct Inst {
    at: usize,
    end: usize,
    op: Op,
}

fn parse_ops(b: &[u8], p: &mut usize) -> Option<Vec<Inst>> {
    let mut out = Vec::new();
    while *p < b.len() {
        let at = *p;
        let opc = *b.get(*p)?;
        *p += 1;
        let op = match opc {
            0x02 => {
                sleb(b, p)?;
                Op::Block
            }
            0x03 => {
                sleb(b, p)?;
                Op::Loop
            }
            0x04 => {
                sleb(b, p)?;
                Op::If
            }
            0x05 => Op::Else,
            0x0b => Op::End,
            0x0c => Op::Br(urd(b, p)?),
            0x0d => Op::BrIf(urd(b, p)?),
            0x0e => {
                let n = urd(b, p)?;
                for _ in 0..=n {
                    urd(b, p)?;
                }
                Op::BrTable
            }
            0x0f | 0x00 => Op::Stop,
            0x20 => Op::LocalGet(urd(b, p)?),
            0x21 => Op::LocalSet(urd(b, p)?),
            0x22 => Op::LocalTee(urd(b, p)?),
            0x28..=0x3e => {
                let align = urd(b, p)?;
                let offset = urd(b, p)?;
                Op::Mem(opc, align, offset)
            }
            0x41 => Op::I32Const(sleb(b, p)?),
            0x6a => Op::I32Add,
            0x01 | 0x1a | 0x1b | 0xd1 => Op::Other,
            0x10 | 0x12 | 0x14 | 0x23..=0x26 | 0x3f | 0x40 | 0xd2 => {
                urd(b, p)?;
                Op::Other
            }
            0x11 | 0x13 => {
                urd(b, p)?;
                urd(b, p)?;
                Op::Other
            }
            0x42 | 0xd0 => {
                sleb(b, p)?;
                Op::Other
            }
            0x43 => {
                nskip(b, p, 4)?;
                Op::Other
            }
            0x44 => {
                nskip(b, p, 8)?;
                Op::Other
            }
            0x45..=0xc4 => Op::Other,
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
                Op::Other
            }
            0xfd => {
                urd(b, p)?;
                Op::Other
            }
            _ => return None,
        };
        out.push(Inst { at, end: *p, op });
        if matches!(op, Op::End) && *p >= b.len() {
            break;
        }
    }
    Some(out)
}

type Map = FxHashMap<u32, i64>;

fn merge(maps: &[Map]) -> Map {
    let Some(first) = maps.first() else {
        return Map::default();
    };
    let mut out = first.clone();
    for m in &maps[1..] {
        out.retain(|k, v| m.get(k) == Some(v));
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Block,
    Loop,
    If,
}

struct Frame {
    kind: Kind,
    entry: Map,
    exits: Vec<Map>,
    then_exit: Option<Map>,
    in_else: bool,
    dead: bool,
}

/// Locals assigned anywhere inside each loop, by the loop instruction's
/// index. A backedge can deliver that assignment to a get earlier in
/// the body, so the local is not a constant for the whole loop.
fn loop_assigns(insts: &[Inst]) -> FxHashMap<usize, Vec<u32>> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, inst) in insts.iter().enumerate() {
        match inst.op {
            Op::Loop => stack.push(i),
            Op::Block | Op::If => stack.push(usize::MAX),
            Op::End => {
                if let Some(s) = stack.pop()
                    && s != usize::MAX
                {
                    ranges.push((s, i));
                }
            }
            _ => {}
        }
    }
    let mut out: FxHashMap<usize, Vec<u32>> = FxHashMap::default();
    for (s, e) in ranges {
        let mut ls = Vec::new();
        for inst in &insts[s..=e] {
            match inst.op {
                Op::LocalSet(n) | Op::LocalTee(n) => {
                    if !ls.contains(&n) {
                        ls.push(n);
                    }
                }
                _ => {}
            }
        }
        out.insert(s, ls);
    }
    out
}

fn defined_const(op: Op, map: &Map) -> Option<i64> {
    match op {
        Op::I32Const(k) => Some(k),
        Op::LocalGet(n) => map.get(&n).copied(),
        _ => None,
    }
}

/// Rewrite one function body. `None` means unchanged or not understood.
fn fold_body(body: &[u8]) -> Option<Vec<u8>> {
    let mut p = 0usize;
    let ng = urd(body, &mut p)?;
    for _ in 0..ng {
        urd(body, &mut p)?;
        let t = *body.get(p)?;
        p += 1;
        if t == 0x63 || t == 0x64 {
            sleb(body, &mut p)?;
        }
    }
    let expr = p;
    let insts = parse_ops(body, &mut p)?;
    if p != body.len() {
        return None;
    }
    let loops = loop_assigns(&insts);
    let mut cur: Map = Map::default();
    let mut frames: Vec<Frame> = Vec::new();
    let mut dead = false;
    // Const value pushed by this instruction, if the fold can delete it.
    let mut pushed: Vec<Option<i64>> = vec![None; insts.len()];
    let mut folds: Vec<(usize, u32)> = Vec::new();
    for (i, inst) in insts.iter().enumerate() {
        if dead && !matches!(inst.op, Op::Else | Op::End) {
            continue;
        }
        match inst.op {
            Op::Block | Op::Loop | Op::If => {
                let kind = match inst.op {
                    Op::Loop => Kind::Loop,
                    Op::If => Kind::If,
                    _ => Kind::Block,
                };
                if kind == Kind::Loop
                    && let Some(ls) = loops.get(&i)
                {
                    for &n in ls {
                        cur.remove(&n);
                    }
                }
                frames.push(Frame {
                    kind,
                    entry: cur.clone(),
                    exits: Vec::new(),
                    then_exit: None,
                    in_else: false,
                    dead,
                });
            }
            Op::Else => {
                let fr = frames.last_mut()?;
                if fr.kind != Kind::If || fr.in_else {
                    return None;
                }
                fr.then_exit = if dead { None } else { Some(cur.clone()) };
                cur.clone_from(&fr.entry);
                dead = fr.dead;
                fr.in_else = true;
            }
            Op::End => {
                // The function's own `end` closes no block.
                let Some(fr) = frames.pop() else {
                    break;
                };
                let mut maps = Vec::new();
                if fr.kind == Kind::If {
                    if fr.in_else {
                        if let Some(t) = fr.then_exit.clone() {
                            maps.push(t);
                        }
                        if !dead {
                            maps.push(cur.clone());
                        }
                    } else {
                        if !dead {
                            maps.push(cur.clone());
                        }
                        maps.push(fr.entry.clone());
                    }
                } else if fr.kind == Kind::Block && !dead {
                    maps.push(cur.clone());
                } else if fr.kind == Kind::Loop && !dead {
                    maps.push(cur.clone());
                }
                maps.extend(fr.exits);
                if maps.is_empty() {
                    dead = true;
                    cur.clear();
                } else {
                    cur = merge(&maps);
                    dead = false;
                }
            }
            Op::Br(n) => {
                note_exit(&mut frames, n, &cur)?;
                dead = true;
            }
            Op::BrIf(n) => {
                note_exit(&mut frames, n, &cur)?;
            }
            Op::BrTable => {
                // Every local the table can skip past is no longer a
                // single reaching const. Drop the map; the branch is
                // unconditional.
                if let Some(fr) = frames.last_mut() {
                    fr.exits.push(Map::default());
                }
                cur.clear();
                dead = true;
            }
            Op::Stop => dead = true,
            Op::LocalSet(n) => {
                if let Some(k) = pushed.get(i.wrapping_sub(1)).copied().flatten() {
                    cur.insert(n, k);
                } else {
                    cur.remove(&n);
                }
            }
            Op::LocalTee(n) => {
                if let Some(k) = pushed.get(i.wrapping_sub(1)).copied().flatten() {
                    cur.insert(n, k);
                    pushed[i] = Some(k);
                } else {
                    cur.remove(&n);
                }
            }
            Op::I32Const(k) => pushed[i] = Some(k),
            Op::LocalGet(_) => pushed[i] = defined_const(inst.op, &cur),
            Op::I32Add => {
                let Some(k) = i.checked_sub(1).and_then(|j| pushed[j]) else {
                    continue;
                };
                let Some(Op::Mem(_, _, off)) = insts.get(i + 1).map(|n| n.op) else {
                    continue;
                };
                let sum = (off as i64).wrapping_add(k);
                if !(0..=u32::MAX as i64).contains(&sum) {
                    continue;
                }
                // Only a pure push can be deleted. A tee also writes
                // the local the later probes read.
                if !matches!(insts[i - 1].op, Op::I32Const(_) | Op::LocalGet(_)) {
                    continue;
                }
                folds.push((i + 1, sum as u32));
            }
            Op::Mem(_, _, _) | Op::Other => {}
        }
    }
    if folds.is_empty() {
        return None;
    }
    let mut drop = vec![false; insts.len()];
    let mut new_off: FxHashMap<usize, u32> = FxHashMap::default();
    for (mem_i, off) in folds {
        let add_i = mem_i - 1;
        let src_i = add_i - 1;
        if drop[src_i] || drop[add_i] || drop[mem_i] {
            continue;
        }
        drop[src_i] = true;
        drop[add_i] = true;
        new_off.insert(mem_i, off);
    }
    let mut out = body[..expr].to_vec();
    for (i, inst) in insts.iter().enumerate() {
        if drop[i] {
            continue;
        }
        if let Some(&off) = new_off.get(&i) {
            let Op::Mem(opc, align, _) = inst.op else {
                return None;
            };
            out.push(opc);
            leb(&mut out, align);
            leb(&mut out, off);
        } else {
            out.extend_from_slice(&body[inst.at..inst.end]);
        }
    }
    Some(out)
}

fn note_exit(frames: &mut [Frame], n: u32, cur: &Map) -> Option<()> {
    let i = frames.len().checked_sub(1 + n as usize)?;
    let fr = &mut frames[i];
    // `br` to a loop restarts it. The assigned-local prepass covers
    // that edge; the value at `end` is the fallthrough.
    if fr.kind == Kind::Loop && n == 0 {
        return Some(());
    }
    // `br d` from inside a loop, where the loop is the target, also
    // restarts. `n` is relative. The target frame is a loop iff this
    // branch restarts it, which is exactly when that frame is a loop
    // and no closer frame was the target. Record nothing on the loop
    // frame; its continuation does not see this edge.
    if fr.kind == Kind::Loop {
        return Some(());
    }
    fr.exits.push(cur.clone());
    Some(())
}

pub(crate) fn apply(bytes: &mut Vec<u8>) {
    let mode = std::env::var("PLIRON_WASM_ADDROFF").ok();
    let dbg = mode.as_deref() == Some("dbg");
    if !matches!(mode.as_deref(), Some("1") | Some("dbg")) {
        return;
    }
    if bytes.len() < 8 || &bytes[..8] != b"\0asm\x01\0\0\0" {
        return;
    }
    let Some((code_at, code_end, bodies)) = locate_code(bytes) else {
        if dbg {
            eprintln!("wasm-addroff: locate_code failed");
        }
        return;
    };
    let mut nfold = 0u32;
    let mut payload = Vec::new();
    leb(&mut payload, bodies.len() as u32);
    let mut changed = false;
    for (s, e) in bodies {
        match fold_body(&bytes[s..e]) {
            Some(nb) => {
                nfold += 1;
                changed = true;
                leb(&mut payload, nb.len() as u32);
                payload.extend_from_slice(&nb);
            }
            None => {
                leb(&mut payload, (e - s) as u32);
                payload.extend_from_slice(&bytes[s..e]);
            }
        }
    }
    if dbg {
        eprintln!("wasm-addroff: bodies={nfold}");
    }
    if !changed {
        return;
    }
    let mut sec = Vec::new();
    sec.push(10);
    leb(&mut sec, payload.len() as u32);
    sec.extend(payload);
    let mut out = bytes[..code_at].to_vec();
    out.extend(sec);
    out.extend_from_slice(&bytes[code_end..]);
    *bytes = out;
}

fn locate_code(bytes: &[u8]) -> Option<(usize, usize, Vec<(usize, usize)>)> {
    let mut p = 8usize;
    while p < bytes.len() {
        let id_at = p;
        let id = *bytes.get(p)?;
        p += 1;
        let sz = urd(bytes, &mut p)? as usize;
        let (s, e) = (p, p.saturating_add(sz));
        if e > bytes.len() {
            return None;
        }
        if id == 10 {
            let mut q = s;
            let nf = urd(bytes, &mut q)? as usize;
            let mut bodies = Vec::with_capacity(nf);
            for _ in 0..nf {
                let bsz = urd(bytes, &mut q)? as usize;
                let be = q + bsz;
                if be > e {
                    return None;
                }
                bodies.push((q, be));
                q = be;
            }
            if q != e {
                return None;
            }
            return Some((id_at, e, bodies));
        }
        p = e;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleb_enc(mut v: i32) -> Vec<u8> {
        let mut o = Vec::new();
        loop {
            let mut b = (v as u8) & 0x7f;
            v >>= 7;
            let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
            if !done {
                b |= 0x80;
            }
            o.push(b);
            if done {
                break;
            }
        }
        o
    }

    fn uleb_enc(v: u32) -> Vec<u8> {
        let mut o = Vec::new();
        leb(&mut o, v);
        o
    }

    fn i32c(k: i32) -> Vec<u8> {
        let mut o = vec![0x41];
        o.extend(sleb_enc(k));
        o
    }

    fn local(op: u8, i: u32) -> Vec<u8> {
        let mut o = vec![op];
        o.extend(uleb_enc(i));
        o
    }

    fn load(opc: u8, align: u32, off: u32) -> Vec<u8> {
        let mut o = vec![opc];
        o.extend(uleb_enc(align));
        o.extend(uleb_enc(off));
        o
    }

    #[test]
    fn const_local_folds_into_the_load_offset() {
        // i32.const 1000; local.tee 1; i32.load offset=108
        // local.get 2; i32.shl; local.get 1; i32.add; i32.load offset=0
        let mut body = vec![0x00];
        body.extend(i32c(1000));
        body.extend(local(0x22, 1));
        body.extend(load(0x28, 2, 108));
        body.extend(local(0x20, 2));
        body.push(0x74);
        body.extend(local(0x20, 1));
        body.push(0x6a);
        body.extend(load(0x28, 2, 0));
        body.push(0x0b);
        let got = fold_body(&body).expect("fold");
        let mut want = vec![0x00];
        want.extend(i32c(1000));
        want.extend(local(0x22, 1));
        want.extend(load(0x28, 2, 108));
        want.extend(local(0x20, 2));
        want.push(0x74);
        want.extend(load(0x28, 2, 1000));
        want.push(0x0b);
        assert_eq!(got, want);
    }

    #[test]
    fn skipped_store_does_not_reach_the_load() {
        // const K, then a block that may store M and falls through with
        // M, or branches out still holding K. The get after the block
        // has two reaching values.
        let mut body = vec![0x00];
        body.extend(i32c(10));
        body.extend(local(0x21, 1));
        body.extend_from_slice(&[0x02, 0x40]);
        body.extend(local(0x20, 0));
        body.extend_from_slice(&[0x0d, 0x00]);
        body.extend(i32c(99));
        body.extend(local(0x21, 1));
        body.push(0x0b);
        body.extend(local(0x20, 2));
        body.extend(local(0x20, 1));
        body.push(0x6a);
        body.extend(load(0x28, 2, 0));
        body.push(0x0b);
        assert!(fold_body(&body).is_none());
    }

    #[test]
    fn loop_store_hides_the_entry_const() {
        let mut body = vec![0x00];
        body.extend(i32c(10));
        body.extend(local(0x21, 1));
        body.extend_from_slice(&[0x03, 0x40]);
        body.extend(local(0x20, 2));
        body.extend(local(0x20, 1));
        body.push(0x6a);
        body.extend(load(0x28, 2, 0));
        body.extend(i32c(3));
        body.extend(local(0x21, 1));
        body.extend_from_slice(&[0x0c, 0x00, 0x0b, 0x0b]);
        assert!(fold_body(&body).is_none());
    }

    #[test]
    fn immediate_const_folds_without_a_local() {
        let mut body = vec![0x00];
        body.extend(local(0x20, 0));
        body.extend(i32c(40));
        body.push(0x6a);
        body.extend(load(0x29, 3, 8));
        body.push(0x0b);
        let got = fold_body(&body).expect("fold");
        let mut want = vec![0x00];
        want.extend(local(0x20, 0));
        want.extend(load(0x29, 3, 48));
        want.push(0x0b);
        assert_eq!(got, want);
    }
}
