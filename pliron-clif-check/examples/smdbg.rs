use cranelift_codegen::entity::EntityRef as _;
use cranelift_codegen::ir::Function;
use cranelift_codegen::ir::instructions::CallInfo;
use rustc_hash::{FxHashMap, FxHashSet};

fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let facts = pliron_clif_opt::diffcheck::OwnedFacts::parse(&src);
    let mut f: Function = cranelift_reader::parse_functions(&src).unwrap().remove(0);
    let set: FxHashSet<(u32, u32)> = facts
        .nocap
        .iter()
        .map(|(r, a)| (r.index() as u32, *a))
        .collect();
    let mut nocap = FxHashSet::default();
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            let info = f.dfg.insts[i].analyze_call(&f.dfg.value_lists, &f.dfg.exception_tables);
            if let CallInfo::Direct(fr, cargs) | CallInfo::DirectWithSig(fr, _, cargs) = info {
                for k in 0..cargs.len() {
                    if set.contains(&(fr.index() as u32, k as u32)) {
                        nocap.insert((i, k as u32));
                    }
                }
            }
        }
    }
    let lt = facts.lifetimes(&f);
    let k = pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &lt, &nocap);
    println!("merged={k} lts={}", lt.len());
    let mut bytes = 0u64;
    let mut live = 0;
    for (_s, d) in f.sized_stack_slots.iter() {
        bytes += d.size as u64;
        if d.size > 0 {
            live += 1;
        }
    }
    println!("live={live} frame={bytes}B");
    for (s, d) in f.sized_stack_slots.iter() {
        println!("{s} size={} align={}", d.size, d.align_shift);
    }
}
