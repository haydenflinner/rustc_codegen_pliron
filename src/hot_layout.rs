//! `PLIRON_HOT_LAYOUT=1`: a table of the crate's type layouts in every
//! object, so the hot-reload loader can refuse a patch whose types no
//! longer match the live process (`examples/pliron-hot`, `layout_table`).
//!
//! A patch is new code over old data: statics, heap values and stack
//! frames built by the code it replaces. When an edit changes a struct's
//! fields, new code reads those values at the wrong offsets — the one
//! hot-reload failure that corrupts instead of failing. The patch object
//! and `base.ref` are both plain `--emit=obj` compiles, so each carries
//! the table in a section (`__DATA,__hot_layout` / `.pliron_hot_layout`)
//! and the loader compares them before it repoints anything.
//! (relax refuses such edits and offers a restart: reload.rs `layout_edit`.)
//!
//! One line per struct, enum and union defined in this crate that needs no
//! type or const arguments (lifetimes are erased):
//! `path \t size \t align \t hash`. The hash covers what old data depends
//! on: the computed layout (field offsets, variants, tag, niche) and each
//! variant's field names and types in declaration order. A renamed field
//! therefore counts as a change — conservative, never wrong.

use rustc_hir::def::DefKind;
use rustc_middle::ty::{self, TyCtxt, TypingEnv};

fn table(tcx: TyCtxt<'_>) -> String {
    // Trimmed paths are for diagnostics only (rustc asserts one follows).
    rustc_middle::ty::print::with_no_trimmed_paths!(table_untrimmed(tcx))
}

fn table_untrimmed(tcx: TyCtxt<'_>) -> String {
    use std::hash::{Hash, Hasher};
    let mut rows = Vec::new();
    for id in tcx.hir_crate_items(()).definitions() {
        if !matches!(tcx.def_kind(id), DefKind::Struct | DefKind::Enum | DefKind::Union) {
            continue;
        }
        let did = id.to_def_id();
        if tcx.generics_of(did).requires_monomorphization(tcx) {
            continue;
        }
        let args = ty::GenericArgs::for_item(tcx, did, |_, _| tcx.lifetimes.re_erased.into());
        let ty = tcx.normalize_erasing_regions(
            TypingEnv::fully_monomorphized(),
            tcx.type_of(did).instantiate(tcx, args),
        );
        let Ok(layout) = tcx.layout_of(TypingEnv::fully_monomorphized().as_query_input(ty)) else {
            continue;
        };
        let mut h = rustc_data_structures::fx::FxHasher::default();
        format!("{:?}", layout.layout).hash(&mut h);
        for v in tcx.adt_def(did).variants() {
            v.name.as_str().hash(&mut h);
            for f in &v.fields {
                f.name.as_str().hash(&mut h);
                format!("{:?}", tcx.type_of(f.did).instantiate_identity()).hash(&mut h);
            }
        }
        rows.push(format!(
            "{}\t{}\t{}\t{:016x}\n",
            tcx.def_path_str(did),
            layout.size.bytes(),
            layout.align.abi.bytes(),
            h.finish()
        ));
    }
    rows.sort();
    rows.concat()
}

/// The table as assembly for this object, or nothing when the knob is off.
pub fn asm(tcx: TyCtxt<'_>) -> String {
    if !crate::env_is("PLIRON_HOT_LAYOUT", "1") {
        return String::new();
    }
    let t = table(tcx);
    if t.is_empty() {
        return String::new();
    }
    let macho = tcx.sess.target.options.binary_format == rustc_target::spec::BinaryFormat::MachO;
    let mut s = String::from(if macho {
        ".section __DATA,__hot_layout\n"
    } else {
        ".section .pliron_hot_layout,\"a\",@progbits\n"
    });
    // `.byte` lists: paths can hold quotes and angle brackets.
    for chunk in t.as_bytes().chunks(32) {
        s.push_str(".byte ");
        for (i, b) in chunk.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&b.to_string());
        }
        s.push('\n');
    }
    s.push_str(if macho { ".section __TEXT,__text,regular,pure_instructions\n" } else { ".text\n" });
    s
}
