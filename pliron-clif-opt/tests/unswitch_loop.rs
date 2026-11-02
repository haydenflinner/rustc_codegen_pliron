//! bcheck `PLIRON_UNSWITCH_LOOP`: a loop whose closed region is too big
//! (40 blocks follow it) is unswitched on an invariant test that only odd
//! iterations reach, by cloning just the loop. Two values the loop
//! defines are used after it, so its exit gets params merging the two
//! versions. Verified and differentially checked (both flag values).

use cranelift_codegen::ir::{Function, InstructionData, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn src() -> String {
    let mut chain = String::new();
    let mut prev = 14;
    for k in 0..40 {
        let b = 7 + k;
        let v = 100 + k;
        chain += &format!("block{b}:\n    v{v} = iadd v{prev}, v11\n    jump block{}\n", b + 1);
        prev = v;
    }
    format!(
        "function %f(i64, i64) -> i64 {{
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v3 = icmp ne v1, v2
    jump block1(v2, v2)
block1(v4: i64, v5: i64):
    v6 = iconst.i64 1
    v8 = band v4, v6
    brif v8, block2, block3(v5)
block2:
    brif v3, block4, block5
block4:
    v9 = iadd v5, v4
    jump block3(v9)
block5:
    v10 = isub v5, v4
    jump block3(v10)
block3(v11: i64):
    v12 = iadd v4, v6
    v13 = icmp ult v12, v0
    brif v13, block1(v12, v11), block6
block6:
    v14 = iadd v11, v12
    jump block7
{chain}block47:
    return v{prev}
}}"
    )
}

fn brifs_on(f: &Function, c: Value) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i], InstructionData::Brif { arg, .. } if f.dfg.resolve_aliases(arg) == c))
        .count()
}

#[test]
fn loop_only_unswitch_repairs_exit_values() {
    // Opt-in; this test binary holds only this test.
    unsafe { std::env::set_var("PLIRON_UNSWITCH_LOOP", "1") };
    let before = cranelift_reader::parse_functions(&src()).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::bcheck::unswitch(&mut after, &mut Analysis::default(), "t");
    assert_eq!(k, 1, "{}", after.display());
    // Only the guard still branches on the flag.
    assert_eq!(brifs_on(&after, Value::from_u32(3)), 1, "{}", after.display());
    cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap_or_else(|e| panic!("{e}\n{}", after.display()));
    if let Err(m) = check(&before, &after, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}
