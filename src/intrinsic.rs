//! Rust intrinsics -> pliron ops / `llvm.*` intrinsic calls / libm calls.

use pliron::value::Value;
use rustc_abi::Align;
use rustc_codegen_ssa::common::IntPredicate;
use rustc_codegen_ssa::mir::IntrinsicResult;
use rustc_codegen_ssa::mir::operand::{OperandRef, OperandValue};
use rustc_codegen_ssa::mir::place::PlaceRef;
use rustc_codegen_ssa::traits::*;
use rustc_middle::ty::layout::{LayoutOf, TyAndLayout};
use rustc_middle::ty::{self, Instance};
use rustc_span::{Span, sym};

use crate::builder::Builder;

impl<'a, 'tcx> IntrinsicCallBuilderMethods<'tcx> for Builder<'a, 'tcx> {
    fn codegen_intrinsic_call(
        &mut self,
        instance: Instance<'tcx>,
        args: &[OperandRef<'tcx, Value>],
        result_layout: TyAndLayout<'tcx>,
        _result_place: Option<rustc_codegen_ssa::mir::place::PlaceValue<Value>>,
        _span: Span,
    ) -> IntrinsicResult<'tcx, Value> {
        let name = self.tcx.item_name(instance.def_id());
        let ret = self.immediate_backend_type(result_layout);
        let a = |i: usize| args[i].immediate();
        let imm = |v: Value| IntrinsicResult::Operand(OperandValue::Immediate(v));
        let float = |s: &str| -> Option<&'static str> {
            Some(match s {
                "sqrtf32" | "sqrtf64" => "llvm.sqrt",
                "fabsf32" | "fabsf64" => "llvm.fabs",
                "floorf32" | "floorf64" => "llvm.floor",
                "ceilf32" | "ceilf64" => "llvm.ceil",
                "truncf32" | "truncf64" => "llvm.trunc",
                "round_ties_even_f32" | "round_ties_even_f64" => "llvm.roundeven",
                "copysignf32" | "copysignf64" => "llvm.copysign",
                "fmaf32" | "fmaf64" | "fmuladdf32" | "fmuladdf64" => "llvm.fma",
                "minimumf32" | "minimumf64" => "llvm.minimum",
                "maximumf32" | "maximumf64" => "llvm.maximum",
                _ => return None,
            })
        };
        let libm = |s: &str| -> Option<&'static str> {
            Some(match s {
                "roundf32" => "roundf",
                "roundf64" => "round",
                "sinf32" => "sinf",
                "sinf64" => "sin",
                "cosf32" => "cosf",
                "cosf64" => "cos",
                "tanf32" => "tanf",
                "tanf64" => "tan",
                "expf32" => "expf",
                "expf64" => "exp",
                "exp2f32" => "exp2f",
                "exp2f64" => "exp2",
                "logf32" => "logf",
                "logf64" => "log",
                "log2f32" => "log2f",
                "log2f64" => "log2",
                "log10f32" => "log10f",
                "log10f64" => "log10",
                "powf32" => "powf",
                "powf64" => "pow",
                "powif32" => "__powisf2",
                "powif64" => "__powidf2",
                "minnumf32" => "fminf",
                "minnumf64" => "fmin",
                "maxnumf32" => "fmaxf",
                "maxnumf64" => "fmax",
                _ => return None,
            })
        };
        let n = name.as_str();
        if n.starts_with("simd_") {
            return match self.simd_intrinsic(n, args, result_layout.ty, ret, _span, instance) {
                Some(v) => imm(v),
                None => self.tcx.dcx().fatal(format!(
                    "SIMD intrinsic `{n}` is not supported by the pliron backend yet"
                )),
            };
        }
        if let Some(i) = float(n) {
            let vs: Vec<_> = (0..args.len()).map(a).collect();
            return imm(self.intrinsic(i, ret, &vs));
        }
        if let Some(f) = libm(n) {
            let vs: Vec<_> = (0..args.len()).map(a).collect();
            return imm(self.call_sym(f, ret, &vs));
        }
        let r = match name {
            sym::va_arg => {
                let v = self.va_arg(a(0), ret);
                return imm(v);
            }
            sym::ctpop | sym::ctlz | sym::cttz | sym::ctlz_nonzero | sym::cttz_nonzero => {
                let x = a(0);
                let ty = self.val_ty(x);
                let i = match name {
                    sym::ctpop => "llvm.ctpop",
                    sym::ctlz | sym::ctlz_nonzero => "llvm.ctlz",
                    _ => "llvm.cttz",
                };
                let r = self.intrinsic(i, ty, &[x]);
                self.intcast(r, ret, false)
            }
            sym::bswap | sym::bitreverse => {
                let x = a(0);
                let ty = self.val_ty(x);
                let i = if name == sym::bswap {
                    "llvm.bswap"
                } else {
                    "llvm.bitreverse"
                };
                self.intrinsic(i, ty, &[x])
            }
            sym::rotate_left
            | sym::rotate_right
            | sym::unchecked_funnel_shl
            | sym::unchecked_funnel_shr => {
                let (x, y, s) = match name {
                    sym::rotate_left | sym::rotate_right => (a(0), a(0), a(1)),
                    _ => (a(0), a(1), a(2)),
                };
                let ty = self.val_ty(x);
                let s = self.intcast(s, ty, false);
                let i = if matches!(name, sym::rotate_left | sym::unchecked_funnel_shl) {
                    "llvm.fshl"
                } else {
                    "llvm.fshr"
                };
                self.intrinsic(i, ty, &[x, y, s])
            }
            sym::saturating_add | sym::saturating_sub => {
                let (x, y) = (a(0), a(1));
                let t = args[0].layout.ty;
                let signed = t.is_signed();
                let ty = self.val_ty(x);
                let op = if name == sym::saturating_add {
                    OverflowOp::Add
                } else {
                    OverflowOp::Sub
                };
                let (r, of) = self.checked_binop(op, t, x, y);
                let w = self.int_width(ty);
                let sat = if signed {
                    let min = self.const_uint_big(ty, 1u128 << (w - 1));
                    let max = self.const_uint_big(ty, (1u128 << (w - 1)) - 1);
                    let zero = self.const_null(ty);
                    let neg = if op == OverflowOp::Add {
                        self.icmp(IntPredicate::IntSLT, x, zero)
                    } else {
                        self.icmp(IntPredicate::IntSLT, x, zero)
                    };
                    self.select(neg, min, max)
                } else if op == OverflowOp::Add {
                    self.const_int(ty, -1)
                } else {
                    self.const_null(ty)
                };
                self.select(of, sat, r)
            }
            sym::black_box => return IntrinsicResult::Operand(args[0].val),
            sym::volatile_load | sym::unaligned_volatile_load => {
                let place = PlaceRef::new_sized(a(0), result_layout);
                return IntrinsicResult::Operand(self.load_operand(place).val);
            }
            sym::volatile_store | sym::unaligned_volatile_store => {
                let dst = PlaceRef::new_sized(a(0), args[1].layout);
                args[1].val.store(self, dst);
                return IntrinsicResult::Operand(OperandValue::ZeroSized);
            }
            sym::catch_unwind => {
                let (try_fn, data, catch_fn) = (a(0), a(1), a(2));
                let ptr = self.type_ptr();
                let void = self.type_void();
                let i32t = self.type_i32();
                let fty = self.type_func(&[ptr], void);
                if self.tcx.sess.panic_strategy() != rustc_target::spec::PanicStrategy::Unwind {
                    self.call_raw(fty, try_fn, &[data], Default::default());
                    self.const_i32(0)
                } else {
                    let a4 = rustc_abi::Align::from_bytes(4).unwrap();
                    let slot = self.alloca(rustc_abi::Size::from_bytes(4), a4);
                    let then = self.append_sibling_block("catch_unwind_ok");
                    let catch = self.append_sibling_block("catch_unwind_caught");
                    let join = self.append_sibling_block("catch_unwind_join");
                    self.st.borrow_mut().last_call = None;
                    self.call_raw(fty, try_fn, &[data], Default::default());
                    let op = self.st.borrow_mut().last_call.take().unwrap();
                    self.st.borrow_mut().invokes.insert(op, (catch, true));
                    self.br(then);
                    self.switch_to_block(then);
                    let z = self.const_i32(0);
                    self.store(z, slot, a4);
                    self.br(join);
                    self.switch_to_block(catch);
                    let exn = self.intrinsic("pliron.eh.exn", ptr, &[]);
                    let cty = self.type_func(&[ptr, ptr], void);
                    self.call_raw(cty, catch_fn, &[data, exn], Default::default());
                    let one = self.const_i32(1);
                    self.store(one, slot, a4);
                    self.br(join);
                    self.switch_to_block(join);
                    self.load(i32t, slot, a4)
                }
            }
            sym::ptr_mask => {
                let isize = self.type_isize();
                let p = self.ptrtoint(a(0), isize);
                let m = self.and(p, a(1));
                self.inttoptr(m, ret)
            }
            sym::is_val_statically_known => self.const_bool(false),
            sym::compare_bytes => {
                let i32t = self.type_i32();
                self.call_sym("memcmp", i32t, &[a(0), a(1), a(2)])
            }
            sym::raw_eq => {
                let tp = instance.args.type_at(0);
                let size = self.layout_of(tp).size.bytes();
                if size == 0 {
                    self.const_bool(true)
                } else {
                    let i32t = self.type_i32();
                    let n = self.const_usize(size);
                    let c = self.call_sym("memcmp", i32t, &[a(0), a(1), n]);
                    let z = self.const_i32(0);
                    self.icmp(IntPredicate::IntEQ, c, z)
                }
            }
            sym::abort => {
                self.abort_immediate();
                return IntrinsicResult::Operand(OperandValue::ZeroSized);
            }
            sym::breakpoint => {
                self.abort_immediate();
                return IntrinsicResult::Operand(OperandValue::ZeroSized);
            }
            sym::prefetch_read_data
            | sym::prefetch_write_data
            | sym::prefetch_read_instruction
            | sym::prefetch_write_instruction => {
                return IntrinsicResult::Operand(OperandValue::ZeroSized);
            }
            _ => {
                return IntrinsicResult::Fallback(ty::Instance::new_raw(
                    instance.def_id(),
                    instance.args,
                ));
            }
        };
        imm(r)
    }

    fn codegen_llvm_intrinsic_call(
        &mut self,
        instance: Instance<'tcx>,
        args: &[OperandRef<'tcx, Value>],
        _is_cleanup: bool,
    ) -> Value {
        let name = self.tcx.symbol_name(instance).name.to_string();
        // fn_abi_of_instance rejects LLVM intrinsics; use the signature directly.
        let sig = self
            .tcx
            .fn_sig(instance.def_id())
            .instantiate(self.tcx, instance.args)
            .skip_norm_wip();
        let sig = self.tcx.instantiate_bound_regions_with_erased(sig);
        let out = self.layout_of(sig.output());
        let ret = if out.is_zst() {
            self.type_void()
        } else {
            self.immediate_backend_type(out)
        };
        let vs: Vec<Value> = args.iter().map(|a| a.immediate()).collect();
        if let Some(v) = self.llvm_x86_intrinsic(&name, args, sig.output(), ret) {
            return v;
        }
        let stub = self.cx.llvm_intrinsic_stub(&name);
        self.call_sym(&stub, ret, &vs)
    }

    fn abort_immediate(&mut self) {
        let v = self.type_void();
        self.intrinsic("llvm.trap", v, &[]);
    }
    fn assume(&mut self, val: Value) {
        // `assume(false)` marks the rest of the block unreachable (e.g. the
        // `unreachable_unchecked` UB-check path); `unreach` then drops the
        // branch into it.
        if crate::pass_enabled("PLIRON_UNREACH") && self.cx.const_to_opt_uint(val) == Some(0) {
            let rest = self.append_sibling_block("assume_false");
            self.unreachable();
            self.switch_to_block(rest);
        }
    }
    fn retag_mem(&mut self, _place: Value, _info: &rustc_codegen_ssa::RetagInfo<Value>) {}
    fn retag_reg(&mut self, ptr: Value, _info: &rustc_codegen_ssa::RetagInfo<Value>) -> Value {
        ptr
    }
    fn expect(&mut self, cond: Value, _expected: bool) -> Value {
        cond
    }
    fn type_checked_load(
        &mut self,
        _llvtable: Value,
        _vtable_byte_offset: u64,
        _typeid: &[u8],
    ) -> Value {
        panic!("type_checked_load is not supported by the pliron backend")
    }
    fn va_start(&mut self, val: Value) {
        // The variadic-argument buffer pointer, delivered by the lowering as the
        // hidden extra parameter of a C-variadic function. The VaList place gets
        // a real SysV `__va_list_tag`: gp/fp offsets exhausted so every va_arg
        // (ours and a forwarded foreign callee's alike) walks `overflow_arg_area`,
        // which points at the packed buffer.
        let buf = self.intrinsic("pliron.va.buf", self.type_ptr(), &[]);
        if self.tcx.sess.target.is_like_wasm {
            // wasm's va_list is a single opaque pointer: the place itself is
            // the cursor into the packed buffer.
            self.store(buf, val, Align::from_bytes(4).unwrap());
            return;
        }
        let i8t = self.type_i8();
        let i32t = self.type_i32();
        let gp = self.const_int(i32t, 48);
        self.store(gp, val, Align::from_bytes(4).unwrap());
        let p4 = self.gep(i8t, val, &[self.const_usize(4)]);
        let fp = self.const_int(i32t, 304);
        self.store(fp, p4, Align::from_bytes(4).unwrap());
        let p8 = self.gep(i8t, val, &[self.const_usize(8)]);
        self.store(buf, p8, Align::EIGHT);
        let p16 = self.gep(i8t, val, &[self.const_usize(16)]);
        self.store(buf, p16, Align::EIGHT);
    }
}
