//! slp store-seeded packing of an unrolled read-modify-write tile:
//! `load acc[k]; load rhs[k]; fma; store acc[k]` for 4 lanes, with the
//! lane loads interleaved between the member stores and addresses built
//! as `(idx + lane)*4` — the shape unrolled GEMM inner loops emit. The
//! merged vector load reads at the emit point (before the vector store),
//! so overlapping member stores must postdate their loads and the
//! leftover scalar lanes must be dead.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const FMA_RMW: &str = "function %f(i64) {
    ss0 = explicit_slot 64, align = 16
    ss1 = explicit_slot 64, align = 16
block0(v8: i64):
    v10 = stack_addr.i64 ss0
    v11 = stack_addr.i64 ss1
    v6 = f32const 0x1.8p0
    v20 = iconst.i64 4
    v21 = iconst.i64 0
    v22 = iconst.i64 1
    v23 = iconst.i64 2
    v24 = iconst.i64 3
    v30 = iadd v8, v21
    v31 = imul v30, v20
    v32 = iadd v11, v31
    v33 = iadd v10, v31
    v34 = load.f32 notrap v32
    v35 = load.f32 notrap v33
    v36 = fma v6, v35, v34
    store.f32 notrap v36, v32
    v40 = iadd v8, v22
    v41 = imul v40, v20
    v42 = iadd v11, v41
    v43 = iadd v10, v41
    v44 = load.f32 notrap v42
    v45 = load.f32 notrap v43
    v46 = fma v6, v45, v44
    store.f32 notrap v46, v42
    v50 = iadd v8, v23
    v51 = imul v50, v20
    v52 = iadd v11, v51
    v53 = iadd v10, v51
    v54 = load.f32 notrap v52
    v55 = load.f32 notrap v53
    v56 = fma v6, v55, v54
    store.f32 notrap v56, v52
    v60 = iadd v8, v24
    v61 = imul v60, v20
    v62 = iadd v11, v61
    v63 = iadd v10, v61
    v64 = load.f32 notrap v62
    v65 = load.f32 notrap v63
    v66 = fma v6, v65, v64
    store.f32 notrap v66, v62
    return
}";

fn run(src: &str) -> (Function, usize) {
    let mut f = parse(src);
    let n = pliron_clif_opt::slp::run(
        &mut f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        true,
        false,
    );
    (f, n)
}

#[test]
fn unrolled_fma_rmw_packs() {
    use cranelift_codegen::ir::{InstructionData, Opcode};
    let (after, n) = run(FMA_RMW);
    let text = after.display().to_string();
    assert!(n > 0, "slp did nothing:\n{text}");
    assert!(text.contains("load.f32x4"), "no vector load:\n{text}");
    assert!(text.contains("splat.f32x4"), "no vector splat:\n{text}");
    let mut vfma = 0;
    let mut stores = 0;
    for b in after.layout.blocks() {
        for i in after.layout.block_insts(b) {
            let d = &after.dfg.insts[i];
            match d.opcode() {
                Opcode::Fma
                    if after
                        .dfg
                        .value_type(after.dfg.inst_results(i)[0])
                        .is_vector() =>
                {
                    vfma += 1
                }
                Opcode::Store => {
                    stores += 1;
                    let InstructionData::Store { args, .. } = d else {
                        unreachable!()
                    };
                    assert!(
                        after.dfg.value_type(args[0]).is_vector(),
                        "scalar store survived:\n{text}"
                    );
                }
                _ => {}
            }
        }
    }
    // One vector fma + one vector store; dead scalar lane insts are left
    // for the pipeline's DCE (slp only sweeps notrap loads).
    assert_eq!((vfma, stores), (1, 1), "expected merged ops:\n{text}");
}

#[test]
fn unrolled_fma_rmw_is_sound() {
    let before = parse(FMA_RMW);
    let (after, _) = run(FMA_RMW);
    let cfg = Config {
        trials: 32,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

#[test]
fn store_then_load_same_lane_rejects() {
    // `store acc[k]; load acc[k]` — the load reads the member store's
    // bytes; a vector load at the emit point would see the pre-store
    // value. Must not pack.
    let src = "function %f(i64) {
        ss0 = explicit_slot 64, align = 16
    block0(v8: i64):
        v10 = stack_addr.i64 ss0
        v20 = iconst.i64 4
        v21 = iconst.i64 0
        v22 = iconst.i64 1
        v23 = iconst.i64 2
        v24 = iconst.i64 3
        v6 = f32const 0x1.8p0
        v30 = iadd v8, v21
        v31 = imul v30, v20
        v32 = iadd v10, v31
        store.f32 notrap v6, v32
        v34 = load.f32 notrap v32
        v40 = iadd v8, v22
        v41 = imul v40, v20
        v42 = iadd v10, v41
        store.f32 notrap v34, v42
        v50 = iadd v8, v23
        v51 = imul v50, v20
        v52 = iadd v10, v51
        store.f32 notrap v34, v52
        v60 = iadd v8, v24
        v61 = imul v60, v20
        v62 = iadd v10, v61
        store.f32 notrap v34, v62
        return
    }";
    let (after, n) = run(src);
    assert_eq!(n, 0, "slp packed a store->load hazard:\n{}", after.display());
}
