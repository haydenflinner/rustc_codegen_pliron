//! PLIRON: `PLIRON_WASM_CONSTLOCAL=1` keeps a constant used several times
//! in a local set at function entry instead of spelling it out at every
//! use. Shape: long `i64`/`i32`/`f64` constants (one IR value per use, as
//! the CLIF emitter makes them) used in the entry block, in a loop body,
//! in both arms of a branch and as an edge argument, next to short
//! constants that must stay immediates. Compiled with and without the
//! knob, re-parsed and interpreted over a range of inputs; the knob must
//! shrink the body.

use std::sync::Mutex;
use waffle::*;

static ENV: Mutex<()> = Mutex::new(());

const M: u64 = 0x8080_8080_8080_8080;
const K32: u32 = 0x9e37_79b9;
const PI: f64 = 3.141592653589793;

fn k64(b: &mut FunctionBody, blk: Block, v: u64) -> Value {
    b.add_op(blk, Operator::I64Const { value: v }, &[], &[Type::I64])
}

fn k32(b: &mut FunctionBody, blk: Block, v: u32) -> Value {
    b.add_op(blk, Operator::I32Const { value: v }, &[], &[Type::I32])
}

fn op(b: &mut FunctionBody, blk: Block, o: Operator, args: &[Value], ty: Type) -> Value {
    b.add_op(blk, o, args, &[ty])
}

fn to(block: Block, args: Vec<Value>) -> BlockTarget {
    BlockTarget { block, args }
}

/// f(a, x):
///   entry: n = (a & 15); acc0 = x ^ M; br loop(0, acc0)
///   loop(i, acc): if i <u n -> body else exit
///   body: t = (acc + M) ^ (x & M); h = i32(t) * K32 + K32;
///         if (h & 1) -> odd else even
///   odd:  br loop(i + 1, t ^ M)
///   even: br loop(i + 1, t + i64(f64(t & 0xffff) * PI + PI))
///   exit: return acc ^ M
fn shape(module: &mut Module) -> (Signature, FunctionBody) {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I64, Type::I64],
        returns: vec![Type::I64],
    });
    let mut b = FunctionBody::new(module, sig);
    let e = b.entry;
    let (a, x) = (b.blocks[e].params[0].1, b.blocks[e].params[1].1);
    let lp = b.add_block();
    let body = b.add_block();
    let odd = b.add_block();
    let even = b.add_block();
    let exit = b.add_block();

    let c15 = k64(&mut b, e, 15);
    let n = op(&mut b, e, Operator::I64And, &[a, c15], Type::I64);
    let m0 = k64(&mut b, e, M);
    let acc0 = op(&mut b, e, Operator::I64Xor, &[x, m0], Type::I64);
    let z = k64(&mut b, e, 0);

    let i = b.add_blockparam(lp, Type::I64);
    let acc = b.add_blockparam(lp, Type::I64);
    let lt = op(&mut b, lp, Operator::I64LtU, &[i, n], Type::I32);

    let m1 = k64(&mut b, body, M);
    let s = op(&mut b, body, Operator::I64Add, &[acc, m1], Type::I64);
    let m2 = k64(&mut b, body, M);
    let xm = op(&mut b, body, Operator::I64And, &[x, m2], Type::I64);
    let t = op(&mut b, body, Operator::I64Xor, &[s, xm], Type::I64);
    let t32 = op(&mut b, body, Operator::I32WrapI64, &[t], Type::I32);
    let ka = k32(&mut b, body, K32);
    let hm = op(&mut b, body, Operator::I32Mul, &[t32, ka], Type::I32);
    let kb = k32(&mut b, body, K32);
    let h = op(&mut b, body, Operator::I32Add, &[hm, kb], Type::I32);
    let one32 = k32(&mut b, body, 1);
    let bit = op(&mut b, body, Operator::I32And, &[h, one32], Type::I32);

    let one_o = k64(&mut b, odd, 1);
    let i1o = op(&mut b, odd, Operator::I64Add, &[i, one_o], Type::I64);
    let m3 = k64(&mut b, odd, M);
    let to_ = op(&mut b, odd, Operator::I64Xor, &[t, m3], Type::I64);

    let one_e = k64(&mut b, even, 1);
    let i1e = op(&mut b, even, Operator::I64Add, &[i, one_e], Type::I64);
    let ffff = k64(&mut b, even, 0xffff);
    let lo = op(&mut b, even, Operator::I64And, &[t, ffff], Type::I64);
    let fl = op(&mut b, even, Operator::F64ConvertI64U, &[lo], Type::F64);
    let pa = op(&mut b, even, Operator::F64Const { value: PI.to_bits() }, &[], Type::F64);
    let fm = op(&mut b, even, Operator::F64Mul, &[fl, pa], Type::F64);
    let pb = op(&mut b, even, Operator::F64Const { value: PI.to_bits() }, &[], Type::F64);
    let fs = op(&mut b, even, Operator::F64Add, &[fm, pb], Type::F64);
    let fi = op(&mut b, even, Operator::I64TruncF64U, &[fs], Type::I64);
    let te = op(&mut b, even, Operator::I64Add, &[t, fi], Type::I64);

    let m4 = k64(&mut b, exit, M);
    let r = op(&mut b, exit, Operator::I64Xor, &[acc, m4], Type::I64);

    b.set_terminator(e, Terminator::Br { target: to(lp, vec![z, acc0]) });
    b.set_terminator(lp, Terminator::CondBr { cond: lt, if_true: to(body, vec![]), if_false: to(exit, vec![]) });
    b.set_terminator(body, Terminator::CondBr { cond: bit, if_true: to(odd, vec![]), if_false: to(even, vec![]) });
    b.set_terminator(odd, Terminator::Br { target: to(lp, vec![i1o, to_]) });
    b.set_terminator(even, Terminator::Br { target: to(lp, vec![i1e, te]) });
    b.set_terminator(exit, Terminator::Return { values: vec![r] });
    (sig, b)
}

