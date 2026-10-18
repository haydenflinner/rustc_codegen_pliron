//! Optimizations over the pliron LLVM dialect: the passes run before
//! Cranelift lowering and share their state through [`state::State`].
//!
//! rustc-independent: this crate builds on stable Rust. `PLIRON_*`
//! configuration comes from `pliron-cfg` (re-exported at the root).

pub mod config {
    //! `PLIRON_*` configuration snapshot (shared with `pliron-clif-opt`).
    pub use pliron_cfg::*;
}
pub use pliron_cfg::{bisect, bisect_in, env_get, env_is, env_set, pass_enabled};

mod helpers;
pub use helpers::{Syms, address_taken, bisect_name, bodied, dump_fact, has_body, opt_body, opt_skip_init};

pub mod callgvn;
pub mod constload;
pub mod domcheck;
pub mod guardelim;
pub mod inline;
pub mod instcombine;
pub mod memcpyopt;
pub mod nnret;
pub mod nocapture;
pub mod nounwind;
pub mod nowrite;
pub mod pgo;
pub mod phisimp;
pub mod retarg;
pub mod spec;
pub mod sroa;
pub mod state;
pub mod types;
pub mod willreturn;
pub mod xoridem;
