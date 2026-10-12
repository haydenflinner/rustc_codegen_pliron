//! Cranelift-IR optimization passes extracted from `rustc_codegen_pliron`.
//!
//! These passes operate on `cranelift_codegen::ir::Function` and have no
//! `rustc_private` dependency, so this crate builds on stable Rust and can
//! host `.clif` filetests and differential tests without relinking the
//! backend dylib.

pub mod bcheck;
pub mod bittab;
pub mod brchain;
pub mod celim;
pub mod clifpeep;
pub mod commute;
pub mod dse;
pub mod edgefwd;
pub mod edgespec;
pub mod ifconv;
pub mod indvars;
pub mod ivrefold;
pub mod jumpthread;
pub mod licm;
pub mod loadfwd;
pub mod loopdel;
pub mod loopidiom;
pub mod looprot;
pub mod loopvec;
pub mod punroll;
pub mod revnorm;
pub mod slotmerge;
pub mod slp;
pub mod switchmap;
pub mod taildup;
pub mod tailmerge;
pub mod tailrec;
pub mod unreach;
pub mod unroll;

/// `PLIRON_<PASS>=0` disables a pass at runtime (default on).
/// Keep semantics identical to `rustc_codegen_pliron::pass_enabled`.
pub fn pass_enabled(var: &str) -> bool {
    std::env::var(var).map_or(true, |v| v != "0")
}
