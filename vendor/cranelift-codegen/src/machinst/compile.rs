//! Compilation backend pipeline: optimized IR to VCode / binemit.

use crate::dominator_tree::DominatorTree;
use crate::ir::Function;
use crate::isa::TargetIsa;
use crate::machinst::*;
use crate::settings::RegallocAlgorithm;
use crate::timing;
use crate::trace;

use regalloc2::{Algorithm, RegallocOptions};

/// Compile the given function down to VCode with allocated registers, ready
/// for binary emission.
pub fn compile<B: LowerBackend + TargetIsa>(
    f: &Function,
    domtree: &DominatorTree,
    regalloc_ctx: &mut regalloc2::Ctx,
    b: &B,
    abi: Callee<<<B as LowerBackend>::MInst as MachInst>::ABIMachineSpec>,
    emit_info: <B::MInst as MachInstEmit>::Info,
    sigs: SigSet,
    ctrl_plane: &mut ControlPlane,
) -> CodegenResult<VCode<B::MInst>> {
    // Compute lowered block order.
    let block_order = BlockLoweringOrder::new(f, domtree, ctrl_plane);

    // Build the lowering context.
    let lower =
        crate::machinst::Lower::new(f, abi, emit_info, block_order, sigs, b.flags().clone())?;

    // Lower the IR.
    let vcode = {
        log::debug!(
            "Number of CLIF instructions to lower: {}",
            f.dfg.num_insts()
        );
        log::debug!("Number of CLIF blocks to lower: {}", f.dfg.num_blocks());

        let _tt = timing::vcode_lower();
        lower.lower(b, ctrl_plane)?
    };

    log::debug!(
        "Number of lowered vcode instructions: {}",
        vcode.num_insts()
    );
    log::debug!("Number of lowered vcode blocks: {}", vcode.num_blocks());
    trace!("vcode from lowering: \n{:?}", vcode);

    // PLIRON: `PLIRON_PREVCODE=<substr>` dumps pre-regalloc VCode for
    // functions whose CLIF name contains <substr>; `all` matches every
    // function. (CLIF names are `u0:N` — correlate to symbols with
    // `PLIRON_CLIF` output.)
    static PREVCODE: std::sync::OnceLock<Option<alloc::string::String>> =
        std::sync::OnceLock::new();
    static RA2_VERBOSE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static RA2_EDITS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let prevcode_match = PREVCODE
        .get_or_init(|| std::env::var("PLIRON_PREVCODE").ok())
        .as_deref()
        .map(|pat| pat == "all" || (!pat.is_empty() && format!("{}", f.name).contains(pat)))
        .unwrap_or(false);
    let ra2_verbose =
        *RA2_VERBOSE.get_or_init(|| std::env::var_os("PLIRON_RA2_VERBOSE").is_some());
    let ra2_edits = *RA2_EDITS.get_or_init(|| std::env::var_os("PLIRON_RA2_EDITS").is_some());
    if prevcode_match {
        eprintln!("==== prevcode {} ====\n{:?}", f.name, vcode);
    }

    // Perform register allocation.
    {
        let _tt = timing::regalloc();
        let mut options = RegallocOptions::default();
        options.verbose_log = b.flags().regalloc_verbose_logs()
            || ra2_verbose;

        if cfg!(debug_assertions) {
            options.validate_ssa = true;
        }

        options.algorithm = match b.flags().regalloc_algorithm() {
            RegallocAlgorithm::Backtracking => Algorithm::Ion,
            RegallocAlgorithm::SinglePass => Algorithm::Fastalloc,
        };

        regalloc2::run_with_ctx(&vcode, vcode.abi.machine_env(), &options, regalloc_ctx)
            .map_err(|err| {
                log::error!(
                    "Register allocation error for vcode\n\
                     {vcode:?}\n\
                     Error: {err:?}\n\
                     CLIF for error:\n\
                     {f:?}",
                );
                err
            })
            .expect("register allocation");

        // PLIRON: `PLIRON_RA2_VERBOSE=1` dumps regalloc decision
        // annotations for the same functions as `PLIRON_PREVCODE`.
        if prevcode_match && ra2_verbose {
            let mut anns: Vec<_> = regalloc_ctx
                .debug_annotations
                .iter()
                .map(|(pp, v)| (*pp, v.clone()))
                .collect();
            anns.sort_by_key(|(pp, _)| (pp.inst().index(), pp.pos() as u8));
            eprintln!("==== ra2annot {} ====", f.name);
            for (pp, v) in anns {
                for a in v {
                    eprintln!("  {:?}: {}", pp, a);
                }
            }
            eprintln!("==== ra2annot end ====");
        }
        // PLIRON: `PLIRON_RA2_EDITS=1` dumps regalloc-inserted moves
        // (program point → edit) for the same functions as
        // `PLIRON_PREVCODE`.
        if prevcode_match && ra2_edits {
            eprintln!("==== ra2edits {} ====", f.name);
            for (pp, edit) in &regalloc_ctx.output.edits {
                let inst = pp.inst();
                // Find the vcode block containing this inst and show
                // the inst's operand vregs -> allocs for context.
                use regalloc2::Function as _;
                let mut blk = None;
                for b in 0..vcode.num_blocks() {
                    let r = vcode.block_insns(regalloc2::Block::new(b));
                    if r.len() > 0
                        && inst.index() >= r.first().index()
                        && inst.index() <= r.last().index()
                    {
                        blk = Some(b);
                        break;
                    }
                }
                let ops: Vec<String> = vcode
                    .inst_operands(inst)
                    .iter()
                    .enumerate()
                    .map(|(i, o)| {
                        let alloc = regalloc_ctx
                            .output
                            .inst_allocs(inst)
                            .get(i)
                            .map(|a| format!("{:?}", a))
                            .unwrap_or_else(|| "?".into());
                        format!("v{}={}", o.vreg().vreg(), alloc)
                    })
                    .collect();
                eprintln!(
                    "  {:?} [blk{:?} inst{} <{}>]: {:?}",
                    pp,
                    blk,
                    inst.index(),
                    ops.join(" "),
                    edit
                );
            }
            eprintln!("==== ra2edits end ====");
        }
        // PLIRON: `PLIRON_RA2_STATS=1` prints one line per function with
        // the regalloc-inserted edits classified by kind and position
        // (`RA2STAT <fn> insts rr_term rr_entry rr_mid st_term st_entry
        // st_mid ld_term ld_entry ld_mid ss nslots`): rr = reg->reg,
        // st = spill store, ld = reload, ss = stack->stack; term = at the
        // block's last inst (edge / block-param moves), entry = before its
        // first inst, mid = anywhere else (call ABI, splits).
        if std::env::var_os("PLIRON_RA2_STATS").is_some() {
            use regalloc2::Function as _;
            let n = vcode.num_insts();
            let mut first = alloc::vec![false; n];
            let mut last = alloc::vec![false; n];
            for b in 0..vcode.num_blocks() {
                let r = vcode.block_insns(regalloc2::Block::new(b));
                if r.len() > 0 {
                    first[r.first().index()] = true;
                    last[r.last().index()] = true;
                }
            }
            let mut c = [[0u32; 3]; 4];
            for (pp, edit) in &regalloc_ctx.output.edits {
                let regalloc2::Edit::Move { from, to } = edit;
                let k = match (from.is_stack(), to.is_stack()) {
                    (false, false) => 0,
                    (false, true) => 1,
                    (true, false) => 2,
                    (true, true) => 3,
                };
                let i = pp.inst().index();
                let pos = if last[i] {
                    0
                } else if first[i] && pp.pos() == regalloc2::InstPosition::Before {
                    1
                } else {
                    2
                };
                c[k][pos] += 1;
            }
            let nslots = regalloc_ctx.output.num_spillslots;
            eprintln!(
                "RA2STAT {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                f.name, n, c[0][0], c[0][1], c[0][2], c[1][0], c[1][1], c[1][2], c[2][0],
                c[2][1], c[2][2], c[3][0] + c[3][1] + c[3][2], nslots, 0
            );
        }
        // PLIRON: `PLIRON_RA2_ALLOC=1` dumps each vreg that regalloc2
        // assigned a stack slot at least once (the actual spilled set).
        if prevcode_match && std::env::var_os("PLIRON_RA2_ALLOC").is_some() {
            use regalloc2::Function as _;
            let mut spilled = std::collections::BTreeMap::new();
            for i in 0..vcode.num_insts() {
                let inst = regalloc2::Inst::new(i);
                for (k, o) in vcode.inst_operands(inst).iter().enumerate() {
                    if let Some(a) = regalloc_ctx.output.inst_allocs(inst).get(k) {
                        if a.is_stack() {
                            spilled
                                .entry(o.vreg().vreg())
                                .or_insert_with(|| format!("{:?}", a));
                        }
                    }
                }
            }
            eprintln!("==== ra2alloc {} ====", f.name);
            for (v, a) in &spilled {
                eprintln!("  v{v} {a}");
            }
            eprintln!("==== ra2alloc end ====");
        }
    }

    // Run the regalloc checker, if requested.
    if b.flags().regalloc_checker() {
        let _tt = timing::regalloc_checker();
        let mut checker = regalloc2::checker::Checker::new(&vcode, &vcode.abi.machine_env());
        checker.prepare(&regalloc_ctx.output);
        checker
            .run()
            .map_err(|err| {
                log::error!("Register allocation checker errors:\n{err:?}\nfor vcode:\n{vcode:?}");
                err
            })
            .expect("register allocation checker");
    }

    Ok(vcode)
}
