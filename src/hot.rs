//! Hot-reload support. With `PLIRON_HOT=<crate name>`, every function defined in
//! that crate is emitted as `<sym>.hot`, and `<sym>` itself becomes a thunk that
//! jumps through the writable slot `__hot_slot.<sym>`. Every reference (calls,
//! vtables, fn pointers) goes through the thunk, so a loader in the running
//! process only has to repoint slots at freshly compiled bodies.
//! Symbol names must match between builds, so use `-Ccodegen-units=1`.

use cranelift_module::Linkage;

pub fn patchable(n: &str) -> bool {
    n != "main" && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn body_name(n: &str) -> String {
    format!("{n}.hot")
}

pub fn thunk_asm(n: &str, l: Linkage) -> String {
    let bind = match l {
        Linkage::Preemptible => format!(".weak {n}\n"),
        Linkage::Export => format!(".globl {n}\n"),
        _ => format!(".globl {n}\n.hidden {n}\n"),
    };
    let s = format!("__hot_slot.{n}");
    format!(
        ".section .text.{n},\"ax\",@progbits\n{bind}.type {n},@function\n{n}:\njmp *{s}(%rip)\n.size {n}, .-{n}\n\
         .section .data.{s},\"aw\",@progbits\n.p2align 3\n.globl {s}\n.hidden {s}\n.type {s},@object\n{s}:\n.quad {n}.hot\n.size {s}, 8\n.text\n"
    )
}
