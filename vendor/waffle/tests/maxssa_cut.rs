//! PLIRON: `PLIRON_WASM_MAXSSA_CUTALIAS=1` drops a cut block's parameter
//! when every edge passes the same value defined outside the blocks the
//! reducifier may duplicate. An irreducible loop (header `h`, side entry
//! into `x`) uses values from the entry block in both loop blocks and
//! after the exit, and a value defined in `x` (which the reducifier
//! copies for the side entry) after the exit. The function is compiled
//! both ways, re-parsed and interpreted; the knob's form must agree with
//! the source and be smaller.

use std::sync::Mutex;
use waffle::*;

static ENV: Mutex<()> = Mutex::new(());

/// ```text
/// fn(c, n) -> i32
/// entry:   k = n * 3; m = n ^ 5; br_if c -> h(0) else x(0)
/// h(i):    i1 = i + k; jump x(i1)
/// x(j):    j1 = j + m + 1; d = j1 * 7; br_if (j1 <u 100) -> h(j1) else exit(j1)
/// exit(r): return r + k + d
/// ```
/// `exit` is reached from `x` and from its copy: `d` has a different
/// name in each, so `exit` must keep a parameter for it, while `k` (from
/// the entry block) needs none.
fn irreducible_module() -> Module<'static> {
    let mut module = Module::empty();
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(&module, sig);
    let e = body.entry;
    let (c, n) = (body.blocks[e].params[0].1, body.blocks[e].params[1].1);
    let i32c = |body: &mut FunctionBody, blk, v: u32| {
        body.add_op(blk, Operator::I32Const { value: v }, &[], &[Type::I32])
    };
    let k3 = i32c(&mut body, e, 3);
    let k = body.add_op(e, Operator::I32Mul, &[n, k3], &[Type::I32]);
    let k5 = i32c(&mut body, e, 5);
    let m = body.add_op(e, Operator::I32Xor, &[n, k5], &[Type::I32]);
    let z = i32c(&mut body, e, 0);
    let h = body.add_block();
    let x = body.add_block();
    let exit = body.add_block();
    let i = body.add_blockparam(h, Type::I32);
    let j = body.add_blockparam(x, Type::I32);
    let r = body.add_blockparam(exit, Type::I32);
    let i1 = body.add_op(h, Operator::I32Add, &[i, k], &[Type::I32]);
    let one = i32c(&mut body, x, 1);
    let j0 = body.add_op(x, Operator::I32Add, &[j, m], &[Type::I32]);
    let j1 = body.add_op(x, Operator::I32Add, &[j0, one], &[Type::I32]);
    let k7 = i32c(&mut body, x, 7);
    let d = body.add_op(x, Operator::I32Mul, &[j1, k7], &[Type::I32]);
    let k100 = i32c(&mut body, x, 100);
    let lt = body.add_op(x, Operator::I32LtU, &[j1, k100], &[Type::I32]);
    let r0 = body.add_op(exit, Operator::I32Add, &[r, k], &[Type::I32]);
    let r1 = body.add_op(exit, Operator::I32Add, &[r0, d], &[Type::I32]);
    let to = |block, args: Vec<Value>| BlockTarget { block, args };
    body.set_terminator(
        e,
        Terminator::CondBr { cond: c, if_true: to(h, vec![z]), if_false: to(x, vec![z]) },
    );
    body.set_terminator(h, Terminator::Br { target: to(x, vec![i1]) });
    body.set_terminator(
        x,
        Terminator::CondBr { cond: lt, if_true: to(h, vec![j1]), if_false: to(exit, vec![j1]) },
    );
    body.set_terminator(exit, Terminator::Return { values: vec![r1] });
    body.validate().expect("valid");
    module.funcs.push(FuncDecl::Body(sig, "irr".into(), body));
    module
}

fn expected(c: u32, n: u32) -> u32 {
    let (k, m) = (n.wrapping_mul(3), n ^ 5);
    let mut v = if c != 0 { k } else { 0 };
    loop {
        v = v.wrapping_add(m).wrapping_add(1);
        if v >= 100 {
            return v.wrapping_add(k).wrapping_add(v.wrapping_mul(7));
        }
        v = v.wrapping_add(k);
    }
}

fn run(on: bool) -> usize {
    let _g = ENV.lock().unwrap();
    if on {
        std::env::set_var("PLIRON_WASM_MAXSSA_CUTALIAS", "1");
    } else {
        std::env::set_var("PLIRON_WASM_MAXSSA_CUTALIAS", "0");
    }
    let bytes = irreducible_module().to_wasm_bytes().expect("compile");
    std::env::remove_var("PLIRON_WASM_MAXSSA_CUTALIAS");
    let mut m2 = Module::from_wasm_bytes(&bytes, &FrontendOptions::default()).expect("reparse");
    m2.expand_all_funcs().expect("expand");
    let f = m2.funcs.iter().next().expect("one function");
    let mut cx = InterpContext::new(&m2).expect("interp");
    for c in [0u32, 1] {
        // n = 200: the side entry exits on its first pass through `x`.
        for n in [1u32, 7, 30, 99, 200] {
            let r = cx
                .call(&m2, f, &[ConstVal::I32(c), ConstVal::I32(n)])
                .ok()
                .expect("returns");
            assert_eq!(r[0], ConstVal::I32(expected(c, n)), "on={} c={} n={}", on, c, n);
        }
    }
    bytes.len()
}

#[test]
fn cut_alias_keeps_semantics_and_shrinks() {
    let stock = run(false);
    let on = run(true);
    // The parameters it drops are often coalesced into their argument's
    // local anyway (no bytes), so only "not larger" holds in general; the
    // parameter count is checked in `reducify.rs`'s unit test.
    assert!(on <= stock, "cut-alias {} B > stock {} B", on, stock);
}
