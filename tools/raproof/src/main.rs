//! Exact register-pressure proofs for random (or replayed) regalloc2 funcs.
//!
//!   raproof [--start S] [--count N] [--replay SEED] [--algo ion|greedy|fast|mcts]
//!           [--prove-fewer] [--budget N]
//!
//! For each seed: build the interference graph, prove a clique lower bound
//! (and an exact chromatic upper bound when the class has ≤ 48 vertices),
//! re-check the certificates independently, run the heuristic allocator,
//! and print whether the heuristic sits on the optimum, above it, or in an
//! unresolved gap. With `--prove-fewer`, when the heuristic footprint is
//! above the bound, search for a colouring with one (or more) fewer
//! registers — the constructive counterexample.
//!
//! Assembly-line view: the largest set of values that must be live together
//! is a proof you need that many registers; a colouring is a proof that
//! many suffice (modulo FixedReg/clobber pins, which only raise the need).
//! Spending `--budget` is the trade-compute-for-quality dial.

use arbitrary::Unstructured;
use regalloc2::fuzzing::func::{self, Func, Options};
use regalloc2::prove::{
    check_bound, check_colouring, interfere_graph, preg_pressure, prove_class, try_fewer,
    verdict, MAX_EXACT,
};
use regalloc2::{Algorithm, RegClass, RegallocOptions};
use std::time::Instant;

const BASE: Options = Options {
    reused_inputs: true,
    fixed_regs: true,
    fixed_nonallocatable: true,
    clobbers: true,
    reftypes: true,
    callsite_ish_constraints: true,
    ..Options::DEFAULT
};

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

struct Case {
    mode: &'static str,
    opts: Options,
    preferred: usize,
    bytes: Vec<u8>,
}

fn case_of(seed: u64) -> Case {
    let mut st = seed ^ 0x5eed_0f0c_a2a2;
    let r = splitmix(&mut st);
    // Smaller than rafuzz — exact proofs are exponential in class size.
    let len = 256usize << (r % 7);
    let len = len + (splitmix(&mut st) as usize % len);
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        bytes.extend_from_slice(&splitmix(&mut st).to_le_bytes());
    }
    let mut opts = BASE;
    let (mode, preferred) = if seed % 4 == 0 {
        let total = 8 + (splitmix(&mut st) % 9) as usize;
        opts.alloc_regs = total;
        ("tight", (total * 3 / 4).max(1))
    } else if seed % 5 == 0 {
        ("plain", 24)
    } else {
        opts.pliron_hints = true;
        ("hints", 24)
    };
    Case {
        mode,
        opts,
        preferred,
        bytes,
    }
}

fn generate(c: &Case) -> Option<Func> {
    (0..8).find_map(|k| {
        let mut u = Unstructured::new(&c.bytes[(k * 97).min(c.bytes.len())..]);
        Func::arbitrary_with_options(&mut u, &c.opts).ok()
    })
}

fn parse_algo(s: &str) -> Algorithm {
    match s {
        "greedy" => Algorithm::Greedy,
        "fast" | "fastalloc" => Algorithm::Fastalloc,
        "mcts" => Algorithm::Mcts,
        "ion" | "" => Algorithm::Ion,
        other => {
            eprintln!("unknown algo {other}; use ion|greedy|fast|mcts");
            std::process::exit(2);
        }
    }
}

fn algo_name(a: Algorithm) -> &'static str {
    match a {
        Algorithm::Greedy => "greedy",
        Algorithm::Fastalloc => "fast",
        Algorithm::Mcts => "mcts",
        Algorithm::Ion => "ion",
    }
}

