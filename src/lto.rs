//! Fat LTO (`-Clto`, Cargo `lto = true`) without cross-crate optimization.
//!
//! Under fat LTO rustc_codegen_ssa leaves upstream Rust objects off the link
//! line (`are_upstream_rust_objects_already_included`) and expects the
//! backend's single fat-LTO module to contain the whole Rust crate graph
//! except `compiler_builtins` and `#![no_builtins]` crates
//! (`ignored_for_lto`), which stay ordinary rlibs on the link line.
//!
//! What we "LTO" today is native code: `fat` gathers
//! - this crate's CGU objects (in memory, or a `serialize_module` payload
//!   when incremental compilation saved them as pre-LTO files),
//! - the allocator shim module (rustc hands it over as one of `modules`),
//! - every Rust object member of each rlib in `each_linked_rlib_for_lto`
//!   (named by the rlib's `lib.rmeta-link` member, as cg_llvm/cg_gcc do),
//!
//! and partially links them (`cc -r` / `ld -r`) into one relocatable object.
//! That is what LLVM's fat LTO hands the linker too (one object holding the
//! merged crate graph), minus the whole-program IR passes: the final link's
//! `-dead_strip` / `--gc-sections` drops what is unreferenced.
//!
//! "Bitcode" is our own versioned payload ([`LtoPayload`], postcard).
//! Objects that rustc asks to carry bitcode (`bitcode_needed`, i.e.
//! `-Cembed-bitcode`, `-Clinker-plugin-lto`, `--emit=llvm-bc`) get it in a
//! linker-discarded section (`objmerge::add_lto_section`); today it only
//! says "the native code is this object", and the version field is where
//! serialized pliron IR for real cross-crate LTO goes. An object without the
//! section is still usable — std's LLVM-built rlibs are plain native code
//! here too.

use std::path::{Path, PathBuf};
use std::process::Command;

use object::{Object as _, ObjectSection as _};
use rustc_codegen_ssa::back::rmeta_link;
use rustc_codegen_ssa::back::write::{CodegenContext, FatLtoInput, SharedEmitter};
use rustc_codegen_ssa::{CompiledModule, ModuleKind};
use rustc_session::Session;
use rustc_session::config::OutputType;
use rustc_target::spec::{Cc, LinkerFlavor, Lld};
use serde::{Deserialize, Serialize};

use crate::PlironCodegenBackend;

/// Leads every encoded payload, so a foreign or truncated section fails
/// loudly instead of decoding as garbage.
const MAGIC: &[u8; 8] = b"PLIRLTO\0";

/// Bump on any change to [`LtoPayload`]'s layout: postcard is not
/// self-describing, and cargo does not rebuild dependencies when only the
/// backend dylib changed, so stale rlibs must be detected, not misread.
pub const VERSION: u32 = 1;

/// The per-module LTO "bitcode" of the pliron backend.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct LtoPayload {
    /// Always first: `decode` reads it alone before trusting the rest.
    pub version: u32,
    /// Native objects carried by value (`serialize_module`). Empty in an
    /// embedded section: the object holding the section is the code.
    pub objects: Vec<LtoObject>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct LtoObject {
    pub name: String,
    pub bytes: Vec<u8>,
}

