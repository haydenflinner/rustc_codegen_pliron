//! `loadfwd`'s pending-param resolution marks `(block, loc)` pairs dead in a
//! transitive pass: a *resolved* pair dies when one of its edge args is another
//! dead pair's minted param. The dead pair's param is rolled back with
//! `remove_block_param`, but the apply loop still appended that pair's resolved
//! edge args — leaving `brif`s that pass one argument too many, and arguments
//! that are detached values.
//! Real-world hit: `AtomicSparseBufferVec<MeshInputUniform>::push` in bevy_pbr
//! during the bevy_solari build — `remove_constant_phis` tripped on
//! `edge.args.len() != dst_formals.len()`.

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Flags};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const SRC: &str = r#"
function u0:0(i64, i64) -> i32 apple_aarch64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    ss2 = explicit_slot 112, align = 4
    ss3 = explicit_slot 112, align = 4
    ss4 = explicit_slot 112, align = 4
    ss5 = explicit_slot 112, align = 16
    ss6 = explicit_slot 112, align = 16
    ss7 = explicit_slot 16, align = 8
    ss8 = explicit_slot 48, align = 8
    ss9 = explicit_slot 112, align = 16
    ss10 = explicit_slot 112, align = 4
    ss11 = explicit_slot 24, align = 8
    ss12 = explicit_slot 24, align = 8
    ss13 = explicit_slot 8, align = 8
    ss14 = explicit_slot 8, align = 8
    gv0 = symbol colocated userextname0
    gv1 = symbol colocated userextname1
    gv2 = symbol colocated userextname2
    gv3 = symbol colocated userextname3
    gv4 = symbol colocated userextname4
    gv5 = symbol colocated userextname5
    gv6 = symbol colocated userextname6
    gv7 = symbol colocated userextname7
    gv8 = symbol colocated userextname8
    gv9 = symbol colocated userextname9
    sig0 = (i64) apple_aarch64
    sig1 = (i64, i64, i64) apple_aarch64
    sig2 = (i64, i64, i64) apple_aarch64
    sig3 = (i64) apple_aarch64
    sig4 = (i64, i64, i64) apple_aarch64
    fn0 = colocated u0:8781 sig0
    fn1 = colocated u0:8565 sig1
    fn2 = colocated u0:8626 sig2
    fn3 = colocated u0:8771 sig3
    fn4 = colocated u0:8596 sig4
block0(v0: i64, v1: i64):
    v1354 = load.i32 notrap v0+16
    v11 -> v1354
    v12 = stack_addr.i64 ss3
    v1159 = symbol_value.i64 gv9
    v1161 = load.i32 notrap v1159
    store notrap v1161, v12+52
    v1171 = symbol_value.i64 gv9
    v1173 = load.i32 notrap v1171
    store notrap v1173, v12+56
    v1183 = symbol_value.i64 gv9
    v1185 = load.i32 notrap v1183
    store notrap v1185, v12+60
    v1195 = symbol_value.i64 gv9
    v1197 = load.i32 notrap v1195
    store notrap v1197, v12+64
    v1205 = iconst.i64 68
    v794 -> v1205
    v1207 = symbol_value.i64 gv9
    v1209 = load.i32 notrap v1207
    store notrap v1209, v12+68
    v1219 = symbol_value.i64 gv9
    v1221 = load.i32 notrap v1219
    store notrap v1221, v12+72
    v1231 = symbol_value.i64 gv9
    v1233 = load.i32 notrap v1231
    store notrap v1233, v12+76
    v1243 = symbol_value.i64 gv9
    v1245 = load.i32 notrap v1243
    store notrap v1245, v12+80
    v1255 = symbol_value.i64 gv9
    v1257 = load.i32 notrap v1255
    store notrap v1257, v12+84
    v1267 = symbol_value.i64 gv9
    v1269 = load.i32 notrap v1267
    store notrap v1269, v12+88
    v1279 = symbol_value.i64 gv9
    v1281 = load.i32 notrap v1279
    store notrap v1281, v12+92
    v1291 = symbol_value.i64 gv9
    v1293 = load.i32 notrap v1291
    store notrap v1293, v12+96
    v1303 = symbol_value.i64 gv9
    v1305 = load.i32 notrap v1303
    store notrap v1305, v12+100
    v1315 = symbol_value.i64 gv9
    v1317 = load.i32 notrap v1315
    store notrap v1317, v12+104
    v1327 = symbol_value.i64 gv9
    v1329 = load.i32 notrap v1327
    store notrap v1329, v12+108
    v17 = load.i64 notrap v12
    v20 = load.i64 notrap v12+8
    v23 = load.i64 notrap v12+16
    v26 = load.i64 notrap v12+24
    v29 = load.i64 notrap v12+32
    v32 = load.i64 notrap v12+40
    v35 = load.i64 notrap v12+48
    v38 = load.i64 notrap v12+56
    v41 = load.i64 notrap v12+64
    v44 = load.i64 notrap v12+72
    v47 = load.i64 notrap v12+80
    v50 = load.i64 notrap v12+88
    v53 = load.i64 notrap v12+96
    v56 = load.i64 notrap v12+104
    v60 = load.i64 notrap v0+16
    v61 = load.i64 notrap v0
    v62 -> v61
    v63 = icmp eq v60, v62
    brif v63, block23, block24
