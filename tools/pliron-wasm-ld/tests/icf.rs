//! Identical-code folding (`icf::fold`, `PLIRON_WASM_ICF=1`) on
//! hand-built linked modules: what folds, what must not, and that the
//! result validates with every reference pointing at a kept body.

use pliron_wasm_ld::icf;
use wasm_encoder as we;
use wasmparser as wp;

/// A function body: `(i32) -> i32`, built by `f`.
type Gen = Box<dyn Fn(&mut we::Function)>;

/// A module with one import (`env.ext: (i32) -> i32`, function 0), the
/// given defined functions (1, 2, ...), each exported as `f<i>`, a table
/// whose element segment lists `table`, and a name section.
fn module(funcs: Vec<Gen>, table: &[u32]) -> Vec<u8> {
    let mut m = we::Module::new();
    let mut ts = we::TypeSection::new();
    ts.ty().function([we::ValType::I32], [we::ValType::I32]);
    m.section(&ts);
    let mut is = we::ImportSection::new();
    is.import("env", "ext", we::EntityType::Function(0));
    m.section(&is);
    let mut fs = we::FunctionSection::new();
    for _ in &funcs {
        fs.function(0);
    }
    m.section(&fs);
    let mut tab = we::TableSection::new();
    tab.table(we::TableType {
        element_type: we::RefType::FUNCREF,
        minimum: table.len() as u64 + 1,
        maximum: None,
        table64: false,
        shared: false,
    });
    m.section(&tab);
    let mut es = we::ExportSection::new();
    for i in 0..funcs.len() {
        es.export(&format!("f{}", i + 1), we::ExportKind::Func, i as u32 + 1);
    }
    m.section(&es);
    let mut els = we::ElementSection::new();
    els.active(
        Some(0),
        &we::ConstExpr::i32_const(1),
        we::Elements::Functions(table.to_vec().into()),
    );
    m.section(&els);
    let mut code = we::CodeSection::new();
    for g in &funcs {
        let mut f = we::Function::new([]);
        g(&mut f);
        f.instructions().end();
        code.function(&f);
    }
    m.section(&code);
    let mut names = we::NameSection::new();
    let mut nm = we::NameMap::new();
    nm.append(0, "ext");
    for i in 0..funcs.len() {
        nm.append(i as u32 + 1, &format!("fn{}", i + 1));
    }
    names.functions(&nm);
    m.section(&names);
    m.finish()
}

/// `x + k`
fn add(k: i32) -> Gen {
    Box::new(move |f| {
        f.instructions().local_get(0).i32_const(k).i32_add();
    })
}

/// `callee(x) + 1`
fn call(callee: u32) -> Gen {
    Box::new(move |f| {
        f.instructions().local_get(0).call(callee).i32_const(1).i32_add();
    })
}

struct Out {
    defined: usize,
    exports: Vec<(String, u32)>,
    table: Vec<u32>,
    names: Vec<(u32, String)>,
    calls: Vec<Vec<u32>>,
}

