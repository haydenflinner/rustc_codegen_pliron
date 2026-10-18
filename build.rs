/// `PLIRON_SRC_REV`: which source tree this dylib was built from. A
/// self-host copy has no `.git` of its own (it sits inside the rust
/// checkout), so selfhost/setup.sh writes `.pliron-src-rev` there; a git
/// checkout asks harness/src-rev.sh.
fn src_rev() -> String {
    println!("cargo::rerun-if-changed=.pliron-src-rev");
    if let Ok(r) = std::fs::read_to_string(".pliron-src-rev") {
        return r.trim().to_string();
    }
    if !std::path::Path::new(".git").exists() {
        return "unknown".into();
    }
    // Rerun when HEAD moves, the index changes, or any source does (the
    // dirty-diff hash) — cargo scans listed directories recursively.
    if let Ok(o) = std::process::Command::new("git").args(["rev-parse", "--git-dir"]).output() {
        let gd = String::from_utf8_lossy(&o.stdout).trim().to_string();
        println!("cargo::rerun-if-changed={gd}/HEAD");
        println!("cargo::rerun-if-changed={gd}/index");
    }
    for d in ["src", "pliron-clif-opt/src", "pliron-clif-lower/src", "pliron-ir-opt/src", "pliron-cfg/src", "vendor/cranelift-codegen/src"] {
        println!("cargo::rerun-if-changed={d}");
    }
    std::process::Command::new("sh")
        .arg("harness/src-rev.sh")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(rustc_in_tree)");
    println!("cargo::rustc-env=PLIRON_SRC_REV={}", src_rev());
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