impl LtoPayload {
    pub fn new(objects: Vec<LtoObject>) -> Self {
        LtoPayload {
            version: VERSION,
            objects,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend(postcard::to_allocvec(self).expect("encode LTO payload"));
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, String> {
        let body = data
            .strip_prefix(MAGIC)
            .ok_or("not a pliron LTO payload (bad magic)")?;
        let (version, _) = postcard::take_from_bytes::<u32>(body).map_err(|e| e.to_string())?;
        if version != VERSION {
            return Err(format!(
                "LTO payload version {version}, this backend reads version {VERSION} \
                 (rebuild the dependency with this backend)"
            ));
        }
        postcard::from_bytes(body).map_err(|e| e.to_string())
    }
}

/// The payload embedded in an object rustc asked to carry bitcode.
pub fn embedded_payload() -> Vec<u8> {
    LtoPayload::new(Vec::new()).encode()
}

/// The embedded payload of a native object, if it has a readable one. An
/// unreadable one (another backend version's) is skipped: the object's
/// own native code is always a valid input.
fn object_payload(obj: &[u8], what: &dyn std::fmt::Display) -> Result<Option<LtoPayload>, String> {
    let f = object::File::parse(obj).map_err(|e| e.to_string())?;
    let (seg, name) = pliron_clif_lower::objmerge::lto_section_name(f.format());
    let Some(s) = f.sections().find(|s| {
        s.name_bytes().ok() == Some(name)
            && (seg.is_empty() || s.segment_name_bytes().ok().flatten() == Some(seg))
    }) else {
        return Ok(None);
    };
    match s.data().map_err(|e| e.to_string()).and_then(LtoPayload::decode) {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            if pliron_cfg::tracing_on() {
                tracing::warn!(target: "pliron::lto", "{what}: ignoring LTO payload: {e}");
            }
            Ok(None)
        }
    }
}

/// `optimize_and_codegen_fat_lto`: one relocatable object holding the
/// local modules and every LTO-participating upstream rlib's Rust objects.
pub fn fat(
    sess: &Session,
    cgcx: &CodegenContext,
    shared_emitter: &SharedEmitter,
    each_linked_rlib_for_lto: &[PathBuf],
    modules: Vec<FatLtoInput<PlironCodegenBackend>>,
) -> CompiledModule {
    let _prof = sess.prof.generic_activity("pliron_fat_lto");
    let dcx = sess.dcx();
    let target = &sess.target;
    if target.is_like_wasm {
        dcx.fatal("fat LTO (-Clto) is not supported by the pliron backend on wasm targets");
    }
    let partial_link = partial_link_cmd(sess).unwrap_or_else(|why| {
        dcx.fatal(format!(
            "fat LTO (-Clto) is not supported by the pliron backend for target `{}`: {why}",
            sess.opts.target_triple
        ))
    });

    // The fat module takes a regular local CGU's name (cg_llvm keeps its
    // costliest module's), so its object lands where rustc expects one.
    let name = modules
        .iter()
        .find_map(|m| match m {
            FatLtoInput::InMemory(m) if m.kind == ModuleKind::Regular => Some(m.name.clone()),
            _ => None,
        })
        .or_else(|| {
            modules.iter().find_map(|m| match m {
                FatLtoInput::Serialized { name, .. } => Some(name.clone()),
                FatLtoInput::InMemory(m) => Some(m.name.clone()),
            })
        })
        .unwrap_or_else(|| "lto".to_string());
    let outs = &cgcx.output_filenames;
    let out = outs.temp_path_for_cgu(OutputType::Object, &name);
    let dir = outs.temp_path_ext_for_cgu("pliron-lto", &name);
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        dcx.fatal(format!("creating {}: {e}", dir.display()));
    }

    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut put = |stem: &str, bytes: &[u8]| {
        // Index prefix: member names are unique per rlib, not globally.
        let p = dir.join(format!("{}-{}", inputs.len(), file_name(stem)));
        if let Err(e) = std::fs::write(&p, bytes) {
            dcx.fatal(format!("writing {}: {e}", p.display()));
        }
        inputs.push(p);
    };

    for m in modules {
        match m {
            FatLtoInput::InMemory(mut m) => {
                if let Some(key) = m.module_llvm.pending.take() {
                    let fin = crate::cgupool::run_until(key, &sess.prof, shared_emitter);
                    m.module_llvm.obj = fin.obj;
                }
                put(&format!("{}.o", m.name), &m.module_llvm.obj);
            }
            FatLtoInput::Serialized { name, bitcode_path } => {
                let data = std::fs::read(&bitcode_path).unwrap_or_else(|e| {
                    dcx.fatal(format!("reading LTO input {}: {e}", bitcode_path.display()))
                });
                let p = LtoPayload::decode(&data).unwrap_or_else(|e| {
                    dcx.fatal(format!("LTO input {}: {e}", bitcode_path.display()))
                });
                for o in p.objects {
                    put(&format!("{name}{}.o", o.name), &o.bytes);
                }
            }
        }
    }

    let mut ours = 0;
    for rlib in each_linked_rlib_for_lto {
        let data = std::fs::read(rlib)
            .unwrap_or_else(|e| dcx.fatal(format!("reading {}: {e}", rlib.display())));
        let link = rmeta_link::read_from_data(&data, rlib).unwrap_or_else(|| {
            dcx.fatal(format!("{}: no readable lib.rmeta-link member", rlib.display()))
        });
        let ar = object::read::archive::ArchiveFile::parse(&*data)
            .unwrap_or_else(|e| dcx.fatal(format!("{}: {e}", rlib.display())));
        for member in ar.members() {
            let member =
                member.unwrap_or_else(|e| dcx.fatal(format!("{}: {e}", rlib.display())));
            let mname = String::from_utf8_lossy(member.name()).trim().to_string();
            if !link.rust_object_files.iter().any(|f| *f == mname) {
                continue;
            }
            let bytes = member
                .data(&*data)
                .unwrap_or_else(|e| dcx.fatal(format!("{}({mname}): {e}", rlib.display())));
            if bytes.starts_with(b"BC\xC0\xDE") || bytes.starts_with(b"\xDE\xC0\x17\x0B") {
                dcx.fatal(format!(
                    "{}({mname}) is LLVM bitcode (built by the LLVM backend with \
                     -Clinker-plugin-lto); the pliron backend can only LTO native objects",
                    rlib.display()
                ));
            }
            let what = format!("{}({mname})", rlib.display());
            match object_payload(bytes, &what) {
                // Ours: the object holding the payload is the code (a
                // future payload version adds IR to optimize instead).
                Ok(Some(_)) => ours += 1,
                // No payload: a native object from another backend (the
                // LLVM-built std) or one built without embedded bitcode.
                Ok(None) => {}
                Err(e) => dcx.fatal(format!("{what}: {e}")),
            }
            put(&mname, bytes);
        }
    }
    drop(put);

