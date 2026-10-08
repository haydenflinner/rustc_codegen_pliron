//! `asm!` / `global_asm!` support.
//!
//! Inline asm uses the same approach as rustc_codegen_cranelift (whose
//! register/stack-slot allocator and wrapper generator are adapted here,
//! MIT/Apache-2.0): every `asm!` becomes an out-of-line function that loads
//! its inputs from a stack buffer, runs the template and stores the outputs
//! back. The module's asm text is assembled by the system assembler and merged
//! into the CGU object (see `lib.rs`).

use std::fmt::Write;

use pliron::basic_block::BasicBlock;
use pliron::context::Ptr;
use rustc_abi::{Align, Size};
use rustc_ast::{InlineAsmOptions, InlineAsmTemplatePiece};
use rustc_codegen_ssa::MemFlags;
use rustc_codegen_ssa::traits::*;
use rustc_data_structures::fx::FxHashMap;
use rustc_hir::def_id::DefId;
use rustc_middle::mir::interpret::Scalar as ConstScalar;
use rustc_middle::ty::layout::LayoutOf;
use rustc_middle::ty::{Instance, Ty, TyCtxt};
use rustc_span::{Span, sym};
use rustc_target::asm::*;

use crate::builder::Builder;
use crate::context::{CodegenCx, ConstVal};

enum AOp {
    In {
        reg: InlineAsmRegOrRegClass,
    },
    Out {
        reg: InlineAsmRegOrRegClass,
        late: bool,
        has_place: bool,
    },
    InOut {
        reg: InlineAsmRegOrRegClass,
        has_out: bool,
    },
    Text(String),
}

impl<'tcx> CodegenCx<'tcx> {
    fn asm_const(&self, value: ConstScalar, ty: Ty<'tcx>, span: Span) -> String {
        match value {
            ConstScalar::Int(int) => {
                rustc_codegen_ssa::common::asm_const_to_str(self.tcx, span, int, self.layout_of(ty))
            }
            ConstScalar::Ptr(ptr, _) => {
                let (prov, off) = ptr.prov_and_relative_offset();
                let alloc = self.tcx.global_alloc(prov.alloc_id());
                let v = self.alloc_to_backend(alloc).ok();
                let Some(ConstVal::Sym { sym, off: o }) = v.and_then(|v| self.cval(v)) else {
                    self.tcx
                        .dcx()
                        .span_fatal(span, "unsupported asm symbol operand")
                };
                let off = o + off.bytes() as i64;
                if off != 0 {
                    format!("{sym}{off:+}")
                } else {
                    sym
                }
            }
        }
    }

    fn asm_tls_sym(&self, def_id: DefId) -> String {
        self.tcx
            .symbol_name(Instance::mono(self.tcx, def_id))
            .name
            .to_string()
    }

