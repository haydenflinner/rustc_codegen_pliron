//! Passes run on wasm32 (i32 pointers) as well as on 64-bit targets. An
//! address-producing instruction built with a literal `I64` is valid CLIF
//! on every target and wrong on wasm32: slotprom's `stack_addr(types::I64,
//! ..)` for its sync stores made the wasm emitter wrap an i32 frame
//! pointer as an i64, and the module did not validate (2026-11-16).
//!
//! This is a source lint: no non-test pass code may build a slot, symbol
//! or function address with a hard-coded 64-bit type. Take the type from
//! an address the function already has (`func.dfg.value_type(addr)`).

use std::path::Path;

#[test]
fn passes_do_not_build_addresses_as_i64() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let needles = [
        "stack_addr(types::I64",
        "stack_addr(I64",
        "symbol_value(types::I64",
        "symbol_value(I64",
        "func_addr(types::I64",
        "func_addr(I64",
        "get_frame_pointer(types::I64",
        "get_stack_pointer(types::I64",
    ];
    let mut bad = Vec::new();
    for e in std::fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        // Unit tests build 64-bit functions on purpose.
        let body = match text.find("#[cfg(test)]\nmod ") {
            Some(cut) => &text[..cut],
            None => &text[..],
        };
        for (n, line) in body.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if needles.iter().any(|x| code.contains(x)) {
                bad.push(format!("{}:{}: {}", p.file_name().unwrap().to_string_lossy(), n + 1, line.trim()));
            }
        }
    }
    assert!(bad.is_empty(), "addresses built with a literal I64:\n{}", bad.join("\n"));
}
