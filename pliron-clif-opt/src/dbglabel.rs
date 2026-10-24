//! `-g` when optimizing: Cranelift value labels for source variables.
//!
//! A label is a `pliron_ir_opt::dbg::Piece` id — some bytes of one
//! variable instance. Lowering labels the values that hold a piece; passes
//! that move a variable out of memory label what they put in its place
//! (slotprom, via [`DbgSlots`]). Labels only describe values, never
//! change code; regalloc reports where each labelled value lives.

use cranelift_codegen::ir::{DataFlowGraph, Value, ValueLabel, ValueLabelAssignments, ValueLabelStart};
use rustc_hash::FxHashMap;

/// Variable stack slots' pieces: `(slot key, byte offset, size)` → labels,
/// for each access lowering saw. A pass promoting that slot field to SSA
/// labels the values it stores with them.
pub type DbgSlots = FxHashMap<(u64, i64, u32), Vec<u32>>;

/// Attach label `l` to each `(value, l)` (no duplicates).
pub fn add(dfg: &mut DataFlowGraph, labels: &[(Value, u32)]) {
    if labels.is_empty() {
        return;
    }
    dfg.collect_debug_info();
    let vl = dfg.values_labels.as_mut().unwrap();
    for &(v, l) in labels {
        let start = ValueLabelStart { from: Default::default(), label: ValueLabel::from_u32(l) };
        match vl.entry(v).or_insert_with(|| ValueLabelAssignments::Starts(Vec::new())) {
            ValueLabelAssignments::Starts(s) => {
                if !s.iter().any(|x| x.label == start.label) {
                    s.push(start)
                }
            }
            ValueLabelAssignments::Alias { .. } => {}
        }
    }
}