    pub fn push_global_asm(
        &self,
        template: &[InlineAsmTemplatePiece],
        operands: &[rustc_codegen_ssa::traits::GlobalAsmOperandRef<'tcx>],
        options: InlineAsmOptions,
        line_spans: &[Span],
    ) {
        let is_x86 = matches!(
            self.tcx.sess.asm_arch,
            Some(InlineAsmArch::X86 | InlineAsmArch::X86_64)
        );
        let intel = is_x86 && !options.contains(InlineAsmOptions::ATT_SYNTAX);
        let mut s = String::new();
        if intel {
            s.push_str(".intel_syntax noprefix\n");
        }
        for piece in template {
            match piece {
                InlineAsmTemplatePiece::String(t) => s.push_str(t),
                InlineAsmTemplatePiece::Placeholder {
                    operand_idx, span, ..
                } => match operands[*operand_idx] {
                    GlobalAsmOperandRef::Const { value, ty } => {
                        s.push_str(&self.asm_const(value, ty, *span))
                    }
                    GlobalAsmOperandRef::SymThreadLocalStatic { def_id } => {
                        s.push_str(&self.asm_tls_sym(def_id))
                    }
                },
            }
        }
        s.push('\n');
        if intel {
            s.push_str(".att_syntax\n");
        }
        let _ = line_spans;
        self.st.borrow_mut().asm.push_str(&s);
    }
}

impl<'tcx> CodegenCx<'tcx> {
    /// `link_name = "llvm.*"` intrinsics used by `core::arch`/`std_detect`.
    /// A few are implemented in assembly; the rest become weak stubs that
    /// trap if they're ever executed, so crates still build.
    pub fn llvm_intrinsic_stub(&self, name: &str) -> String {
        let sym: String = format!(
            "__pliron_{}",
            name.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>()
        );
        let mut st = self.st.borrow_mut();
        if !st.llvm_stubs.insert(sym.clone()) {
            return sym;
        }
        let x86 = matches!(self.tcx.sess.asm_arch, Some(InlineAsmArch::X86_64));
        let body = match name {
            "llvm.x86.xgetbv" if x86 => {
                "    mov ecx, edi\n    xgetbv\n    shl rdx, 32\n    or rax, rdx\n    ret\n"
            }
            "llvm.x86.rdtsc" if x86 => "    rdtsc\n    shl rdx, 32\n    or rax, rdx\n    ret\n",
            "llvm.x86.sse2.pause" if x86 => "    pause\n    ret\n",
            "llvm.x86.avx.vzeroupper" if x86 => "    vzeroupper\n    ret\n",
            "llvm.x86.avx.vzeroall" if x86 => "    vzeroall\n    ret\n",
            "llvm.x86.sse2.lfence" if x86 => "    lfence\n    ret\n",
            "llvm.x86.sse2.mfence" if x86 => "    mfence\n    ret\n",
            "llvm.x86.sse.sfence" if x86 => "    sfence\n    ret\n",
            _ if x86 => "    ud2\n",
            _ => "    brk #0x1\n",
        };
        let syntax = if x86 { ".intel_syntax noprefix\n" } else { "" };
        let back = if x86 { ".att_syntax\n" } else { "" };
        write!(
            st.asm,
            ".section .text.{sym},\"ax\",@progbits\n.weak {sym}\n.hidden {sym}\n.type {sym},@function\n{sym}:\n{syntax}{body}{back}.size {sym}, .-{sym}\n.text\n"
        )
        .unwrap();
        sym
    }
}

struct Gen<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    arch: InlineAsmArch,
    def_id: DefId,
    template: &'a [InlineAsmTemplatePiece],
    ops: &'a [AOp],
    options: InlineAsmOptions,
    regs: Vec<Option<InlineAsmReg>>,
    slots_clobber: Vec<Option<Size>>,
    slots_in: Vec<Option<Size>>,
    slots_out: Vec<Option<Size>>,
    slot_size: Size,
}

impl Gen<'_, '_> {
    fn allocate_registers(&mut self) {
        let sess = self.tcx.sess;
        let map = allocatable_registers(
            self.arch,
            sess.relocation_model(),
            self.tcx.asm_target_features(self.def_id),
            &sess.target,
        );
        let mut allocated = FxHashMap::<InlineAsmReg, (bool, bool)>::default();
        let mut regs = vec![None; self.ops.len()];
        for (i, op) in self.ops.iter().enumerate() {
            match *op {
                AOp::In {
                    reg: InlineAsmRegOrRegClass::Reg(reg),
                } => {
                    regs[i] = Some(reg);
                    allocated.entry(reg).or_default().0 = true;
                }
                AOp::Out {
                    reg: InlineAsmRegOrRegClass::Reg(reg),
                    late: true,
                    ..
                } => {
                    regs[i] = Some(reg);
                    allocated.entry(reg).or_default().1 = true;
                }
                AOp::Out {
                    reg: InlineAsmRegOrRegClass::Reg(reg),
                    ..
                }
                | AOp::InOut {
                    reg: InlineAsmRegOrRegClass::Reg(reg),
                    ..
                } => {
                    regs[i] = Some(reg);
                    allocated.insert(reg, (true, true));
                }
                _ => {}
            }
        }
        let pick = |allocated: &FxHashMap<InlineAsmReg, (bool, bool)>,
                    class: InlineAsmRegClass,
                    busy: fn((bool, bool)) -> bool| {
            map[&class]
                .iter()
                .copied()
                .find(|reg| {
                    let mut used = false;
                    reg.overlapping_regs(|r| {
                        if allocated.get(&r).copied().is_some_and(busy) {
                            used = true;
                        }
                    });
                    !used
                })
                .expect("cannot allocate asm registers")
        };
        for (i, op) in self.ops.iter().enumerate() {
            if let AOp::Out {
                reg: InlineAsmRegOrRegClass::RegClass(c),
                late: false,
                ..
            }
            | AOp::InOut {
                reg: InlineAsmRegOrRegClass::RegClass(c),
                ..
            } = *op
            {
                let r = pick(&allocated, c, |_| true);
                regs[i] = Some(r);
                allocated.insert(r, (true, true));
            }
        }
        for (i, op) in self.ops.iter().enumerate() {
            match *op {
                AOp::In {
                    reg: InlineAsmRegOrRegClass::RegClass(c),
                } => {
                    let r = pick(&allocated, c, |u| u.0);
                    regs[i] = Some(r);
                    allocated.entry(r).or_default().0 = true;
                }
                AOp::Out {
                    reg: InlineAsmRegOrRegClass::RegClass(c),
                    late: true,
                    ..
                } => {
                    let r = pick(&allocated, c, |u| u.1);
                    regs[i] = Some(r);
                    allocated.entry(r).or_default().1 = true;
                }
                _ => {}
            }
        }
        self.regs = regs;
    }

