//! `pliron-prof` — merge and inspect `pliron.prof` files.
//!
//!   pliron-prof merge <in.prof>... -o <out.prof>
//!   pliron-prof dump <in.prof>
//!   pliron-prof fnorder <in.fnorder>... -o <order.txt>

use pliron_prof::{load_all, serialize_all, IcallMap, ProfileMap};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("merge") => {
            let mut m: ProfileMap = Default::default();
            let mut ic: IcallMap = Default::default();
            let mut out = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "-o" {
                    out = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                let data = std::fs::read(&args[i]).unwrap_or_else(|e| {
                    eprintln!("pliron-prof: {}: {e}", args[i]);
                    std::process::exit(1);
                });
                if !load_all(&data, &mut m, &mut ic) {
                    eprintln!("pliron-prof: {}: bad profile", args[i]);
                    std::process::exit(1);
                }
                i += 1;
            }
            let Some(out) = out else {
                eprintln!("pliron-prof merge: missing -o <out.prof>");
                std::process::exit(1);
            };
            std::fs::write(&out, serialize_all(&m, &ic)).unwrap();
            let sites: usize = ic.values().map(|s| s.len()).sum();
            if sites > 0 {
                eprintln!("pliron-prof: {} functions, {sites} indirect-call sites -> {out}", m.len());
            } else {
                eprintln!("pliron-prof: {} functions -> {out}", m.len());
            }
        }
        Some("dump") => {
            for f in &args[1..] {
                let data = std::fs::read(f).unwrap();
                let mut m: ProfileMap = Default::default();
                let mut ic: IcallMap = Default::default();
                if !load_all(&data, &mut m, &mut ic) {
                    eprintln!("pliron-prof: {f}: bad profile");
                    continue;
                }
                println!("{f}: {} functions", m.len());
                let mut v: Vec<_> = m.iter().collect();
                v.sort_by_key(|(_, (b, _))| std::cmp::Reverse(b.first().copied().unwrap_or(0)));
                for (&(sh, fp), (blocks, edges)) in v {
                    let tot: u64 = blocks.iter().copied().max().unwrap_or(0);
                    println!("  {sh:016x} fp={fp:016x} entry={tot}");
                    println!("    blocks: {blocks:?}");
                    if !edges.is_empty() {
                        println!("    edges:  {edges:?}");
                    }
                    for (i, s) in ic.get(&(sh, fp)).map(Vec::as_slice).unwrap_or(&[]).iter().enumerate() {
                        let t: Vec<String> =
                            s.targets.iter().map(|t| format!("{}x{}", t.count, t.name)).collect();
                        println!("    icall {i}: other={} {}", s.other, t.join(" "));
                    }
                }
            }
        }
        Some("fnorder") => {
            // Merge `PLIRON_FNORDER_FILE` dumps (`sym<TAB>count`) into a
            // linker order file: symbols hottest first, one per line.
            // ld64 `-order_file`, lld `--symbol-ordering-file`.
            let mut counts: std::collections::HashMap<String, u64> = Default::default();
            let mut out = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "-o" {
                    out = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                let data = std::fs::read_to_string(&args[i]).unwrap_or_else(|e| {
                    eprintln!("pliron-prof: {}: {e}", args[i]);
                    std::process::exit(1);
                });
                for line in data.lines() {
                    if let Some((sym, c)) = line.split_once('\t') {
                        *counts.entry(sym.to_string()).or_default() +=
                            c.parse::<u64>().unwrap_or(0);
                    }
                }
                i += 1;
            }
            let Some(out) = out else {
                eprintln!("pliron-prof fnorder: missing -o <order.txt>");
                std::process::exit(1);
            };
            let mut v: Vec<_> = counts.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let n = v.len();
            // Order files match nlist names: Mach-O prepends `_`.
            let s: String = v
                .into_iter()
                .map(|(sym, _)| {
                    if cfg!(target_os = "macos") {
                        format!("_{sym}\n")
                    } else {
                        sym + "\n"
                    }
                })
                .collect();
            std::fs::write(&out, s).unwrap();
            eprintln!("pliron-prof: {n} symbols -> {out}");
        }
        _ => {
            eprintln!("usage: pliron-prof <merge <in>... -o <out> | dump <in>... | fnorder <in>... -o <out>>");
            std::process::exit(2);
        }
    }
}
