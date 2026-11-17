//! PLIRON: `PLIRON_WASM_BLKHOIST=1` moves the straight-line code in front
//! of a `block` opener to the start of its body, so a value a leaf sets
//! is read right after it instead of across the openers (the `if` ->
//! `block`+`br_if` rewrite opens its blocks after the leaf that computes
//! the condition's inputs). Shapes: a diamond whose condition and both
//! arms read a value of the entry leaf, and the same diamond inside a
//! loop (the header's leaf must stay in the loop body, not move into a
//! block that a back edge skips). Each is compiled with and without the
//! knob, re-parsed and interpreted over a range of inputs; with the knob
//! no `local.set X; block..; local.get X` remains, without it there is
//! one (the shape still shows what the knob is for).

use std::sync::Mutex;
use waffle::*;

static ENV: Mutex<()> = Mutex::new(());

fn k(b: &mut FunctionBody, blk: Block, v: u32) -> Value {
    b.add_op(blk, Operator::I32Const { value: v }, &[], &[Type::I32])
}

fn op(b: &mut FunctionBody, blk: Block, o: Operator, a: Value, c: Value) -> Value {
    b.add_op(blk, o, &[a, c], &[Type::I32])
}

fn to(block: Block, args: Vec<Value>) -> BlockTarget {
    BlockTarget { block, args }
}

fn sig2(module: &mut Module) -> Signature {
    module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    })
}

/// diamond(a, x): v = a * x + 7; if v <u 100 { v * 3 } else { v ^ x }, then + v.
fn diamond(module: &mut Module) -> (Signature, FunctionBody) {
    let sig = sig2(module);
    let mut b = FunctionBody::new(module, sig);
    let e = b.entry;
    let (a, x) = (b.blocks[e].params[0].1, b.blocks[e].params[1].1);
    let t = b.add_block();
    let f = b.add_block();
    let j = b.add_block();
    let m = op(&mut b, e, Operator::I32Mul, a, x);
    let c7 = k(&mut b, e, 7);
    let v = op(&mut b, e, Operator::I32Add, m, c7);
    let c100 = k(&mut b, e, 100);
    let lt = op(&mut b, e, Operator::I32LtU, v, c100);
    let c3 = k(&mut b, t, 3);
    let vt = op(&mut b, t, Operator::I32Mul, v, c3);
    let vf = op(&mut b, f, Operator::I32Xor, v, x);
    let p = b.add_blockparam(j, Type::I32);
    let r = op(&mut b, j, Operator::I32Add, p, v);
    b.set_terminator(e, Terminator::CondBr { cond: lt, if_true: to(t, vec![]), if_false: to(f, vec![]) });
    b.set_terminator(t, Terminator::Br { target: to(j, vec![vt]) });
    b.set_terminator(f, Terminator::Br { target: to(j, vec![vf]) });
    b.set_terminator(j, Terminator::Return { values: vec![r] });
    (sig, b)
}

fn diamond_ref(a: u32, x: u32) -> u32 {
    let v = a.wrapping_mul(x).wrapping_add(7);
    let p = if v < 100 { v.wrapping_mul(3) } else { v ^ x };
    p.wrapping_add(v)
}

/// looped(a, x): `loop(i, acc)`: v = acc * 5 + i; if v & 1 { acc' = v ^ x }
/// else { acc' = v + a }; i' = i + 1; continue while i' <u (a & 7); return acc'.
fn looped(module: &mut Module) -> (Signature, FunctionBody) {
    let sig = sig2(module);
    let mut b = FunctionBody::new(module, sig);
    let e = b.entry;
    let (a, x) = (b.blocks[e].params[0].1, b.blocks[e].params[1].1);
    let z = k(&mut b, e, 0);
    let h = b.add_block();
    let t = b.add_block();
    let f = b.add_block();
    let l = b.add_block();
    let exit = b.add_block();
    let i = b.add_blockparam(h, Type::I32);
    let acc = b.add_blockparam(h, Type::I32);
    let c5 = k(&mut b, h, 5);
    let m = op(&mut b, h, Operator::I32Mul, acc, c5);
    let v = op(&mut b, h, Operator::I32Add, m, i);
    let c1 = k(&mut b, h, 1);
    let bit = op(&mut b, h, Operator::I32And, v, c1);
    let vt = op(&mut b, t, Operator::I32Xor, v, x);
    let vf = op(&mut b, f, Operator::I32Add, v, a);
    let nacc = b.add_blockparam(l, Type::I32);
    let c1b = k(&mut b, l, 1);
    let i1 = op(&mut b, l, Operator::I32Add, i, c1b);
    let c7 = k(&mut b, l, 7);
    let n = op(&mut b, l, Operator::I32And, a, c7);
    let more = op(&mut b, l, Operator::I32LtU, i1, n);
    b.set_terminator(e, Terminator::Br { target: to(h, vec![z, x]) });
    b.set_terminator(h, Terminator::CondBr { cond: bit, if_true: to(t, vec![]), if_false: to(f, vec![]) });
    b.set_terminator(t, Terminator::Br { target: to(l, vec![vt]) });
    b.set_terminator(f, Terminator::Br { target: to(l, vec![vf]) });
    b.set_terminator(l, Terminator::CondBr { cond: more, if_true: to(h, vec![i1, nacc]), if_false: to(exit, vec![]) });
    b.set_terminator(exit, Terminator::Return { values: vec![nacc] });
    (sig, b)
}