    fn allocate_stack_slots(&mut self) {
        let arch = self.arch;
        let mut size = Size::ZERO;
        let n = self.ops.len();
        let (mut clob, mut sin, mut sout) = (vec![None; n], vec![None; n], vec![None; n]);
        let new_slot = |size: &mut Size, class: InlineAsmRegClass| {
            let bytes = class
                .supported_types(arch, true)
                .iter()
                .map(|(ty, _)| ty.size())
                .filter_map(InlineAsmSize::fixed_size_bytes)
                .max()
                .expect("expected fixed-size type");
            let off = size.align_to(Align::from_bytes(bytes).unwrap());
            *size = off + Size::from_bytes(bytes);
            off
        };
        let abi_clobber = InlineAsmClobberAbi::parse(
            arch,
            &self.tcx.sess.target,
            &self.tcx.sess.internal_target_features,
            sym::C,
        )
        .unwrap()
        .clobbered_regs();
        for (i, reg) in self
            .regs
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.map(|r| (i, r)))
        {
            let mut need_save = true;
            for r in abi_clobber {
                r.overlapping_regs(|r| {
                    if r == reg {
                        need_save = false;
                    }
                });
                if !need_save {
                    break;
                }
            }
            if need_save {
                clob[i] = Some(new_slot(&mut size, reg.reg_class()));
            }
        }
        for (i, op) in self.ops.iter().enumerate() {
            if let AOp::InOut { reg, has_out: true } = *op {
                let s = new_slot(&mut size, reg.reg_class());
                sin[i] = Some(s);
                sout[i] = Some(s);
            }
        }
        let before_in = size;
        for (i, op) in self.ops.iter().enumerate() {
            if let AOp::In { reg }
            | AOp::InOut {
                reg,
                has_out: false,
            } = *op
            {
                sin[i] = Some(new_slot(&mut size, reg.reg_class()));
            }
        }
        let after_in = size;
        size = before_in;
        for (i, op) in self.ops.iter().enumerate() {
            if let AOp::Out {
                reg,
                has_place: true,
                ..
            } = *op
            {
                sout[i] = Some(new_slot(&mut size, reg.reg_class()));
            }
        }
        self.slot_size = size.max(after_in);
        self.slots_clobber = clob;
        self.slots_in = sin;
        self.slots_out = sout;
    }

    fn is_vreg(reg: InlineAsmReg) -> bool {
        matches!(reg, InlineAsmReg::X86(r) if matches!(r.reg_class(),
            X86InlineAsmRegClass::xmm_reg | X86InlineAsmRegClass::ymm_reg | X86InlineAsmRegClass::zmm_reg))
    }

    fn save(&self, s: &mut String, reg: InlineAsmReg, off: Size) {
        match self.arch {
            InlineAsmArch::X86_64 => {
                if Self::is_vreg(reg) {
                    let name = reg.name();
                    let mov = if name.starts_with("xmm") {
                        "movups"
                    } else {
                        "vmovups"
                    };
                    writeln!(s, "    {mov} [rbx+0x{:x}], {name}", off.bytes()).unwrap();
                } else {
                    write!(s, "    mov [rbx+0x{:x}], ", off.bytes()).unwrap();
                    reg.emit(s, self.arch, None).unwrap();
                    s.push('\n');
                }
            }
            InlineAsmArch::AArch64 => {
                s.push_str("    str ");
                match reg {
                    InlineAsmReg::AArch64(r) if r.vreg_index().is_some() => {
                        reg.emit(s, self.arch, Some('q'))
                    }
                    _ => reg.emit(s, self.arch, None),
                }
                .unwrap();
                writeln!(s, ", [x19, 0x{:x}]", off.bytes()).unwrap();
            }
            a => unimplemented!("inline asm on {a:?}"),
        }
    }

    fn restore(&self, s: &mut String, reg: InlineAsmReg, off: Size) {
        match self.arch {
            InlineAsmArch::X86_64 => {
                if Self::is_vreg(reg) {
                    let name = reg.name();
                    let mov = if name.starts_with("xmm") {
                        "movups"
                    } else {
                        "vmovups"
                    };
                    write!(s, "    {mov} {name}").unwrap();
                } else {
                    s.push_str("    mov ");
                    reg.emit(s, self.arch, None).unwrap();
                }
                writeln!(s, ", [rbx+0x{:x}]", off.bytes()).unwrap();
            }
            InlineAsmArch::AArch64 => {
                s.push_str("    ldr ");
                match reg {
                    InlineAsmReg::AArch64(r) if r.vreg_index().is_some() => {
                        reg.emit(s, self.arch, Some('q'))
                    }
                    _ => reg.emit(s, self.arch, None),
                }
                .unwrap();
                writeln!(s, ", [x19, 0x{:x}]", off.bytes()).unwrap();
            }
            a => unimplemented!("inline asm on {a:?}"),
        }
    }

    fn wrapper(&self, name: &str) -> String {
        let mut s = String::new();
        writeln!(s, ".globl {name}\n.hidden {name}\n.type {name},@function").unwrap();
        writeln!(s, ".section .text.{name},\"ax\",@progbits\n{name}:").unwrap();
        let x86 = self.arch == InlineAsmArch::X86_64;
        let att = self.options.contains(InlineAsmOptions::ATT_SYNTAX);
        let noreturn = self.options.contains(InlineAsmOptions::NORETURN);
        if x86 {
            s.push_str(".intel_syntax noprefix\n    push rbp\n    mov rbp,rsp\n    push rbx\n    mov rbx,rdi\n");
        } else {
            s.push_str("    stp fp, lr, [sp, #-32]!\n    mov fp, sp\n    str x19, [sp, #24]\n    mov x19, x0\n");
        }
        let pairs = |slots: &[Option<Size>]| -> Vec<(InlineAsmReg, Size)> {
            self.regs
                .iter()
                .zip(slots.iter().copied())
                .filter_map(|(r, s)| r.zip(s))
                .collect()
        };
        if !noreturn {
            for (r, o) in pairs(&self.slots_clobber) {
                self.save(&mut s, r, o);
            }
        }
        for (r, o) in pairs(&self.slots_in) {
            self.restore(&mut s, r, o);
        }
        if x86 && att {
            s.push_str(".att_syntax\n");
        }
        for piece in self.template {
            match piece {
                InlineAsmTemplatePiece::String(t) => s.push_str(t),
                InlineAsmTemplatePiece::Placeholder {
                    operand_idx,
                    modifier,
                    ..
                } => match &self.ops[*operand_idx] {
                    AOp::Text(t) => s.push_str(t),
                    _ => {
                        if att {
                            s.push('%');
                        }
                        let reg = self.regs[*operand_idx].unwrap();
                        if x86 && Self::is_vreg(reg) {
                            let name = reg.name();
                            match modifier {
                                Some(p) => write!(s, "{p}mm{}", &name[3..]).unwrap(),
                                None => s.push_str(&name),
                            }
                        } else {
                            reg.emit(&mut s, self.arch, *modifier).unwrap();
                        }
                    }
                },
            }
        }
        s.push('\n');
        if x86 && att {
            s.push_str(".intel_syntax noprefix\n");
        }
        if !noreturn {
            for (r, o) in pairs(&self.slots_out) {
                self.save(&mut s, r, o);
            }
            for (r, o) in pairs(&self.slots_clobber) {
                self.restore(&mut s, r, o);
            }
            if x86 {
                s.push_str("    pop rbx\n    pop rbp\n    ret\n");
            } else {
                s.push_str("    ldr x19, [sp, #24]\n    ldp fp, lr, [sp], #32\n    ret\n");
            }
        } else if x86 {
            s.push_str("    ud2\n");
        } else {
            s.push_str("    brk #0x1\n");
        }
        if x86 {
            s.push_str(".att_syntax\n");
        }
        writeln!(s, ".size {name}, .-{name}\n.text\n").unwrap();
        s
    }
}

