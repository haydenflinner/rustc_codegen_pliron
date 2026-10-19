//! `iconst128`: `iconst.i128` is valid IR per the builder but has no
//! isel rule on any target (and the reader rejects it — so this must be
//! built programmatically). The step rewrites it to an `iconcat` pair
//! of sign-extended i64 limbs.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{AbiParam, Function, InstBuilder, Signature, UserFuncName, types};
use cranelift_codegen::isa::CallConv;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

#[test]
fn iconst128_lowers_to_iconcat() {
    // -5 → lo=-5, hi=-1; 5 → lo=5, hi=0; MIN keeps its own pattern.
    for k in [-5i64, 5, 0, i64::MIN] {
        let mut f = Function::with_name_signature(
            UserFuncName::user(0, 0),
            Signature::new(CallConv::SystemV),
        );
        f.signature.returns.push(AbiParam::new(types::I128));
        let b = f.dfg.make_block();
        f.layout.append_block(b);
        let mut pos = FuncCursor::new(&mut f).at_bottom(b);
        let k128 = pos.ins().iconst(types::I128, k);
        pos.ins().return_(&[k128]);
        let before = f.clone();
        let n = pliron_clif_opt::clifpeep::iconst128(&mut f, true);
        assert_eq!(n, 1, "k={k}");
        let text = f.display().to_string();
        assert!(!text.contains("iconst.i128"), "k={k}:\n{text}");
        assert!(text.contains("iconcat"), "k={k}:\n{text}");
        // The i128 value is unchanged.
        check(&before, &f, &Facts::default(), &Config::default()).unwrap();
    }
}
