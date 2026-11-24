//! Facts rustc hands the builder that the LLVM dialect has no field for,
//! kept as `pliron_*` op attributes (`src/builder.rs`, `note_op`): an op
//! attribute travels with the op when a pass clones it, unlike a side table.
//! The dialect's own fields carry the rest: `nsw`/`nuw` on add/sub/mul/shl
//! (`IntegerOverflowFlagsAttr`), `nuw` on GEPs (`GepNoWrapFlagsAttr`), load
//! and store alignment (`AlignableOpInterface`), `nneg` on zext.
//!
//! | attribute | on | meaning |
//! |---|---|---|
//! | `pliron_trunc_nuw` | trunc | zero-extending the result gives the operand back |
//! | `pliron_trunc_nsw` | trunc | sign-extending the result gives the operand back |
//! | `pliron_disjoint` | or | no bit set in both operands (an add) |
//! | `pliron_exact` | udiv, sdiv | no remainder |
//! | `pliron_range` | load | the value lies in `start..=end`, wrapping (`"start:end"`) |
//! | `pliron_cold_cases` | switch | cold case indices, `else` for a cold default |
//! | `pliron_loop_hints` | br | rustc's `UnrollAttr`s, `Debug`-printed |

use pliron::builtin::attributes::{BoolAttr, StringAttr};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::operation::Operation;

fn key(k: &str) -> Identifier {
    Identifier::try_new(k.to_string()).unwrap()
}

/// A boolean `pliron_*` fact on `op`.
pub fn flag(ctx: &Context, op: Ptr<Operation>, k: &str) -> bool {
    op.deref(ctx).attributes.get::<BoolAttr>(ctx, &key(k)).is_some_and(|b| bool::from(b.clone()))
}

fn string(ctx: &Context, op: Ptr<Operation>, k: &str) -> Option<String> {
    op.deref(ctx).attributes.get::<StringAttr>(ctx, &key(k)).map(|s| String::from(s.clone()))
}

/// A load's `range` (`start..=end`, wrapping: `start > end` wraps around).
pub fn load_range(ctx: &Context, op: Ptr<Operation>) -> Option<(u128, u128)> {
    let s = string(ctx, op, "pliron_range")?;
    let (a, b) = s.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// A switch's cold case indices and whether its default is cold.
pub fn cold_cases(ctx: &Context, op: Ptr<Operation>) -> Option<(Vec<usize>, bool)> {
    let s = string(ctx, op, "pliron_cold_cases")?;
    let mut idx = Vec::new();
    let mut default = false;
    for p in s.split(',') {
        if p == "else" {
            default = true;
        } else {
            idx.push(p.parse().ok()?);
        }
    }
    Some((idx, default))
}

/// A branch's loop hints, if rustc gave any.
pub fn loop_hints(ctx: &Context, op: Ptr<Operation>) -> Option<String> {
    string(ctx, op, "pliron_loop_hints")
}

/// `PLIRON_TRACE=pliron::facts`: how many of each fact the CGU's bodies carry.
pub fn report(ctx: &Context, st: &crate::state::State) {
    if !(pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::facts", tracing::Level::INFO)) {
        return;
    }
    let (mut nuw, mut nsw, mut range, mut disj, mut tr, mut exact, mut cold, mut hints) = (0, 0, 0, 0, 0, 0, 0, 0);
    for f in st.funcs.values() {
        if !crate::has_body(ctx, f.op) {
            continue;
        }
        for b in crate::inline::blocks(ctx, f.op) {
            for op in crate::inline::ops(ctx, b) {
                if let Some(fl) = op.deref(ctx).attributes.get::<pliron_llvm::attributes::IntegerOverflowFlagsAttr>(
                    ctx,
                    &pliron_llvm::op_interfaces::ATTR_KEY_INTEGER_OVERFLOW_FLAGS,
                ) {
                    nuw += usize::from(fl.nuw);
                    nsw += usize::from(fl.nsw);
                }
                range += usize::from(load_range(ctx, op).is_some());
                disj += usize::from(flag(ctx, op, "pliron_disjoint"));
                tr += usize::from(flag(ctx, op, "pliron_trunc_nuw") || flag(ctx, op, "pliron_trunc_nsw"));
                exact += usize::from(flag(ctx, op, "pliron_exact"));
                cold += usize::from(cold_cases(ctx, op).is_some());
                hints += usize::from(loop_hints(ctx, op).is_some());
            }
        }
    }
    tracing::info!(target: "pliron::facts", "facts {}: nuw {nuw} nsw {nsw} range {range} disjoint {disj} trunc {tr} exact {exact} cold-switch {cold} loop-hints {hints}", st.cgu);
}