impl<'a, 'tcx> Builder<'a, 'tcx> {
    pub fn inline_asm(
        &mut self,
        template: &[InlineAsmTemplatePiece],
        operands: &[InlineAsmOperandRef<'tcx, Self>],
        options: InlineAsmOptions,
        line_spans: &[Span],
        instance: Instance<'_>,
        dest: Option<Ptr<BasicBlock>>,
    ) {
        let span = line_spans.first().copied().unwrap_or(rustc_span::DUMMY_SP);
        let arch = match self.tcx.sess.asm_arch {
            Some(a @ (InlineAsmArch::X86_64 | InlineAsmArch::AArch64)) => a,
            a => self.tcx.dcx().span_fatal(
                span,
                format!("inline asm on {a:?} is not supported by the pliron backend"),
            ),
        };
        let ops: Vec<AOp> = operands
            .iter()
            .map(|o| match o {
                InlineAsmOperandRef::In { reg, .. } => AOp::In { reg: *reg },
                InlineAsmOperandRef::Out { reg, late, place } => AOp::Out {
                    reg: *reg,
                    late: *late,
                    has_place: place.is_some(),
                },
                InlineAsmOperandRef::InOut { reg, out_place, .. } => AOp::InOut {
                    reg: *reg,
                    has_out: out_place.is_some(),
                },
                InlineAsmOperandRef::Const { value, ty } => {
                    AOp::Text(self.cx.asm_const(*value, *ty, span))
                }
                InlineAsmOperandRef::SymThreadLocalStatic { def_id } => {
                    AOp::Text(self.cx.asm_tls_sym(*def_id))
                }
                InlineAsmOperandRef::Label { .. } => self
                    .tcx
                    .dcx()
                    .span_fatal(span, "asm goto is not supported by the pliron backend"),
            })
            .collect();
        let mut g = Gen {
            tcx: self.tcx,
            arch,
            def_id: instance.def_id(),
            template,
            ops: &ops,
            options,
            regs: vec![],
            slots_clobber: vec![],
            slots_in: vec![],
            slots_out: vec![],
            slot_size: Size::ZERO,
        };
        g.allocate_registers();
        g.allocate_stack_slots();
        let name = {
            let mut st = self.st.borrow_mut();
            st.counter += 1;
            let cgu: String = st
                .cgu
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format!(
                "__pliron_asm_{:x}_{cgu}_{}",
                self.tcx
                    .stable_crate_id(rustc_hir::def_id::LOCAL_CRATE)
                    .as_u64(),
                st.counter
            )
        };
        let text = g.wrapper(&name);
        self.st.borrow_mut().asm.push_str(&text);

        let size = Size::from_bytes(g.slot_size.bytes().next_multiple_of(16).max(16));
        let a16 = Align::from_bytes(16).unwrap();
        let slot = self.alloca(size, a16);
        for (i, o) in operands.iter().enumerate() {
            let v = match o {
                InlineAsmOperandRef::In { value, .. } => value.immediate(),
                InlineAsmOperandRef::InOut { in_value, .. } => in_value.immediate(),
                _ => continue,
            };
            let off = self.const_usize(g.slots_in[i].unwrap().bytes());
            let p = self.inbounds_ptradd(slot, off);
            self.store(v, p, Align::ONE);
        }
        let void = self.type_void();
        self.call_sym(&name, void, &[slot]);
        if options.contains(InlineAsmOptions::NORETURN) {
            self.unreachable();
            return;
        }
        for (i, o) in operands.iter().enumerate() {
            let place = match o {
                InlineAsmOperandRef::Out { place: Some(p), .. } => p,
                InlineAsmOperandRef::InOut {
                    out_place: Some(p), ..
                } => p,
                _ => continue,
            };
            let off = self.const_usize(g.slots_out[i].unwrap().bytes());
            let src = self.inbounds_ptradd(slot, off);
            let n = self.const_usize(place.layout.size.bytes());
            self.memcpy(
                place.val.llval,
                place.val.align,
                src,
                Align::ONE,
                n,
                MemFlags::empty(),
                None,
            );
        }
        if let Some(d) = dest {
            self.br(d);
        }
    }
}
