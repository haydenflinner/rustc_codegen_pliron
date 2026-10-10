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
    let prevcode_match = std::env::var("PLIRON_PREVCODE")
        .ok()
        .map(|pat| pat == "all" || (!pat.is_empty() && format!("{}", f.name).contains(&pat)))
        .unwrap_or(false);
    if prevcode_match {
        eprintln!("==== prevcode {} ====\n{:?}", f.name, vcode);
    }

    // Perform register allocation.
    {
        let _tt = timing::regalloc();
        let mut options = RegallocOptions::default();
        options.verbose_log = b.flags().regalloc_verbose_logs();

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

        // PLIRON: `PLIRON_RA2_EDITS=1` dumps regalloc-inserted moves
        // (program point → edit) for the same functions as
        // `PLIRON_PREVCODE`.
        if prevcode_match && std::env::var_os("PLIRON_RA2_EDITS").is_some() {
            eprintln!("==== ra2edits {} ====", f.name);
            for (pp, edit) in &regalloc_ctx.output.edits {
                eprintln!("  {:?}: {:?}", pp, edit);
            }
            eprintln!("==== ra2edits end ====");
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
