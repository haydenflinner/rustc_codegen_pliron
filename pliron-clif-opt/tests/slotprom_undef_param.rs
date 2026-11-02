//! slotprom `PLIRON_SLOTPROM_UNDEFPARAM`: a promoted field whose reaching
//! value at a bounded call is a param fed only undefined values (the entry
//! undef, or itself around a loop) gets no writeback: it would store undefined
//! bytes over undefined memory. pulldown's `Parser::next` zeroed
//! `item_to_event`'s sret slot before every call. A field written on any
//! path into the call keeps its writeback.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Function, Inst, Opcode};
use rustc_hash::FxHashSet;

// ss0 is an sret-style temp filled by `fn0`. Two paths merge before the
// call: neither writes field +0 (its reaching value is a param fed only
// undefs), one writes field +8 (a real value on that edge).
const SRC: &str = "function %f(i64, i64) -> i64 {
    ss0 = explicit_slot 16, align = 8
    sig0 = (i64)
    fn0 = %g sig0
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    brif v0, block1, block2
block1:
    store.i64 notrap v1, v2+8
    jump block3
block2:
    jump block3
block3:
    v5 = stack_addr.i64 ss0
    call fn0(v5)
    v6 = load.i64 notrap v5
    v7 = load.i64 notrap v5+8
    v8 = iadd v6, v7
    return v8
}";

fn stores(f: &Function) -> Vec<String> {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Store)
        .map(|i| f.dfg.display_inst(i).to_string())
        .collect()
}

#[test]
fn undefined_field_gets_no_writeback() {
    // Opt-in; every test in this binary sets the same value.
    unsafe { std::env::set_var("PLIRON_SLOTPROM_UNDEFPARAM", "1") };
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let call = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| f.dfg.insts[i].opcode() == Opcode::Call)
        .unwrap();
    let nocap: FxHashSet<(Inst, u32)> = [(call, 0)].into_iter().collect();
    let cfg = ControlFlowGraph::with_function(&f);
    pliron_clif_opt::slotprom::run(&mut f, &cfg, &nocap, false, &Default::default());
    let st = stores(&f);
    // Only field +8's writeback remains.
    assert_eq!(st.len(), 1, "{st:?}\n{}", f.display());
    assert!(st[0].contains("+8"), "{st:?}\n{}", f.display());
    cranelift_codegen::verify_function(&f, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .expect("verifier");
}

// A callback's sret temp in a loop: `fn0` fills ss0 each iteration and the
// loop reads it back. The value reaching the next call is the previous
// call's reload, which memory already holds, so the writeback goes too
// (`parse_block`'s special-byte loop).
const LOOP: &str = "function %f(i64, i64) -> i64 {
    ss0 = explicit_slot 16, align = 8
    sig0 = (i64)
    fn0 = %g sig0
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v5 = stack_addr.i64 ss0
    call fn0(v5)
    v6 = load.i64 notrap v5
    v7 = iadd v4, v6
    v8 = iconst.i64 1
    v9 = iadd v3, v8
    v10 = icmp ult v9, v0
    brif v10, block1(v9, v7), block2
block2:
    return v7
}";

#[test]
fn reloaded_field_gets_no_writeback() {
    unsafe { std::env::set_var("PLIRON_SLOTPROM_UNDEFPARAM", "1") };
    let mut f = cranelift_reader::parse_functions(LOOP).unwrap().remove(0);
    let call = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| f.dfg.insts[i].opcode() == Opcode::Call)
        .unwrap();
    let nocap: FxHashSet<(Inst, u32)> = [(call, 0)].into_iter().collect();
    let cfg = ControlFlowGraph::with_function(&f);
    pliron_clif_opt::slotprom::run(&mut f, &cfg, &nocap, false, &Default::default());
    assert!(stores(&f).is_empty(), "{:?}\n{}", stores(&f), f.display());
    cranelift_codegen::verify_function(&f, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .expect("verifier");
}

// A field reloaded after one call and copied into another slot's field:
// the copy's memory never held that value, so its writeback before the
// second call stays (a global "is a reload" check dropped it and
// miscompiled blind-sig).
const COPY: &str = "function %f(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    sig0 = (i64)
    fn0 = %g sig0
    fn1 = %h sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = stack_addr.i64 ss1
    store.i64 notrap v0, v2
    call fn0(v1)
    v3 = load.i64 notrap v1
    store.i64 notrap v3, v2
    call fn1(v2)
    v4 = load.i64 notrap v2
    return v4
}";

#[test]
fn copied_reload_keeps_its_writeback() {
    unsafe { std::env::set_var("PLIRON_SLOTPROM_UNDEFPARAM", "1") };
    let mut f = cranelift_reader::parse_functions(COPY).unwrap().remove(0);
    let calls: Vec<Inst> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Call)
        .collect();
    let nocap: FxHashSet<(Inst, u32)> = calls.iter().map(|&c| (c, 0)).collect();
    let cfg = ControlFlowGraph::with_function(&f);
    pliron_clif_opt::slotprom::run(&mut f, &cfg, &nocap, false, &Default::default());
    // ss1's writeback before `fn1` (the copied reload) must remain.
    assert_eq!(stores(&f).len(), 1, "{:?}\n{}", stores(&f), f.display());
    cranelift_codegen::verify_function(&f, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .expect("verifier");
}
