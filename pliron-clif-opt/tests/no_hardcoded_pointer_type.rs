//! Passes run on wasm32 (i32 pointers) as well as on 64-bit targets. An
//! address-producing instruction built with a literal `I64` is valid CLIF
//! on every target and wrong on wasm32: slotprom's `stack_addr(types::I64,
//! ..)` for its sync stores made the wasm emitter wrap an i32 frame
//! pointer as an i64, and the module did not validate (2026-11-16).
//!
//! This is a source lint. In non-test pass code:
//!
//! 1. no slot, symbol, function or frame address is built with a literal
//!    integer type, however the type is spelled (`types::I64`,
//!    `ir::types::I64`, a bare imported `I64`, `I32`);
//! 2. no type is *defaulted* to a literal pointer-width integer
//!    (`.unwrap_or(types::I64)`, `.map_or(types::I64, ..)`): that is how
//!    slotprom and slotmerge guessed the address type of a function that
//!    had no `stack_addr` to copy it from.
//!
//! The pointer type is `target::ptr_type(&p.target)` (`Pipe::target`,
//! `pliron_cfg::target::Target`), or the type of an address the function
//! already has (`func.dfg.value_type(addr)`). Everything else a pass wants
//! to know about the target is a field or method of `Target`.

use std::path::Path;

/// Instruction builders whose first argument is the type of an address.
const ADDR_BUILDERS: &[&str] = &[
    "stack_addr",
    "dynamic_stack_addr",
    "symbol_value",
    "tls_value",
    "func_addr",
    "global_value",
    "get_frame_pointer",
    "get_stack_pointer",
    "get_return_address",
];

/// Combinators that supply a default when no value was found.
const DEFAULTS: &[&str] = &["unwrap_or", "map_or", "unwrap_or_else"];

/// `text` with any `a::b::` path prefix removed from its start.
fn strip_path(mut text: &str) -> &str {
    loop {
        let id = text.len() - text.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_').len();
        match text[id..].strip_prefix("::") {
            Some(rest) if id > 0 => text = rest,
            _ => return text,
        }
    }
}

/// Does `arg` (the text after an opening parenthesis) start with a literal
/// 32- or 64-bit integer type, under any path?
fn literal_ptr_int(arg: &str) -> bool {
    let arg = strip_path(arg.trim_start().trim_start_matches("|| ").trim_start());
    ["I64", "I32"].iter().any(|t| {
        arg.strip_prefix(t)
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    })
}

/// Byte offsets just past each `name(` in `code` where `name` is a whole
/// identifier.
fn calls<'a>(code: &'a str, name: &'a str) -> impl Iterator<Item = usize> + 'a {
    code.match_indices(name).filter_map(move |(at, _)| {
        let before_ok = !code[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
        let after = &code[at + name.len()..];
        (before_ok && after.starts_with('(')).then_some(at + name.len() + 1)
    })
}

fn violations(code: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    if ADDR_BUILDERS.iter().any(|b| calls(code, b).any(|at| literal_ptr_int(&code[at..]))) {
        out.push("address built with a literal integer type");
    }
    if DEFAULTS.iter().any(|d| calls(code, d).any(|at| literal_ptr_int(&code[at..]))) {
        out.push("type defaulted to a literal pointer-width integer");
    }
    out
}

/// Non-test source lines of `text`, comments removed.
fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    // Unit tests build 64-bit functions on purpose.
    let body = match text.find("#[cfg(test)]\nmod ") {
        Some(cut) => &text[..cut],
        None => text,
    };
    body.lines().enumerate().map(|(n, l)| (n + 1, l.split("//").next().unwrap_or("")))
}

#[test]
fn passes_ask_the_target() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut bad = Vec::new();
    for e in std::fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&p).unwrap();
        for (n, code) in code_lines(&text) {
            for why in violations(code) {
                bad.push(format!("{name}:{n}: {why}: {}", code.trim()));
            }
        }
    }
    assert!(bad.is_empty(), "passes must ask `Target` (pliron_cfg::target):\n{}", bad.join("\n"));
}

/// The lint finds what it is for, in each spelling.
#[test]
fn lint_catches_each_shape() {
    for bad in [
        "let a = pos.ins().stack_addr(types::I64, ss, 0);",
        "let a = pos.ins().stack_addr(ir::types::I64, ss, 0);",
        "let a = pos.ins().stack_addr(I64, ss, 0);",
        "let a = pos.ins().stack_addr( cranelift_codegen::ir::types::I32, ss, 0);",
        "let g = pos.ins().symbol_value(types::I64, gv);",
        "let f = pos.ins().func_addr(I64, fr);",
        ".map_or(types::I64, |i| func.dfg.value_type(func.dfg.first_result(i)));",
        ".unwrap_or(types::I64);",
        ".unwrap_or_else(|| ir::types::I32)",
    ] {
        assert!(!violations(bad).is_empty(), "not caught: {bad}");
    }
    for ok in [
        "let a = pos.ins().stack_addr(pty, ss, 0);",
        "let a = pos.ins().stack_addr(addr_ty, *ss, 0);",
        "let k = pos.ins().iconst(types::I64, 0);",
        "let pty = crate::target::ptr_type(target);",
        ".unwrap_or(types::I8)",
        ".unwrap_or(I64X2)",
    ] {
        assert!(violations(ok).is_empty(), "false positive: {ok}: {:?}", violations(ok));
    }
}
