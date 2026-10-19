//! edgespec threads an edge into a loop header from inside the loop. The
//! header's params bind in parallel and the args still name the old
//! iteration's values; binding them one by one (or chasing the new binding)
//! read `prev` as the new `cur`. Found by the differential checker in std's
//! `SocketAncillary` cmsg walk.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

#[test]
fn back_edge_into_header_keeps_old_param_values() {
    // Walk a list: `cur` = 0 ends it; the result says whether `prev` was set.
    let before = parse(
        "function %f(i64) -> i8 {
        block0(v0: i64):
            v1 = iconst.i64 0
            jump block1(v0, v1)

        block1(v2: i64, v3: i64):
            v4 = icmp eq v2, v1
            brif v4, block3(v3), block2

        block2:
            v5 = load.i64 notrap v2
            v6 = iconst.i64 16
            v7 = icmp ult v5, v6
            brif v7, block4, block1(v5, v2)

        block4:
            jump block1(v1, v2)

        block3(v8: i64):
            v9 = icmp eq v8, v1
            brif v9, block5, block6

        block5:
            v10 = iconst.i8 0
            return v10

        block6:
            v11 = iconst.i8 1
            return v11
        }",
    );
    let mut after = before.clone();
    pliron_clif_opt::edgespec::run(&mut after, &mut pliron_clif_opt::jumpthread::Cd::new());
    let cfg = Config {
        trials: 128,
        ..Config::default()
    };
    match check(&before, &after, &Facts::default(), &cfg) {
        Ok(s) => assert!(s.compared > 0, "{s:?}"),
        Err(m) => panic!("{m}\n---- after\n{}", after.display()),
    }
}
