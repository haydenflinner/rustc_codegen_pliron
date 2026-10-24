//! rustc side of `-g`: turns the scopes and spans `rustc_codegen_ssa`
//! hands the Builder into the per-CGU tables of `pliron_ir_opt::dbg`
//! (see there for the pipeline). `DIScope` is a scope id — a
//! (subprogram, lexical block) pair — and `DILocation` a location id.

use pliron::value::Value;
use pliron_ir_opt::dbg::{DbgInfo, Decl, Field, Global, Loc, Sub, Type, Var, Variant};
use rustc_abi::{FieldIdx, Size, TagEncoding, Variants};
use rustc_codegen_ssa::debuginfo::type_names;
use rustc_codegen_ssa::mir::debuginfo::VariableKind;
use rustc_middle::ty::layout::LayoutOf;
use rustc_middle::ty::{self, Instance, Ty, Unnormalized};
use rustc_session::config::DebugInfo;
use rustc_span::{
    BytePos, FileName, Pos, RemapPathScopeComponents, SourceFile, SourceFileAndLine, Span,
};

use crate::context::CodegenCx;

/// The CU tables for a new CGU, or `None` without `-g`.
pub fn cu_info(tcx: ty::TyCtxt<'_>, cgu: &str) -> Option<Box<DbgInfo>> {
    if tcx.sess.opts.debuginfo == DebugInfo::None {
        return None;
    }
    // Same CU naming as rustc_codegen_llvm: a unique `<root>/@/<cgu>` per
    // object (ld64 drops debug info of objects sharing a `DW_AT_name`).
    let mut name = tcx
        .sess
        .local_crate_source_file()
        .map(|src| src.path(RemapPathScopeComponents::DEBUGINFO).to_path_buf())
        .unwrap_or_else(|| tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE).as_str().into());
    name.push("@");
    name.push(cgu);
    let dir = tcx
        .sess
        .source_map()
        .working_dir()
        .path(RemapPathScopeComponents::DEBUGINFO)
        .to_string_lossy()
        .into_owned();
    let producer = format!("rustc version {} (rustc_codegen_pliron)", tcx.sess.cfg_version);
    let mut d = DbgInfo::new(name.to_string_lossy().into_owned(), dir, producer);
    d.keep_slots = tcx.sess.opts.debuginfo == DebugInfo::Full
        && tcx.sess.opts.optimize == rustc_session::config::OptLevel::No;
    d.values = pliron_cfg::pass_enabled("PLIRON_DBGVALS");
    Some(Box::new(d))
}

impl<'tcx> CodegenCx<'tcx> {
    fn dbg_file(&self, sf: &SourceFile) -> u32 {
        if let Some(&i) = self.dbg_files.borrow().get(&sf.start_pos) {
            return i;
        }
        // rustc_codegen_llvm's naming: relative to the working directory
        // (the CU's comp_dir) when under it, else the remapped path.
        let name = match &sf.name {
            FileName::Real(f) => {
                let (wd, p) = f.embeddable_name(RemapPathScopeComponents::DEBUGINFO);
                match p.strip_prefix(wd) {
                    Ok(rel) => rel.to_string_lossy().into_owned(),
                    Err(_) => p.to_string_lossy().into_owned(),
                }
            }
            other => other.display(RemapPathScopeComponents::DEBUGINFO).to_string(),
        };
        let i = self.st.borrow_mut().dbg.as_mut().unwrap().file(&name);
        self.dbg_files.borrow_mut().insert(sf.start_pos, i);
        i
    }

    /// (file, 1-based line, 1-based column) of `pos`; line 0 when unknown.
    fn dbg_pos(&self, pos: BytePos) -> (u32, u32, u32) {
        match self.tcx.sess.source_map().lookup_line(pos) {
            Ok(SourceFileAndLine { sf, line }) => {
                let col = (sf.relative_position(pos) - sf.lines()[line]).to_u32() + 1;
                let col = if self.tcx.sess.target.is_like_msvc { 0 } else { col };
                (self.dbg_file(&sf), line as u32 + 1, col)
            }
            Err(sf) => (self.dbg_file(&sf), 0, 0),
        }
    }

