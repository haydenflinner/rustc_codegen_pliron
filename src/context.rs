//! Per-CGU codegen context: owns the pliron `Context` and the side tables the
//! Cranelift lowering needs (constants, symbols, linkage).

use std::cell::RefCell;

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::SingleBlockRegionInterface;
use pliron::builtin::ops::ModuleOp;
use pliron::builtin::types::{IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed, TypedHandle};
use pliron::value::Value;
use pliron_llvm::ops::{FuncOp, GlobalOp, UndefOp};
use pliron_llvm::types::FuncType;
use rustc_abi::{HasDataLayout, TargetDataLayout, VariantIdx};
use rustc_attr_ir::Linkage as RLinkage;
use rustc_codegen_ssa::traits::*;
use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::ConstAllocation;
use rustc_middle::mono::Visibility;
use rustc_middle::ty::layout::{
    FnAbiError, FnAbiOfHelpers, FnAbiRequest, HasTyCtxt, HasTypingEnv, LayoutError,
    LayoutOfHelpers, TyAndLayout,
};
use rustc_middle::ty::{self, ExistentialTraitRef, Instance, Ty, TyCtxt};
use rustc_session::Session;
use rustc_span::Span;
use rustc_target::callconv::FnAbi;
use rustc_target::spec::{HasTargetSpec, Target};

use crate::types::{TyK, classify};

pub use pliron_ir_opt::state::*;

pub struct CodegenCx<'tcx> {
    pub tcx: TyCtxt<'tcx>,
    pub pctx: RefCell<Context>,
    pub module: Ptr<Operation>,
    pub st: RefCell<State>,
    /// `static_addr_of` memoization — rustc-side because the key is an
    /// interned `ConstAllocation`.
    pub const_allocs: RefCell<FxHashMap<ConstAllocation<'tcx>, Value>>,
    pub vtables: RefCell<FxHashMap<(Ty<'tcx>, Option<ExistentialTraitRef<'tcx>>), Value>>,
    pub tcache: RefCell<FxHashMap<(Ty<'tcx>, Option<VariantIdx>), TypeHandle>>,
    pub scache: RefCell<FxHashMap<Ty<'tcx>, TypeHandle>>,
    /// CGU symbol this codegen context is building (set at construction;
    /// read-only afterwards — tracing uses it without borrowing `st`).
    pub cgu_name: String,
}

pub fn map_linkage(l: RLinkage, vis: Visibility) -> Linkage {
    match l {
        RLinkage::Internal => Linkage::Local,
        RLinkage::WeakAny | RLinkage::WeakODR | RLinkage::LinkOnceAny | RLinkage::LinkOnceODR => {
            Linkage::Preemptible
        }
        RLinkage::ExternalWeak => Linkage::Import,
        _ => match vis {
            Visibility::Hidden => Linkage::Hidden,
            _ => Linkage::Export,
        },
    }
}

impl<'tcx> CodegenCx<'tcx> {
    pub fn new(tcx: TyCtxt<'tcx>, name: &str) -> Self {
        let mut ctx = Context::new();
        let mname: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let module = ModuleOp::new(&mut ctx, Identifier::try_new(format!("m_{mname}")).unwrap())
            .get_operation();
        CodegenCx {
            tcx,
            cgu_name: name.to_string(),
            pctx: RefCell::new(ctx),
            module,
            st: RefCell::new(State {
                eh_personality: rustc_symbol_mangling::eh_personality_symbol(tcx),
                ..State::default()
            }),
            const_allocs: Default::default(),
            vtables: Default::default(),
            tcache: Default::default(),
            scache: Default::default(),
        }
    }

    pub fn new_value(&self, ty: TypeHandle, cv: ConstVal) -> Value {
        let v = {
            let mut ctx = self.pctx.borrow_mut();
            let op = UndefOp::new(&mut ctx, ty).get_operation();
            op.deref(&ctx).get_result(&ctx, 0)
        };
        self.st.borrow_mut().consts.insert(v, cv);
        v
    }

    pub fn cval(&self, v: Value) -> Option<ConstVal> {
        self.st.borrow().consts.get(v).cloned()
    }

    pub fn ty_of(&self, v: Value) -> TypeHandle {
        v.get_type(&self.pctx.borrow())
    }

