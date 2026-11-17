//! rustc-side wasm glue. The CLIF -> waffle -> wasm-object emit lives in
//! the stable `pliron-wasm` crate (moved so non-rustc drivers — JITs,
//! .pliron-text consumers — can run `lower_clif_wasm` + emit without
//! `rustc_private`); this module keeps the one function that reads
//! rustc's target spec and re-exports the emit entry point.

pub(crate) use pliron_wasm::lower_to_wasm_clif;

/// LLVM's `generic` wasm CPU features, adjusted by `-Ctarget-cpu=mvp` and
/// `-Ctarget-feature`. Tools like wasm-bindgen read them from `target_features`.
// r[impl wasm.obj.features]
pub fn target_features(
    target: &rustc_target::spec::Target,
    opts: &rustc_session::config::Options,
) -> Vec<String> {
    let mut f: Vec<String> = if opts.cg.target_cpu.as_deref() == Some("mvp") {
        vec![]
    } else {
        [
            "bulk-memory",
            "multivalue",
            "mutable-globals",
            "nontrapping-fptoint",
            "reference-types",
            "sign-ext",
        ]
        .map(String::from)
        .to_vec()
    };
    let cg = opts.cg.target_feature.as_str();
    for t in target
        .features
        .split(',')
        .chain(cg.split(','))
        .map(str::trim)
    {
        if let Some(n) = t.strip_prefix('+') {
            if !f.iter().any(|x| x == n) {
                f.push(n.into());
            }
        } else if let Some(n) = t.strip_prefix('-') {
            f.retain(|x| x != n);
        }
    }
    f
}