    /// The subprogram of `instance`, created on first use. `defined`: this
    /// CGU is about to emit its body (as opposed to inlining it).
    pub fn dbg_scope_fn(&self, instance: Instance<'tcx>, defined: bool) -> u32 {
        let tcx = self.tcx;
        let cached = self.dbg_insts.borrow().get(&instance).copied();
        let s = cached.unwrap_or_else(|| {
            let def_id = instance.def_id();
            let (file, line, _) = self.dbg_pos(tcx.def_span(def_id).lo());
            let mut name = String::with_capacity(64);
            type_names::push_item_name(tcx, def_id, true, &mut name);
            let generics = tcx.generics_of(tcx.typeck_root_def_id(def_id));
            let args = instance.args.truncate_to(tcx, generics);
            let args = tcx.normalize_erasing_regions(
                ty::TypingEnv::fully_monomorphized(),
                Unnormalized::new_wip(args),
            );
            type_names::push_generic_args(tcx, args, &mut name);
            let linkage = tcx.symbol_name(instance).name.to_string();
            let s = self.st.borrow_mut().dbg.as_mut().unwrap().sub(Sub { name, linkage, file, line });
            self.dbg_insts.borrow_mut().insert(instance, s);
            s
        });
        let mut st = self.st.borrow_mut();
        let d = st.dbg.as_mut().unwrap();
        if defined {
            let sym = d.subs[s as usize].linkage.clone();
            d.fn_sub.insert(sym, s);
        }
        d.scope(s, 0)
    }

    /// `static` `def_id` (symbol `sym`), as rustc_codegen_llvm's
    /// `build_global_var_di_node` describes it.
    pub fn dbg_static(&self, def_id: rustc_hir::def_id::DefId, instance: Instance<'tcx>, sym: &str) {
        let tcx = self.tcx;
        let ty = instance.ty(tcx, ty::TypingEnv::fully_monomorphized());
        let ty = self.dbg_type(ty);
        let (file, line, _) = self.dbg_pos(tcx.def_span(def_id).lo());
        let mut name = String::new();
        type_names::push_item_name(tcx, def_id, true, &mut name);
        let g = Global { name, sym: sym.to_string(), ty, file, line };
        self.st.borrow_mut().dbg.as_mut().unwrap().globals.push(g);
    }

    /// `dbg_create_lexical_block`: a block in `parent` (a scope id).
    pub fn dbg_block(&self, parent: u32) -> u32 {
        self.st.borrow_mut().dbg.as_mut().unwrap().new_block(parent)
    }

    pub fn dbg_loc(&self, scope: u32, inlined_at: Option<u32>, span: Span) -> u32 {
        let (file, line, col) = self.dbg_pos(span.lo());
        let mut st = self.st.borrow_mut();
        let d = st.dbg.as_mut().unwrap();
        let (sub, block) = d.scopes[scope as usize];
        let l = Loc { file, line, col, scope: sub, inlined_at: inlined_at.unwrap_or(0), block };
        d.loc(l)
    }

    pub fn dbg_var(
        &self,
        name: rustc_span::Symbol,
        ty: Ty<'tcx>,
        scope: u32,
        kind: VariableKind,
        span: Span,
    ) -> u32 {
        let (file, line, _) = self.dbg_pos(span.lo());
        let ty = self.dbg_type(ty);
        let arg = match kind {
            VariableKind::ArgumentVariable(i) => i as u32,
            VariableKind::LocalVariable => 0,
        };
        let mut st = self.st.borrow_mut();
        let d = st.dbg.as_mut().unwrap();
        let (scope, block) = d.scopes[scope as usize];
        d.vars.push(Var { name: name.to_string(), ty, scope, block, arg, file, line });
        d.vars.len() as u32 - 1
    }