    pub fn kind(&self, ty: TypeHandle) -> TyK {
        classify(&self.pctx.borrow(), ty)
    }

    pub fn int_ty(&self, w: u32) -> TypeHandle {
        IntegerType::get(&mut self.pctx.borrow_mut(), w, Signedness::Signless).into()
    }

    pub fn ident(&self, sym: &str) -> Identifier {
        let mut st = self.st.borrow_mut();
        if let Some(i) = st.sym_to_ident.get(sym) {
            return i.clone();
        }
        st.counter += 1;
        let mut s = format!("s{}_", st.counter);
        s.extend(sym.chars().map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        }));
        let id = Identifier::try_new(s.clone()).unwrap();
        st.sym_to_ident.insert(sym.to_string(), id.clone());
        st.ident_to_sym.insert(s, sym.to_string());
        id
    }

    fn append_to_module(&self, ctx: &mut Context, op: Ptr<Operation>) {
        let m = Operation::get_op::<ModuleOp>(self.module, ctx).unwrap();
        m.append_operation(ctx, op, 0);
    }

    pub fn declare_fn_sym(
        &self,
        sym: &str,
        fn_ty: TypeHandle,
        linkage: Linkage,
        exts: Exts,
    ) -> Ptr<Operation> {
        if let Some(f) = self.st.borrow_mut().funcs.get_mut(sym) {
            if linkage != Linkage::Import {
                f.linkage = linkage;
                f.exts = exts;
                f.ty = fn_ty;
            }
            return f.op;
        }
        let id = self.ident(sym);
        let op = {
            let mut ctx = self.pctx.borrow_mut();
            let fty = TypedHandle::<FuncType>::from_handle(fn_ty, &ctx).unwrap();
            let op = FuncOp::new(&mut ctx, id, fty).get_operation();
            self.append_to_module(&mut ctx, op);
            op
        };
        self.st.borrow_mut().funcs.insert(
            sym.to_string(),
            FuncInfo {
                op,
                ty: fn_ty,
                linkage,
                exts,
                no_inline: false,
                always_inline: false,
                inline_hint: false,
                cold: false,
                nounwind: false,
                odr: false,
                sret_ty: None,
                frozen: Vec::new(),
                noalias: Vec::new(),
                nonnull: Vec::new(),
                nparams: 0,
                tail_cc: false,
            },
        );
        op
    }

    pub fn declare_global(&self, sym: &str, info: GlobalInfo) {
        if let Some(g) = self.st.borrow_mut().globals.get_mut(sym) {
            if info.linkage != Linkage::Import {
                *g = info;
            }
            return;
        }
        let id = self.ident(sym);
        {
            let mut ctx = self.pctx.borrow_mut();
            let op = GlobalOp::new(&mut ctx, id, info.ty).get_operation();
            self.append_to_module(&mut ctx, op);
        }
        self.st.borrow_mut().globals.insert(sym.to_string(), info);
    }

    pub fn sym_addr(&self, sym: &str) -> Value {
        let p = self.type_ptr();
        self.new_value(
            p,
            ConstVal::Sym {
                sym: sym.to_string(),
                off: 0,
            },
        )
    }

    pub fn fn_sym(&self, f: Ptr<Operation>) -> String {
        let st = self.st.borrow();
        st.funcs
            .iter()
            .find(|(_, i)| i.op == f)
            .map(|(n, _)| n.clone())
            .unwrap()
    }

    pub fn private_global(&self, init: Value, align: u64, mutable: bool) -> Value {
        let n = {
            let mut st = self.st.borrow_mut();
            st.counter += 1;
            st.counter
        };
        let sym = format!("__rcg_alloc.{n}");
        let ty = self.ty_of(init);
        self.declare_global(
            &sym,
            GlobalInfo {
                ty,
                init: Some(init),
                align,
                mutable,
                tls: false,
                linkage: Linkage::Local,
                used: false,
                section: None,
            },
        );
        self.sym_addr(&sym)
    }

    fn mark_attrs(&self, sym: &str, instance: Instance<'tcx>, fn_abi: &FnAbi<'tcx, Ty<'tcx>>) {
        use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags as F;
        let cold = self
            .tcx
            .codegen_instance_attrs(instance.def)
            .flags
            .contains(F::COLD);
        if let Some(f) = self.st.borrow_mut().funcs.get_mut(sym) {
            f.cold |= cold;
            f.nounwind |= !fn_abi.can_unwind;
            f.tail_cc |= fn_abi.conv == rustc_abi::CanonAbi::RustTail;
            (f.frozen, f.noalias, f.nonnull, f.nparams) = frozen_params(fn_abi);
        }
        if let rustc_target::callconv::PassMode::Indirect { .. } = fn_abi.ret.mode {
            let t = self.backend_type(fn_abi.ret.layout);
            if let Some(f) = self.st.borrow_mut().funcs.get_mut(sym) {
                f.sret_ty = Some(t);
            }
        }
    }

    pub fn fn_sig(&self, fn_abi: &FnAbi<'tcx, Ty<'tcx>>) -> (TypeHandle, Exts) {
        (
            self.fn_decl_backend_type(fn_abi),
            crate::type_of::exts_of(fn_abi),
        )
    }

    pub fn get_static_addr(&self, def_id: rustc_hir::def_id::DefId) -> Value {
        let instance = Instance::mono(self.tcx, def_id);
        let sym = self.tcx.symbol_name(instance).name;
        if let Some(il) = self.tcx.codegen_fn_attrs(def_id).import_linkage {
            // Like LLVM/cg_clif: `#[linkage]` on a foreign static means the static
            // holds the (possibly null) address of `sym`, so emit a local pointer
            // global initialized with it.
            use rustc_attr_ir::Linkage as L;
            let weak = matches!(il, L::ExternalWeak | L::WeakAny);
            let r = format!(
                "_rust_extern_with_linkage_{:016x}_{sym}",
                self.tcx.stable_crate_id(rustc_hir::def_id::LOCAL_CRATE)
            );
            if !self.st.borrow().globals.contains_key(&r) {
                if !self.st.borrow().globals.contains_key(sym) {
                    let ty = self.type_i8();
                    let linkage = if weak {
                        Linkage::Preemptible
                    } else {
                        Linkage::Import
                    };
                    self.declare_global(
                        sym,
                        GlobalInfo {
                            ty,
                            init: None,
                            align: 1,
                            mutable: false,
                            tls: false,
                            linkage,
                            used: false,
                            section: None,
                        },
                    );
                }
                let init = self.sym_addr(sym);
                let ty = self.type_ptr();
                let align = self.tcx.data_layout.pointer_align().abi.bytes();
                self.declare_global(
                    &r,
                    GlobalInfo {
                        ty,
                        init: Some(init),
                        align,
                        mutable: false,
                        tls: false,
                        linkage: Linkage::Local,
                        used: false,
                        section: None,
                    },
                );
            }
            return self.sym_addr(&r);
        }
        if !self.st.borrow().globals.contains_key(sym) {
            let ty = self.type_i8();
            self.declare_global(
                sym,
                GlobalInfo {
                    ty,
                    init: None,
                    align: 1,
                    mutable: true,
                    tls: self.tcx.is_thread_local_static(def_id),
                    linkage: Linkage::Import,
                    used: false,
                    section: None,
                },
            );
        }
        self.sym_addr(sym)
    }

    pub fn print_ir(&self) -> String {
        use pliron::printable::Printable;
        let ctx = self.pctx.borrow();
        self.module.deref(&ctx).disp(&ctx).to_string()
    }
}

