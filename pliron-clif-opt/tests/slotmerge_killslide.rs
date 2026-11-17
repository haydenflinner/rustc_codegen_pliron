//! `PLIRON_SM_KILLSLIDE=1`: an end kill moves past the reads of its slot
//! that a pass sank behind it within the block. `slp` pairs two loads of
//! an `sret` result into one vector load at the position of the stores
//! it feeds, after the end marker anchored on the last scalar load
//! (regex-syntax `Parser::parse_with_comments`). Without the slide the
//! stale-marker check drops every marker of the slot, its live range
//! becomes the loop it sits in, and the `Result` slots of the parser's
//! exclusive match arms (seven of 128 bytes) cannot share.

use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, StackSlot, Value};
use rustc_hash::FxHashMap;

type Lifetimes = Vec<(Vec<StackSlot>, Option<Inst>, Block, bool)>;

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

fn inst(f: &Function, b: u32, n: usize) -> Option<Inst> {
    f.layout.block_insts(Block::from_u32(b)).nth(n)
}

// A loop with two exclusive arms, each filling its own slot through an
// `sret` call and reading it back. In block2 the second read (v6) sits
// after ss0's end marker, which is anchored on the first read (v5).
const SRC: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 16, align = 8
    ss1 = explicit_slot 16, align = 8
    sig0 = (i64 sret)
    fn0 = %fill0 sig0
    fn1 = %fill1 sig0
block0(v0: i64):
    v1 = iconst.i64 0
    jump block1(v1)
block1(v2: i64):
    v3 = band v2, v0
    brif v3, block2, block3
block2:
    v4 = stack_addr.i64 ss0
    call fn0(v4)
    v5 = load.i64 v4
    v6 = load.i64 v4+8
    v7 = iadd v5, v6
    jump block4(v7)
block3:
    v8 = stack_addr.i64 ss1
    call fn1(v8)
    v9 = load.i64 v8
    v10 = load.i64 v8+8
    v11 = iadd v9, v10
    jump block4(v11)
block4(v12: i64):
    v13 = iadd v2, v12
    v14 = icmp ult v13, v0
    brif v14, block1(v13), block5
block5:
    return v13
}";

#[test]
fn sunk_read_slides_the_kill() {
    // SAFETY: set before any config read in this test binary.
    unsafe {
        std::env::set_var("PLIRON_SLOTMERGE_MARK", "1");
        std::env::set_var("PLIRON_SM_MARKFLOW", "1");
        std::env::set_var("PLIRON_SM_KILLSLIDE", "1");
    }
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let (ss0, ss1) = (StackSlot::from_u32(0), StackSlot::from_u32(1));
    let (b2, b3) = (Block::from_u32(2), Block::from_u32(3));
    let lifetimes: Lifetimes = vec![
        (vec![ss0], None, b2, true),
        // Anchored on v5's load: v6's load was sunk past the marker.
        (vec![ss0], inst(&f, 2, 2), b2, false),
        (vec![ss1], None, b3, true),
        (vec![ss1], inst(&f, 3, 3), b3, false),
    ];
    pliron_clif_opt::slotmerge::run(
        &mut f,
        &FxHashMap::default(),
        &lifetimes,
        &FxHashMap::default(),
        &pliron_clif_opt::target::Target::host(),
    );
    assert_eq!(slot_of(&f, 4), slot_of(&f, 8), "exclusive arms' slots not shared:\n{}", f.display());
}
