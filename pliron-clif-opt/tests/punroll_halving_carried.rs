//! The halving unroll (`binary_search` over a constant-length table)
//! must repoint uses of the loop's values that sit past the exit. The
//! exit region is dominated by the loop block, so it may name a loop
//! value directly instead of receiving it as an exit-edge argument.
//! The one-block form only remapped the edge arguments: regex-syntax's
//! `unicode::class` (an 18-entry `binary_search_by`, found index used
//! after the loop) was left with `ishl v2273, 4` naming a value of the
//! deleted loop, and the build of `~/code/dsa` stopped on the verifier.

use cranelift_codegen::ir::{Function, InstructionData};
use cranelift_codegen::settings::{self, Configurable};
use pliron_clif_opt::pipeline::Analysis;

/// `size` 18 halves while `size - size/2 >= 2`; `base` is selected
/// each round. `block2` reads the last `base` (`v8`) directly.
const SRC: &str = "function u0:0(i32) -> i32 fast {
block0(v0: i32):
    v1 = iconst.i32 18
    v2 = iconst.i32 0
    v9 = iconst.i32 1
    v10 = iconst.i32 2
    jump block1(v1, v2)

block1(v3: i32, v4: i32):
    v5 = ushr v3, v9
    v6 = iadd v5, v4
    v7 = icmp ugt v6, v0
    v8 = select v7, v4, v6
    v11 = isub v3, v5
    v12 = icmp uge v11, v10
    brif v12, block1(v11, v8), block2

block2:
    v13 = iadd v8, v9
    return v13
}";

fn loops(f: &Function) -> usize {
    f.layout
        .blocks()
        .filter(|&b| {
            f.layout.last_inst(b).is_some_and(|t| match f.dfg.insts[t] {
                InstructionData::Brif { blocks, .. } => {
                    blocks.iter().any(|bc| bc.block(&f.dfg.value_lists) == b)
                }
                _ => false,
            })
        })
        .count()
}

#[test]
fn value_named_past_the_exit_follows_the_last_copy() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    assert_eq!(loops(&f), 1);
    let n = pliron_clif_opt::punroll::run(
        &mut f,
        &mut Analysis::default(),
        "halving",
        &Default::default(),
        /* wasm */ true,
    );
    assert!(n >= 1, "the halving loop was not unrolled:\n{}", f.display());
    assert_eq!(loops(&f), 0, "{}", f.display());
    let mut b = settings::builder();
    b.enable("enable_verifier").unwrap();
    cranelift_codegen::verifier::verify_function(&f, &settings::Flags::new(b))
        .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
}