impl<'tcx> BackendTypes for CodegenCx<'tcx> {
    type Function = Ptr<Operation>;
    type BasicBlock = Ptr<BasicBlock>;
    type Funclet = Value;
    type Value = Value;
    type Type = TypeHandle;
    type FunctionSignature = TypeHandle;
    type DIScope = ();
    type DILocation = ();
    type DIVariable = ();
}

impl<'tcx> HasTyCtxt<'tcx> for CodegenCx<'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }
}

impl<'tcx> HasDataLayout for CodegenCx<'tcx> {
    fn data_layout(&self) -> &TargetDataLayout {
        &self.tcx.data_layout
    }
}

impl<'tcx> HasTargetSpec for CodegenCx<'tcx> {
    fn target_spec(&self) -> &Target {
        &self.tcx.sess.target
    }
}

impl<'tcx> HasTypingEnv<'tcx> for CodegenCx<'tcx> {
    fn typing_env(&self) -> ty::TypingEnv<'tcx> {
        ty::TypingEnv::fully_monomorphized()
    }
}

impl<'tcx> LayoutOfHelpers<'tcx> for CodegenCx<'tcx> {
    fn handle_layout_err(&self, err: LayoutError<'tcx>, span: Span, ty: Ty<'tcx>) -> ! {
        if let LayoutError::SizeOverflow(_)
        | LayoutError::ReferencesError(_)
        | LayoutError::InvalidSimd { .. } = err
        {
            self.tcx.dcx().span_fatal(span, err.to_string())
        } else {
            self.tcx
                .dcx()
                .emit_fatal(rustc_codegen_ssa::diagnostics::FailedToGetLayout { span, ty, err })
        }
    }
}

