//! esplit clones a merge block for one predecessor edge and must map
//! the clone's operands onto the clone's own params and results. The
//! map was looked up by the operand's exact value, so an operand that
//! reaches a param through an alias (what GVN leaves behind) missed it
//! and the clone kept using the original block's value, which does not
//! dominate it. `loro_internal::state::movable_list_state` hit this in
//! the wasm build of `~/code/dsa`: `v3996 = ireduce.i8 v3948` "uses
//! value v3948 from non-dominating inst" (`v3948 -> v625`). Nothing
//! verifies between esplit and the wasm emitter, so the module was
//! emitted reading a local that path never wrote.

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};
use pliron_clif_opt::pipeline::Analysis;

/// UTF-8 `is_ascii` count: on the ASCII edge `c < 128` folds, so
/// `block2` is cloned for it. `v15` reads the block's own `v14`
/// through the alias `v20`.
const SRC: &str = "function u0:0(i32, i32) -> i32 fast {
block0(v0: i32, v1: i32):
    v2 = uload8.i32 notrap v0
    v3 = iconst.i32 128
    v4 = band v2, v3
    v5 = iconst.i32 0
    v21 = iconst.i32 255
    v6 = icmp ne v4, v5
    brif v6, block1, block2(v2)

block1:
    v7 = uload8.i32 notrap v0+1
    v8 = iadd v2, v7
    jump block2(v8)

block2(v9: i32):
    v10 = icmp ult v9, v3
    v11 = uextend.i32 v10
    v14 = band v9, v21
    v20 -> v14
    v15 = iadd v20, v1
    v12 = iadd v11, v15
    jump block3(v12)

block3(v13: i32):
    return v13
}";

fn verify(f: &Function) {
    let mut b = settings::builder();
    b.enable("enable_verifier").unwrap();
    cranelift_codegen::verifier::verify_function(f, &settings::Flags::new(b))
        .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
}

#[test]
fn clone_maps_an_aliased_operand() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    verify(&f);
    let n = pliron_clif_opt::esplit::run(&mut f, &mut Analysis::default());
    assert_eq!(n, 1, "the ASCII edge was not specialised:\n{}", f.display());
    verify(&f);
}
