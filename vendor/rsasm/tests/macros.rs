//! Macros: `.macro`, `.rept`, `.irp`, `.irpc`.

#![cfg(feature = "x86")]

mod common;
use common::*;

#[test]
fn a_macro_substitutes_its_parameters() {
    assert_eq!(
        text(".macro two a, b\n.byte \\a\n.byte \\b\n.endm\ntwo 1, 2\n"),
        vec![1, 2]
    );
    // Invoked more than once, with different arguments each time.
    assert_eq!(
        text(".macro one x\n.byte \\x\n.endm\none 1\none 2\none 3\n"),
        vec![1, 2, 3]
    );
}

#[test]
fn macros_expand_to_instructions_not_just_data() {
    let expanded = text(".macro ret2\nnop\nret\n.endm\nret2\n");
    assert_eq!(expanded, text("nop\nret\n"));
}

#[test]
fn arguments_may_be_named_defaulted_or_required() {
    assert_eq!(
        text(".macro m a, b=7\n.byte \\a, \\b\n.endm\nm 1\n"),
        vec![1, 7]
    );
    assert_eq!(
        text(".macro m a, b=7\n.byte \\a, \\b\n.endm\nm 1, 2\n"),
        vec![1, 2]
    );
    // Named arguments may be given out of order.
    assert_eq!(
        text(".macro m a, b\n.byte \\a, \\b\n.endm\nm b=2, a=1\n"),
        vec![1, 2]
    );
    // An omitted argument with no default expands to nothing.
    assert_eq!(text(".macro m a, b\n.byte \\a\n.endm\nm 5\n"), vec![5]);
    // `:req` makes omitting it an error.
    let e = errors(".macro m a:req\n.byte \\a\n.endm\nm\n");
    assert!(e.contains("requires an argument for `a`"), "{e}");
}

#[test]
fn vararg_absorbs_the_rest_of_the_line() {
    assert_eq!(
        text(".macro m first, rest:vararg\n.byte \\first\n.byte \\rest\n.endm\nm 1, 2, 3, 4\n"),
        vec![1, 2, 3, 4]
    );
}

#[test]
fn the_invocation_counter_makes_labels_unique() {
    // The reason expansion is textual: `\@` has to paste into an identifier.
    let out = text(".macro loopy\n.L\\@_top:\n  jmp .L\\@_top\n.endm\nloopy\nloopy\n");
    // Two independent two-byte self-jumps; if the labels had collided the
    // second would have been a duplicate-symbol error.
    assert_eq!(out, vec![0xeb, 0xfe, 0xeb, 0xfe]);
}

#[test]
fn the_empty_paste_ends_a_parameter_name() {
    assert_eq!(
        text(".macro m p\n\\p\\()_x = 3\n.byte \\p\\()_x\n.endm\nm foo\n"),
        vec![3]
    );
}

#[test]
fn string_literals_survive_expansion() {
    // A body full of escapes must not be mangled by the substituter.
    assert_eq!(
        text(".macro m\n.ascii \"a\\nb\"\n.endm\nm\n"),
        b"a\nb".to_vec()
    );
}

#[test]
fn macros_may_call_other_macros() {
    assert_eq!(
        text(
            ".macro inner v\n.byte \\v\n.endm\n.macro outer a, b\ninner \\a\ninner \\b\n.endm\nouter 4, 5\n"
        ),
        vec![4, 5]
    );
}

#[test]
fn a_macro_may_be_defined_inside_another() {
    // The inner `.endm` must not terminate the outer body.
    assert_eq!(
        text(".macro maker\n.macro made\n.byte 9\n.endm\n.endm\nmaker\nmade\n"),
        vec![9]
    );
}

