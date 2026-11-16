//! What the passes may ask about the compilation target.
//!
//! One [`Target`] value is built per compilation from rustc's target
//! (`src/target.rs`, the only place that reads rustc's target for the
//! passes) and handed to the IR passes as `State::target` and to the CLIF
//! pipeline as `Pipe::target`. Tests and `clif-check` build one by hand
//! ([`Target::host`], [`Target::aarch64`], [`Target::wasm32`], or
//! `pliron_clif_opt::target::of_isa`).
//!
//! The rule: a pass asks `Target`. No literal word sizes, no literal
//! pointer types, no private `wasm: bool`. A fact goes here when a pass
//! needs it, as a field (data rustc or the embedder supplies) or as a
//! method (a consequence of the architecture family), and its doc comment
//! names the uses. A gate that only records where something was measured
//! ("wasm only: native was not timed") is written `target.is_wasm()` at
//! the use, with that comment; it is not a capability and gets no name.
//!
//! Plain data: this crate has no rustc and no Cranelift types. Each side
//! converts (`pliron_clif_opt::target::ptr_type`).
//!
//! Not here: CPU-feature facts (`Pipe::simd`, `Pipe::cmp64`), which come
//! from the session's feature set through the lowering, and the ABI
//! (`Pipe::tcfg`, the lowering's own `wasm` flag).

/// Architecture family. `Other` is every target Cranelift compiles for
/// that no pass distinguishes (riscv64, s390x).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Arch {
    Aarch64,
    X86_64,
    /// wasm32 through waffle: the CLIF pipeline runs with i32 pointers on
    /// a bookkeeping host ISA, and V8 compiles the result.
    Wasm32,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub arch: Arch,
    /// Pointer width in bits: 32 on wasm32, 64 on the native targets.
    /// The CLIF pointer type is `pliron_clif_opt::target::ptr_type`;
    /// `pliron_ir_opt::types::size_align` lays out `ptr` with it.
    pub ptr_bits: u32,
}

impl Target {
    pub const fn aarch64() -> Target {
        Target {
            arch: Arch::Aarch64,
            ptr_bits: 64,
        }
    }

    pub const fn x86_64() -> Target {
        Target {
            arch: Arch::X86_64,
            ptr_bits: 64,
        }
    }

    pub const fn wasm32() -> Target {
        Target {
            arch: Arch::Wasm32,
            ptr_bits: 32,
        }
    }

    /// A 64-bit target no pass distinguishes.
    pub const fn other64() -> Target {
        Target {
            arch: Arch::Other,
            ptr_bits: 64,
        }
    }

    /// The machine this code was compiled for: unit tests, and embedders
    /// that JIT for the host.
    pub const fn host() -> Target {
        if cfg!(target_arch = "aarch64") {
            Target::aarch64()
        } else if cfg!(target_arch = "x86_64") {
            Target::x86_64()
        } else {
            Target::other64()
        }
    }

    pub const fn is_wasm(&self) -> bool {
        matches!(self.arch, Arch::Wasm32)
    }

    pub const fn is_aarch64(&self) -> bool {
        matches!(self.arch, Arch::Aarch64)
    }

    pub const fn is_x64(&self) -> bool {
        matches!(self.arch, Arch::X86_64)
    }

    /// Size of a pointer in bytes.
    pub const fn ptr_bytes(&self) -> u64 {
        (self.ptr_bits / 8) as u64
    }

    /// The machine word in bytes, for cost rules that count in register
    /// values: an integer or float of this size or less is one register
    /// value (one wasm local, one GPR). The pointer width on every target
    /// we compile for (4 on wasm32, where an `i32` or `f32` is a whole
    /// value). Uses: sroa's fragment rule (`PLIRON_SROA_FRAGWORD`
    /// overrides it).
    pub const fn word_bytes(&self) -> u64 {
        self.ptr_bytes()
    }
}

/// [`Target::host`]: what `State::default()` carries in unit tests. The
/// backend always sets the field from rustc's target, and the lowering
/// checks it against the ISA it is given.
impl Default for Target {
    fn default() -> Self {
        Target::host()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors() {
        assert_eq!(Target::wasm32().ptr_bytes(), 4);
        assert_eq!(Target::aarch64().ptr_bytes(), 8);
        assert_eq!(Target::wasm32().word_bytes(), 4);
        assert_eq!(Target::x86_64().word_bytes(), 8);
        assert!(Target::wasm32().is_wasm() && !Target::x86_64().is_wasm());
        assert_eq!(Target::default(), Target::host());
        assert_eq!(Target::host().ptr_bits, usize::BITS);
    }
}
