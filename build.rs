fn main() {
    println!("cargo::rustc-check-cfg=cfg(rustc_in_tree)");
    // Vendored into a rust checkout: fork-only rustc APIs are available —
    // but only when rustc_private resolves to the in-tree crates (i.e. the
    // bootstrap build). A bare `cargo build` in the worktree picks up the
    // pinned toolchain's rustc-dev, so require the driving `RUSTC` to live
    // under the worktree too.
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let sess = std::path::Path::new(&dir).join("../rustc_session/src/session.rs");
    let tree_root = std::fs::canonicalize(std::path::Path::new(&dir).join("../.."))
        .unwrap_or_else(|_| std::path::Path::new(&dir).to_path_buf());
    let in_tree_rustc = std::env::var("RUSTC")
        .ok()
        .and_then(|r| std::fs::canonicalize(&r).ok())
        .is_some_and(|r| r.starts_with(&tree_root));
    if sess.exists() && in_tree_rustc {
        println!("cargo::rustc-cfg=rustc_in_tree");
    }
}