fn main() {
    let mut start = 0u64;
    let mut count = 50u64;
    let mut replay: Option<u64> = None;
    let mut algorithm = Algorithm::Ion;
    let mut prove_fewer = false;
    let mut budget = 2_000_000usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--start" => start = args.next().unwrap().parse().unwrap(),
            "--count" => count = args.next().unwrap().parse().unwrap(),
            "--replay" => replay = Some(args.next().unwrap().parse().unwrap()),
            "--algo" => algorithm = parse_algo(&args.next().unwrap()),
            "--prove-fewer" => prove_fewer = true,
            "--budget" => budget = args.next().unwrap().parse().unwrap(),
            "-h" | "--help" => {
                eprintln!(
                    "usage: raproof [--start S] [--count N] [--replay SEED] \
                     [--algo ion|greedy|fast|mcts] [--prove-fewer] [--budget N]"
                );
                return;
            }
            _ => {
                eprintln!("unknown arg {a}");
                std::process::exit(2);
            }
        }
    }

    let algo = algo_name(algorithm);
    let seeds: Vec<u64> = match replay {
        Some(s) => vec![s],
        None => (start..start + count).collect(),
    };

    let mut above = 0u64;
    let mut match_opt = 0u64;
    let mut gap = 0u64;
    let mut fewer_hits = 0u64;
    let mut fewer_tight = 0u64;
    let mut skipped = 0u64;
    let mut reported = 0u64;
    let mut bad_cert = 0u64;
    let t0 = Instant::now();

    for seed in seeds {
        let c = case_of(seed);
        let Some(func) = generate(&c) else {
            skipped += 1;
            continue;
        };
        let env = if c.opts.alloc_regs < 32 {
            func::machine_env_sized(c.preferred, c.opts.alloc_regs)
        } else {
            func::machine_env()
        };
        let g = interfere_graph(&func);
        let out = match regalloc2::run(
            &func,
            &env,
            &RegallocOptions {
                validate_ssa: false,
                verbose_log: false,
                algorithm,
            },
        ) {
            Ok(o) => o,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let mut any = false;
        let mut block = format!("seed={seed} mode={} algo={algo}", c.mode);
        for class in [RegClass::Int, RegClass::Float, RegClass::Vector] {
            let b = prove_class(&g, class);
            if b.n_class == 0 {
                continue;
            }
            if let Err(e) = check_bound(&g, &b) {
                bad_cert += 1;
                block.push_str(&format!("\n  {class:?}: BAD CERT: {e}"));
                any = true;
                continue;
            }
            any = true;
            let pressure = preg_pressure(&out, class);
            let v = verdict(&b, pressure);
            match v {
                "heuristic_above_optimum" => above += 1,
                "heuristic_matches_optimum" | "matches_clique_lower" => match_opt += 1,
                "gap_above_clique_unresolved" => gap += 1,
                _ => {}
            }
            let up = b
                .upper_k
                .map(|k| format!("{k}"))
                .unwrap_or_else(|| "—".into());
            block.push_str(&format!(
                "\n  {class:?}: n={} lower={} upper={up} {algo}_pregs={pressure} {v}",
                b.n_class, b.lower_k
            ));
            if !b.exact {
                block.push_str(&format!(" [n>{MAX_EXACT}, clique-only]"));
            }
            if v == "heuristic_above_optimum" {
                if let Some(cert) = &b.upper {
                    let show: Vec<_> = b.lower.nodes.iter().take(16).copied().collect();
                    block.push_str(&format!(
                        "\n    proof: clique {show:?} ⇒ ≥{}; colouring ⇒ ≤{} (checked)",
                        b.lower_k, cert.k
                    ));
                }
            }
            let want_fewer = prove_fewer
                && (v == "heuristic_above_optimum" || v == "gap_above_clique_unresolved")
                && pressure > b.lower_k as usize;
            if want_fewer {
                let mut bud = budget;
                if let Some(f) = try_fewer(&g, class, pressure, &mut bud) {
                    if let Err(e) = check_colouring(&g, &f.colouring.colour, f.k) {
                        block.push_str(&format!("\n    fewer: BAD COLOURING: {e}"));
                    } else {
                        fewer_hits += 1;
                        if f.tight {
                            fewer_tight += 1;
                        }
                        let sample: Vec<_> = f
                            .colouring
                            .colour
                            .iter()
                            .enumerate()
                            .filter(|&(_, c)| *c != u32::MAX)
                            .take(12)
                            .map(|(i, c)| format!("v{i}→r{c}"))
                            .collect();
                        block.push_str(&format!(
                            "\n    fewer: K={} (was {pressure}) tight={} assign=[{}]",
                            f.k,
                            f.tight,
                            sample.join(", ")
                        ));
                    }
                } else {
                    block.push_str(&format!(
                        "\n    fewer: no colouring < {pressure} within budget (lower={})",
                        b.lower_k
                    ));
                }
            }
        }
        if any {
            reported += 1;
            println!("{block}");
        } else {
            skipped += 1;
        }
    }

    eprintln!(
        "raproof: reported={reported} skipped={skipped} above_opt={above} match={match_opt} \
         gap={gap} fewer_hits={fewer_hits} fewer_tight={fewer_tight} bad_cert={bad_cert} \
         in {:.1}s (MAX_EXACT={MAX_EXACT})",
        t0.elapsed().as_secs_f64()
    );
}
