//! The IR passes ask `st.target` (`pliron_cfg::target::Target`) about the
//! target. Two passes once read the layout's process-wide pointer-width
//! flag instead and spelled their answer as a literal: sroa's fragment
//! rule counted in 8-byte words on wasm32 too (a performance loss, fixed
//! 2026-11-16 as `PLIRON_SROA_FRAGWORD`), and constload sized a pointer
//! relocation as `if ptr32() { 4 } else { 8 }`.
//!
//! This is a source lint over non-test code in `src/`: outside `types.rs`
//! (which owns the layout and its mirror of the pointer width) no pass
//! reads `ptr32()`/`PTR32`, and none names the CLIF pointer-width types
//! `clt::I32`/`clt::I64` (a pass that needs the CLIF type of a pointer
//! takes it from `types::leaves`).

use std::path::Path;

fn violations(code: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    if code.contains("ptr32()") || code.contains("PTR32") {
        out.push("reads the layout's pointer-width flag: ask `st.target`");
    }
    for t in ["clt::I32", "clt::I64", "types::I32", "types::I64"] {
        if let Some(at) = code.find(t) {
            let rest = &code[at + t.len()..];
            if !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                out.push("names a pointer-width CLIF type");
                break;
            }
        }
    }
    out
}

#[test]
fn ir_passes_ask_the_target() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut bad = Vec::new();
    for e in std::fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if p.extension().is_none_or(|x| x != "rs") || name == "types.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        let body = match text.find("#[cfg(test)]\nmod ") {
            Some(cut) => &text[..cut],
            None => &text[..],
        };
        for (n, line) in body.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for why in violations(code) {
                bad.push(format!("{name}:{}: {why}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(bad.is_empty(), "IR passes must ask `st.target`:\n{}", bad.join("\n"));
}

#[test]
fn lint_catches_each_shape() {
    for bad in [
        ".unwrap_or(if crate::types::ptr32() { 4 } else { 8 });",
        "if ptr32() { 4 } else { 8 }",
        "let t = clt::I64;",
        "out.push((base, cranelift_codegen::ir::types::I32));",
    ] {
        assert!(!violations(bad).is_empty(), "not caught: {bad}");
    }
    for ok in [".unwrap_or(st.target.word_bytes());", "size == st.target.ptr_bytes()", "let v = clt::I64X2;"] {
        assert!(violations(ok).is_empty(), "false positive: {ok}");
    }
}
