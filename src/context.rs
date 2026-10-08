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
use rustc_codegen_ssa::common::AtomicRmwBinOp;
use rustc_codegen_ssa::traits::*;
use rustc_data_structures::fx::{FxHashMap, FxIndexMap};
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

#[derive(Clone, Debug)]
pub enum ConstVal {
    /// Raw little-endian bits of a scalar of the value's type.
    Bits(u128),
    Zero,
    Undef,
    Bytes(Vec<u8>),
    /// Struct / array / vector of other constants.
    Agg(Vec<Value>),
    /// Address of a symbol plus a byte offset.
    Sym {
        sym: String,
        off: i64,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArgExt {
    #[default]
    None,
    Zext,
    Sext,
    /// Hidden struct-return pointer (`sret`).
    SRet,
    /// Aggregate passed by value on the stack (`byval`), with its size in bytes.
    ByVal(u32),
}

#[derive(Clone, Debug, Default)]
pub struct Exts {
    pub params: Vec<ArgExt>,
    pub ret: ArgExt,
}

pub struct FuncInfo {
    pub op: Ptr<Operation>,
    pub ty: TypeHandle,
    pub linkage: Linkage,
    pub exts: Exts,
    pub no_inline: bool,
    pub always_inline: bool,
    /// `#[cold]`: blocks calling it are laid out out of line.
    pub cold: bool,
    /// Declared or inferred unable to unwind (see nounwind.rs).
    pub nounwind: bool,
    /// Backend type of the value returned through the sret pointer.
    pub sret_ty: Option<TypeHandle>,
}

pub struct GlobalInfo {
    pub ty: TypeHandle,
    pub init: Option<Value>,
    pub align: u64,
    pub mutable: bool,
    pub tls: bool,
    pub linkage: Linkage,
    pub used: bool,
    pub section: Option<String>,
}

#[derive(Clone)]
pub struct CallInfo {
    pub fn_ty: TypeHandle,
    pub exts: Exts,
}

#[derive(Default)]
pub struct State<'tcx> {
    pub consts: FxHashMap<Value, ConstVal>,
    pub funcs: FxIndexMap<String, FuncInfo>,
    pub globals: FxIndexMap<String, GlobalInfo>,
    pub ident_to_sym: FxHashMap<String, String>,
    pub sym_to_ident: FxHashMap<String, Identifier>,
    pub allocas: FxHashMap<Value, (u64, u64)>,
    /// Allocas lowered as Cranelift variables (see sroa.rs), with their value type.
    pub promoted: FxHashMap<Value, TypeHandle>,
    pub calls: FxHashMap<Ptr<Operation>, CallInfo>,
    pub intrinsics: FxHashMap<Ptr<Operation>, String>,
    pub rmw: FxHashMap<Ptr<Operation>, AtomicRmwBinOp>,
    pub const_allocs: FxHashMap<ConstAllocation<'tcx>, Value>,
    pub strs: FxHashMap<String, Value>,
    pub counter: usize,
    pub cgu: String,
    pub asm: String,
    /// call op → (landing block, is catch_unwind catch-all)
    pub invokes: FxHashMap<Ptr<Operation>, (Ptr<BasicBlock>, bool)>,
    pub last_call: Option<Ptr<Operation>>,
    pub llvm_stubs: rustc_data_structures::fx::FxHashSet<String>,
    /// Volatile loads/stores/mem intrinsics: memory passes must leave them alone.
    pub volatile: rustc_data_structures::fx::FxHashSet<Ptr<Operation>>,
    /// Local fns with no remaining references after inlining; not lowered.
    pub dead_fns: rustc_data_structures::fx::FxHashSet<String>,
    /// Lower non-volatile loads/stores as `notrap` (set by finish_module at -O).
    pub notrap: bool,
    /// cond_br op → expected condition value (`likely`/`unlikely`).
    pub expect: FxHashMap<Ptr<Operation>, bool>,
    /// `#[link(wasm_import_module = ..)]` functions: symbol -> (module, name).
    pub wasm_imports: FxHashMap<String, (String, String)>,
}

pub struct CodegenCx<'tcx> {
    pub tcx: TyCtxt<'tcx>,
    pub pctx: RefCell<Context>,
    pub module: Ptr<Operation>,
    pub st: RefCell<State<'tcx>>,
    pub vtables: RefCell<FxHashMap<(Ty<'tcx>, Option<ExistentialTraitRef<'tcx>>), Value>>,
    pub tcache: RefCell<FxHashMap<(Ty<'tcx>, Option<VariantIdx>), TypeHandle>>,
    pub scache: RefCell<FxHashMap<Ty<'tcx>, TypeHandle>>,
}

pub fn mask(bits: u128, w: u32) -> u128 {
    if w >= 128 {
        bits
    } else {
        bits & ((1u128 << w) - 1)
    }
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
            pctx: RefCell::new(ctx),
            module,
            st: RefCell::new(State::default()),
            vtables: Default::default(),
            tcache: Default::default(),
            scache: Default::default(),
        }
    }

    pub fn new_value(&self, ty: TypeHandle, cv: ConstVal) -> Value {
        let v = {
            let mut ctx = self.pctx.borrow_mut();
            let op = UndefOp::new(&mut ctx, ty).get_operation();
            op.deref(&ctx).get_result(0)
        };
        self.st.borrow_mut().consts.insert(v, cv);
        v
    }

    pub fn cval(&self, v: Value) -> Option<ConstVal> {
        self.st.borrow().consts.get(&v).cloned()
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
                cold: false,
                nounwind: false,
                sret_ty: None,
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
    type Funclet = ();
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
        self.tcx
            .dcx()
            .span_fatal(span, format!("failed to get layout for `{ty}`: {err:?}"))
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
        self.declare_fn_sym("rust_eh_personality", ty, Linkage::Import, Exts::default())
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
                used: attrs.flags.intersects(F::USED_COMPILER | F::USED_LINKER),
                section: attrs.link_section.map(|s| s.to_string()),
            },
        );
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
        }
        self.mark_attrs(symbol_name, instance, fn_abi);
    }
}

impl<'tcx> StaticCodegenMethods for CodegenCx<'tcx> {
    fn static_addr_of(&self, alloc: ConstAllocation<'_>, _kind: Option<&str>) -> Value {
        // SAFETY of lifetime: allocations are interned for 'tcx.
        let alloc: ConstAllocation<'tcx> = unsafe { std::mem::transmute(alloc) };
        if let Some(v) = self.st.borrow().const_allocs.get(&alloc) {
            return *v;
        }
        let init = self.const_alloc_to_value(alloc.inner());
        let a = alloc.inner();
        let v = self.private_global(init, a.align.bytes(), a.mutability.is_mut());
        self.st.borrow_mut().const_allocs.insert(alloc, v);
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
