//! Small `br_table`s with few targets as bit tests (`PLIRON_BRBITS=1`,
//! opt-in).
//!
//! A `match` on a byte with a handful of special values becomes a
//! `br_table` over the byte's range: lol_html's `tag_name_state` switches
//! on `b - 9` with 54 entries, all but six going to the default (letters,
//! digits) and the six to three targets (whitespace, `/`, `>`). The table
//! dispatch is a bounds test plus `csel; adr; ldrb; adr; add; br` for every
//! byte in range. LLVM lowers such a switch with bit tests (its
//! `SwitchLowering` "bit test clusters": at most three destinations within
//! one machine word): `lsr x, mask, b; tbnz`.
//!
//! Rule: a `br_table` of at most 64 entries whose non-default entries go to
//! at most `MAX_TARGETS` distinct destinations. The block keeps the bounds
//! test, then tests the index against the union of the non-default entries
//! (one `ushr` of a constant and a bit test, all the default path runs),
//! then against each destination's set, most entries first; the last
//! destination takes what remains. Destination and default arguments are
//! kept as they were.
//!
//! Measured 2026-11-22 (exact non-nop), not for default: html +0.34%,
//! pulldown +0.52% together with `PLIRON_ELABKEEP_WIDE=1` (without it the
//! 64-bit mask is rebuilt in the loop on every byte: `mov; movk x3`), regex
//! -0.12%. lol_html's lexer and `TagScanner` switches hit their special
//! bytes often enough that the chain of tests costs more than the table.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, Block, BlockArg, BlockCall, Function, InstBuilder, InstructionData, Value};

const MAX_TARGETS: usize = 3;

type Dest = (Block, Vec<BlockArg>);

fn dest(func: &Function, bc: BlockCall) -> Dest {
    (bc.block(&func.dfg.value_lists), bc.args(&func.dfg.value_lists).collect())
}

pub fn run(func: &mut Function) -> usize {
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut n = 0;
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[t] else { continue };
        let jt = &func.dfg.jump_tables[table];
        let entries: Vec<Dest> = jt.as_slice().iter().map(|&bc| dest(func, bc)).collect();
        let dflt = dest(func, jt.default_block());
        let len = entries.len();
        if len == 0 || len > 64 {
            continue;
        }
        // Distinct non-default destinations, with their index masks.
        let mut targets: Vec<(Dest, u64, u32)> = Vec::new();
        let mut ok = true;
        for (i, e) in entries.iter().enumerate() {
            if *e == dflt {
                continue;
            }
            match targets.iter_mut().find(|(d, _, _)| d == e) {
                Some((_, m, c)) => {
                    *m |= 1u64 << i;
                    *c += 1;
                }
                None => {
                    if targets.len() == MAX_TARGETS {
                        ok = false;
                        break;
                    }
                    targets.push((e.clone(), 1u64 << i, 1));
                }
            }
        }
        if !ok || targets.is_empty() || !pliron_cfg::bisect("brbits") {
            continue;
        }
        targets.sort_by(|a, b| b.2.cmp(&a.2));
        let all: u64 = targets.iter().fold(0, |m, (_, x, _)| m | x);
        let idx = func.dfg.resolve_aliases(arg);
        let ity = func.dfg.value_type(idx);
        func.layout.remove_inst(t);
        // Bounds test in the original block, then the union test.
        let b_in = func.dfg.make_block();
        func.layout.insert_block_after(b_in, b);
        {
            let mut pos = FuncCursor::new(func).at_bottom(b);
            let k = pos.ins().iconst(ity, len as i64);
            let c = pos.ins().icmp(IntCC::UnsignedLessThan, idx, k);
            pos.ins().brif(c, b_in, &[], dflt.0, &dflt.1);
        }
        // Bit `idx` of `mask`, computed at the bottom of `at`.
        let bit = |func: &mut Function, at: Block, mask: u64| -> Value {
            let mut pos = FuncCursor::new(func).at_bottom(at);
            let wide = if ity == types::I64 { idx } else { pos.ins().uextend(types::I64, idx) };
            let m = pos.ins().iconst(types::I64, mask as i64);
            let sh = pos.ins().ushr(m, wide);
            pos.ins().band_imm(sh, 1)
        };
        let c = bit(func, b_in, all);
        if targets.len() == 1 {
            let d = &targets[0].0;
            FuncCursor::new(func).at_bottom(b_in).ins().brif(c, d.0, &d.1, dflt.0, &dflt.1);
            n += 1;
            continue;
        }
        let mut cur = func.dfg.make_block();
        func.layout.insert_block_after(cur, b_in);
        FuncCursor::new(func).at_bottom(b_in).ins().brif(c, cur, &[], dflt.0, &dflt.1);
        // One test per destination, most entries first; the last takes the rest.
        let last = targets.len() - 1;
        for j in 0..last {
            let c = bit(func, cur, targets[j].1);
            let hit = targets[j].0.clone();
            if j + 1 == last {
                let rest = targets[last].0.clone();
                FuncCursor::new(func).at_bottom(cur).ins().brif(c, hit.0, &hit.1, rest.0, &rest.1);
            } else {
                let nb = func.dfg.make_block();
                func.layout.insert_block_after(nb, cur);
                FuncCursor::new(func).at_bottom(cur).ins().brif(c, hit.0, &hit.1, nb, &[]);
                cur = nb;
            }
        }
        n += 1;
    }
    n
}
