//! `sizes <file.clif>` — run each selected pipeline step in order on a
//! corpus/diffcheck dump and print the instruction count after each, to
//! find where a function balloons. No diffcheck, no verifier.

use std::path::Path;

use cranelift_codegen::Context;
use cranelift_codegen::settings::Configurable;
use cranelift_module::FuncId;
use pliron_clif_opt::diffcheck::OwnedFacts;
use pliron_clif_opt::pipeline::{self, FnFlags, Pipe, Stats};

fn main() {
    let path = std::env::args().nth(1).expect("usage: sizes <file.clif>");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut fs = cranelift_reader::parse_functions(&text).unwrap();
    let func = fs.remove(0);
    let facts = OwnedFacts::parse(&text);

    let isa = {
        let mut fb = cranelift_codegen::settings::builder();
        fb.set("is_pic", "true").unwrap();
        cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
            .unwrap()
            .finish(cranelift_codegen::settings::Flags::new(fb))
            .unwrap()
    };

    let mut clctx = Context::for_function(func);
    let mut derived = facts.derived.clone();
    let mut stats = Stats::default();
    let flags = FnFlags {
        jumpthread: true,
        unreach: true,
        peep: true,
        tailmerge: true,
        taildup: true,
        tailrec: true,
        loadfwd: true,
        slot_dse: true,
        dse: true,
        indvars: true,
        licm: true,
        loopidiom: true,
        loopvec: true,
        bcheck: true,
        slp: true,
        keep_dbg_slots: false,
    };
    let func_id = facts
        .self_name
        .as_deref()
        .and_then(|n| n.strip_prefix("u0:"))
        .and_then(|n| n.parse().ok())
        .map(FuncId::from_u32)
        .unwrap_or(FuncId::from_u32(0));
    let x64 = isa.triple().architecture == target_lexicon::Architecture::X86_64;
    let (e1, e2, e3, e5, e6, mut e7, e8): (
        _,
        _,
        _,
        _,
        _,
        rustc_hash::FxHashSet<(cranelift_codegen::ir::Inst, u32)>,
        _,
    ) = Default::default();
    {
        use cranelift_codegen::entity::EntityRef as _;
        use cranelift_codegen::ir::instructions::CallInfo;
        let set: rustc_hash::FxHashSet<(u32, u32)> = facts
            .nocap
            .iter()
            .map(|(r, a)| (r.index() as u32, *a))
            .collect();
        for b in clctx.func.layout.blocks() {
            for i in clctx.func.layout.block_insts(b) {
                let info = clctx.func.dfg.insts[i].analyze_call(
                    &clctx.func.dfg.value_lists,
                    &clctx.func.dfg.exception_tables,
                );
                if let CallInfo::Direct(fr, cargs) | CallInfo::DirectWithSig(fr, _, cargs) = info {
                    for k in 0..cargs.len() {
                        if set.contains(&(fr.index() as u32, k as u32)) {
                            e7.insert((i, k as u32));
                        }
                    }
                }
            }
        }
    }
    let (mut nonnull, mut frozen, mut noalias, mut deref, mut e4, mut lifetimes) = (
        facts.nonnull.clone(),
        facts.frozen.clone(),
        facts.noalias.clone(),
        facts.deref.clone(),
        Default::default(),
        facts.lifetimes(&clctx.func),
    );
    let mut p = Pipe {
        clctx: &mut clctx,
        isa: &*isa,
        fname: Path::new(&path).file_name().unwrap().to_str().unwrap(),
        func_id,
        nonnull: &mut nonnull,
        derived: &mut derived,
        frozen: &mut frozen,
        noalias: &mut noalias,
        deref: &mut deref,
        nowrite: &facts.nowrite,
        call_frozen: &e1,
        scoped: &e2,
        invariant: &e3,
        operand_frozen: &mut e4,
        block_region: &e5,
        prof: &e8,
        prof_hook: None,
        fn_names: &[],
        region_parent: &e6,
        lifetimes: &mut lifetimes,
        nocap: &e7,
        dbg_slots: &Default::default(),
        tcfg: isa.frontend_config(),
        simd: true,
        x64,
        cmp64: true,
        amode_scale: pipeline::amode_scale(&*isa),
        has_eh: false,
        flags: &flags,
        dump: false,
        verify_full: false,
        stats: &mut stats,
        carry_log: Vec::new(),
        cur_step: "",
    };

    let um = |p: &Pipe| -> usize {
        p.clctx
            .func
            .layout
            .blocks()
            .flat_map(|b| p.clctx.func.layout.block_insts(b))
            .filter(|&i| {
                p.clctx.func.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Umulhi
            })
            .count()
    };
    let frame = |p: &Pipe| -> (usize, u64) {
        let mut bytes = 0u64;
        let mut n = 0usize;
        for (_ss, d) in p.clctx.func.stencil.sized_stack_slots.iter() {
            n += 1;
            bytes += d.size as u64;
        }
        (n, bytes)
    };
    let (sn, sb) = frame(&p);
    println!(
        "start {} insts, {sn} slots/{sb}B",
        p.clctx.func.dfg.num_insts()
    );
    let dump_at = std::env::var("DUMP_STEP").ok();
    for n in pipeline::selected_names(&p) {
        if dump_at.as_deref() == Some(n) {
            print!("{}", p.clctx.func.display());
            println!(";; dump before {n}");
        }
        pipeline::run_named(&mut p, n);
        let u = um(&p);
        let (sn2, sb2) = frame(&p);
        let fr = if (sn2, sb2) != (sn, sb) {
            format!(" slots={sn2}/{sb2}B")
        } else {
            String::new()
        };
        println!(
            "{n:>20} {} insts{}{}",
            p.clctx.func.dfg.num_insts(),
            if u > 0 {
                format!(" umulhi={u}")
            } else {
                String::new()
            },
            fr
        );
    }
    println!("merged={} slots end", p.stats.merged);
}