impl<'tcx> FnAbiOfHelpers<'tcx> for CodegenCx<'tcx> {
    fn handle_fn_abi_err(&self, err: FnAbiError<'tcx>, span: Span, _req: FnAbiRequest<'tcx>) -> ! {
        self.tcx
            .dcx()
            .span_fatal(span, format!("failed to get fn ABI: {err:?}"))
    }
}

impl<'tcx> MiscCodegenMethods<'tcx> for CodegenCx<'tcx> {
    fn vtables(&self) -> &RefCell<FxHashMap<(Ty<'tcx>, Option<ExistentialTraitRef<'tcx>>), Value>> {
        &self.vtables
    }

    fn get_fn(&self, instance: Instance<'tcx>) -> Ptr<Operation> {
        let sym = self.tcx.symbol_name(instance).name;
        if let Some(f) = self.st.borrow().funcs.get(sym) {
            return f.op;
        }
        let fn_abi = self.fn_abi_of_instance(instance, ty::List::empty());
        let (ty, exts) = self.fn_sig(fn_abi);
        if self.tcx.sess.target.is_like_wasm
            && let Some(module) = self
                .tcx
                .wasm_import_module_map(instance.def_id().krate)
                .get(&instance.def_id())
        {
            let name = self
                .tcx
                .codegen_fn_attrs(instance.def_id())
                .symbol_name
                .unwrap_or_else(|| self.tcx.item_name(instance.def_id()));
            self.st
                .borrow_mut()
                .wasm_imports
                .insert(sym.to_string(), (module.clone(), name.to_string()));
        }
        let op = self.declare_fn_sym(sym, ty, Linkage::Import, exts);
        self.mark_attrs(sym, instance, fn_abi);
        op
    }

    fn get_fn_addr(
        &self,
        instance: Instance<'tcx>,
        _schema: Option<&rustc_session::PointerAuthSchema>,
    ) -> Value {
        self.get_fn(instance);
        self.sym_addr(self.tcx.symbol_name(instance).name)
    }

    fn eh_personality(&self) -> Ptr<Operation> {
        let void = self.type_void();
        let ty = self.type_func(&[], void);
        let name = self.st.borrow().eh_personality.clone();
        self.declare_fn_sym(&name, ty, Linkage::Import, Exts::default())
    }

    fn sess(&self) -> &Session {
        self.tcx.sess
    }

    fn set_frame_pointer_type(&self, _llfn: Ptr<Operation>) {}

    fn apply_target_cpu_attr(&self, _llfn: Ptr<Operation>) {}

    fn declare_c_main(&self, fn_type: TypeHandle) -> Option<Ptr<Operation>> {
        let entry = self.tcx.sess.target.entry_name.as_ref();
        if self.st.borrow().funcs.contains_key(entry) {
            return None;
        }
        Some(self.declare_fn_sym(entry, fn_type, Linkage::Export, Exts::default()))
    }

    fn intrinsic_call_expects_place_always(&self, _name: rustc_span::Symbol) -> bool {
        false
    }
}

