//! Portable SIMD intrinsics, scalarized lane-by-lane into pliron
//! extractelement/insertelement + scalar ops. The Cranelift lowering
//! flattens vectors into per-lane values, so this costs nothing extra there.

use pliron::r#type::TypeHandle;
use pliron::value::Value;
use pliron_llvm::ops::InsertElementOp;
use rustc_codegen_ssa::common::{IntPredicate, RealPredicate, TypeKind};
use rustc_codegen_ssa::mir::operand::OperandRef;
use rustc_codegen_ssa::traits::*;
use rustc_middle::ty::Ty;

use crate::builder::Builder;
use crate::context::ConstVal;

#[derive(Clone, Copy)]
struct Elem {
    signed: bool,
    float: bool,
}

impl<'a, 'tcx> Builder<'a, 'tcx> {
    fn elem_of(&self, t: Ty<'tcx>) -> (u64, Elem) {
        let (n, et) = t.simd_size_and_type(self.tcx);
        (n, Elem { signed: et.is_signed(), float: et.is_floating_point() })
    }

    fn lane(&mut self, v: Value, i: u64) -> Value {
        let idx = self.const_i32(i as i32);
        self.extract_element(v, idx)
    }

    fn lanes(&mut self, v: Value, n: u64) -> Vec<Value> {
        (0..n).map(|i| self.lane(v, i)).collect()
    }

    fn build_vec(&mut self, ty: TypeHandle, vals: Vec<Value>) -> Value {
        let mut v = self.const_undef(ty);
        for (i, e) in vals.into_iter().enumerate() {
            let idx = self.const_i32(i as i32);
            v = self.mk(|c| InsertElementOp::new(c, v, e, idx));
        }
        v
    }

    fn nonzero(&mut self, x: Value) -> Value {
        let z = self.const_null(self.val_ty(x));
        self.icmp(IntPredicate::IntNE, x, z)
    }

    fn lane_bin(&mut self, op: &str, e: Elem, x: Value, y: Value) -> Option<Value> {
        Some(match (op, e.float) {
            ("add", false) => self.add(x, y),
            ("add", true) => self.fadd(x, y),
            ("sub", false) => self.sub(x, y),
            ("sub", true) => self.fsub(x, y),
            ("mul", false) => self.mul(x, y),
            ("mul", true) => self.fmul(x, y),
            ("div", true) => self.fdiv(x, y),
            ("div", false) if e.signed => self.sdiv(x, y),
            ("div", false) => self.udiv(x, y),
            ("rem", true) => self.frem(x, y),
            ("rem", false) if e.signed => self.srem(x, y),
            ("rem", false) => self.urem(x, y),
            ("shl", _) => self.shl(x, y),
            ("shr", _) if e.signed => self.ashr(x, y),
            ("shr", _) => self.lshr(x, y),
            ("and", _) => self.and(x, y),
            ("or", _) => self.or(x, y),
            ("xor", _) => self.xor(x, y),
            ("min" | "fmin", _) | ("max" | "fmax", _) => {
                let is_min = op.ends_with("min");
                let c = if e.float {
                    self.fcmp(if is_min { RealPredicate::RealOLT } else { RealPredicate::RealOGT }, x, y)
                } else {
                    let p = match (is_min, e.signed) {
                        (true, true) => IntPredicate::IntSLT,
                        (true, false) => IntPredicate::IntULT,
                        (false, true) => IntPredicate::IntSGT,
                        (false, false) => IntPredicate::IntUGT,
                    };
                    self.icmp(p, x, y)
                };
                self.select(c, x, y)
            }
            _ => return None,
        })
    }

    fn lane_cmp(&mut self, op: &str, e: Elem, x: Value, y: Value) -> Option<Value> {
        Some(if e.float {
            let p = match op {
                "eq" => RealPredicate::RealOEQ,
                "ne" => RealPredicate::RealUNE,
                "lt" => RealPredicate::RealOLT,
                "le" => RealPredicate::RealOLE,
                "gt" => RealPredicate::RealOGT,
                "ge" => RealPredicate::RealOGE,
                _ => return None,
            };
            self.fcmp(p, x, y)
        } else {
            use IntPredicate::*;
            let p = match (op, e.signed) {
                ("eq", _) => IntEQ,
                ("ne", _) => IntNE,
                ("lt", true) => IntSLT,
                ("lt", false) => IntULT,
                ("le", true) => IntSLE,
                ("le", false) => IntULE,
                ("gt", true) => IntSGT,
                ("gt", false) => IntUGT,
                ("ge", true) => IntSGE,
                ("ge", false) => IntUGE,
                _ => return None,
            };
            self.icmp(p, x, y)
        })
    }

