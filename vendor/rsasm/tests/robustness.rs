//! Malformed input must produce diagnostics, never a panic or a hang.

#![cfg(feature = "x86")]

mod common;
use common::*;

/// Every one of these is nonsense in some way. The only requirement is that
/// the assembler reports something and returns.
const BAD: &[&str] = &[
    "",
    "\0",
    "   ",
    "\n\n\n",
    ":",
    "::",
    ",",
    "()",
    "[",
    "]",
    "(",
    ")",
    "$",
    "%",
    "%%",
    "%rax",
    "* ",
    ".",
    "..",
    ". =",
    ". = .",
    "= 1",
    "foo",
    "foo:",
    "foo::",
    "foo: :",
    "1:",
    "1b",
    "1f",
    "0:0:0:",
    "mov",
    "mov ,",
    "mov , ,",
    "mov %rax,",
    "mov ,%rax",
    "mov $,%rax",
    "movq %rax %rbx",
    "movq (%rax",
    "movq (%rax,",
    "movq (%rax,%rbx,",
    "movq (%rax,%rbx,3), %rcx",
    "movq (%rip,%rax), %rcx",
    "movq (%rax,%rsp), %rcx",
    "movq %ah, %r8b",
    "jmp *",
    "jmp $",
    ".byte",
    ".byte ,",
    ".byte 1,",
    ".byte 1 2",
    ".ascii",
    ".ascii 1",
    ".ascii \"unterminated",
    ".quad 1 +",
    ".quad (1",
    ".quad 1)",
    ".quad 1/0",
    ".quad 1%0",
    ".quad 0x",
    ".quad 99999999999999999999999999",
    ".quad 1st",
    ".set",
    ".set a",
    ".set a,",
    ".set a, a",
    ".set a, b\n.set b, c\n.set c, a\n.quad a",
    ".globl",
    ".globl ,",
    ".type f",
    ".type f, @nosuch",
    ".size f",
    ".comm",
    ".comm a",
    ".comm a, -1",
    ".section",
    ".section ,",
    ".align",
    ".align 3",
    ".align -1",
    ".p2align 1000",
    ".org",
    ".org -1",
    ".space",
    ".space -1",
    ".fill 1, 99, 1",
    ".fill -1",
    ".if",
    ".if 1",
    ".else",
    ".endif",
    ".if 1\n.else\n.else\n.endif",
    ".elseif 1",
    ".include",
    ".include \"nosuchfile\"",
    ".incbin \"nosuchfile\"",
    ".arch",
    ".arch nosuch",
    ".intel_syntax noprefix\nmov [",
    ".intel_syntax noprefix\nmov rax, [rbx+]",
    ".intel_syntax noprefix\nmov rax, [rbx*rcx]",
    ".intel_syntax noprefix\nmov rax, [-rbx]",
    ".intel_syntax noprefix\nmov rax, [rax+rbx+rcx]",
    ".intel_syntax noprefix\nmov qword ptr",
    "/* unterminated",
    "\"\\",
    "'",
    "\\",
    "nop\u{80}",
    "\u{1F600}",
    ".macro foo\n.endm",
    ".rept 3\nnop\n.endr",
];

#[test]
fn malformed_input_never_panics() {
    for src in BAD {
        // The result is irrelevant; not unwinding and not hanging is the test.
        let _ = try_text(src);
    }
}

#[test]
fn malformed_input_is_reported_rather_than_ignored() {
    // A sample where silence would definitely be a bug.
    for src in [
        "movq (%rax",
        ".byte 1 2",
        ".quad 1/0",
        ".align 3",
        ".arch nosuch",
        "frobnicate",
        ".if 1",
    ] {
        assert!(
            try_text(src).is_err(),
            "`{src}` should have produced a diagnostic"
        );
    }
}

#[test]
fn deeply_nested_expressions_do_not_blow_the_stack() {
    // A hand-written file would not look like this, but a generator might.
    let depth = 500;
    let src = format!(".quad {}1{}", "(".repeat(depth), ")".repeat(depth));
    let _ = try_text(&src);
    let src = format!(".quad {}1", "-".repeat(depth));
    let _ = try_text(&src);
}

#[test]
fn a_long_chain_of_branches_still_settles() {
    // Every jump is short until the ones after it grow, so the layout loop
    // has to iterate; it must converge rather than hit its pass limit.
    let mut src = String::new();
    for i in 0..200 {
        src.push_str(&format!("    jmp l{i}\n"));
    }
    src.push_str("    .space 300\n");
    for i in 0..200 {
        src.push_str(&format!("l{i}: nop\n"));
    }
    let out = text(&src);
    assert!(out.len() > 300);
}

#[test]
fn conditionals_may_nest_deeply() {
    let depth = 200;
    let mut src = String::new();
    for _ in 0..depth {
        src.push_str(".if 1\n");
    }
    src.push_str("nop\n");
    for _ in 0..depth {
        src.push_str(".endif\n");
    }
    assert_eq!(text(&src), vec![0x90]);
}
