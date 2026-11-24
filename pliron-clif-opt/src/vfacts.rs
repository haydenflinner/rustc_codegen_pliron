//! Value facts rustc gave and lowering kept (`pliron_ir_opt::facts` on the
//! pliron side): a load's `range` and the no-wrap flags of an add, sub or
//! mul (`unchecked_*`). `ranges::Ranges` reads them (`with_facts`), and the
//! differential checker stops a trial that breaks one: the original's
//! behavior there is undefined (`diffcheck::Facts::vfacts`).
//!
//! A load range is a property of the value, so a pass that replaces the
//! value's definition with an equivalent one keeps it true. A no-wrap flag
//! is a property of one computation: a pass may rebuild `isub x, 1` in
//! place as `iadd x, -1` under the same value, and `nuw` on that add would
//! claim `x == 0`. So [`VFacts::nw_of`] answers only while the definition
//! is still the instruction and operands lowering saw.

use cranelift_codegen::ir::{Function, InstructionData, Opcode, Value, ValueDef};
use rustc_hash::FxHashMap;

/// `nw` bit: no signed wrap.
pub const NSW: u8 = 1;
/// `nw` bit: no unsigned wrap.
pub const NUW: u8 = 2;

#[derive(Default, Clone, Debug)]
pub struct VFacts {
    /// Loaded integer value -> (bit width, start, end): the value lies in
    /// `start..=end`, wrapping (start > end wraps through zero).
    pub range: FxHashMap<Value, (u32, u64, u64)>,
    /// `iadd`/`isub`/`imul` result -> its `NSW | NUW` bits and the
    /// opcode and operands they were given for.
    pub nw: FxHashMap<Value, Nw>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nw {
    pub bits: u8,
    pub op: Opcode,
    pub args: [Value; 2],
}

impl VFacts {
    pub fn is_empty(&self) -> bool {
        self.range.is_empty() && self.nw.is_empty()
    }

    /// `v`'s no-wrap bits, while `v` is still defined by the operation
    /// they were recorded for (same opcode, same operands up to aliases).
    pub fn nw_of(&self, func: &Function, v: Value) -> Option<u8> {
        let nw = self.nw.get(&v)?;
        let ValueDef::Result(inst, 0) = func.dfg.value_def(v) else { return None };
        let InstructionData::Binary { opcode, args } = func.dfg.insts[inst] else { return None };
        let same = |a: Value, b: Value| match (func.dfg.try_resolve_aliases(a), func.dfg.try_resolve_aliases(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        };
        (opcode == nw.op && same(args[0], nw.args[0]) && same(args[1], nw.args[1])).then_some(nw.bits)
    }

    /// Whether `x` (the value's bits, zero-extended) satisfies `v`'s range.
    pub fn in_range(&self, v: Value, x: u64) -> bool {
        let Some(&(bits, start, end)) = self.range.get(&v) else { return true };
        let m = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let x = x & m;
        if start <= end { start <= x && x <= end } else { x >= start || x <= end }
    }
}
