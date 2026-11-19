//! The Cranelift side of [`pliron_cfg::target::Target`]: the pipeline's
//! `Pipe::target`, its pointer type, and a `Target` for an ISA (tests,
//! `clif-check`, the lowering's consistency check).

use cranelift_codegen::ir::Type;
use cranelift_codegen::isa::TargetIsa;
pub use pliron_cfg::target::{Arch, Target};

/// The CLIF type of an address on `t` (`i32` on wasm32). A pass that
/// builds a `stack_addr`, `symbol_value` or other address from nothing
/// takes its type from here, never from a literal
/// (`tests/no_hardcoded_pointer_type.rs`).
pub fn ptr_type(t: &Target) -> Type {
    Type::int(t.ptr_bits as u16).expect("pointer width is an integer type")
}

/// The target `isa` compiles for. Not for wasm32, whose ISA is the
/// host's and only bookkeeping: that is [`Target::wasm32`].
pub fn of_isa(isa: &dyn TargetIsa) -> Target {
    use target_lexicon::Architecture as A;
    let arch = match isa.triple().architecture {
        A::Aarch64(_) => Arch::Aarch64,
        A::X86_64 => Arch::X86_64,
        _ => Arch::Other,
    };
    Target {
        arch,
        ptr_bits: u32::from(isa.pointer_bits()),
        platform_reg_free: arch == Arch::Aarch64
            && isa.triple().operating_system == target_lexicon::OperatingSystem::Linux,
        ..Target::other64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::types;

    #[test]
    fn pointer_types() {
        assert_eq!(ptr_type(&Target::wasm32()), types::I32);
        assert_eq!(ptr_type(&Target::aarch64()), types::I64);
    }

    #[test]
    fn host_isa_is_host_target() {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        let isa = cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
            .unwrap()
            .finish(flags)
            .unwrap();
        assert_eq!(of_isa(&*isa), Target::host());
    }
}