impl<'tcx> PreDefineCodegenMethods<'tcx> for CodegenCx<'tcx> {
    fn predefine_static(
        &mut self,
        def_id: rustc_hir::def_id::DefId,
        linkage: RLinkage,
        visibility: Visibility,
        symbol_name: &str,
    ) {
        let attrs = self.tcx.codegen_fn_attrs(def_id);
        use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags as F;
        let ty = self.type_i8();
        self.declare_global(
            symbol_name,
            GlobalInfo {
                ty,
                init: None,
                align: 1,
                mutable: true,
                tls: attrs.flags.contains(F::THREAD_LOCAL),
                linkage: map_linkage(linkage, visibility),
                // `used(compiler)` only forces emission; `used(linker)` asks the
                // linker to keep the symbol through GC.
                used: attrs.flags.contains(F::USED_LINKER),
                section: attrs.link_section.map(|s| s.to_string()),
            },
        );
        self.add_aliases(symbol_name, &attrs.foreign_item_symbol_aliases);
    }

    fn predefine_fn(
        &mut self,
        instance: Instance<'tcx>,
        linkage: RLinkage,
        visibility: Visibility,
        symbol_name: &str,
    ) {
        let fn_abi = self.fn_abi_of_instance(instance, ty::List::empty());
        let (ty, exts) = self.fn_sig(fn_abi);
        self.declare_fn_sym(symbol_name, ty, map_linkage(linkage, visibility), exts);
        use rustc_attr_ir::InlineAttr;
        let inline = self.tcx.codegen_instance_attrs(instance.def).inline.clone();
        if let Some(f) = self.st.borrow_mut().funcs.get_mut(symbol_name) {
            f.no_inline = matches!(inline, InlineAttr::Never);
            f.always_inline = matches!(inline, InlineAttr::Always | InlineAttr::Force { .. });
            f.inline_hint = matches!(inline, InlineAttr::Hint);
            f.odr = matches!(linkage, RLinkage::WeakODR | RLinkage::LinkOnceODR);
        }
        self.mark_attrs(symbol_name, instance, fn_abi);
        let attrs = self.tcx.codegen_instance_attrs(instance.def);
        if let Some(a) = attrs.alignment {
            self.st.borrow_mut().fn_align.insert(symbol_name.to_string(), a.bytes());
        }
        self.add_aliases(symbol_name, &attrs.foreign_item_symbol_aliases);
    }
}

impl<'tcx> CodegenCx<'tcx> {
    fn add_aliases(
        &self,
        aliasee: &str,
        aliases: &[(rustc_hir::def_id::DefId, RLinkage, Visibility)],
    ) {
        for &(alias, linkage, _) in aliases {
            let name = self.tcx.symbol_name(Instance::mono(self.tcx, alias)).name;
            let weak = matches!(linkage, RLinkage::WeakAny | RLinkage::WeakODR);
            self.st
                .borrow_mut()
                .aliases
                .push((name.to_string(), aliasee.to_string(), weak));
        }
    }
}

impl<'tcx> StaticCodegenMethods for CodegenCx<'tcx> {
    fn static_addr_of(&self, alloc: ConstAllocation<'_>, _kind: Option<&str>) -> Value {
        // SAFETY of lifetime: allocations are interned for 'tcx.
        let alloc: ConstAllocation<'tcx> = unsafe { std::mem::transmute(alloc) };
        if let Some(v) = self.const_allocs.borrow().get(&alloc) {
            return *v;
        }
        let init = self.const_alloc_to_value(alloc.inner());
        let a = alloc.inner();
        let v = self.private_global(init, a.align.bytes(), a.mutability.is_mut());
        self.const_allocs.borrow_mut().insert(alloc, v);
        v
    }

    fn codegen_static(&mut self, def_id: rustc_hir::def_id::DefId) {
        let Ok(alloc) = self.tcx.eval_static_initializer(def_id) else {
            return;
        };
        let init = self.const_alloc_to_value(alloc.inner());
        let ty = self.ty_of(init);
        let instance = Instance::mono(self.tcx, def_id);
        let sym = self.tcx.symbol_name(instance).name;
        let mut st = self.st.borrow_mut();
        let g = st.globals.get_mut(sym).expect("static was not predefined");
        g.ty = ty;
        g.init = Some(init);
        g.align = alloc.inner().align.bytes();
        g.mutable = alloc.inner().mutability.is_mut();
    }
}

impl<'tcx> AsmCodegenMethods<'tcx> for CodegenCx<'tcx> {
    fn codegen_global_asm(
        &mut self,
        template: &[rustc_ast::InlineAsmTemplatePiece],
        operands: &[GlobalAsmOperandRef<'tcx>],
        options: rustc_ast::InlineAsmOptions,
        line_spans: &[Span],
        _extra: &[String],
    ) {
        self.push_global_asm(template, operands, options, line_spans)
    }

    fn mangled_name(&self, instance: Instance<'tcx>) -> String {
        self.tcx.symbol_name(instance).name.to_string()
    }
}

