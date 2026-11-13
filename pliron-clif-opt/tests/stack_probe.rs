//! The aarch64 inline stack-probe loop never writes below `sp`.
//!
//! Cranelift's loop stored at `[sp, x16]` with `sp` unmoved. Linux grows
//! the stack for such a store, valgrind does not (it extends the stack
//! only for accesses at or above `sp`): a program with a frame of more
//! than three guard pages died under valgrind in that function's
//! prologue (`bench/heldout/irdiff.py rust-compression`). The patched loop
//! moves `sp` one page at a time, stores at `[sp]`, and puts `sp` back
//! (`PLIRON_PROBE_SP=0` restores the old loop).

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::ir::{self, AbiParam, InstBuilder, MemFlagsData, Signature, StackSlotData, StackSlotKind, UserFuncName, types};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

fn words(frame: u32) -> Vec<u32> {
    let mut fb = settings::builder();
    fb.set("opt_level", "speed_and_size").unwrap();
    fb.enable("enable_probestack").unwrap();
    fb.set("probestack_strategy", "inline").unwrap();
    let isa = cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap();
    let mut sig = Signature::new(CallConv::triple_default(&target_lexicon::Triple::host()));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    let mut f = ir::Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut fbc = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut f, &mut fbc);
        let blk = b.create_block();
        b.append_block_params_for_function_params(blk);
        b.switch_to_block(blk);
        b.seal_block(blk);
        let x = b.block_params(blk)[0];
        let ss = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, frame, 3));
        let a = b.ins().stack_addr(types::I64, ss, 0);
        // An address the optimizer cannot resolve to one slot offset.
        let p = b.ins().iadd(a, x);
        b.ins().store(MemFlagsData::new(), x, p, 0);
        let r = b.ins().load(types::I64, MemFlagsData::new(), a, 8);
        b.ins().return_(&[r]);
        b.finalize(isa.frontend_config());
    }
    let mut ctx = cranelift_codegen::Context::for_function(f);
    let code = ctx.compile(&*isa, &mut Default::default()).unwrap();
    code.code_buffer().chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect()
}

/// `str wzr, [sp, xN]`: the store below an unmoved `sp`.
fn stores_below_sp(w: u32) -> bool {
    // str w31(zr), [x31(sp), xM, lsl #0 | uxtx]: size=10 111000 001 Rm option S 10 Rn Rt
    w & 0xffe0_0fff == 0xb820_0bff
}

const SUB_SP_PAGE: u32 = 0xd140_07ff; // sub sp, sp, #1, lsl #12
const STR_WZR_SP: u32 = 0xb900_03ff; // str wzr, [sp]

#[test]
fn probe_loop_moves_sp() {
    // 64KB: sixteen guard pages, past the three that are unrolled.
    let ws = words(64 * 1024);
    assert!(!ws.iter().any(|&w| stores_below_sp(w)), "a probe stores below sp: {ws:08x?}");
    let at = ws.windows(2).position(|p| p == [SUB_SP_PAGE, STR_WZR_SP]);
    assert!(at.is_some(), "no `sub sp, sp, #4096; str wzr, [sp]` probe: {ws:08x?}");
    // sp is restored after the loop: sub sp, sp, x16, uxtx.
    assert!(ws.contains(&0xcb30_63ff), "sp is not restored after the probe loop: {ws:08x?}");
}

#[test]
fn unrolled_probes_move_sp() {
    let ws = words(3 * 4096);
    assert!(!ws.iter().any(|&w| stores_below_sp(w)), "a probe stores below sp: {ws:08x?}");
    // `stur wzr, [sp]` here (upstream's unrolled form already moved sp).
    assert!(ws.windows(2).any(|p| p[0] == SUB_SP_PAGE && (p[1] == STR_WZR_SP || p[1] == 0xb800_03ff)), "{ws:08x?}");
}
