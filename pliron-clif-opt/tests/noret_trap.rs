//! `TrapCode::NORETURN_CALL` (`PLIRON_NORETTRAP=1` lowering): the trap after
//! a call that cannot return is dropped at emission when code follows it,
//! and kept when it ends the function (the return address must stay inside
//! it) or when the trap is an ordinary one (`user2`, a may-return callee).
//! A `try_call` whose continuation is such a lone trap emits nothing after
//! the call, and its dead continuation block emits nothing either.
//!
//! Compiles for the host ISA and reads the trap and call-site records.
//! On an Apple-silicon Mac the x86-64 leg runs under Rosetta:
//! `cargo test --target x86_64-apple-darwin -p pliron-clif-opt --test noret_trap`.

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::TrapCode;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, isa};

struct Out {
    /// (offset, code) of every emitted trap.
    traps: Vec<(u32, TrapCode)>,
    ret_addrs: Vec<u32>,
    len: u32,
}

impl Out {
    fn count(&self, code: TrapCode) -> usize {
        self.traps.iter().filter(|t| t.1 == code).count()
    }
    /// Traps sitting at a call's return address.
    fn after_call(&self, code: TrapCode) -> usize {
        self.traps
            .iter()
            .filter(|t| t.1 == code && self.ret_addrs.contains(&t.0))
            .count()
    }
}

fn compile(src: &str) -> Out {
    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    let isa = isa::lookup(target_lexicon::Triple::host())
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap();
    let func = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut ctx = Context::for_function(func);
    let code = ctx
        .compile(&*isa, &mut ControlPlane::default())
        .map_err(|e| format!("{e:?}"))
        .unwrap();
    let buf = &code.buffer;
    Out {
        traps: buf.traps().iter().map(|t| (t.offset, t.code)).collect(),
        ret_addrs: buf.call_sites().map(|c| c.ret_addr).collect(),
        len: buf.data().len() as u32,
    }
}

/// Three calls into a noreturn callee, each in its own block, then a
/// returning block: `trap CODE` after each call.
fn three_sites(code: &str) -> String {
    format!(
        "function %f(i32) -> i32 {{
    sig0 = (i32)
    fn0 = %die sig0
block0(v0: i32):
    brif v0, block1, block4
block1:
    v1 = iconst.i32 1
    brif v0, block2, block3
block2:
    call fn0(v1)
    trap {code}
block3:
    v2 = iconst.i32 2
    call fn0(v2)
    trap {code}
block4:
    v3 = iconst.i32 3
    v6 = iconst.i32 0
    v4 = icmp eq v0, v6
    brif v4, block5, block6
block5:
    call fn0(v3)
    trap {code}
block6:
    v5 = iconst.i32 7
    return v5
}}"
    )
}

#[test]
fn noreturn_trap_dropped_when_code_follows() {
    let o = compile(&three_sites("user250"));
    let n = o.count(TrapCode::NORETURN_CALL);
    // Only a trap that ends the function survives.
    assert!(n <= 1, "{} NORETURN_CALL traps kept: {:?}", n, o.traps);
    for &(off, c) in &o.traps {
        if c == TrapCode::NORETURN_CALL {
            assert!(off + 4 >= o.len.saturating_sub(4), "kept mid-function: {off} of {}", o.len);
        }
    }
    assert_eq!(o.ret_addrs.len(), 3);
}

#[test]
fn ordinary_trap_after_call_kept() {
    // `user2` is lowering's plain `unreachable`: the callee may return.
    let o = compile(&three_sites("user2"));
    let c = TrapCode::unwrap_user(2);
    assert_eq!(o.count(c), 3, "{:?}", o.traps);
    assert_eq!(o.after_call(c), 3, "{:?} / {:?}", o.traps, o.ret_addrs);
}

#[test]
fn noreturn_trap_kept_at_function_end() {
    let o = compile(
        "function %f(i32) {
    sig0 = (i32)
    fn0 = %die sig0
block0(v0: i32):
    call fn0(v0)
    trap user250
}",
    );
    assert_eq!(o.count(TrapCode::NORETURN_CALL), 1, "{:?}", o.traps);
    assert_eq!(o.after_call(TrapCode::NORETURN_CALL), 1);
}

#[test]
fn noreturn_trap_without_call_kept() {
    // The code only promises "right after a noreturn call"; without one
    // the trap stays.
    let o = compile(
        "function %f(i32) -> i32 {
block0(v0: i32):
    brif v0, block1, block2
block1:
    trap user250
block2:
    v1 = iconst.i32 7
    return v1
}",
    );
    assert_eq!(o.count(TrapCode::NORETURN_CALL), 1, "{:?}", o.traps);
}

/// An invoke of a noreturn callee: `try_call` whose normal return goes to
/// a lone trap, then code (the handler and a returning path).
fn invoke(code: &str) -> String {
    format!(
        "function %f(i64) -> i64 tail {{
    sig0 = (i64) tail
    fn0 = %die sig0
block0(v0: i64):
    brif v0, block1, block4
block1:
    try_call fn0(v0), sig0, block2(), [ default: block3(exn0) ]
block2:
    trap {code}
block3(v1: i64) cold:
    return v1
block4:
    v2 = iconst.i64 7
    return v2
}}"
    )
}

#[test]
fn noreturn_try_call_continuation_elided() {
    let o = compile(&invoke("user250"));
    assert_eq!(o.ret_addrs.len(), 1);
    // Nothing after the call: neither the inline continuation trap nor
    // the (dead) lone continuation block, unless it ends the function.
    assert_eq!(o.after_call(TrapCode::NORETURN_CALL), 0, "{:?} / {:?}", o.traps, o.ret_addrs);
    assert!(o.count(TrapCode::NORETURN_CALL) <= 1, "{:?}", o.traps);
}

#[test]
fn ordinary_try_call_continuation_kept() {
    let o = compile(&invoke("user2"));
    assert_eq!(o.after_call(TrapCode::unwrap_user(2)), 1, "{:?} / {:?}", o.traps, o.ret_addrs);
}