    /// `dbg_var_addr`: only allocas get a location (the frame slot lowering
    /// gives them); a fragment of a split variable is left out.
    pub fn dbg_declare(
        &self,
        var: u32,
        loc: u32,
        alloca: Value,
        direct: Size,
        indirect: &[Size],
        fragment: &Option<std::ops::Range<Size>>,
    ) {
        if fragment.is_some() {
            return;
        }
        let mut st = self.st.borrow_mut();
        let Some(&(size, _)) = st.allocas.get(alloca) else {
            return;
        };
        let d = st.dbg.as_mut().unwrap();
        let offset = direct.bytes() as u32;
        let derefs = indirect.iter().map(|s| s.bytes() as u32).collect();
        d.declare(alloca, Decl { var, loc, offset, derefs, size });
    }

    /// The `DbgInfo::types` id describing `ty`, built on first use. The id
    /// is reserved before the fields are, so recursive types terminate.
    pub fn dbg_type(&self, ty: Ty<'tcx>) -> u32 {
        if let Some(&i) = self.dbg_types.borrow().get(&ty) {
            return i;
        }
        // A pattern type is its base type to a debugger (as rustc_codegen_llvm
        // describes it): `NonNull`'s `pointer`, `Cap`'s `usize`.
        if let ty::Pat(base, _) = *ty.kind() {
            let id = self.dbg_type(base);
            self.dbg_types.borrow_mut().insert(ty, id);
            return id;
        }
        let l = self.layout_of(ty);
        let name = type_names::compute_debuginfo_type_name(self.tcx, ty, true);
        let size = l.size.bytes() as u32;
        let id = {
            let mut st = self.st.borrow_mut();
            let d = st.dbg.as_mut().unwrap();
            d.types.push(Type::Opaque { name: name.clone(), size });
            d.types.len() as u32 - 1
        };
        self.dbg_types.borrow_mut().insert(ty, id);
        let t = self.dbg_type_desc(ty, l, name, size);
        let tparams = if matches!(t, Type::Struct { .. } | Type::Enum { .. }) {
            self.dbg_tparams(ty)
        } else {
            Vec::new()
        };
        let mut st = self.st.borrow_mut();
        let d = st.dbg.as_mut().unwrap();
        d.types[id as usize] = t;
        if !tparams.is_empty() {
            d.tparams.insert(id, tparams);
        }
        id
    }

    /// `ty`'s type parameters, named as declared (rustc_codegen_llvm's
    /// `build_generic_type_param_di_nodes`): lifetimes and consts left out.
    fn dbg_tparams(&self, ty: Ty<'tcx>) -> Vec<(String, u32)> {
        let ty::Adt(def, args) = *ty.kind() else {
            return Vec::new();
        };
        if args.types().next().is_none() {
            return Vec::new();
        }
        let tcx = self.tcx;
        let generics = tcx.generics_of(def.did());
        let mut names = Vec::new();
        let mut g = Some(generics);
        // Parent generics come first in `args`.
        let mut chain = Vec::new();
        while let Some(x) = g {
            chain.push(x);
            g = x.parent.map(|p| tcx.generics_of(p));
        }
        for x in chain.into_iter().rev() {
            names.extend(x.own_params.iter().map(|p| p.name.to_string()));
        }
        args.iter()
            .zip(names)
            .filter_map(|(a, n)| {
                let t = a.as_type()?;
                let t = tcx.normalize_erasing_regions(ty::TypingEnv::fully_monomorphized(), Unnormalized::new_wip(t));
                Some((n, self.dbg_type(t)))
            })
            .collect()
    }

