//! Shape-adaptive small-switch lowering (`PLIRON_SWITCHADAPT=1`, opt-in).
//!
//! Cranelift's backend lowers every `br_table` as a table load plus an
//! indirect branch. LLVM instead picks compare chains and bit tests for
//! sparse switches. This pass handles the shape `brchain` cannot: a sparse
//! table whose few *destinations* each cover several non-contiguous case
//! indices. The html lexer shape is typical: bytes 9, 10, 12 and 32 share
//! the whitespace arm, byte 47 and byte 62 have singleton arms, and the
//! other entries (letters/digits) take the default. That is three direct
//! tests, not a 54-entry indirect dispatch.
//!
//! The rewrite preserves `br_table` exactly. The leading unsigned bounds
//! test sends out-of-range values to the default; in-range values go
//! through equality/range/bitmask tests and the last fall-through is the
//! same default edge. Bit tests are only emitted when the range test
//! proves the shift index is below 64, so the AArch64/x64 modulo-shift
//! behavior cannot alias an out-of-range value into a case.
//!
//! When the index is the usual narrow-switch form `uextend ((c - s) mod W)`
//! the tests are written against the raw narrow value `c` at `case + s`.
//! That removes the subtraction entirely when the table was its only use
//! (the same shape `PLIRON_SWITCH_PRETEST` recognizes in the aarch64 table
//! lowering). `s + len` must not wrap in the narrow type and must be at
//! most 64; html's byte lexers satisfy both (`9 + 54 = 63`).
//!
//! The pass is deliberately separate from `brchain`: `brchain` emits
//! equality/range chains for tiny or contiguous tables late in the
//! pipeline, while this pass runs early enough that same-destination case
//! sets have not yet been split into per-edge copies and LICM can see the
//! bitmask constants. It is opt-in until the exact-count profitability
//! model is validated.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, Block, BlockArg, BlockCall, Function, InstBuilder, InstructionData, Opcode, Type, Value,
};

/// Maximum number of tests in the direct dispatch. A table dispatch is
/// about seven AArch64 instructions including its bounds test; four
/// cmp/branch tests are the outer bound, and a bit-test group often
/// covers several cases in one test.
const MAX_TESTS: usize = 4;

type Dest = (Block, Vec<BlockArg>);

fn dest(func: &Function, bc: BlockCall) -> Dest {
    (bc.block(&func.dfg.value_lists), bc.args(&func.dfg.value_lists).collect())
}