fn looped_ref(a: u32, x: u32) -> u32 {
    let (mut i, mut acc) = (0u32, x);
    loop {
        let v = acc.wrapping_mul(5).wrapping_add(i);
        acc = if v & 1 != 0 { v ^ x } else { v.wrapping_add(a) };
        i += 1;
        if i >= a & 7 {
            return acc;
        }
    }
}

/// `local.set X` followed by block/loop openers and `local.get X`.
fn parked(bytes: &[u8]) -> usize {
    use wasmparser::{Operator as O, Parser, Payload};
    let mut n = 0;
    for p in Parser::new(0).parse_all(bytes) {
        if let Payload::CodeSectionEntry(body) = p.unwrap() {
            let ops: Vec<O> = body
                .get_operators_reader()
                .unwrap()
                .into_iter()
                .map(|o| o.unwrap())
                .collect();
            for (i, o) in ops.iter().enumerate() {
                if let O::LocalSet { local_index } = o {
                    let mut j = i + 1;
                    while matches!(ops.get(j), Some(O::Block { .. } | O::Loop { .. })) {
                        j += 1;
                    }
                    if j > i + 1 && matches!(ops.get(j), Some(O::LocalGet { local_index: g }) if g == local_index) {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

type Shape = (fn(&mut Module) -> (Signature, FunctionBody), fn(u32, u32) -> u32);

fn run(on: bool, shape: Shape) -> usize {
    let _g = ENV.lock().unwrap();
    if on {
        std::env::set_var("PLIRON_WASM_BLKHOIST", "1");
    } else {
        std::env::remove_var("PLIRON_WASM_BLKHOIST");
    }
    let mut module = Module::empty();
    let (sig, body) = (shape.0)(&mut module);
    body.validate().expect("valid");
    module.funcs.push(FuncDecl::Body(sig, "f".into(), body));
    let bytes = module.to_wasm_bytes().expect("compile");
    std::env::remove_var("PLIRON_WASM_BLKHOIST");
    let mut m2 = Module::from_wasm_bytes(&bytes, &FrontendOptions::default()).expect("reparse");
    m2.expand_all_funcs().expect("expand");
    let f = m2.funcs.iter().next().expect("one function");
    let mut cx = InterpContext::new(&m2).expect("interp");
    for a in [0u32, 1, 2, 3, 7, 12, 30, 0xffff_fff0] {
        for x in [0u32, 1, 5, 99, 1000] {
            let r = cx
                .call(&m2, f, &[ConstVal::I32(a), ConstVal::I32(x)])
                .ok()
                .expect("returns");
            assert_eq!(r[0], ConstVal::I32((shape.1)(a, x)), "on={} a={} x={}", on, a, x);
        }
    }
    parked(&bytes)
}

#[test]
fn block_hoist_keeps_semantics() {
    for shape in [
        (diamond as fn(&mut Module) -> (Signature, FunctionBody), diamond_ref as fn(u32, u32) -> u32),
        (looped, looped_ref),
    ] {
        let off = run(false, shape);
        let on = run(true, shape);
        assert!(off > 0, "the shape no longer parks a value across block openers");
        assert_eq!(on, 0, "a value is still parked across block openers");
    }
}
