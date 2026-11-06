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

// r[impl native.hot.slot]
pub fn thunk_asm(n: &str, l: Linkage, macho: bool, x86: bool) -> String {
    if macho {
        // Mach-O object names carry the `_` the writer adds for declared
        // symbols; asm spelling is raw, so add it here to match.
        let f = format!("_{n}");
        let s = format!("__hot_slot._{n}");
        let bind = match l {
            Linkage::Preemptible => format!(".weak_definition {f}\n"),
            Linkage::Export => format!(".globl {f}\n"),
            _ => format!(".private_extern {f}\n"),
        };
        let jmp = if x86 {
            format!("jmp *{s}(%rip)")
        } else {
            format!("adrp x16, {s}@PAGE\nldr x16, [x16, {s}@PAGEOFF]\nbr x16")
        };
        // `.align` is p2align on Darwin; slots share one 16-char-legal
        // section since `__hot_slot.<mangled>` would not fit
        format!(
            ".section __TEXT,__text,regular,pure_instructions\n{bind}.align 2\n{f}:\n{jmp}\n\
             .section __DATA,__hot_slots\n.private_extern {s}\n.no_dead_strip {s}\n.align 3\n{s}:\n.quad {f}.hot\n"
        )
    } else {
        let bind = match l {
            Linkage::Preemptible => format!(".weak {n}\n"),
            Linkage::Export => format!(".globl {n}\n"),
            _ => format!(".globl {n}\n.hidden {n}\n"),
        };
        let s = format!("__hot_slot.{n}");
        let jmp = if x86 {
            format!("jmp *{s}(%rip)")
        } else {
            format!("adrp x16, {s}\nldr x16, [x16, :lo12:{s}]\nbr x16")
        };
        format!(
            ".section .text.{n},\"ax\",@progbits\n{bind}.type {n},@function\n{n}:\n{jmp}\n.size {n}, .-{n}\n\
             .section .data.{s},\"aw\",@progbits\n.p2align 3\n.globl {s}\n.hidden {s}\n.type {s},@object\n{s}:\n.quad {n}.hot\n.size {s}, 8\n.text\n"
        )
    }
}