fn same_target(func: &Function, a: BlockCall, b: BlockCall) -> bool {
    a.block(&func.dfg.value_lists) == b.block(&func.dfg.value_lists)
        && a.args(&func.dfg.value_lists).eq(b.args(&func.dfg.value_lists))
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// The value the emitted tests read, and the addition needed to turn a
/// table index into that value's domain.
#[derive(Clone, Copy)]
struct Domain {
    x: Value,
    ty: Type,
    /// Case value for table index `i`: `add + i`.
    add: u64,
    /// Unsigned upper bound: values `>= upper` take the default.
    upper: u64,
}

/// Recognize `uextend ((c - s) mod W)` / `uextend ((c + (W-s)) mod W)`.
/// Returns the raw narrow `c` with `add=s` when the live case interval
/// `[s, s+len)` neither wraps the narrow type nor crosses the 64-bit
/// bitmask domain. Otherwise the table index is used unchanged.
fn domain(func: &Function, arg: Value, len: usize) -> Domain {
    let ity = func.dfg.value_type(arg);
    let direct = Domain { x: arg, ty: ity, add: 0, upper: len as u64 };
    let v = func.dfg.resolve_aliases(arg);
    let Some(ui) = func.dfg.value_def(v).inst() else { return direct };
    let InstructionData::Unary { opcode: Opcode::Uextend, arg: inner } = func.dfg.insts[ui] else {
        return direct;
    };
    let inner = func.dfg.resolve_aliases(inner);
    let Some(si) = func.dfg.value_def(inner).inst() else { return direct };
    let InstructionData::Binary { opcode: op, args } = func.dfg.insts[si] else {
        return direct;
    };
    if !matches!(op, Opcode::Isub | Opcode::Iadd) {
        return direct;
    }
    let ty = func.dfg.value_type(inner);
    if ty.bits() > 16 || ty.bits() >= 64 {
        return direct;
    }
    let w = 1u64 << ty.bits();
    let (c, k) = if let Some(k) = iconst(func, args[1]) {
        (args[0], k as u64)
    } else if op == Opcode::Iadd {
        if let Some(k) = iconst(func, args[0]) {
            (args[1], k as u64)
        } else {
            return direct;
        }
    } else {
        return direct;
    };
    let j = k & (w - 1);
    let s = match op {
        Opcode::Isub => j,
        // Only a negative narrow addend is a subtraction modulo W.
        Opcode::Iadd if j >= w / 2 => w - j,
        _ => return direct,
    };
    // Reject the identity and any interval that wraps in the narrow type.
    // The 64 cap is what makes the raw value safe as a bitmask shift after
    // the single upper-bound test (bytes below `s` can reach the tests but
    // no case bit exists below `s`, so they fall through correctly).
    if s == 0 || s + len as u64 > w || s + len as u64 > 64 {
        return direct;
    }
    Domain { x: c, ty, add: s, upper: s + len as u64 }
}

#[derive(Clone)]
enum Test {
    /// `x == k`.
    Eq(u64, Dest),
    /// `lo <= x <= hi`, emitted as one unsigned range test.
    Range(u64, u64, Dest),
    /// Bit `x` of `mask` is set (x is already known `< 64`).
    Bits(u64, Dest),
}

/// Cases of one destination, in table order.
struct Group {
    dest: Dest,
    idxs: Vec<u64>,
}

fn groups(func: &Function, entries: &[BlockCall], dflt: BlockCall) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    for (i, &e) in entries.iter().enumerate() {
        if same_target(func, e, dflt) {
            continue;
        }
        let d = dest(func, e);
        match out.iter_mut().find(|g| g.dest == d) {
            Some(g) => g.idxs.push(i as u64),
            None => out.push(Group { dest: d, idxs: vec![i as u64] }),
        }
    }
    // Cases of a multi-case destination are usually a character class;
    // test those first. Ties stay in table order. This is LLVM's bit-test
    // cluster order for the html lexer shape.
    out.sort_by_key(|g| (std::cmp::Reverse(g.idxs.len()), g.idxs[0]));
    out
}

/// Maximal consecutive runs of a sorted case list.
fn runs(idxs: &[u64]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for &i in idxs {
        match out.last_mut() {
            Some((_, hi)) if *hi + 1 == i => *hi = i,
            _ => out.push((i, i)),
        }
    }
    out
}

fn tests_for(domain: Domain, groups: &[Group]) -> Option<Vec<Test>> {
    let mut out = Vec::new();
    let mut has_bits = false;
    for g in groups {
        let idxs: Vec<u64> = g.idxs.iter().map(|&i| i + domain.add).collect();
        let rs = runs(&idxs);
        if rs.len() == 1 {
            let (lo, hi) = rs[0];
            out.push(if lo == hi { Test::Eq(lo, g.dest.clone()) } else { Test::Range(lo, hi, g.dest.clone()) });
        } else {
            // The domain construction caps the raw upper bound at 64, and
            // the direct-index table itself is capped at 64 entries. The
            // leading range test therefore makes the modulo-shift exact.
            let mask = idxs.iter().fold(0u64, |m, &i| {
                if i >= 64 { return m }
                m | (1u64 << i)
            });
            if mask == 0 || idxs.iter().any(|&i| i >= 64) {
                // Should not happen with the current caps; keep the pass
                // conservative if a wider form is added later.
                for &(lo, hi) in &rs {
                    out.push(if lo == hi { Test::Eq(lo, g.dest.clone()) } else { Test::Range(lo, hi, g.dest.clone()) });
                }
            } else {
                has_bits = true;
                out.push(Test::Bits(mask, g.dest.clone()));
            }
        }
    }
    if out.len() > MAX_TESTS || !has_bits {
        return None;
    }
    Some(out)
}