fn inspect(m: &[u8]) -> Out {
    wp::Validator::new().validate_all(m).expect("folded module validates");
    let mut o = Out { defined: 0, exports: vec![], table: vec![], names: vec![], calls: vec![] };
    for p in wp::Parser::new(0).parse_all(m) {
        match p.unwrap() {
            wp::Payload::FunctionSection(r) => o.defined = r.count() as usize,
            wp::Payload::ExportSection(r) => {
                for e in r {
                    let e = e.unwrap();
                    o.exports.push((e.name.to_string(), e.index));
                }
            }
            wp::Payload::ElementSection(r) => {
                for e in r {
                    if let wp::ElementItems::Functions(fs) = e.unwrap().items {
                        o.table.extend(fs.into_iter().map(|f| f.unwrap()));
                    }
                }
            }
            wp::Payload::CodeSectionEntry(b) => {
                let mut v = vec![];
                let mut ops = b.get_operators_reader().unwrap();
                while !ops.eof() {
                    if let wp::Operator::Call { function_index } = ops.read().unwrap() {
                        v.push(function_index);
                    }
                }
                o.calls.push(v);
            }
            wp::Payload::CustomSection(c) => {
                if let wp::KnownCustom::Name(r) = c.as_known() {
                    for s in r {
                        if let wp::Name::Function(nm) = s.unwrap() {
                            for n in nm {
                                let n = n.unwrap();
                                o.names.push((n.index, n.name.to_string()));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    o
}

#[test]
fn two_identical_functions_fold() {
    let m = module(vec![add(7), add(7), add(8)], &[1, 2, 3]);
    let (out, st) = icf::fold(&m).unwrap().expect("something folds");
    assert_eq!(st.removed, 1);
    let o = inspect(&out);
    assert_eq!(o.defined, 2);
    // Both exports survive and name the kept body; f3 is the next one.
    assert_eq!(o.exports, vec![("f1".into(), 1), ("f2".into(), 1), ("f3".into(), 2)]);
    // Table slots keep their positions; two now hold the same function.
    assert_eq!(o.table, vec![1, 1, 2]);
    // One name per kept body: the kept (lowest-index) member's.
    assert_eq!(o.names, vec![(0, "ext".into()), (1, "fn1".into()), (2, "fn3".into())]);
    assert!(out.len() < m.len());
}

#[test]
fn a_different_constant_does_not_fold() {
    let m = module(vec![add(7), add(9)], &[1, 2]);
    assert!(icf::fold(&m).unwrap().is_none());
}

#[test]
fn a_different_call_target_does_not_fold() {
    // fn3 calls fn1 (x+7), fn4 calls fn2 (x+9): same bytes, different callees.
    let m = module(vec![add(7), add(9), call(1), call(2)], &[]);
    assert!(icf::fold(&m).unwrap().is_none());
}

#[test]
fn an_import_and_a_defined_target_do_not_fold() {
    // fn2 calls the import, fn3 calls fn1: never equal.
    let m = module(vec![add(7), call(0), call(1)], &[]);
    assert!(icf::fold(&m).unwrap().is_none());
}

#[test]
fn callers_fold_once_their_callees_do() {
    // fn1 == fn2; fn3 calls fn1, fn4 calls fn2: both pairs fold.
    let m = module(vec![add(7), add(7), call(1), call(2)], &[3, 4]);
    let (out, st) = icf::fold(&m).unwrap().unwrap();
    assert_eq!(st.removed, 2);
    let o = inspect(&out);
    assert_eq!(o.defined, 2);
    assert_eq!(o.calls, vec![vec![], vec![1]]);
    assert_eq!(o.table, vec![2, 2]);
}

#[test]
fn mutually_recursive_identical_pairs_fold() {
    // fn1 <-> fn2 and fn3 <-> fn4, each `callee(x) + 1`: two cycles of the
    // same shape. The optimistic start folds them (one 2-cycle, or one
    // self-loop: fn1 and fn2 are themselves identical modulo each other).
    let m = module(vec![call(2), call(1), call(4), call(3)], &[1, 3]);
    let (out, st) = icf::fold(&m).unwrap().unwrap();
    assert_eq!(st.removed, 3, "all four are one class");
    let o = inspect(&out);
    assert_eq!(o.defined, 1);
    assert_eq!(o.calls, vec![vec![1]]);
    assert_eq!(o.table, vec![1, 1]);
}

#[test]
fn a_cycle_with_a_different_member_does_not_fold_with_a_plain_cycle() {
    // fn1 <-> fn2 is `call(x)+1` both ways; fn3 -> fn4 -> fn5 (x+9):
    // fn3 and fn4 look like fn1/fn2 until refinement sees fn5.
    let m = module(vec![call(2), call(1), call(4), call(5), add(9)], &[]);
    let (out, st) = icf::fold(&m).unwrap().unwrap();
    // fn1 and fn2 fold together; fn3 and fn4 stay apart from them.
    assert_eq!(st.removed, 1);
    let o = inspect(&out);
    assert_eq!(o.defined, 4);
}