block23:
    jump block24
block24:
    v69 = iconst.i64 112
    v113 -> v69
    v100 = iconst.i64 1
    v297 -> v100
    v307 -> v100
    v101 = iadd.i64 v60, v100  ; v100 = 1
    v110 -> v101
    store notrap v101, v0+16
    v102 = uextend.i64 v11
    v105 = load.i64 notrap v0+8
    v111 = icmp ult v102, v110
    brif v111, block3, block5
block5 cold:
    trap user2
block3:
    v114 = imul.i64 v102, v113  ; v113 = 112
    v115 = iadd.i64 v105, v114
    v121 = stack_addr.i64 ss10
    v477 -> v121
    v1363 = load.i64x2 notrap v1
    store notrap v1363, v121
    v1364 = load.i64x2 notrap v1+16
    store notrap v1364, v121+16
    v1365 = load.i64x2 notrap v1+32
    store notrap v1365, v121+32
    v1366 = load.i64x2 notrap v1+48
    store notrap v1366, v121+48
    v1367 = load.i64x2 notrap v1+64
    store notrap v1367, v121+64
    v1368 = load.i64x2 notrap v1+80
    store notrap v1368, v121+80
    v1369 = load.i64x2 notrap v1+96
    store notrap v1369, v121+96
    v478 = load.i32 notrap v477
    v497 = load.i32 notrap v121+4
    v516 = load.i32 notrap v121+8
    v535 = load.i32 notrap v121+12
    v554 = load.i32 notrap v121+16
    v573 = load.i32 notrap v121+20
    v592 = load.i32 notrap v121+24
    v611 = load.i32 notrap v121+28
    v630 = load.i32 notrap v121+32
    v649 = load.i32 notrap v121+36
    v668 = load.i32 notrap v121+40
    v687 = load.i32 notrap v121+44
    v706 = load.i32 notrap v121+48
    v725 = load.i32 notrap v121+52
    v744 = load.i32 notrap v121+56
    v763 = load.i32 notrap v121+60
    v782 = load.i32 notrap v121+64
    v795 = iadd v115, v794  ; v794 = 68
    v801 = load.i32 notrap v121+68
    atomic_store notrap aligned v801, v795
    v820 = load.i32 notrap v121+72
    v839 = load.i32 notrap v121+76
    v858 = load.i32 notrap v121+80
    v877 = load.i32 notrap v121+84
    v896 = load.i32 notrap v121+88
    v915 = load.i32 notrap v121+92
    v934 = load.i32 notrap v121+96
    v953 = load.i32 notrap v121+100
    v972 = load.i32 notrap v121+104
    v991 = load.i32 notrap v121+108
    v1355 = iconst.i32 6
    v1356 = ushr.i32 v1354, v1355  ; v1355 = 6
    v286 -> v1356
    v287 = uextend.i64 v286
    v1357 = iconst.i64 6
    v1358 = ushr v287, v1357  ; v1357 = 6
    v289 -> v1358
    v298 = iadd v289, v297  ; v297 = 1
    v294 = load.i64 notrap v0+376
    v299 = icmp ult v294, v298
    brif v299, block8(v294), block133
block133:
    v308 = iadd.i64 v287, v307  ; v307 = 1
    v304 = load.i64 notrap v0+400
    v309 = icmp ult v304, v308
    brif v309, block13(v304), block11(v304)
block11(v1374: i64):
    v321 = load.i64 notrap v0+376
    v9011 = iconst.i32 0
    return v9011
block118 cold:
    v9118 = iconst.i32 0
    return v9118
block117:
    v9117 = iconst.i32 0
    return v9117
block120 cold:
    v9120 = iconst.i32 0
    return v9120
block119:
    v9119 = iconst.i32 0
    return v9119
block13(v1373: i64):
    v373 = load.i64 notrap v0+384
    v374 -> v373
    v375 = icmp eq v1373, v374
    brif v375, block125, block126
block125:
    v9125 = iconst.i32 0
    return v9125
block126:
    v1375 = load.i64 notrap v0+400
    v1376 = icmp ult v1375, v308
    brif v1376, block13(v1375), block11(v1375)
block8(v1370: i64):
    v395 = load.i64 notrap v0+360
    v396 -> v395
    v397 = icmp eq v1370, v396
    brif v397, block110, block111
block110:
    v9110 = iconst.i32 0
    return v9110
block111:
    v1371 = load.i64 notrap v0+376
    v1372 = icmp ult v1371, v298
    brif v1372, block8(v1371), block133
}

"#;

#[test]
fn dead_pair_edge_args_not_appended() {
    let mut f = parse(SRC);
    cranelift_codegen::verify_function(&f, &Flags::new(settings::builder()))
        .expect("test input must be verifier-clean");
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f,
        &cfg,
        &pf,
        &ps,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
    );
    cranelift_codegen::verify_function(&f, &Flags::new(settings::builder()))
        .expect("loadfwd left arity-mismatched edges");
}
