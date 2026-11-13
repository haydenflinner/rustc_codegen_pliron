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

/// `PLIRON_HOT_REACH=1`: each thunk first sets its function's byte in the
/// mark table, so a runtime can tell which functions a run entered
/// (`pliron_hot::reach`, tests-on-save: `examples/hot-test.sh`).
fn reach_on() -> bool {
    pliron_cfg::env_is("PLIRON_HOT_REACH", "1")
}

/// The section arm64 Mach-O thunks live in. Not `__text`: the loader may
/// rewrite a thunk's slot load into a direct `b` (`PLIRON_HOT_DIRECT`),
/// which means flipping its page writable for a moment, and no other
/// code may share that page. `thunks_wrap` pads the section to whole
/// 16 KiB pages.
const THUNK_SEC: &str = "__TEXT,__hot_thunks,regular,pure_instructions";

/// Page-align both ends of this object's thunk block (arm64 Mach-O).
pub fn thunks_wrap(asm: &str, macho: bool, x86: bool) -> String {
    if asm.is_empty() || !macho || x86 {
        return asm.to_string();
    }
    format!(".section {THUNK_SEC}\n.p2align 14\n{asm}.section {THUNK_SEC}\n.p2align 14\n")
}

// r[impl native.hot.slot]
pub fn thunk_asm(n: &str, l: Linkage, macho: bool, x86: bool) -> String {
    let reach = reach_on();
    if macho {
        // Mach-O object names carry the `_` the writer adds for declared
        // symbols; asm spelling is raw, so add it here to match.
        let f = format!("_{n}");
        let s = format!("__hot_slot._{n}");
        let k = format!("__hot_mark._{n}");
        let bind = match l {
            Linkage::Preemptible => format!(".weak_definition {f}\n"),
            Linkage::Export => format!(".globl {f}\n"),
            _ => format!(".private_extern {f}\n"),
        };
        let mark = match (reach, x86) {
            (false, _) => String::new(),
            (true, true) => format!("movb $1, {k}(%rip)\n"),
            (true, false) => format!(
                "adrp x17, {k}@PAGE\nadd x17, x17, {k}@PAGEOFF\nmov w16, #1\nstrb w16, [x17]\n"
            ),
        };
        let jmp = if x86 {
            format!("jmp *{s}(%rip)")
        } else {
            format!("adrp x16, {s}@PAGE\nldr x16, [x16, {s}@PAGEOFF]\nbr x16")
        };
        let sec = if x86 { "__TEXT,__text,regular,pure_instructions" } else { THUNK_SEC };
        let marks = if reach {
            format!(".section __DATA,__hot_marks\n.private_extern {k}\n.no_dead_strip {k}\n{k}:\n.byte 0\n")
        } else {
            String::new()
        };
        // `.align` is p2align on Darwin; slots share one 16-char-legal
        // section since `__hot_slot.<mangled>` would not fit
        format!(
            ".section {sec}\n{bind}.align 2\n{f}:\n{mark}{jmp}\n\
             .section __DATA,__hot_slots\n.private_extern {s}\n.no_dead_strip {s}\n.align 3\n{s}:\n.quad {f}.hot\n{marks}"
        )
    } else {
        let bind = match l {
            Linkage::Preemptible => format!(".weak {n}\n"),
            Linkage::Export => format!(".globl {n}\n"),
            _ => format!(".globl {n}\n.hidden {n}\n"),
        };
        let s = format!("__hot_slot.{n}");
        let k = format!("__hot_mark.{n}");
        let mark = match (reach, x86) {
            (false, _) => String::new(),
            (true, true) => format!("movb $1, {k}(%rip)\n"),
            (true, false) => {
                format!("adrp x17, {k}\nadd x17, x17, :lo12:{k}\nmov w16, #1\nstrb w16, [x17]\n")
            }
        };
        let jmp = if x86 {
            format!("jmp *{s}(%rip)")
        } else {
            format!("adrp x16, {s}\nldr x16, [x16, :lo12:{s}]\nbr x16")
        };
        let marks = if reach {
            format!(".section .data.{k},\"aw\",@progbits\n.globl {k}\n.hidden {k}\n.type {k},@object\n{k}:\n.byte 0\n.size {k}, 1\n")
        } else {
            String::new()
        };
        format!(
            ".section .text.{n},\"ax\",@progbits\n{bind}.type {n},@function\n{n}:\n{mark}{jmp}\n.size {n}, .-{n}\n\
             .section .data.{s},\"aw\",@progbits\n.p2align 3\n.globl {s}\n.hidden {s}\n.type {s},@object\n{s}:\n.quad {n}.hot\n.size {s}, 8\n{marks}.text\n"
        )
    }
}
