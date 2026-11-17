//! PLIRON: `PLIRON_WASM_BRTAB_SHARE=1` shares `br_table` edge blocks per
//! distinct (target, args) and branches straight to argument-less
//! targets. The function is compiled, re-parsed and interpreted for every
//! selector value, with and without the knob; the results must agree
//! with the source semantics, and the shared form must be smaller.

use std::sync::Mutex;
use waffle::*;

static ENV: Mutex<()> = Mutex::new(());

/// ```text
/// fn(sel, x) -> i32
/// entry: br_table sel [a(x), b, a(x), c(x), a(5), b, a(x)] default c(x)
/// a(p): return p + 1000
/// b:    return 7
/// c(q): return q + 2000
/// ```
fn table_module() -> (Module<'static>, Func) {
    let mut module = Module::empty();
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(&module, sig);
    let entry = body.entry;
    let (sel, x) = (body.blocks[entry].params[0].1, body.blocks[entry].params[1].1);
    let c5 = body.add_op(entry, Operator::I32Const { value: 5 }, &[], &[Type::I32]);
    let a = body.add_block();
    let b = body.add_block();
    let c = body.add_block();
    let p = body.add_blockparam(a, Type::I32);
    let q = body.add_blockparam(c, Type::I32);
    let k1000 = body.add_op(a, Operator::I32Const { value: 1000 }, &[], &[Type::I32]);
    let ra = body.add_op(a, Operator::I32Add, &[p, k1000], &[Type::I32]);
    body.set_terminator(a, Terminator::Return { values: vec![ra] });
    let k7 = body.add_op(b, Operator::I32Const { value: 7 }, &[], &[Type::I32]);
    body.set_terminator(b, Terminator::Return { values: vec![k7] });
    let k2000 = body.add_op(c, Operator::I32Const { value: 2000 }, &[], &[Type::I32]);
    let rc = body.add_op(c, Operator::I32Add, &[q, k2000], &[Type::I32]);
    body.set_terminator(c, Terminator::Return { values: vec![rc] });
    let to = |block, args: Vec<Value>| BlockTarget { block, args };
    body.set_terminator(
        entry,
        Terminator::Select {
            value: sel,
            targets: vec![
                to(a, vec![x]),
                to(b, vec![]),
                to(a, vec![x]),
                to(c, vec![x]),
                to(a, vec![c5]),
                to(b, vec![]),
                to(a, vec![x]),
            ],
            default: to(c, vec![x]),
        },
    );
    body.validate().expect("valid");
    let f = module.funcs.push(FuncDecl::Body(sig, "t".into(), body));
    (module, f)
}

fn expected(sel: u32, x: u32) -> u32 {
    match sel {
        0 | 2 | 6 => x + 1000,
        1 | 5 => 7,
        4 => 1005,
        _ => x + 2000,
    }
}

/// Compile under the given knob, re-parse, interpret every selector.
fn run(share: bool) -> usize {
    let _g = ENV.lock().unwrap();
    if share {
        std::env::set_var("PLIRON_WASM_BRTAB_SHARE", "1");
    } else {
        std::env::remove_var("PLIRON_WASM_BRTAB_SHARE");
    }
    let (module, _) = table_module();
    let bytes = module.to_wasm_bytes().expect("compile");
    std::env::remove_var("PLIRON_WASM_BRTAB_SHARE");
    let mut m2 = Module::from_wasm_bytes(&bytes, &FrontendOptions::default()).expect("reparse");
    m2.expand_all_funcs().expect("expand");
    let f = m2.funcs.iter().next().expect("one function");
    let mut cx = InterpContext::new(&m2).expect("interp");
    for sel in 0..10u32 {
        for x in [0u32, 3, 41] {
            let r = cx
                .call(&m2, f, &[ConstVal::I32(sel), ConstVal::I32(x)])
                .ok()
                .expect("returns");
            assert_eq!(
                r[0],
                ConstVal::I32(expected(sel, x)),
                "share={} sel={} x={}",
                share,
                sel,
                x
            );
        }
    }
    bytes.len()
}

#[test]
fn brtab_share_keeps_semantics_and_shrinks() {
    let stock = run(false);
    let shared = run(true);
    assert!(shared < stock, "shared {} B >= stock {} B", shared, stock);
}