fn reference(a: u64, x: u64) -> u64 {
    let n = a & 15;
    let mut acc = x ^ M;
    let mut i = 0u64;
    while i < n {
        let t = acc.wrapping_add(M) ^ (x & M);
        let h = (t as u32).wrapping_mul(K32).wrapping_add(K32);
        acc = if h & 1 != 0 {
            t ^ M
        } else {
            t.wrapping_add(((t & 0xffff) as f64 * PI + PI) as u64)
        };
        i += 1;
    }
    acc ^ M
}

fn run(on: bool) -> usize {
    let _g = ENV.lock().unwrap();
    if on {
        std::env::set_var("PLIRON_WASM_CONSTLOCAL", "1");
    } else {
        std::env::remove_var("PLIRON_WASM_CONSTLOCAL");
    }
    let mut module = Module::empty();
    let (sig, body) = shape(&mut module);
    body.validate().expect("valid");
    module.funcs.push(FuncDecl::Body(sig, "f".into(), body));
    let bytes = module.to_wasm_bytes().expect("compile");
    std::env::remove_var("PLIRON_WASM_CONSTLOCAL");
    let mut m2 = Module::from_wasm_bytes(&bytes, &FrontendOptions::default()).expect("reparse");
    m2.expand_all_funcs().expect("expand");
    let f = m2.funcs.iter().next().expect("one function");
    let mut cx = InterpContext::new(&m2).expect("interp");
    for a in [0u64, 1, 2, 3, 7, 12, 15, 31] {
        for x in [0u64, 1, 5, 1000, M, u64::MAX, 0x1234_5678_9abc_def0] {
            let r = cx
                .call(&m2, f, &[ConstVal::I64(a), ConstVal::I64(x)])
                .ok()
                .expect("returns");
            assert_eq!(r[0], ConstVal::I64(reference(a, x)), "on={} a={} x={:#x}", on, a, x);
        }
    }
    bytes.len()
}

#[test]
fn const_locals_keep_semantics_and_shrink() {
    let off = run(false);
    let on = run(true);
    assert!(on < off, "constant locals did not shrink the body: {} >= {}", on, off);
}
