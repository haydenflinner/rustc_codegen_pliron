//! Static constructors: `__wasm_call_ctors` calls the `.init_array`
//! entries by priority, and in a module where nothing else would call it
//! every exported function is wrapped so it has run, once, before the
//! export's body. The objects are written by hand in the `pliron.link`
//! format; `tests/wasm/smoke.sh` runs the same thing from Rust source in
//! node.
// r[verify wasm.ld.ctors]

use std::collections::HashMap;

use wasm_encoder as we;
use wasmparser as wp;

/// A defined function: `()`, or `(i32) -> i32` when `arg`. `calls` names
/// the `env` function its body calls first.
struct F {
    name: &'static str,
    global: bool,
    arg: bool,
    calls: Option<&'static str>,
}

fn f(name: &'static str) -> F {
    F {
        name,
        global: false,
        arg: false,
        calls: None,
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend((s.len() as u32).to_le_bytes());
    out.extend(s.as_bytes());
}

/// An object with `funcs` and one `.init_array` data object per entry of
/// `ctors`: (priority, or `None` for a plain `.init_array`, function).
fn object(funcs: &[F], ctors: &[(Option<u32>, &str)]) -> Vec<u8> {
    let imports: Vec<&str> = funcs.iter().filter_map(|f| f.calls).collect();
    let mut m = we::Module::new();
    let mut ts = we::TypeSection::new();
    ts.ty().function([], []);
    ts.ty().function([we::ValType::I32], [we::ValType::I32]);
    m.section(&ts);
    let mut is = we::ImportSection::new();
    for i in &imports {
        is.import("env", i, we::EntityType::Function(0));
    }
    m.section(&is);
    let mut fs = we::FunctionSection::new();
    let mut es = we::ExportSection::new();
    let mut code = we::CodeSection::new();
    let mut link = Vec::new();
    link.extend((funcs.len() as u32).to_le_bytes());
    for (i, f) in funcs.iter().enumerate() {
        fs.function(f.arg as u32);
        es.export(f.name, we::ExportKind::Func, (imports.len() + i) as u32);
        let mut b = we::Function::new([]);
        if let Some(c) = f.calls {
            let c = imports.iter().position(|i| *i == c).unwrap();
            b.instruction(&we::Instruction::Call(c as u32));
        }
        if f.arg {
            b.instruction(&we::Instruction::LocalGet(0));
        }
        b.instruction(&we::Instruction::End);
        code.function(&b);
        put_str(&mut link, f.name);
        link.push(f.global as u8);
    }
    m.section(&fs);
    m.section(&es);
    m.section(&code);
    link.extend((ctors.len() as u32).to_le_bytes());
    for (i, (_, func)) in ctors.iter().enumerate() {
        put_str(&mut link, &format!("ctor_slot{i}"));
        link.push(4); // INIT_ARRAY
        link.extend(4u32.to_le_bytes()); // align
        link.extend(4u32.to_le_bytes()); // size
        link.extend([0u8; 4]);
        link.extend(1u32.to_le_bytes()); // one relocation:
        link.extend(0u32.to_le_bytes()); // offset 0,
        link.push(1); // a function,
        put_str(&mut link, func);
        link.extend(0u32.to_le_bytes()); // addend 0
    }
    // The trailer: (data index, priority) of the `.init_array.N` entries,
    // left out when there are none (as the backend does).
    let prios: Vec<(usize, u32)> = ctors
        .iter()
        .enumerate()
        .filter_map(|(i, (p, _))| p.map(|p| (i, p)))
        .collect();
    if !prios.is_empty() {
        link.extend((prios.len() as u32).to_le_bytes());
        for (i, p) in prios {
            link.extend((i as u32).to_le_bytes());
            link.extend(p.to_le_bytes());
        }
    }
    m.section(&we::CustomSection {
        name: "pliron.link".into(),
        data: link.into(),
    });
    m.finish()
}

/// The linked module: export name -> function name, and each function's
/// body as text, calls by callee name.
struct Linked {
    exports: HashMap<String, String>,
    bodies: HashMap<String, Vec<String>>,
}

fn link(test: &str, objects: &[Vec<u8>], args: &[&str]) -> Linked {
    let dir = std::env::temp_dir().join(format!("pliron-wasm-ld-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut argv = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        let p = dir.join(format!("{i}.o"));
        std::fs::write(&p, o).unwrap();
        argv.push(p.to_str().unwrap().to_string());
    }
    let out = dir.join("out.wasm");
    argv.extend(args.iter().map(|a| a.to_string()));
    argv.extend(["--no-entry".to_string(), "-o".to_string(), out.to_str().unwrap().to_string()]);
    pliron_wasm_ld::link(argv).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    wp::Validator::new().validate_all(&bytes).unwrap();

    let mut names: HashMap<u32, String> = HashMap::new();
    let mut exports = Vec::new();
    let mut raw = Vec::new();
    let mut nimports = 0;
    for p in wp::Parser::new(0).parse_all(&bytes) {
        match p.unwrap() {
            wp::Payload::ImportSection(r) => nimports = r.into_imports().count() as u32,
            wp::Payload::ExportSection(r) => {
                for e in r {
                    let e = e.unwrap();
                    if e.kind == wp::ExternalKind::Func {
                        exports.push((e.name.to_string(), e.index));
                    }
                }
            }
            wp::Payload::CodeSectionEntry(b) => raw.push(b),
            wp::Payload::CustomSection(c) => {
                if let wp::KnownCustom::Name(r) = c.as_known() {
                    for n in r {
                        if let wp::Name::Function(map) = n.unwrap() {
                            for n in map {
                                let n = n.unwrap();
                                names.insert(n.index, n.name.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut bodies = HashMap::new();
    for (i, b) in raw.iter().enumerate() {
        let mut ops = Vec::new();
        for op in b.get_operators_reader().unwrap() {
            ops.push(match op.unwrap() {
                wp::Operator::Call { function_index } => format!("call {}", names[&function_index]),
                wp::Operator::LocalGet { local_index } => format!("local.get {local_index}"),
                wp::Operator::I32Const { .. } => "i32.const".to_string(),
                wp::Operator::I32Load { .. } => "i32.load".to_string(),
                wp::Operator::I32Store { .. } => "i32.store".to_string(),
                wp::Operator::I32Eqz => "i32.eqz".to_string(),
                wp::Operator::If { .. } => "if".to_string(),
                wp::Operator::End => "end".to_string(),
                op => format!("{op:?}"),
            });
        }
        bodies.insert(names[&(nimports + i as u32)].clone(), ops);
    }
    Linked {
        exports: exports
            .into_iter()
            .map(|(n, i)| (n, names[&i].clone()))
            .collect(),
        bodies,
    }
}

/// Two objects, four constructors: priorities 65535 and 20 in the first,
/// 10 and 65535 in the second.
fn two_objects(extra: Option<F>) -> Vec<Vec<u8>> {
    let mut a = vec![
        f("c_default_a"),
        f("c_20"),
        F {
            global: true,
            arg: true,
            ..f("get")
        },
    ];
    a.extend(extra);
    vec![
        object(&a, &[(None, "c_default_a"), (Some(20), "c_20")]),
        object(
            &[f("c_10"), f("c_default_b")],
            &[(Some(10), "c_10"), (None, "c_default_b")],
        ),
    ]
}

const ORDER: [&str; 5] = [
    "call c_10",
    "call c_20",
    "call c_default_a",
    "call c_default_b",
    "end",
];

#[test]
fn exports_run_the_constructors_once_by_priority() {
    let m = link("wrap", &two_objects(None), &["--export", "get"]);
    // Lower priority first, equal priorities in link order.
    assert_eq!(m.bodies["__wasm_call_ctors"], ORDER);
    // The export is a wrapper: constructors (once), then the function.
    assert_eq!(m.exports["get"], "get.command_export");
    assert_eq!(
        m.bodies["get.command_export"],
        ["call __wasm_call_ctors_once", "local.get 0", "call get", "end"]
    );
    // The "done" word is set before the constructors are called.
    assert_eq!(
        m.bodies["__wasm_call_ctors_once"],
        [
            "i32.const",
            "i32.load",
            "i32.eqz",
            "if",
            "i32.const",
            "i32.const",
            "i32.store",
            "call __wasm_call_ctors",
            "end",
            "end"
        ]
    );
}

#[test]
fn exported_call_ctors_is_left_to_the_embedder() {
    let m = link(
        "export",
        &two_objects(None),
        &["--export", "get", "--export", "__wasm_call_ctors"],
    );
    assert_eq!(m.bodies["__wasm_call_ctors"], ORDER);
    assert_eq!(m.exports["get"], "get");
    assert_eq!(m.exports["__wasm_call_ctors"], "__wasm_call_ctors");
    assert!(!m.bodies.contains_key("__wasm_call_ctors_once"));
}

#[test]
fn a_program_that_calls_the_constructors_is_not_wrapped() {
    // `_initialize` calls `__wasm_call_ctors` itself (a WASI reactor).
    let init = F {
        global: true,
        calls: Some("__wasm_call_ctors"),
        ..f("_initialize")
    };
    let m = link(
        "called",
        &two_objects(Some(init)),
        &["--export", "get", "--export", "_initialize"],
    );
    assert_eq!(m.bodies["__wasm_call_ctors"], ORDER);
    assert_eq!(m.bodies["_initialize"], ["call __wasm_call_ctors", "end"]);
    assert_eq!(m.exports["get"], "get");
    assert_eq!(m.exports["_initialize"], "_initialize");
    assert!(!m.bodies.contains_key("__wasm_call_ctors_once"));
}

#[test]
fn a_call_from_dead_code_does_not_count() {
    // The same `_initialize`, but not exported: nothing reaches it, so
    // the exports still have to run the constructors.
    let init = F {
        global: true,
        calls: Some("__wasm_call_ctors"),
        ..f("_initialize")
    };
    let m = link("dead", &two_objects(Some(init)), &["--export", "get"]);
    assert_eq!(m.exports["get"], "get.command_export");
    assert!(!m.bodies.contains_key("_initialize"));
}

#[test]
fn no_constructors_no_wrappers() {
    let get = F {
        global: true,
        arg: true,
        ..f("get")
    };
    let m = link("none", &[object(&[get], &[])], &["--export", "get"]);
    assert_eq!(m.exports["get"], "get");
    assert_eq!(m.bodies["__wasm_call_ctors"], ["end"]);
    assert!(!m.bodies.contains_key("__wasm_call_ctors_once"));
}
