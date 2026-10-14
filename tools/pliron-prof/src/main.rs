//! `pliron-prof` — merge and inspect `pliron.prof` files.
//!
//!   pliron-prof merge <in.prof>... -o <out.prof>
//!   pliron-prof dump <in.prof>

use pliron_prof::{parse, serialize, ProfileMap};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("merge") => {
            let mut m: ProfileMap = Default::default();
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
                let Some(pm) = parse(&data) else {
                    eprintln!("pliron-prof: {}: bad profile", args[i]);
                    std::process::exit(1);
                };
                for (k, (b, e)) in pm {
                    let (ob, oe) = m.entry(k).or_default();
                    ob.resize(ob.len().max(b.len()), 0);
                    for (i, &c) in b.iter().enumerate() {
                        ob[i] += c;
                    }
                    for &(p, s, c) in &e {
                        if let Some(x) = oe.iter_mut().find(|x| x.0 == p && x.1 == s) {
                            x.2 += c;
                        } else {
                            oe.push((p, s, c));
                        }
                    }
                }
                i += 1;
            }
            let Some(out) = out else {
                eprintln!("pliron-prof merge: missing -o <out.prof>");
                std::process::exit(1);
            };
            std::fs::write(&out, serialize(&m)).unwrap();
            eprintln!("pliron-prof: {} functions -> {out}", m.len());
        }
        Some("dump") => {
            for f in &args[1..] {
                let data = std::fs::read(f).unwrap();
                let Some(m) = parse(&data) else {
                    eprintln!("pliron-prof: {f}: bad profile");
                    continue;
                };
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
                }
            }
        }
        _ => {
            eprintln!("usage: pliron-prof <merge <in>... -o <out> | dump <in>...>");
            std::process::exit(2);
        }
    }
}