impl<'tcx> DebugInfoCodegenMethods<'tcx> for CodegenCx<'tcx> {
    fn create_vtable_debuginfo(
        &self,
        _ty: Ty<'tcx>,
        _trait_ref: Option<ExistentialTraitRef<'tcx>>,
        _vtable: Value,
    ) {
    }
}

impl<'tcx> TypeMembershipCodegenMethods<'tcx> for CodegenCx<'tcx> {}

pub fn layout_ty_key<'tcx>(l: TyAndLayout<'tcx>) -> (Ty<'tcx>, Option<VariantIdx>) {
    let v = match l.variants {
        rustc_abi::Variants::Single { index } => Some(index),
        _ => None,
    };
    (l.ty, v)
}

/// Frozen pointer params of `fn_abi` by backend param index (the layout of
/// `fn_decl_backend_type`), and the backend param count.
fn frozen_params(
    fn_abi: &FnAbi<'_, Ty<'_>>,
) -> (
    Vec<(usize, u64)>,
    Vec<(usize, u64, bool)>,
    Vec<usize>,
    usize,
) {
    use rustc_target::callconv::{ArgAttribute as A, ArgAttributes, PassMode};
    // `pointee_size` is also set for nullable refs (`Option<&T>`), which
    // LLVM lowers to `dereferenceable_or_null`: only `NonNull` makes the
    // bytes unconditionally loadable. Frozen loads become `can_move`, and
    // `deref` licenses licm's speculative hoists — both would move a load
    // above its null check (regex-automata `find_fwd_imp` read `*pre` with
    // `pre == None`).
    let size = |a: &ArgAttributes| {
        let n = a.pointee_size.bytes();
        (a.regular.contains(A::NoAlias | A::ReadOnly | A::NonNull) && n > 0).then_some(n)
    };
    let nn = |a: Option<&ArgAttributes>| a.is_some_and(|a| a.regular.contains(A::NonNull));
    let mut v = Vec::new();
    let mut na = Vec::new();
    let mut null = Vec::new();
    let mut i = usize::from(matches!(fn_abi.ret.mode, PassMode::Indirect { .. }));
    let nargs = if fn_abi.c_variadic {
        fn_abi.fixed_count as usize
    } else {
        fn_abi.args.len()
    };
    for arg in &fn_abi.args[..nargs] {
        let (a, n) = match &arg.mode {
            PassMode::Ignore => continue,
            PassMode::Direct(a) => (Some(a), 1),
            PassMode::Pair(a, _) => (Some(a), 2),
            PassMode::IndirectUnsized { .. } => (None, 2),
            PassMode::Cast { pad_i32_count, .. } => (None, *pad_i32_count as usize + 1),
            PassMode::Indirect { attrs, .. } => (Some(attrs), 1),
        };
        if let Some(s) = a.and_then(size) {
            v.push((i, s));
        }
        if let Some(a) = a.filter(|a| a.regular.contains(A::NoAlias)) {
            // 0 bytes = no dereferenceability claim (nullable: see `size`);
            // the `noalias` itself still holds when non-null.
            let n = if a.regular.contains(A::NonNull) { a.pointee_size.bytes() } else { 0 };
            na.push((i, n, !a.regular.contains(A::ReadOnly)));
        }
        // `nonnull` leaf attrs: direct/pair element attrs and the
        // indirect/meta pointers. Cast args are packed; skip them.
        let ats: [Option<&ArgAttributes>; 2] = match &arg.mode {
            PassMode::Direct(a) => [Some(a), None],
            PassMode::Pair(a, b) => [Some(a), Some(b)],
            PassMode::Indirect { attrs, .. } => [Some(attrs), None],
            PassMode::IndirectUnsized { attrs, meta_attrs } => [Some(attrs), Some(meta_attrs)],
            _ => [None, None],
        };
        for (k, at) in ats.into_iter().enumerate() {
            if nn(at) {
                null.push(i + k);
            }
        }
        i += n;
    }
    (v, na, null, i)
}