#[test]
fn exitm_leaves_the_expansion_early() {
    assert_eq!(
        text(".macro m\n.byte 1\n.exitm\n.byte 2\n.endm\nm\n.byte 3\n"),
        vec![1, 3]
    );
    // ...and only the innermost one.
    assert_eq!(
        text(
            ".macro inner\n.byte 1\n.exitm\n.byte 2\n.endm\n.macro outer\ninner\n.byte 3\n.endm\nouter\n"
        ),
        vec![1, 3]
    );
}

#[test]
fn labels_on_the_invocation_line_belong_to_the_call_site() {
    assert_eq!(
        text("here: .macro m\n.byte 1\n.endm\nstart: m\n.byte start\n"),
        vec![1, 0]
    );
}

#[test]
fn rept_repeats_its_body() {
    assert_eq!(text(".rept 3\n.byte 7\n.endr\n"), vec![7, 7, 7]);
    assert_eq!(text(".rept 0\n.byte 7\n.endr\n"), Vec::<u8>::new());
    // Nested repeats.
    assert_eq!(
        text(".rept 2\n.rept 3\n.byte 1\n.endr\n.endr\n"),
        vec![1; 6]
    );
}

#[test]
fn irp_walks_a_list_and_irpc_walks_characters() {
    assert_eq!(text(".irp v, 1, 2, 3\n.byte \\v\n.endr\n"), vec![1, 2, 3]);
    assert_eq!(
        text(".irpc c, abc\n.ascii \"\\c\"\n.endr\n"),
        b"abc".to_vec()
    );
    // An empty list produces nothing.
    assert_eq!(text(".irp v\n.byte 1\n.endr\n"), Vec::<u8>::new());
}

#[test]
fn a_repeat_inside_a_macro_does_not_end_it() {
    assert_eq!(
        text(".macro m n\n.rept 2\n.byte \\n\n.endr\n.endm\nm 6\n"),
        vec![6, 6]
    );
}

#[test]
fn purgem_removes_a_definition() {
    // After purging, the name is an instruction again rather than a macro.
    let out = text(".macro nop\n.byte 0xcc\n.endm\nnop\n.purgem nop\nnop\n");
    assert_eq!(out, vec![0xcc, 0x90]);
    assert!(errors(".purgem nosuch\n").contains("no macro named"));
}

#[test]
fn macros_are_inert_inside_a_false_conditional() {
    assert_eq!(
        text(".if 0\n.macro m\n.byte 1\n.endm\n.endif\n.byte 2\n"),
        vec![2]
    );
}

#[test]
fn errors_are_reported_rather_than_looping_forever() {
    assert!(errors(".macro m\n.byte 1\n").contains("unterminated"));
    assert!(errors(".rept 2\n.byte 1\n").contains("unterminated"));
    assert!(errors(".endm\n").contains("without a matching"));
    assert!(errors(".endr\n").contains("without a matching"));
    assert!(errors(".exitm\n").contains("outside a macro"));
    assert!(errors(".macro\n.endm\n").contains("needs a name"));
    assert!(errors(".macro m a, a\n.endm\n").contains("duplicate"));
    assert!(errors(".macro m a:bogus\n.endm\n").contains("qualifier"));
    assert!(errors(".macro m\n.endm\n.macro m\n.endm\n").contains("already defined"));
    assert!(errors(".rept -1\n.endr\n").contains("negative"));
    assert!(errors(".rept x\n.endr\n").contains("plain number"));
    assert!(errors(".macro m a\n.byte \\a\n.endm\nm 1, 2\n").contains("argument"));
}

#[test]
fn recursion_is_bounded() {
    // Unbounded recursion must be diagnosed, not run out of stack.
    let e = errors(".macro r\nr\n.endm\nr\n");
    assert!(e.contains("nested too deeply"), "{e}");
}

#[test]
fn a_diagnostic_inside_a_macro_names_the_macro() {
    let e = errors(".macro m\nfrobnicate\n.endm\nm\n");
    assert!(e.contains("frobnicate"), "{e}");
    assert!(
        e.contains("<macro m>"),
        "the location should name the macro: {e}"
    );
}
