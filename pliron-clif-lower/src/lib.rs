//! pliron LLVM dialect → Cranelift lowering. Stable Rust, no rustc:
//! the embedder (the rustc backend, a JIT) supplies a [`lower::Host`] and
//! a `cranelift_module::Module`.

pub mod eh;
pub mod hot;
pub mod lower;
pub mod objmerge;
pub mod scalarize;
pub mod scan;

pub mod context {
    pub use pliron_ir_opt::state::*;
}

pub use pliron_clif_opt::*;
pub use pliron_ir_opt as iropt;
pub use pliron_ir_opt::types;
pub use pliron_ir_opt::{
    address_taken, constload, domcheck, has_body, inline, instcombine, memcpyopt, nnret, nocapture,
    nounwind, nowrite, pgo, phisimp, retarg, spec, sroa,
};


/// Strip LLVM's `\x01` no-mangle marker.
pub fn obj_sym(n: &str) -> &str {
    n.strip_prefix('\u{1}').unwrap_or(n)
}
