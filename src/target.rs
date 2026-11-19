//! rustc's target as the passes' [`Target`]: the one place the backend
//! turns `Session` facts into what the IR passes (`State::target`) and
//! the CLIF pipeline (`Pipe::target`) may ask. A pure function of the
//! session, called where a CGU's `State` and the crate's `CrateCfg` are
//! created.

pub(crate) use pliron_cfg::target::{Arch, Target};
use rustc_session::Session;

pub(crate) fn of(sess: &Session) -> Target {
    use rustc_target::spec::Arch as R;
    let arch = match sess.target.arch {
        R::AArch64 => Arch::Aarch64,
        R::X86_64 => Arch::X86_64,
        R::Wasm32 => Arch::Wasm32,
        _ => Arch::Other,
    };
    let base = match arch {
        Arch::Aarch64 => Target::aarch64(),
        Arch::X86_64 => Target::x86_64(),
        Arch::Wasm32 => Target::wasm32(),
        Arch::Other => Target::other64(),
    };
    let t = Target {
        ptr_bits: sess.target.pointer_width as u32,
        opt_size: matches!(
            sess.opts.optimize,
            rustc_session::config::OptLevel::Size | rustc_session::config::OptLevel::SizeMin
        ),
        // x18: Linux ELF only (`rustc_target`'s `os`; Android also
        // reserves it for the shadow call stack).
        platform_reg_free: arch == Arch::Aarch64 && sess.target.os == rustc_target::spec::Os::Linux,
        ..base
    };
    // `types::size_align` lays out pointers from a process-wide mirror
    // set in `target_config`; the two must describe the same target.
    debug_assert_eq!(crate::types::ptr32(), t.ptr_bits == 32);
    t
}
