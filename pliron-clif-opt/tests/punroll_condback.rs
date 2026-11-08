//! `punroll` on a rotated loop with a conditional backedge — the
//! `iter.any(|&b| b == c)` shape `while p != end && *p != c`: the count
//! test sits in the header brif (a pos-0 side exit) and the latch brif's
//! non-header arm leaves the body (the data exit). The group copies keep
//! only the byte test; the `p == end` count test survives in the group
//! guard, the last copy and the scalar remainder.

use cranelift_codegen::ir::{Function, Inst, Opcode, Value};
use cranelift_codegen::settings;
use pliron_clif_opt::pipeline::Analysis;

const ANY: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)
block1(v10: i64):
    v11 = icmp eq v10, v1
    brif v11, block3, block2
block2:
    v12 = load.i8 notrap v10
    v13 = iconst.i8 96
    v14 = icmp ne v12, v13
    v15 = iconst.i64 1
    v16 = iadd v10, v15
    brif v14, block1(v16), block4
block3:
    v17 = iconst.i64 0
    return v17
block4:
    v18 = iconst.i64 1
    return v18
}";

fn insts(f: &Function) -> impl Iterator<Item = Inst> + '_ {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
}

/// Brifs whose condition is an icmp mentioning `bound` — the surviving
/// `p == end` count tests.
fn count_brifs_on(f: &Function, bound: Value) -> usize {
    insts(f)
        .filter(|&i| {
            if f.dfg.insts[i].opcode() != Opcode::Brif {
                return false;
            }
            let c = f.dfg.resolve_aliases(f.dfg.inst_args(i)[0]);
            matches!(f.dfg.value_def(c).inst(), Some(d) if
                f.dfg.insts[d].opcode() == Opcode::Icmp
                    && f.dfg.inst_args(d).iter().any(|&a| f.dfg.resolve_aliases(a) == bound))
        })
        .count()
}

#[test]
fn conditional_backedge_unrolls_with_one_group_test() {
    let mut f = cranelift_reader::parse_functions(ANY).unwrap().remove(0);
    let bound = f.dfg.block_params(f.layout.entry_block().unwrap())[1];
    let mut an = Analysis::default();
    let n = pliron_clif_opt::punroll::run(&mut f, &mut an, "condback", &Default::default());
    assert!(n > 0, "loop not unrolled:\n{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&f, &flags)
        .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    // K unrolled copies plus the rolled scalar remainder.
    assert!(
        insts(&f)
            .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
            .count()
            >= 5,
        "{}",
        f.display()
    );
    // `p == end` branches survive only in the last group copy and the
    // scalar remainder — never once per unrolled element.
    assert!(count_brifs_on(&f, bound) <= 3, "{}", f.display());
}

#[test]
fn do_while_latch_still_unrolls() {
    // Both dests of the latch brif stay inside the body — the classic
    // counted do-while, unaffected by the conditional-backedge arm.
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v0, v2)
block1(v10: i64, v11: i64):
    v12 = load.i8 notrap v10
    v13 = iconst.i64 1
    v14 = iadd v10, v13
    v15 = uextend.i64 v12
    v16 = iadd v11, v15
    v17 = icmp ult v14, v1
    brif v17, block1(v14, v16), block2(v16)
block2(v20: i64):
    return v20
}";
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut an = Analysis::default();
    let n = pliron_clif_opt::punroll::run(&mut f, &mut an, "dowhile", &Default::default());
    assert!(n > 0, "do-while loop not unrolled:\n{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&f, &flags)
        .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
}
