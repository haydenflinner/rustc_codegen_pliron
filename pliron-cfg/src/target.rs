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
    /// `-Copt-level=s|z`: code size comes before speed. With wasm32 it
    /// puts the inliner in its size-first mode (`inline::Mode::of`).
    pub opt_size: bool,
}

impl Target {
    pub const fn aarch64() -> Target {
        Target {
            arch: Arch::Aarch64,
            ptr_bits: 64,
            opt_size: false,
        }
    }

    pub const fn x86_64() -> Target {
        Target {
            arch: Arch::X86_64,
            ptr_bits: 64,
            opt_size: false,
        }
    }

    pub const fn wasm32() -> Target {
        Target {
            arch: Arch::Wasm32,
            ptr_bits: 32,
            opt_size: false,
        }
    }

    /// A 64-bit target no pass distinguishes.
    pub const fn other64() -> Target {
        Target {
            arch: Arch::Other,
            ptr_bits: 64,
            opt_size: false,
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

    /// The pipeline's `Pipe::isa` is this target's own ISA. False on
    /// wasm32, where it is the host's and only bookkeeping: steps that
    /// would consult it for legality (the egraph, unreachable-code
    /// elimination) take the flags alone, and nothing may tune for the
    /// host's encoder (`clifpeep::set_bigoff`).
    pub const fn own_isa(&self) -> bool {
        !self.is_wasm()
    }

    /// Code goes through our register allocator (regalloc2), so a step
    /// can shape it for register pressure and ABI pinning. False on
    /// wasm32 (V8 allocates). Uses: `coldarg`.
    pub const fn has_regalloc(&self) -> bool {
        !self.is_wasm()
    }

    /// A `br_table` is a real jump-table dispatch the engine compiles
    /// well, so it is not worth turning into compare chains or selects.
    /// True on wasm32 (V8's `br_table`). Uses: `brchain`, ifconv's
    /// `br_table` arm (`ifconv::run`, jumpthread's `ifconv_tables`).
    pub const fn cheap_br_table(&self) -> bool {
        self.is_wasm()
    }

    /// A 64-bit multiply-high (`umulhi.i64`) is one instruction. False on
    /// wasm32, where it is a ~20-op sequence, so `divconst` narrows
    /// divisions by constants proved below 2^32 to `umulhi.i32` and runs
    /// ahead of the egraph there.
    pub const fn mulhi64(&self) -> bool {
        !self.is_wasm()
    }

    /// The code emitter folds `base + reg*{1,2,4,8}` into a load's
    /// addressing mode (x86 SIB, aarch64 extended register). False on
    /// wasm32, where every address is computed explicitly, and on
    /// targets we have not tuned for: indvars then wants stepped pointer
    /// induction variables rather than `base + iv*s`.
    pub const fn amode_scale(&self) -> bool {
        matches!(self.arch, Arch::Aarch64 | Arch::X86_64)
    }

    /// A load or store may carry a negative immediate offset. False on
    /// wasm32: the memarg offset is unsigned and added without wrapping,
    /// so the emitter would turn `p-4` into `p+0xffff_fffc`. Uses:
    /// `ivmerge` (a rebased address folds its constant into the offset
    /// only when the result is allowed).
    pub const fn neg_mem_offset(&self) -> bool {
        !self.is_wasm()
    }

    /// (allocatable, callee-saved) integer registers, for register
    /// pressure estimates (licm's `PLIRON_LICM_INNERPRESS=2`). `None` on
    /// wasm32, which has no register file of ours.
    pub const fn int_regs(&self) -> Option<(u32, u32)> {
        match self.arch {
            Arch::Aarch64 => Some((26, 10)),
            Arch::X86_64 | Arch::Other => Some((14, 5)),
            Arch::Wasm32 => None,
        }
    }

    /// Same architecture and pointer width: what the lowering checks
    /// against the ISA it is handed (`opt_size` is not an ISA fact).
    pub const fn same_machine(&self, o: &Target) -> bool {
        self.arch as u8 == o.arch as u8 && self.ptr_bits == o.ptr_bits
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