fn emit_test(pos: &mut FuncCursor, d: Domain, test: &Test) -> Value {
    match test {
        Test::Eq(k, _) => {
            let k = pos.ins().iconst(d.ty, *k as i64);
            pos.ins().icmp(IntCC::Equal, d.x, k)
        }
        Test::Range(lo, hi, _) => {
            let x = if *lo == 0 {
                d.x
            } else {
                let k = pos.ins().iconst(d.ty, *lo as i64);
                pos.ins().isub(d.x, k)
            };
            let w = pos.ins().iconst(d.ty, (hi - lo) as i64);
            pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, x, w)
        }
        Test::Bits(mask, _) => {
            let x = if d.ty == types::I64 { d.x } else { pos.ins().uextend(types::I64, d.x) };
            let m = pos.ins().iconst(types::I64, *mask as i64);
            let sh = pos.ins().ushr(m, x);
            pos.ins().band_imm_u(sh, 1)
        }
    }
}

fn rewrite(func: &mut Function, b: Block, term: cranelift_codegen::ir::Inst, d: Domain, tests: &[Test], dflt: Dest) {
    func.layout.remove_inst(term);
    // The original block keeps only the exact bounds test. Out-of-range is
    // the table default; in-range enters the direct chain.
    let bin = func.dfg.make_block();
    func.layout.insert_block_after(bin, b);
    {
        let mut pos = FuncCursor::new(func).at_bottom(b);
        let k = pos.ins().iconst(d.ty, d.upper as i64);
        let c = pos.ins().icmp(IntCC::UnsignedLessThan, d.x, k);
        pos.ins().brif(c, bin, &[], dflt.0, &dflt.1);
    }
    let mut cur = bin;
    for (i, test) in tests.iter().enumerate() {
        let hit = match test {
            Test::Eq(_, d) | Test::Range(_, _, d) | Test::Bits(_, d) => d.clone(),
        };
        let next = if i + 1 == tests.len() {
            None
        } else {
            let nb = func.dfg.make_block();
            func.layout.insert_block_after(nb, cur);
            Some(nb)
        };
        let mut pos = FuncCursor::new(func).at_bottom(cur);
        let c = emit_test(&mut pos, d, test);
        match next {
            Some(nb) => pos.ins().brif(c, hit.0, &hit.1, nb, &[]),
            None => pos.ins().brif(c, hit.0, &hit.1, dflt.0, &dflt.1),
        };
        if let Some(nb) = next {
            cur = nb;
        }
    }
}

pub fn run(func: &mut Function) -> usize {
    run_with(func, false)
}

/// `cheap_br_table`: wasm engines already lower a dense jump table well;
/// leave those tables alone, as `brchain` does.
pub fn run_with(func: &mut Function, cheap_br_table: bool) -> usize {
    if cheap_br_table {
        return 0;
    }
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut todo = Vec::new();
    for b in blocks {
        let Some(term) = func.layout.last_inst(b) else { continue };
        let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[term] else { continue };
        let jt = &func.dfg.jump_tables[table];
        let entries: Vec<BlockCall> = jt.as_slice().to_vec();
        let len = entries.len();
        if len == 0 || len > 64 {
            continue;
        }
        let dflt_bc = jt.default_block();
        let gs = groups(func, &entries, dflt_bc);
        if gs.is_empty() {
            continue;
        }
        // This pass is for sparse switches. Dense small enums are either
        // `brchain` material (few cases) or better left as tables (many
        // singleton destinations); requiring both sparsity and a grouped
        // destination keeps those out even when they accidentally fit the
        // test-count cap.
        let live: usize = gs.iter().map(|g| g.idxs.len()).sum();
        if live * 2 > len || !gs.iter().any(|g| g.idxs.len() > 1) {
            continue;
        }
        let d = domain(func, arg, len);
        let Some(tests) = tests_for(d, &gs) else { continue };
        if !pliron_cfg::bisect("switchadapt") {
            continue;
        }
        todo.push((b, term, d, tests, dest(func, dflt_bc)));
    }
    let n = todo.len();
    for (b, term, d, tests, dflt) in todo {
        rewrite(func, b, term, d, &tests, dflt);
    }
    n
}