    fn dbg_type_desc(
        &self,
        ty: Ty<'tcx>,
        l: rustc_middle::ty::layout::TyAndLayout<'tcx>,
        name: String,
        size: u32,
    ) -> Type {
        use rustc_middle::ty::TyKind as K;
        let tcx = self.tcx;
        let base = |name: String, enc: gimli_ate::Ate| Type::Base { name, size, enc: enc as u8 };
        let fields_of = |names: &dyn Fn(usize) -> String| -> Vec<Field> {
            (0..l.fields.count())
                .map(|i| Field {
                    name: names(i),
                    ty: self.dbg_type(l.field(self, i).ty),
                    offset: l.fields.offset(i).bytes() as u32,
                })
                .collect()
        };
        let strukt = |fields: Vec<Field>| Type::Struct {
            name: name.clone(),
            size,
            align: l.align.abi.bytes() as u32,
            fields,
            union: false,
        };
        match *ty.kind() {
            K::Bool => base(name, gimli_ate::Ate::Boolean),
            K::Char => base(name, gimli_ate::Ate::Utf),
            K::Int(_) => base(name, gimli_ate::Ate::Signed),
            K::Uint(_) => base(name, gimli_ate::Ate::Unsigned),
            K::Float(_) => base(name, gimli_ate::Ate::Float),
            K::Never => base(name, gimli_ate::Ate::Unsigned),
            K::Tuple(t) if t.is_empty() => base(name, gimli_ate::Ate::Unsigned),
            K::RawPtr(p, _) | K::Ref(_, p, _) => self.dbg_pointer(name, p, l),
            K::Adt(def, _) if def.is_box() => self.dbg_pointer(name, ty.expect_boxed_ty(), l),
            K::FnPtr(..) => Type::Ptr { name, pointee: 0 },
            K::Array(e, _) => match l.fields {
                rustc_abi::FieldsShape::Array { count, .. } => {
                    Type::Array { name, elem: self.dbg_type(e), len: count, size }
                }
                _ => Type::Opaque { name, size },
            },
            K::Tuple(_) => strukt(fields_of(&|i| format!("__{i}"))),
            K::Closure(def_id, _) => {
                let names = tcx.closure_saved_names_of_captured_variables(def_id);
                strukt(fields_of(&|i| {
                    names.get(FieldIdx::from_usize(i)).map_or(format!("__{i}"), |n| n.to_string())
                }))
            }
            K::Adt(def, _) if def.is_struct() || def.is_union() => {
                let v = def.non_enum_variant();
                // Tuple structs' fields are `__0`, `__1`, ... (LLVM's names,
                // which rust-lldb's formatters expect).
                let tuple = v.ctor_kind() == Some(rustc_hir::def::CtorKind::Fn);
                let fields = fields_of(&|i| {
                    if tuple { format!("__{i}") } else { v.fields[FieldIdx::from_usize(i)].name.to_string() }
                });
                if def.is_union() {
                    Type::Struct { name, size, align: l.align.abi.bytes() as u32, fields, union: true }
                } else {
                    strukt(fields)
                }
            }
            K::Adt(def, _) if def.is_enum() => {
                let fieldless = def.variants().iter().all(|v| v.fields.is_empty());
                match &l.variants {
                    Variants::Multiple { tag_encoding: TagEncoding::Direct, .. } if fieldless => {
                        let signed = def.repr().discr_type().is_signed();
                        let variants = def
                            .discriminants(tcx)
                            .map(|(vi, d)| {
                                let v = d.val as i128;
                                let bits = (size * 8).min(128);
                                let v = if signed && bits < 128 { v << (128 - bits) >> (128 - bits) } else { v };
                                (def.variant(vi).name.to_string(), v)
                            })
                            .collect();
                        Type::CEnum { name, size, signed, variants }
                    }
                    &Variants::Single { index } if l.fields.count() > 0 => {
                        let v = def.variant(index);
                        let tuple = v.ctor_kind() == Some(rustc_hir::def::CtorKind::Fn);
                        strukt(fields_of(&|i| match v.fields.get(FieldIdx::from_usize(i)) {
                            Some(f) if !tuple => f.name.to_string(),
                            _ => format!("__{i}"),
                        }))
                    }
                    Variants::Multiple { tag, tag_encoding, tag_field, variants } => {
                        let tag_size = tag.primitive().size(self).bytes();
                        let mask = if tag_size >= 16 { u128::MAX } else { (1u128 << (tag_size * 8)) - 1 };
                        let vs = variants
                            .indices()
                            .map(|vi| {
                                let discr = match tag_encoding {
                                    TagEncoding::Direct => {
                                        ty.discriminant_for_variant(tcx, vi).map(|d| (d.val & mask) as u64)
                                    }
                                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                                        let inr = vi >= niche_variants.start && vi <= niche_variants.last;
                                        (vi != *untagged_variant && inr).then(|| {
                                            let rel = (vi.as_u32() - niche_variants.start.as_u32()) as u128;
                                            (rel.wrapping_add(*niche_start) & mask) as u64
                                        })
                                    }
                                };
                                let v = def.variant(vi);
                                let vl = l.for_variant(self, vi);
                                let tuple = v.ctor_kind() == Some(rustc_hir::def::CtorKind::Fn);
                                let fields = (0..vl.fields.count())
                                    .map(|i| Field {
                                        name: match v.fields.get(FieldIdx::from_usize(i)) {
                                            Some(f) if !tuple => f.name.to_string(),
                                            _ => format!("__{i}"),
                                        },
                                        ty: self.dbg_type(vl.field(self, i).ty),
                                        offset: vl.fields.offset(i).bytes() as u32,
                                    })
                                    .collect();
                                Variant { name: v.name.to_string(), discr, fields }
                            })
                            .collect();
                        let tag_off = l.fields.offset(tag_field.as_usize()).bytes() as u32;
                        Type::Enum {
                            name,
                            size,
                            align: l.align.abi.bytes() as u32,
                            tag: (tag_off, tag_size as u32),
                            variants: vs,
                        }
                    }
                    _ => Type::Opaque { name, size },
                }
            }
            _ => Type::Opaque { name, size },
        }
    }

    /// `&T` / `*T` / `Box<T>`: a plain pointer when thin, else LLVM's
    /// two-field struct (`data_ptr`+`length` or `pointer`+`vtable`).
    fn dbg_pointer(&self, name: String, pointee: Ty<'tcx>, l: rustc_middle::ty::layout::TyAndLayout<'tcx>) -> Type {
        let tcx = self.tcx;
        let env = ty::TypingEnv::fully_monomorphized();
        if pointee.is_sized(tcx, env) || l.size.bytes() <= 8 {
            let p = if pointee.is_sized(tcx, env) { self.dbg_type(pointee) } else { 0 };
            return Type::Ptr { name, pointee: p };
        }
        let usize_t = self.dbg_type(tcx.types.usize);
        let (data, meta, meta_name) = match pointee.kind() {
            ty::Slice(e) => (Ty::new_imm_ptr(tcx, *e), usize_t, "length"),
            ty::Str => (Ty::new_imm_ptr(tcx, tcx.types.u8), usize_t, "length"),
            _ => {
                let vt = Ty::new_imm_ptr(tcx, Ty::new_array(tcx, tcx.types.usize, 3));
                (Ty::new_imm_ptr(tcx, tcx.types.unit), self.dbg_type(vt), "vtable")
            }
        };
        let data_name = if meta_name == "length" { "data_ptr" } else { "pointer" };
        let fields = vec![
            Field { name: data_name.into(), ty: self.dbg_type(data), offset: 0 },
            Field { name: meta_name.into(), ty: meta, offset: 8 },
        ];
        Type::Struct { name, size: l.size.bytes() as u32, align: 8, fields, union: false }
    }
}

/// The `DW_ATE_*` encodings `Type::Base` uses (this crate has no gimli).
mod gimli_ate {
    pub enum Ate {
        Boolean = 0x02,
        Float = 0x04,
        Signed = 0x05,
        Unsigned = 0x07,
        Utf = 0x10,
    }
}
