//! jump threading (`jumpthread::round`) binds a merge block's params to an
//! edge's args. When the edge is a back edge into a loop header, the args
//! name the *previous* iteration's params (same SSA values as the header's
//! params), but the evaluation env held the new bindings already: the arg
//! `v1019` read as the freshly bound `v1019 = 0`, so the threaded
//! `v1019 == 0` test was decided the wrong way. Found via `fnbisect` on
//! burn-fusion's `Work::claim` (5 `burn-fusion` lib tests failed under pliron
//! only; step `jtround`).

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::vmap::VMap;
use rustc_hash::FxHashSet;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

#[test]
fn back_edge_args_naming_header_params_keep_old_values() {
    // `block232` is the loop header; the edge `block235 -> block232` passes the
    // header's own `v1019..v1021` as the new `v1022..v1024`. The new
    // `v1019` is the constant 0 on that edge, which must not leak into the
    // arg `v1019` (the old iteration's value, != 0 on the false edge).
    let before = parse(
        "function %f(i64, i64, i64) -> i64 {
        block0(v0: i64, v1: i64, v2: i64):
            jump block232(v2, v1, v0, v0, v1, v2)

        block232(v1019: i64, v1020: i64, v1021: i64, v1022: i64, v1023: i64, v1024: i64):
            v1114 = iconst.i64 0
            v1031 = icmp eq v1022, v1114
            brif v1031, block235(v1022, v1023, v1024), block233(v1019, v1020, v1021, v1022, v1023, v1024)

        block233(v1162: i64, v1163: i64, v1164: i64, v1165: i64, v1166: i64, v1167: i64):
            v1032 = icmp eq v1165, v1166
            v1120 = iconst.i64 0
            brif v1032, block235(v1120, v1120, v1120), block244

        block235(v1025: i64, v1026: i64, v1027: i64):
            v1115 = iconst.i64 0
            v1033 = icmp eq v1019, v1115
            brif v1033, block238(v1025, v1026, v1027), block232(v1115, v1115, v1115, v1019, v1020, v1021)

        block238(v1168: i64, v1169: i64, v1170: i64):
            v1171 = iconst.i64 3
            v1172 = imul v1169, v1171
            v1173 = iadd v1168, v1172
            v1174 = iconst.i64 5
            v1175 = imul v1170, v1174
            v1176 = iadd v1173, v1175
            return v1176

        block244:
            v1177 = iconst.i64 999
            return v1177
        }",
    );
    let mut after = before.clone();
    let sh = u32::MAX; // every shape bit: over-admits phases, never skips one
    let mut derived = VMap::default();
    pliron_clif_opt::jumpthread::run(&mut after, &FxHashSet::default(), &mut derived, sh);
    let cfg = Config {
        trials: 512,
        ..Config::default()
    };
    match check(&before, &after, &Facts::default(), &cfg) {
        Ok(s) => assert!(s.compared > 0, "{s:?}"),
        Err(m) => panic!("{m}\n---- after\n{}", after.display()),
    }
}
