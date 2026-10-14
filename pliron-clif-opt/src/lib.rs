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
pub mod config;
pub mod constdedup;
pub mod diffcheck;
pub mod divconst;
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
pub mod memspec;
pub mod niche;
pub mod paramsink;
pub mod pipeline;
pub mod proots;
pub mod punroll;
pub mod rangefold;
pub mod revnorm;
pub mod slotmerge;
pub mod slotprom;
pub mod slp;
pub mod splink;
pub mod stepcache;
pub mod switchmap;
pub mod taildup;
pub mod tailmerge;
pub mod tailrec;
pub(crate) mod trace;
pub mod unreach;
pub mod unroll;
pub(crate) mod uses;
pub mod veclegalize;

pub use config::{bisect, env_get, env_is, env_set, pass_enabled};
