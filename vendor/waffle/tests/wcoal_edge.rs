//! PLIRON: `PLIRON_WCOAL_EDGE=1` merges a block parameter's local with an
//! edge argument's local when they do not interfere. Shapes where a wrong
//! merge would show: a rotating loop (`(a, b) -> (b, a + b)`, the swap
//! problem), a loop whose parameter is read after the next value is
//! computed, and a join reached from three edges with different
//! arguments. Each is compiled with and without the knob, re-parsed and
//! interpreted over a range of inputs.

use std::sync::Mutex;
use waffle::*;

static ENV: Mutex<()> = Mutex::new(());

fn sig2(module: &mut Module) -> Signature {
    module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    })
}

fn k(body: &mut FunctionBody, blk: Block, v: u32) -> Value {
    body.add_op(blk, Operator::I32Const { value: v }, &[], &[Type::I32])
}

fn op(body: &mut FunctionBody, blk: Block, o: Operator, a: Value, b: Value) -> Value {
    body.add_op(blk, o, &[a, b], &[Type::I32])
}

fn to(block: Block, args: Vec<Value>) -> BlockTarget {
    BlockTarget { block, args }
}

/// fib(n, x): `loop(i, a, b): if i <u n -> loop(i+1, b, a+b) else exit(a ^ x)`.
fn fib(module: &mut Module) -> (Signature, FunctionBody) {
    let sig = sig2(module);
    let mut b = FunctionBody::new(module, sig);
    let e = b.entry;
    let (n, x) = (b.blocks[e].params[0].1, b.blocks[e].params[1].1);
    let z = k(&mut b, e, 0);
    let one = k(&mut b, e, 1);
    let lp = b.add_block();
    let body = b.add_block();
    let exit = b.add_block();
    let i = b.add_blockparam(lp, Type::I32);
    let a = b.add_blockparam(lp, Type::I32);
    let c = b.add_blockparam(lp, Type::I32);
    let lt = op(&mut b, lp, Operator::I32LtU, i, n);
    let one2 = k(&mut b, body, 1);
    let i1 = op(&mut b, body, Operator::I32Add, i, one2);
    let s = op(&mut b, body, Operator::I32Add, a, c);
    let r = op(&mut b, exit, Operator::I32Xor, a, x);
    b.set_terminator(e, Terminator::Br { target: to(lp, vec![z, z, one]) });
    b.set_terminator(lp, Terminator::CondBr { cond: lt, if_true: to(body, vec![]), if_false: to(exit, vec![]) });
    b.set_terminator(body, Terminator::Br { target: to(lp, vec![i1, c, s]) });
    b.set_terminator(exit, Terminator::Return { values: vec![r] });
    (sig, b)
}

fn fib_ref(n: u32, x: u32) -> u32 {
    let (mut a, mut c) = (0u32, 1u32);
    for _ in 0..n {
        let s = a.wrapping_add(c);
        a = c;
        c = s;
    }
    a ^ x
}

/// late(n, x): `loop(i, acc): if i <u n -> body` where `body` computes
/// `i1 = i + 1` and then still reads `i` (`acc + i * x`) before the back
/// edge `loop(i1, acc')`; exit returns `acc`.
fn late(module: &mut Module) -> (Signature, FunctionBody) {
    let sig = sig2(module);
    let mut b = FunctionBody::new(module, sig);
    let e = b.entry;
    let (n, x) = (b.blocks[e].params[0].1, b.blocks[e].params[1].1);
    let z = k(&mut b, e, 0);
    let lp = b.add_block();
    let body = b.add_block();
    let exit = b.add_block();
    let i = b.add_blockparam(lp, Type::I32);
    let acc = b.add_blockparam(lp, Type::I32);
    let lt = op(&mut b, lp, Operator::I32LtU, i, n);
    let one = k(&mut b, body, 1);
    let i1 = op(&mut b, body, Operator::I32Add, i, one);
    let m = op(&mut b, body, Operator::I32Mul, i, x);
    let acc1 = op(&mut b, body, Operator::I32Add, acc, m);
    let acc2 = op(&mut b, body, Operator::I32Xor, acc1, i1);
    b.set_terminator(e, Terminator::Br { target: to(lp, vec![z, x]) });
    b.set_terminator(lp, Terminator::CondBr { cond: lt, if_true: to(body, vec![]), if_false: to(exit, vec![]) });
    b.set_terminator(body, Terminator::Br { target: to(lp, vec![i1, acc2]) });
    b.set_terminator(exit, Terminator::Return { values: vec![acc] });
    (sig, b)
}