    if pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::lto",
            "fat LTO {name}: {} objects from {} rlibs ({ours} with our payload) -> {}",
            inputs.len(), each_linked_rlib_for_lto.len(), out.display());
    }
    if inputs.len() == 1 {
        if let Err(e) = std::fs::rename(&inputs[0], &out) {
            dcx.fatal(format!("writing {}: {e}", out.display()));
        }
    } else {
        let mut cmd = partial_link;
        cmd.arg("-o").arg(&out).args(&inputs);
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::lto", "{cmd:?}");
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => dcx.fatal(format!(
                "fat LTO partial link failed ({}): {cmd:?}\n{}{}",
                o.status,
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )),
            Err(e) => dcx.fatal(format!("fat LTO partial link: running {cmd:?}: {e}")),
        }
    }
    // Mach-O keeps DWARF in the input objects (the debug map in `out` names
    // them for dsymutil), so they must outlive the final link there.
    let keep = cgcx.save_temps
        || (target.is_like_darwin
            && sess.opts.debuginfo != rustc_session::config::DebugInfo::None);
    if !keep {
        let _ = std::fs::remove_dir_all(&dir);
    }

    CompiledModule {
        name,
        kind: ModuleKind::Regular,
        object: Some(out),
        global_asm_object: None,
        dwarf_object: None,
        bytecode: None,
        assembly: None,
        llvm_ir: None,
    }
}

fn file_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .collect()
}

/// A relocatable-link command (`-o out inputs...` still to append) for the
/// session's linker: the cc driver or ld rustc itself would link with.
fn partial_link_cmd(sess: &Session) -> Result<Command, String> {
    let (linker, flavor) = rustc_codegen_ssa::back::link::linker_and_flavor(sess);
    let target = &sess.target;
    // As rustc's `add_apple_link_args`: `-arch` is the llvm triple's arch
    // (arm64, arm64e, x86_64), and the deployment target is the session's —
    // left out, ld stamps the host OS version on the output and the final
    // link warns that an object targets a newer OS than the image.
    let apple_arch = target.llvm_target.split('-').next().unwrap_or("").to_string();
    let macos = target.os == rustc_target::spec::Os::MacOs;
    let min = || sess.apple_deployment_target().fmt_full().to_string();
    let mut cmd = Command::new(&linker);
    match flavor {
        LinkerFlavor::Darwin(Cc::Yes, _) => {
            cmd.args(["-r", "-nostdlib"]);
            if macos {
                cmd.args(["-arch", &apple_arch]).arg(format!("-mmacosx-version-min={}", min()));
            } else {
                cmd.arg("-target")
                    .arg(&*rustc_codegen_ssa::back::versioned_llvm_target(sess));
            }
            // Keep hidden symbols private-extern: ld64 -r would localize them,
            // and compiler_builtins / native libs may still reference them.
            cmd.arg("-Wl,-keep_private_externs");
        }
        LinkerFlavor::Darwin(Cc::No, lld) => {
            if lld == Lld::Yes && is_multiflavor_lld(&linker) {
                cmd.args(["-flavor", "darwin"]);
            }
            cmd.args(["-r", "-arch", &apple_arch, "-keep_private_externs"]);
            if macos {
                let v = min();
                cmd.args(["-platform_version", "macos", &v, &v]);
            }
        }
        LinkerFlavor::Gnu(Cc::Yes, _) | LinkerFlavor::Unix(Cc::Yes) => {
            cmd.args(["-r", "-nostdlib"]);
        }
        LinkerFlavor::Gnu(Cc::No, lld) => {
            if lld == Lld::Yes && is_multiflavor_lld(&linker) {
                cmd.args(["-flavor", "gnu"]);
            }
            cmd.arg("-r");
        }
        LinkerFlavor::Unix(Cc::No) => {
            cmd.arg("-r");
        }
        f => return Err(format!("linker flavor {f:?} has no relocatable (-r) link")),
    }
    Ok(cmd)
}

/// `lld`/`rust-lld` need `-flavor`; `ld.lld`/`ld64.lld` pick it from argv[0].
fn is_multiflavor_lld(linker: &Path) -> bool {
    matches!(linker.file_stem().and_then(|s| s.to_str()), Some("lld" | "rust-lld"))
}
