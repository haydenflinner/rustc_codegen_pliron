// `PLIRON_MEMCPYOPT_NOPCOPY=1` target shape: `phisimp` binding a
// block-parameter copy destination to its source leaves a
// `memcpy(a, a)`, which pins the `Option<Fb>` temp out of sroa
// ("accesses: out of range / self-copy"). The test stanza checks the
// sroa refusal is present without the knob and gone with it.

use std::fmt::Write as _;

pub enum Fb { A(u8), B(bool), C(Box<u8>), None }

pub struct Ctx { sim: Sim, out: Out }
pub struct Sim { ns: u8 }
pub struct Out { flag: bool }

#[inline(never)]
pub fn get_feedback_start(s: &Sim, h: u64) -> Result<Fb, u8> {
    if s.ns == 7 { Err(h as u8) } else if h & 1 == 0 { Ok(Fb::None) } else { Ok(Fb::B(true)) }
}
#[inline(never)]
pub fn get_feedback_end(s: &Sim, h: u64) -> Fb {
    if s.ns == 9 { Fb::A(h as u8) } else { Fb::None }
}
#[inline(never)]
pub fn end_hint(o: &mut Out, n: u64) -> u8 { o.flag = n & 2 != 0; (n & 1) as u8 }
#[inline(never)]
pub fn start_hint(o: &mut Out, n: u64, ns: u8) -> u8 { o.flag = ns > 0; ((n ^ ns as u64) & 3) as u8 }
#[inline(never)]
fn boxit(e: Fb) -> Box<Fb> { Box::new(e) }

pub struct S {
    tag_start: Option<usize>,
    is_end: bool,
    hash: u64,
    ttype: Option<u8>,
    cdata: bool,
}

pub enum Res { Ok, Err(Box<u8>), Directive(usize, Box<Fb>) }

#[inline]
fn try_apply(s: &mut S, ctx: &mut Ctx) -> Result<Option<Fb>, u8> {
    let feedback = if s.is_end {
        get_feedback_end(&ctx.sim, s.hash)
    } else {
        get_feedback_start(&ctx.sim, s.hash)?
    };
    Ok(match feedback {
        Fb::A(t) => { s.ttype = Some(t); None }
        Fb::B(c) => { s.cdata = c; None }
        Fb::C(_) => Some(feedback),
        Fb::None => None,
    })
}

#[inline(never)]
pub fn selfcopy(s: &mut S, ctx: &mut Ctx, input: &[u8]) -> Result<Res, Box<u8>> {
    let tag_start = s.tag_start.take().ok_or_else(|| Box::new(9u8))?;
    let unhandled = try_apply(s, ctx).map_err(|e| Box::new(e))?;
    let is_end = s.is_end;
    s.is_end = false;
    if let Some(u) = unhandled {
        return Ok(Res::Directive(tag_start, boxit(u)));
    }
    let d = emit_tag_hint(s, ctx, input, is_end);
    match d {
        0 => Ok(Res::Ok),
        _ => {
            let f = s.ttype.take();
            let _ = f;
            Ok(Res::Directive(tag_start, Box::new(Fb::None)))
        }
    }
}

#[inline]
fn emit_tag_hint(s: &mut S, ctx: &mut Ctx, input: &[u8], is_end: bool) -> u8 {
    let name_start = s.tag_start.unwrap_or(0);
    let end = input.len().min(name_start + 8);
    let name = (s.hash ^ (end as u64)) & 0xffff;
    if is_end {
        end_hint(&mut ctx.out, name)
    } else {
        s.hash = name;
        let ns = ctx.sim.ns;
        start_hint(&mut ctx.out, name, ns)
    }
}

fn main() {
    let mut s = S { tag_start: Some(3), is_end: false, hash: 17, ttype: None, cdata: false };
    let mut ctx = Ctx { sim: Sim { ns: 1 }, out: Out { flag: false } };
    let mut w = String::new();
    for i in 0..100u64 {
        s.tag_start = Some(i as usize);
        s.is_end = i & 1 == 0;
        s.hash = i * 31 + 5;
        match selfcopy(&mut s, &mut ctx, b"<abcdef>") {
            Ok(Res::Ok) => write!(w, "ok ").unwrap(),
            Ok(Res::Err(b)) => write!(w, "e{} ", b).unwrap(),
            Ok(Res::Directive(t, _)) => write!(w, "d{} ", t).unwrap(),
            Err(e) => write!(w, "E{} ", e).unwrap(),
        }
    }
    println!("{}", w);
}