    fn lane_cast(&mut self, x: Value, from: Elem, to: Elem, to_ty: TypeHandle) -> Value {
        match (from.float, to.float) {
            (false, false) => self.intcast(x, to_ty, from.signed),
            (false, true) if from.signed => self.sitofp(x, to_ty),
            (false, true) => self.uitofp(x, to_ty),
            (true, false) if to.signed => self.fptosi(x, to_ty),
            (true, false) => self.fptoui(x, to_ty),
            (true, true) => {
                let (fw, tw) = (self.float_width(self.val_ty(x)), self.float_width(to_ty));
                if fw < tw {
                    self.fpext(x, to_ty)
                } else if fw > tw {
                    self.fptrunc(x, to_ty)
                } else {
                    x
                }
            }
        }
    }

    /// Returns `None` for SIMD intrinsics that aren't implemented yet.
    pub fn simd_intrinsic(
        &mut self,
        name: &str,
        args: &[OperandRef<'tcx, Value>],
        ret_rty: Ty<'tcx>,
        ret: TypeHandle,
    ) -> Option<Value> {
        let a = |i: usize| args[i].immediate();
        let op = name.strip_prefix("simd_")?;
        if op == "splat" {
            let (n, _) = self.elem_of(ret_rty);
            return Some(self.vector_splat(n as usize, a(0)));
        }
        if op == "expose_provenance" || op == "with_exposed_provenance" {
            let (n, _) = self.elem_of(ret_rty);
            let et = self.element_type(ret);
            let xs = self.lanes(a(0), n);
            let out = xs
                .into_iter()
                .map(|x| if op == "expose_provenance" { self.ptrtoint(x, et) } else { self.inttoptr(x, et) })
                .collect();
            return Some(self.build_vec(ret, out));
        }
        if op == "cast_ptr" {
            // Opaque pointers: pointer-to-pointer lane casts are no-ops.
            return Some(a(0));
        }
        let (n, e) = self.elem_of(args[0].layout.ty);
        let base = op.strip_suffix("_dyn").unwrap_or(op);
        match base {
            "add" | "sub" | "mul" | "div" | "rem" | "shl" | "shr" | "and" | "or" | "xor" | "fmin"
            | "fmax" | "minimum_number_nsz" | "maximum_number_nsz" => {
                let op = match base {
                    "minimum_number_nsz" => "fmin",
                    "maximum_number_nsz" => "fmax",
                    o => o,
                };
                let (xs, ys) = (self.lanes(a(0), n), self.lanes(a(1), n));
                let mut out = Vec::new();
                for (x, y) in xs.into_iter().zip(ys) {
                    out.push(self.lane_bin(op, e, x, y)?);
                }
                Some(self.build_vec(ret, out))
            }
            "saturating_add" | "saturating_sub" => {
                let sym = if base == "saturating_add" { "add" } else { "sub" };
                let llvm = format!("llvm.{}{sym}.sat", if e.signed { "s" } else { "u" });
                let et = self.element_type(ret);
                let (xs, ys) = (self.lanes(a(0), n), self.lanes(a(1), n));
                let out = xs.into_iter().zip(ys).map(|(x, y)| self.intrinsic(&llvm, et, &[x, y])).collect();
                Some(self.build_vec(ret, out))
            }
            "neg" => {
                let xs = self.lanes(a(0), n);
                let out = xs.into_iter().map(|x| if e.float { self.fneg(x) } else { self.neg(x) }).collect();
                Some(self.build_vec(ret, out))
            }
            "fabs" | "fsqrt" | "floor" | "ceil" | "trunc" | "round_ties_even" | "ctpop" | "ctlz"
            | "cttz" | "bswap" | "bitreverse" | "round" => {
                let i = match base {
                    "fsqrt" => "llvm.sqrt".to_string(),
                    "round_ties_even" => "llvm.roundeven".to_string(),
                    o => format!("llvm.{o}"),
                };
                let et = self.element_type(ret);
                let xs = self.lanes(a(0), n);
                let out = xs.into_iter().map(|x| self.intrinsic(&i, et, &[x])).collect();
                Some(self.build_vec(ret, out))
            }
            "eq" | "ne" | "lt" | "le" | "gt" | "ge" => {
                let et = self.element_type(ret);
                let (xs, ys) = (self.lanes(a(0), n), self.lanes(a(1), n));
                let mut out = Vec::new();
                for (x, y) in xs.into_iter().zip(ys) {
                    let c = self.lane_cmp(base, e, x, y)?;
                    out.push(self.sext(c, et));
                }
                Some(self.build_vec(ret, out))
            }
            "select" => {
                let (ms, xs, ys) = (self.lanes(a(0), n), self.lanes(a(1), n), self.lanes(a(2), n));
                let mut out = Vec::new();
                for ((m, x), y) in ms.into_iter().zip(xs).zip(ys) {
                    let c = self.nonzero(m);
                    out.push(self.select(c, x, y));
                }
                Some(self.build_vec(ret, out))
            }
            "select_bitmask" => {
                let (rn, _) = self.elem_of(args[1].layout.ty);
                let m = a(0);
                let mt = self.val_ty(m);
                let (xs, ys) = (self.lanes(a(1), rn), self.lanes(a(2), rn));
                let mut out = Vec::new();
                for (i, (x, y)) in xs.into_iter().zip(ys).enumerate() {
                    let sh = self.const_uint(mt, i as u64);
                    let b = self.lshr(m, sh);
                    let one = self.const_uint(mt, 1);
                    let b = self.and(b, one);
                    let c = self.nonzero(b);
                    out.push(self.select(c, x, y));
                }
                Some(self.build_vec(ret, out))
            }
            "gather" => {
                // Masked-off lanes load their fallback value from a stack slot,
                // so every lane can load unconditionally.
                let (_, ety) = ret_rty.simd_size_and_type(self.tcx);
                let el = self.layout_of(ety);
                let (size, align) = (el.size, el.align.abi);
                let et = self.element_type(ret);
                let slot = self.alloca(size * n, align);
                let (vs, ps, ms) = (self.lanes(a(0), n), self.lanes(a(1), n), self.lanes(a(2), n));
                let mut out = Vec::new();
                for (i, ((v, p), m)) in vs.into_iter().zip(ps).zip(ms).enumerate() {
                    let off = self.const_usize(size.bytes() * i as u64);
                    let fb = self.inbounds_ptradd(slot, off);
                    self.store(v, fb, align);
                    let c = self.nonzero(m);
                    let p = self.select(c, p, fb);
                    out.push(self.load(et, p, align));
                }
                Some(self.build_vec(ret, out))
            }
            "fma" | "relaxed_fma" => {
                let et = self.element_type(ret);
                let (xs, ys, zs) = (self.lanes(a(0), n), self.lanes(a(1), n), self.lanes(a(2), n));
                let out = xs
                    .into_iter()
                    .zip(ys)
                    .zip(zs)
                    .map(|((x, y), z)| self.intrinsic("llvm.fma", et, &[x, y, z]))
                    .collect();
                Some(self.build_vec(ret, out))
            }
            "extract" => Some(self.extract_element(a(0), a(1))),
            "insert" => {
                let (v, idx, x) = (a(0), a(1), a(2));
                Some(self.mk(|c| InsertElementOp::new(c, v, x, idx)))
            }
            "shuffle" => {
                let idx = a(2);
                let Some(ConstVal::Agg(ids)) = self.cval(idx) else { return None };
                let (xs, ys) = (self.lanes(a(0), n), self.lanes(a(1), n));
                let mut out = Vec::new();
                for i in ids {
                    let k = self.const_to_opt_u128(i, false)? as u64;
                    out.push(if k < n { xs[k as usize] } else { ys[(k - n) as usize] });
                }
                Some(self.build_vec(ret, out))
            }
            "bitmask" => {
                if self.type_kind(ret) != TypeKind::Integer {
                    return None;
                }
                let xs = self.lanes(a(0), n);
                let mut acc = self.const_null(ret);
                for (i, x) in xs.into_iter().enumerate() {
                    let lt = self.val_ty(x);
                    let w = self.int_width(lt);
                    let sh = self.const_uint(lt, w - 1);
                    let b = self.lshr(x, sh);
                    let b = self.intcast(b, ret, false);
                    let s = self.const_uint(ret, i as u64);
                    let b = self.shl(b, s);
                    acc = self.or(acc, b);
                }
                Some(acc)
            }
            "reduce_any" | "reduce_all" => {
                let xs = self.lanes(a(0), n);
                let mut acc = self.const_bool(base == "reduce_all");
                for x in xs {
                    let c = self.nonzero(x);
                    acc = if base == "reduce_all" { self.and(acc, c) } else { self.or(acc, c) };
                }
                Some(acc)
            }
            _ if base.starts_with("reduce_") => {
                let r = base.strip_prefix("reduce_").unwrap();
                let (r, ordered) = match r.strip_suffix("_ordered") {
                    Some(r) => (r, true),
                    None => (r.strip_suffix("_unordered").unwrap_or(r), false),
                };
                let xs = self.lanes(a(0), n);
                let mut it = xs.into_iter();
                let mut acc = if ordered { a(1) } else { it.next()? };
                for x in it {
                    acc = self.lane_bin(r, e, acc, x)?;
                }
                Some(acc)
            }
            "cast" | "as" => {
                let (_, to) = self.elem_of(ret_rty);
                let et = self.element_type(ret);
                let xs = self.lanes(a(0), n);
                let out = xs.into_iter().map(|x| self.lane_cast(x, e, to, et)).collect();
                Some(self.build_vec(ret, out))
            }
            _ => None,
        }
    }

    /// Lane-wise emulation of the `llvm.x86.*` SIMD intrinsics that
    /// runtime-dispatching crates (aho-corasick, half, ...) actually reach.
    pub fn llvm_x86_intrinsic(
        &mut self,
        name: &str,
        args: &[OperandRef<'tcx, Value>],
        ret_rty: Ty<'tcx>,
        ret: TypeHandle,
    ) -> Option<Value> {
        use rustc_codegen_ssa::common::RealPredicate::*;
        let a = |i: usize| args[i].immediate();
        match name {
            "llvm.x86.ssse3.pshuf.b.128" | "llvm.x86.avx2.pshuf.b" => {
                // r[i] = b[i] & 0x80 ? 0 : a[16 * (i / 16) + (b[i] & 15)]
                let (n, _) = self.elem_of(ret_rty);
                let one = rustc_abi::Align::ONE;
                let slot = self.alloca(rustc_abi::Size::from_bytes(n), one);
                self.store(a(0), slot, one);
                let (i8t, isz) = (self.type_i8(), self.type_isize());
                let zero = self.const_u8(0);
                let bs = self.lanes(a(1), n);
                let mut out = Vec::new();
                for (i, b) in bs.into_iter().enumerate() {
                    let m = self.const_u8(0x0f);
                    let idx = self.and(b, m);
                    let idx = self.zext(idx, isz);
                    let base = self.const_usize(i as u64 / 16 * 16);
                    let off = self.add(idx, base);
                    let p = self.inbounds_ptradd(slot, off);
                    let v = self.load(i8t, p, one);
                    let hb = self.const_u8(0x80);
                    let h = self.and(b, hb);
                    let c = self.nonzero(h);
                    out.push(self.select(c, zero, v));
                }
                Some(self.build_vec(ret, out))
            }
            "llvm.x86.sse.cmp.ps" | "llvm.x86.sse2.cmp.pd" => {
                // Predicates 16..31 only differ in signaling, so the low 4 bits suffice.
                let imm = self.const_to_opt_u128(a(2), false)? as u8 & 0xf;
                let pred = [
                    RealOEQ, RealOLT, RealOLE, RealUNO, RealUNE, RealUGE, RealUGT, RealORD, RealUEQ, RealULT,
                    RealULE, RealPredicateFalse, RealONE, RealOGE, RealOGT, RealPredicateTrue,
                ][imm as usize];
                let (n, _) = self.elem_of(ret_rty);
                let et = self.element_type(ret);
                let it = self.type_ix(if name.ends_with(".ps") { 32 } else { 64 });
                let (xs, ys) = (self.lanes(a(0), n), self.lanes(a(1), n));
                let mut out = Vec::new();
                for (x, y) in xs.into_iter().zip(ys) {
                    let m = match pred {
                        RealPredicateFalse => self.const_int(it, 0),
                        RealPredicateTrue => self.const_int(it, -1),
                        p => {
                            let c = self.fcmp(p, x, y);
                            self.sext(c, it)
                        }
                    };
                    out.push(self.bitcast(m, et));
                }
                Some(self.build_vec(ret, out))
            }
            "llvm.x86.vcvtps2ph.128" => {
                // <4 x f32> -> low 4 lanes of <8 x i16>; callers use round-to-nearest.
                let (f16, i16t) = (self.type_f16(), self.type_i16());
                let xs = self.lanes(a(0), 4);
                let mut out = Vec::new();
                for x in xs {
                    let h = self.fptrunc(x, f16);
                    out.push(self.bitcast(h, i16t));
                }
                for _ in 0..4 {
                    out.push(self.const_i16(0));
                }
                Some(self.build_vec(ret, out))
            }
            _ => None,
        }
    }
}
