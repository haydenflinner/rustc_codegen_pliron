use pliron_clif_opt::slotmerge;
use rustc_hash::{FxHashMap, FxHashSet};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pliron::slotmerge=debug".into()),
        )
        .init();
    pliron_cfg::mark_tracing_on();
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let mut funcs = cranelift_reader::parse_functions(&src).unwrap();
    let nowrite = FxHashMap::default();
    let lifetimes = Vec::new();
    let nocap = FxHashSet::default();
    for f in &mut funcs {
        let n = slotmerge::run(f, &nowrite, &lifetimes, &nocap);
        eprintln!("merged {n} in {}", f.name);
    }
}
