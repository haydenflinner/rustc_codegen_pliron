fn main() {
    println!("cargo::rustc-check-cfg=cfg(rustc_in_tree)");
    // Vendored into a rust checkout: fork-only rustc APIs are available.
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    if std::path::Path::new(&dir).join("../rustc_session/src/session.rs").exists() {
        println!("cargo::rustc-cfg=rustc_in_tree");
    }
}
