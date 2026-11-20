// `PLIRON_PARTINLINE`: a callee's entry guard (enum drop glue: "return unless
// the variant owns something") is copied in front of each call the inliner
// kept. `Fb::Req` owns a boxed closure that holds a counted token (lol_html's
// `TreeBuilderFeedback::RequestLexeme`), so `drop_glue::<Fb>` is the box's
// drop behind a one-variant test. The output counts the token drops: a guard copied with the wrong
// sense, or a call left out, changes it.
use std::sync::atomic::{AtomicU32, Ordering};

static DROPS: AtomicU32 = AtomicU32::new(0);

pub struct Tok(u32);
impl Tok {
    // A method on `&self`: the closure captures the whole token (edition
    // 2021 captures `t.0` alone for a field read), so the box owns it.
    #[inline(never)]
    fn get(&self) -> u32 {
        self.0
    }
}
impl Drop for Tok {
    fn drop(&mut self) {
        DROPS.fetch_add(self.0 & 1 | 1, Ordering::Relaxed);
    }
}

pub enum Fb {
    Switch(u8),
    Allow(bool),
    Req(Box<dyn FnMut(u32) -> u32>),
    None,
}

#[inline(never)]
fn make(x: u32) -> Fb {
    match x % 13 {
        0 => {
            let t = Tok(x);
            let v = vec![format!("{x}"), String::from("q")];
            Fb::Req(Box::new(move |y| y.wrapping_mul(t.get()).wrapping_add(v.len() as u32)))
        }
        1 | 5 => Fb::Switch(x as u8),
        2 => Fb::Allow(x % 2 == 0),
        _ => Fb::None,
    }
}

#[inline(never)]
fn finish(x: u32, st: &mut u64) -> Option<u32> {
    let mut fb = make(x);
    let r = match &mut fb {
        Fb::Switch(t) => {
            *st += *t as u64;
            None
        }
        Fb::Allow(a) => {
            *st += *a as u64;
            None
        }
        Fb::Req(f) => Some(f(3)),
        Fb::None => None,
    };
    drop(fb);
    r
}

#[inline(never)]
fn finish_known(x: u32) -> u32 {
    // The variant is known at the drop: the copied guard folds.
    let fb = if x % 2 == 0 { Fb::Switch(x as u8) } else { Fb::None };
    let n = matches!(fb, Fb::Switch(_)) as u32;
    drop(fb);
    n
}

#[inline(never)]
fn drop_any(x: u32) {
    // The variant is unknown at the drop: the guard decides, at run time,
    // whether the box (and its counted token) is dropped.
    drop(make(x));
}

fn main() {
    let mut st = 0u64;
    let mut t = 0u64;
    for i in 0..20_000u32 {
        if let Some(v) = finish(i, &mut st) {
            t = t.wrapping_add(v as u64);
        }
        t += finish_known(i) as u64;
        drop_any(i);
    }
    println!("{t} {st} {}", DROPS.load(Ordering::Relaxed));
}