fn late_ref(n: u32, x: u32) -> u32 {
    let mut acc = x;
    for i in 0..n {
        acc = acc.wrapping_add(i.wrapping_mul(x)) ^ i.wrapping_add(1);
    }
    acc
}

/// join(s, x): three edges into `j(p, q)`: `(x, s)`, `(s, x)`, `(x+s, x)`
/// picked by `s & 3`; `j` returns `p * 3 - q`.
fn join(module: &mut Module) -> (Signature, FunctionBody) {
    let sig = sig2(module);
    let mut b = FunctionBody::new(module, sig);
    let e = b.entry;
    let (s, x) = (b.blocks[e].params[0].1, b.blocks[e].params[1].1);
    let three = k(&mut b, e, 3);
    let sel = op(&mut b, e, Operator::I32And, s, three);
    let sum = op(&mut b, e, Operator::I32Add, x, s);
    let j = b.add_block();
    let p = b.add_blockparam(j, Type::I32);
    let q = b.add_blockparam(j, Type::I32);
    let three2 = k(&mut b, j, 3);
    let p3 = op(&mut b, j, Operator::I32Mul, p, three2);
    let r = op(&mut b, j, Operator::I32Sub, p3, q);
    b.set_terminator(
        e,
        Terminator::Select {
            value: sel,
            targets: vec![to(j, vec![x, s]), to(j, vec![s, x]), to(j, vec![sum, x])],
            default: to(j, vec![x, s]),
        },
    );
    b.set_terminator(j, Terminator::Return { values: vec![r] });
    (sig, b)
}

fn join_ref(s: u32, x: u32) -> u32 {
    let (p, q) = match s & 3 {
        1 => (s, x),
        2 => (x.wrapping_add(s), x),
        _ => (x, s),
    };
    p.wrapping_mul(3).wrapping_sub(q)
}

type Shape = (fn(&mut Module) -> (Signature, FunctionBody), fn(u32, u32) -> u32);

fn run(on: bool, shape: Shape) -> usize {
    let _g = ENV.lock().unwrap();
    if on {
        std::env::set_var("PLIRON_WCOAL_EDGE", "1");
    } else {
        std::env::remove_var("PLIRON_WCOAL_EDGE");
    }
    let mut module = Module::empty();
    let (sig, body) = (shape.0)(&mut module);
    body.validate().expect("valid");
    module.funcs.push(FuncDecl::Body(sig, "f".into(), body));
    let bytes = module.to_wasm_bytes().expect("compile");
    std::env::remove_var("PLIRON_WCOAL_EDGE");
    let mut m2 = Module::from_wasm_bytes(&bytes, &FrontendOptions::default()).expect("reparse");
    m2.expand_all_funcs().expect("expand");
    let f = m2.funcs.iter().next().expect("one function");
    let mut cx = InterpContext::new(&m2).expect("interp");
    for a in [0u32, 1, 2, 3, 7, 12, 30] {
        for x in [0u32, 1, 5, 1000] {
            let r = cx
                .call(&m2, f, &[ConstVal::I32(a), ConstVal::I32(x)])
                .ok()
                .expect("returns");
            assert_eq!(r[0], ConstVal::I32((shape.1)(a, x)), "on={} a={} x={}", on, a, x);
        }
    }
    bytes.len()
}

#[test]
fn edge_coalescing_keeps_semantics() {
    for shape in [(fib as fn(&mut Module) -> (Signature, FunctionBody), fib_ref as fn(u32, u32) -> u32), (late, late_ref), (join, join_ref)] {
        let off = run(false, shape);
        let on = run(true, shape);
        assert!(on <= off, "edge coalescing grew the body: {} > {}", on, off);
    }
}
