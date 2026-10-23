//! `br_table` if-conversion merged arms that reach the merge through the
//! same forwarder chain when their final edge args were the same `Value`.
//! Here that value is `v22 = band v20, 1`, a result computed in the shared
//! hop `block43`. `thru_psub` only renames chain params, so `v22` looked
//! identical for the default arm (`v20 = 0` via `block41`) and the table
//! arm (`v20 = 1` via `block42`). The table collapsed to one arm, and every
//! in-range index took the default's value. polars-plan's
//! `DataTypeSelector::into_columns` filter (a `matches!` over dtype tags)
//! then rejected every column: nushell's `polars selector signed-integer`
//! returned nothing. (The input is the shape jumpthread hands ifconv
//! after threading the other path into `block18` straight to `block3`.)

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::ifconv;
use pliron_clif_opt::vmap::VMap;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const SRC: &str = "function %f(i64, i8) -> i32 {
block0(v0: i64, v7: i8):
    brif.i8 v7, block3, block40

block40:
    v40 = ireduce.i8 v0
    v41 = iconst.i8 7
    v11 = band v40, v41
    v12 = uextend.i64 v11
    v13 = iconst.i64 -1
    v14 = iadd v12, v13
    v15 = iconst.i64 4
    v16 = icmp ugt v14, v15
    brif v16, block41, block62

block62:
    v17 = ireduce.i32 v14
    br_table v17, block41, [block42, block42, block42, block42, block42]

block42:
    v18 = iconst.i8 1
    jump block43(v18)

block41:
    v19 = iconst.i8 0
    jump block43(v19)

block43(v20: i8):
    v21 = iconst.i8 1
    v22 = band v20, v21
    jump block18(v22)

block18(v24: i8):
    v25 = iconst.i8 1
    v26 = band v24, v25
    brif.i8 v26, block4, block3

block3:
    v30 = iconst.i32 0
    return v30

block4:
    v31 = iconst.i32 7
    return v31
}";

#[test]
fn table_arms_through_shared_chain_stay_apart() {
    let before = parse(SRC);
    let mut after = before.clone();
    ifconv::run(&mut after, &mut VMap::default(), None);
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&after, &flags).unwrap();
    // 5 of 8 tag values take the table arm; 256 trials hit it many times.
    if let Err(m) = check(
        &before,
        &after,
        &Facts::default(),
        &Config {
            trials: 256,
            ..Config::default()
        },
    ) {
        panic!("{m}\nafter:\n{}", after.display());
    }
}
