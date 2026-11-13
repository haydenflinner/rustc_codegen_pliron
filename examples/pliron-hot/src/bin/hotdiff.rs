//! `hotdiff OLD.o NEW.o`: the functions whose code differs between two
//! builds of one crate, one symbol per line (new functions included).
//! Same fingerprints the hot-reload loader diffs patches with: code bytes
//! plus relocation targets by name, so a function that merely moved is
//! not reported. `.hot` body suffixes (`PLIRON_HOT` builds) are dropped,
//! which makes the names match `pliron_hot::reach`'s.
//! Used by `examples/hot-test.sh` to pick the tests to rerun after a save.
use std::collections::BTreeSet;

fn prints(path: &str) -> Result<pliron_hot::Prints, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let f = object::File::parse(&*bytes).map_err(|e| format!("{path}: {e}"))?;
    Ok(pliron_hot::fingerprints(&f))
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 3 {
        eprintln!("usage: hotdiff OLD.o NEW.o");
        std::process::exit(2);
    }
    let (old, new) = match (prints(&a[1]), prints(&a[2])) {
        (Ok(o), Ok(n)) => (o, n),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("hotdiff: {e}");
            std::process::exit(2);
        }
    };
    let mut out = BTreeSet::new();
    for (name, h) in &new {
        if old.get(name) != Some(h) {
            out.insert(name.strip_suffix(".hot").unwrap_or(name).to_string());
        }
    }
    for n in out {
        println!("{n}");
    }
}
