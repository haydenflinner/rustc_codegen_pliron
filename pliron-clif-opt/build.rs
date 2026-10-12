//! Bake a content fingerprint into `PLIRON_BUILD_ID` so step-cache keys
//! invalidate on any pass-source or config-semantics change — a stale
//! hit would be a silent miscompile. Covers our own sources plus
//! pliron-cfg (env semantics can change step behavior) and the git HEAD
//! for provenance.

use std::hash::{Hash, Hasher};

fn hash_dir(h: &mut impl Hasher, dir: &str) {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("build.rs: read_dir {dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    for f in files {
        std::fs::read(&f)
            .unwrap_or_else(|e| panic!("build.rs: read {}: {e}", f.display()))
            .hash(h);
        println!("cargo:rerun-if-changed={}", f.display());
    }
}

fn main() {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash_dir(&mut h, "src");
    hash_dir(&mut h, "../pliron-cfg/src");
    if let Ok(o) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        && let Ok(head) = String::from_utf8(o.stdout)
    {
        head.trim().hash(&mut h);
    }
    println!("cargo:rustc-env=PLIRON_BUILD_ID={:016x}", h.finish());
}
