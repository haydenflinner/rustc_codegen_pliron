//! pliron LLVM dialect -> waffle IR -> wasm32 "object" module.
//!
//! An object is a complete wasm module. Linear memory, `__stack_pointer` and
//! `__indirect_function_table` are imported from `env`; functions it calls but
//! doesn't define are `env.<sym>` imports, and symbol addresses are immutable
//! i32 globals `GOT.mem.<sym>` (data) / `GOT.func.<sym>` (table index).
//! Defined functions are exported under their symbol. Linkage and data
//! objects (bytes + relocations) go in the `pliron.link` custom section, which
//! `tools/pliron-wasm-ld` uses to merge objects into one module.
//!
//! Integers narrower than their wasm container (i1..i31 in i32, i33..i63 in
//! i64) are kept zero-extended; i128 and f128 are (lo, hi) i64 pairs; f16 is
//! its raw bits. Functions that hit an unsupported construct are emitted as
//! `unreachable` stubs (`PLIRON_WASM_VERBOSE=1` lists them).

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use cranelift_codegen::ir::{Type as ClType, types as clt};
use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;
use pliron_llvm::attributes::{FCmpPredicateAttr, ICmpPredicateAttr};
use pliron_llvm::ops::*;
use rustc_codegen_ssa::common::AtomicRmwBinOp;
use rustc_data_structures::fx::FxHashMap;
use smallvec::{SmallVec, smallvec};
use waffle::entity::EntityRef;
use waffle::{
    Block as WBlock, BlockTarget, Export, ExportKind, Func, FuncDecl, FunctionBody, Global,
    GlobalData, Import, ImportKind, Memory, MemoryArg, MemoryData, Module, Operator as O,
    Signature, SignatureData, Table, TableData, Terminator, Type as WT, Value as WV, ValueDef,
};

use crate::context::{ArgExt, ConstVal, State};
use crate::lower::{has_body, rpo, write_const};
use crate::types::{TyK, classify, leaves, members, size_align, struct_offsets};

type Vals = SmallVec<[WV; 2]>;

/// Scalar leaves for wasm: `leaves`, with 128-bit values split into (lo, hi)
/// i64 pairs and f16 as raw i16 bits.
pub fn wleaves(ctx: &Context, ty: TypeHandle) -> Vec<(u64, ClType)> {
    let mut out = Vec::new();
    for (o, t) in leaves(ctx, ty) {
        match t {
            clt::I128 | clt::F128 => {
                out.push((o, clt::I64));
                out.push((o + 8, clt::I64));
            }
            clt::F16 => out.push((o, clt::I16)),
            _ => out.push((o, t)),
        }
    }
    out
}

fn wty(t: ClType) -> WT {
    match t {
        clt::I8 | clt::I16 | clt::I32 => WT::I32,
        clt::I64 => WT::I64,
        clt::F32 => WT::F32,
        clt::F64 => WT::F64,
        t => panic!("wasm: no scalar type for {t}"),
    }
}

fn wsig(ctx: &Context, fn_ty: TypeHandle) -> SignatureData {
    let TyK::Func(ret, args, var_arg) = classify(ctx, fn_ty) else {
        panic!("not a function type")
    };
    SignatureData {
        // C variadics: the extra arguments go in a buffer passed as one i32.
        params: args
            .iter()
            .flat_map(|a| wleaves(ctx, *a))
            .map(|(_, t)| wty(t))
            .chain(var_arg.then_some(WT::I32))
            .collect(),
        returns: wleaves(ctx, ret).into_iter().map(|(_, t)| wty(t)).collect(),
    }
}

/// (first leaf, leaf count) of the member at `idx` inside `ty`.
fn wleaf_range(ctx: &Context, mut ty: TypeHandle, idx: &[u32]) -> (usize, usize) {
    let mut start = 0;
    for &i in idx {
        let ms = members(ctx, ty);
        start += ms[..i as usize]
            .iter()
            .map(|m| wleaves(ctx, *m).len())
            .sum::<usize>();
        ty = ms[i as usize];
    }
    (start, wleaves(ctx, ty).len())
}

thread_local!(static QUIET: Cell<bool> = const { Cell::new(false) });

/// Run `f`, turning a panic into `Err(message)` without printing it.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |i| {
            if !QUIET.with(|q| q.get()) {
                prev(i)
            }
        }));
    });
    QUIET.with(|q| q.set(true));
    let r = catch_unwind(AssertUnwindSafe(f));
    QUIET.with(|q| q.set(false));
    r.map_err(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    })
}

struct FDecl {
    name: String,
    sig: Signature,
    ty: Option<TypeHandle>,
    body: Option<FunctionBody>,
}

struct Obj<'a, 'tcx> {
    ctx: &'a Context,
    st: &'a State<'tcx>,
    m: Module<'static>,
    sigs: FxHashMap<(Vec<WT>, Vec<WT>), Signature>,
    funcs: Vec<FDecl>,
    fidx: FxHashMap<String, usize>,
    got: FxHashMap<(bool, String), Global>,
    sp: Global,
    table: Table,
    mem: Memory,
}

impl<'a, 'tcx> Obj<'a, 'tcx> {
    fn sig(&mut self, s: SignatureData) -> Signature {
        let key = (s.params.clone(), s.returns.clone());
        if let Some(x) = self.sigs.get(&key) {
            return *x;
        }
        let x = self.m.signatures.push(s);
        self.sigs.insert(key, x);
        x
    }

    fn import_global(&mut self, module: &str, name: &str, mutable: bool) -> Global {
        let g = self.m.globals.push(GlobalData {
            ty: WT::I32,
            value: None,
            mutable,
        });
        self.m.imports.push(Import {
            module: module.into(),
            name: name.into(),
            kind: ImportKind::Global(g),
        });
        g
    }

    /// `GOT.func.<sym>` (table index) or `GOT.mem.<sym>` (address).
    fn got(&mut self, sym: &str) -> Global {
        let func = self.st.funcs.contains_key(sym);
        if let Some(g) = self.got.get(&(func, sym.to_string())) {
            return *g;
        }
        let g = self.import_global(if func { "GOT.func" } else { "GOT.mem" }, sym, false);
        self.got.insert((func, sym.to_string()), g);
        g
    }

    /// A callable function index for `name`, importing it with `sig` if this
    /// module doesn't know it.
    fn libfunc(&mut self, name: &str, sig: SignatureData) -> usize {
        if let Some(&i) = self.fidx.get(name) {
            return i;
        }
        let sig = self.sig(sig);
        self.funcs.push(FDecl {
            name: name.into(),
            sig,
            ty: None,
            body: None,
        });
        self.fidx.insert(name.into(), self.funcs.len() - 1);
        self.funcs.len() - 1
    }
}

pub fn lower_to_wasm(ctx: &Context, st: &State<'_>, name: &str) -> Vec<u8> {
    let mut m = Module::empty();
    let mem = m.memories.push(MemoryData {
        initial_pages: 0,
        maximum_pages: None,
        segments: vec![],
    });
    m.imports.push(Import {
        module: "env".into(),
        name: "memory".into(),
        kind: ImportKind::Memory(mem),
    });
    let table = m.tables.push(TableData {
        ty: WT::FuncRef,
        initial: 0,
        max: None,
        func_elements: None,
    });
    m.imports.push(Import {
        module: "env".into(),
        name: "__indirect_function_table".into(),
        kind: ImportKind::Table(table),
    });
    let mut o = Obj {
        ctx,
        st,
        m,
        sigs: FxHashMap::default(),
        funcs: Vec::new(),
        fidx: FxHashMap::default(),
        got: FxHashMap::default(),
        sp: Global::invalid(),
        table,
        mem,
    };
    o.sp = o.import_global("env", "__stack_pointer", true);

    for (n, f) in &st.funcs {
        let s = wsig(ctx, f.ty);
        let sig = o.sig(s);
        o.funcs.push(FDecl {
            name: n.clone(),
            sig,
            ty: Some(f.ty),
            body: None,
        });
        o.fidx.insert(n.clone(), o.funcs.len() - 1);
    }
    let verbose = std::env::var_os("PLIRON_WASM_VERBOSE").is_some();
    let mut stubs = 0;
    let mut defined = 0;
    for (n, f) in &st.funcs {
        if !has_body(ctx, f.op) {
            continue;
        }
        defined += 1;
        let i = o.fidx[n];
        let sig = o.funcs[i].sig;
        let body = match guarded(|| {
            let mut fl = FL::new(&mut o, sig);
            fl.lower(f.op);
            fl.finish()
        }) {
            Ok(b) => b,
            Err(e) => {
                stubs += 1;
                if verbose {
                    eprintln!("pliron-wasm: stub {n}: {e}");
                }
                stub(&o.m, sig)
            }
        };
        o.funcs[i].body = Some(body);
    }

    // Only import functions that are actually called; GOT.func globals
    // name their targets, so they don't need an import.
    let mut called = vec![false; o.funcs.len()];
    for f in &o.funcs {
        for v in f.body.iter().flat_map(|b| b.values.values()) {
            if let ValueDef::Operator(O::Call { function_index }, ..) = v {
                called[function_index.index()] = true;
            }
        }
    }
    // wasm numbers imported functions first.
    let order: Vec<usize> = (0..o.funcs.len())
        .filter(|&i| o.funcs[i].body.is_none() && called[i])
        .chain((0..o.funcs.len()).filter(|&i| o.funcs[i].body.is_some()))
        .collect();
    let mut remap = vec![0usize; o.funcs.len()];
    for (new, &old) in order.iter().enumerate() {
        remap[old] = new;
    }
    let mut decls: Vec<Option<FDecl>> =
        std::mem::take(&mut o.funcs).into_iter().map(Some).collect();
    for &old in &order {
        let mut d = decls[old].take().unwrap();
        let f = Func::new(o.m.funcs.len());
        match d.body.as_mut() {
            None => {
                o.m.funcs.push(FuncDecl::Import(d.sig, d.name.clone()));
                let (module, name) = st
                    .wasm_imports
                    .get(&d.name)
                    .cloned()
                    .unwrap_or_else(|| ("env".into(), d.name));
                o.m.imports.push(Import {
                    module,
                    name,
                    kind: ImportKind::Func(f),
                });
            }
            Some(b) => {
                for v in b.values.values_mut() {
                    if let ValueDef::Operator(O::Call { function_index }, ..) = v {
                        *function_index = Func::new(remap[function_index.index()]);
                    }
                }
                let bytes = match guarded(|| b.compile().map_err(|e| e.to_string())) {
                    Ok(Ok(c)) => c.into_raw_body(),
                    Ok(Err(e)) | Err(e) => {
                        stubs += 1;
                        if verbose {
                            eprintln!("pliron-wasm: stub {} (backend): {e}", d.name);
                        }
                        stub(&o.m, d.sig).compile().unwrap().into_raw_body()
                    }
                };
                o.m.funcs
                    .push(FuncDecl::Compiled(d.sig, d.name.clone(), bytes));
                o.m.exports.push(Export {
                    name: d.name,
                    kind: ExportKind::Func(f),
                });
            }
        }
    }
    if stubs > 0 {
        eprintln!(
            "pliron-wasm: {name}: {stubs}/{defined} functions stubbed (PLIRON_WASM_VERBOSE=1 lists them)"
        );
    }
    let mut bytes =
        o.m.to_wasm_bytes()
            .unwrap_or_else(|e| panic!("pliron-wasm: {name}: {e}"));
    custom_section(&mut bytes, "pliron.link", &link_section(ctx, st));
    bytes
}

fn stub(m: &Module, sig: Signature) -> FunctionBody {
    let mut b = FunctionBody::new(m, sig);
    let e = b.entry;
    b.set_terminator(e, Terminator::Unreachable);
    b
}

fn leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        out.push(if v != 0 { b | 0x80 } else { b });
        if v == 0 {
            break;
        }
    }
}

fn custom_section(out: &mut Vec<u8>, name: &str, payload: &[u8]) {
    let mut body = Vec::new();
    leb(&mut body, name.len() as u32);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(payload);
    out.push(0);
    leb(out, body.len() as u32);
    out.extend(body);
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// Link flags: bit 0 = global (visible to other objects), bit 1 = weak.
const INIT_ARRAY: u8 = 4;

fn link_flags(l: Linkage) -> u8 {
    match l {
        Linkage::Local => 0,
        Linkage::Preemptible => 3,
        _ => 1,
    }
}

/// `pliron.link` v1, all integers u32 LE, strings length-prefixed:
/// `nfuncs { name, u8 flags }` for defined functions, then
/// `ndata { name, u8 flags, align, bytes, nrelocs { off, u8 is_func, sym, i32 addend } }`
/// for defined data objects. Flags: bit 0 global, bit 1 preemptible, bit 2 an
/// `.init_array` entry (constructor pointers the linker calls from `__wasm_call_ctors`).
fn link_section(ctx: &Context, st: &State<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    let fs: Vec<_> = st
        .funcs
        .iter()
        .filter(|(_, f)| has_body(ctx, f.op))
        .collect();
    put_u32(&mut out, fs.len() as u32);
    for (n, f) in fs {
        put_str(&mut out, n);
        let l = if f.linkage == Linkage::Import {
            Linkage::Export
        } else {
            f.linkage
        };
        out.push(link_flags(l));
    }
    let gs: Vec<_> = st
        .globals
        .iter()
        .filter(|(n, g)| g.init.is_some() && !st.funcs.contains_key(*n))
        .collect();
    put_u32(&mut out, gs.len() as u32);
    for (n, g) in gs {
        let init = g.init.unwrap();
        let (size, _) = size_align(ctx, init.get_type(ctx));
        let mut bytes = vec![0u8; size as usize];
        let mut relocs = Vec::new();
        write_const(ctx, st, init, 0, &mut bytes, &mut relocs);
        put_str(&mut out, n);
        let l = if g.linkage == Linkage::Import {
            Linkage::Export
        } else {
            g.linkage
        };
        let ctor = g
            .section
            .as_deref()
            .is_some_and(|s| s.starts_with(".init_array"));
        out.push(link_flags(l) | if ctor { INIT_ARRAY } else { 0 });
        put_u32(&mut out, g.align.max(1) as u32);
        put_u32(&mut out, bytes.len() as u32);
        out.extend_from_slice(&bytes);
        put_u32(&mut out, relocs.len() as u32);
        for (off, sym, addend) in relocs {
            put_u32(&mut out, off as u32);
            out.push(st.funcs.contains_key(&sym) as u8);
            put_str(&mut out, &sym);
            out.extend_from_slice(&(addend as i32).to_le_bytes());
        }
    }
    out
}

struct FL<'o, 'a, 'tcx> {
    o: &'o mut Obj<'a, 'tcx>,
    ctx: &'a Context,
    st: &'a State<'tcx>,
    b: FunctionBody,
    cur: WBlock,
    vals: FxHashMap<Value, Vals>,
    cconst: FxHashMap<Value, Vals>,
    blocks: FxHashMap<Ptr<BasicBlock>, WBlock>,
    terminated: bool,
    sp0: WV,
    fp: WV,
    frame_c: WV,
    mask_c: WV,
    frame: u64,
    frame_align: u64,
}

impl<'o, 'a, 'tcx> FL<'o, 'a, 'tcx> {
    fn new(o: &'o mut Obj<'a, 'tcx>, sig: Signature) -> Self {
        let b = FunctionBody::new(&o.m, sig);
        let cur = b.entry;
        let (ctx, st) = (o.ctx, o.st);
        let mut fl = FL {
            o,
            ctx,
            st,
            b,
            cur,
            vals: FxHashMap::default(),
            cconst: FxHashMap::default(),
            blocks: FxHashMap::default(),
            terminated: false,
            sp0: WV::invalid(),
            fp: WV::invalid(),
            frame_c: WV::invalid(),
            mask_c: WV::invalid(),
            frame: 0,
            frame_align: 16,
        };
        let sp = fl.o.sp;
        fl.sp0 = fl.op(O::GlobalGet { global_index: sp }, &[], WT::I32);
        fl.frame_c = fl.i32c(0);
        let t = fl.op(O::I32Sub, &[fl.sp0, fl.frame_c], WT::I32);
        fl.mask_c = fl.i32c(!15);
        fl.fp = fl.op(O::I32And, &[t, fl.mask_c], WT::I32);
        fl.op0(O::GlobalSet { global_index: sp }, &[fl.fp]);
        fl
    }

    fn finish(mut self) -> FunctionBody {
        let frame = crate::types::align_to(self.frame, self.frame_align) as u32;
        let mask = !(self.frame_align as u32 - 1);
        for (v, c) in [(self.frame_c, frame), (self.mask_c, mask)] {
            if let ValueDef::Operator(op, ..) = &mut self.b.values[v] {
                *op = O::I32Const { value: c };
            }
        }
        self.b
    }

    fn op(&mut self, o: O, args: &[WV], ty: WT) -> WV {
        self.b.add_op(self.cur, o, args, &[ty])
    }

    fn op0(&mut self, o: O, args: &[WV]) {
        self.b.add_op(self.cur, o, args, &[]);
    }

    /// An op with several results: one value per result.
    fn opn(&mut self, o: O, args: &[WV], tys: &[WT]) -> Vals {
        let v = self.b.add_op(self.cur, o, args, tys);
        match tys.len() {
            0 => Vals::new(),
            1 => smallvec![v],
            _ => tys
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let p = self.b.add_value(ValueDef::PickOutput(v, i as u32, *t));
                    self.b.append_to_block(self.cur, p);
                    p
                })
                .collect(),
        }
    }

    fn i32c(&mut self, v: u32) -> WV {
        self.op(O::I32Const { value: v }, &[], WT::I32)
    }

    fn i64c(&mut self, v: u64) -> WV {
        self.op(O::I64Const { value: v }, &[], WT::I64)
    }

    fn ic(&mut self, w: u32, v: u64) -> WV {
        if w > 32 {
            self.i64c(v)
        } else {
            self.i32c(v as u32)
        }
    }

    fn ty_of(&self, v: WV) -> WT {
        match &self.b.values[v] {
            ValueDef::BlockParam(_, _, t)
            | ValueDef::PickOutput(_, _, t)
            | ValueDef::Placeholder(t) => *t,
            ValueDef::Operator(_, _, tys) => self.b.type_pool[*tys][0],
            ValueDef::Alias(a) => self.ty_of(*a),
            _ => panic!("type of {v}"),
        }
    }

    fn ma(&self, off: u64) -> MemoryArg {
        MemoryArg {
            align: 0,
            offset: off as u32,
            memory: self.o.mem,
        }
    }

    fn load(&mut self, t: ClType, p: WV, off: u64) -> WV {
        let memory = self.ma(off);
        let (o, ty) = match t {
            clt::I8 => (O::I32Load8U { memory }, WT::I32),
            clt::I16 => (O::I32Load16U { memory }, WT::I32),
            clt::I32 => (O::I32Load { memory }, WT::I32),
            clt::I64 => (O::I64Load { memory }, WT::I64),
            clt::F32 => (O::F32Load { memory }, WT::F32),
            clt::F64 => (O::F64Load { memory }, WT::F64),
            t => panic!("load {t}"),
        };
        self.op(o, &[p], ty)
    }

    fn store(&mut self, t: ClType, v: WV, p: WV, off: u64) {
        let memory = self.ma(off);
        let o = match t {
            clt::I8 => O::I32Store8 { memory },
            clt::I16 => O::I32Store16 { memory },
            clt::I32 => O::I32Store { memory },
            clt::I64 => O::I64Store { memory },
            clt::F32 => O::F32Store { memory },
            clt::F64 => O::F64Store { memory },
            t => panic!("store {t}"),
        };
        self.op0(o, &[p, v]);
    }

    fn slot(&mut self, size: u64, align: u64) -> WV {
        let align = align.max(1);
        self.frame_align = self.frame_align.max(align);
        let off = crate::types::align_to(self.frame, align);
        self.frame = off + size;
        self.add_imm(self.fp, off as i64)
    }

    fn add_imm(&mut self, x: WV, k: i64) -> WV {
        if k == 0 {
            return x;
        }
        let c = self.i32c(k as u32);
        self.op(O::I32Add, &[x, c], WT::I32)
    }

    fn sel(&mut self, c: WV, x: WV, y: WV) -> WV {
        let t = self.ty_of(x);
        self.op(O::Select, &[x, y, c], t)
    }

    /// Clear the bits above `w` (no-op for full-width containers).
    fn norm(&mut self, v: WV, w: u32) -> WV {
        match w {
            32 | 64 => v,
            w if w > 32 => {
                let m = self.i64c((1u64 << w) - 1);
                self.op(O::I64And, &[v, m], WT::I64)
            }
            w => {
                let m = self.i32c(((1u64 << w) - 1) as u32);
                self.op(O::I32And, &[v, m], WT::I32)
            }
        }
    }

    /// Sign-extend a `w`-bit value to its container.
    fn sext(&mut self, v: WV, w: u32) -> WV {
        match w {
            32 | 64 => v,
            8 => self.op(O::I32Extend8S, &[v], WT::I32),
            16 => self.op(O::I32Extend16S, &[v], WT::I32),
            w if w > 32 => {
                let s = self.i64c(64 - w as u64);
                let x = self.op(O::I64Shl, &[v, s], WT::I64);
                self.op(O::I64ShrS, &[x, s], WT::I64)
            }
            w => {
                let s = self.i32c(32 - w);
                let x = self.op(O::I32Shl, &[v, s], WT::I32);
                self.op(O::I32ShrS, &[x, s], WT::I32)
            }
        }
    }

    /// Convert between i32 and i64 containers.
    fn to_wide(&mut self, v: WV, wide: bool, signed: bool) -> WV {
        match (self.ty_of(v), wide) {
            (WT::I32, true) => self.op(
                if signed {
                    O::I64ExtendI32S
                } else {
                    O::I64ExtendI32U
                },
                &[v],
                WT::I64,
            ),
            (WT::I64, false) => self.op(O::I32WrapI64, &[v], WT::I32),
            _ => v,
        }
    }

    fn ib(&mut self, w: u32, o32: O, o64: O, a: WV, b: WV) -> WV {
        if w > 32 {
            self.op(o64, &[a, b], WT::I64)
        } else {
            self.op(o32, &[a, b], WT::I32)
        }
    }

    fn get(&mut self, v: Value) -> Vals {
        if let Some(x) = self.vals.get(&v) {
            return x.clone();
        }
        if let Some(x) = self.cconst.get(&v) {
            return x.clone();
        }
        let cv = self
            .st
            .consts
            .get(&v)
            .cloned()
            .unwrap_or_else(|| panic!("value used before definition"));
        let r = self.mat(v.get_type(self.ctx), cv);
        self.cconst.insert(v, r.clone());
        r
    }

    fn get1(&mut self, v: Value) -> WV {
        let x = self.get(v);
        assert_eq!(x.len(), 1, "expected a scalar");
        x[0]
    }

    fn leafc(&mut self, t: ClType, bits: u64) -> WV {
        match wty(t) {
            WT::I32 => self.i32c(bits as u32),
            WT::I64 => self.i64c(bits),
            WT::F32 => self.op(O::F32Const { value: bits as u32 }, &[], WT::F32),
            WT::F64 => self.op(O::F64Const { value: bits }, &[], WT::F64),
            _ => unreachable!(),
        }
    }

    fn mat(&mut self, ty: TypeHandle, cv: ConstVal) -> Vals {
        match cv {
            ConstVal::Bits(bits) => {
                let lv = wleaves(self.ctx, ty);
                match lv.len() {
                    1 => smallvec![self.leafc(lv[0].1, bits as u64)],
                    2 => smallvec![self.i64c(bits as u64), self.i64c((bits >> 64) as u64)],
                    n => panic!("{n}-leaf scalar constant"),
                }
            }
            ConstVal::Zero | ConstVal::Undef => wleaves(self.ctx, ty)
                .into_iter()
                .map(|(_, t)| self.leafc(t, 0))
                .collect(),
            ConstVal::Bytes(bs) => bs.iter().map(|b| self.i32c(*b as u32)).collect(),
            ConstVal::Agg(elems) => elems.iter().flat_map(|e| self.get(*e)).collect(),
            ConstVal::Sym { sym, off } => {
                let g = self.o.got(&sym);
                let base = self.op(O::GlobalGet { global_index: g }, &[], WT::I32);
                smallvec![self.add_imm(base, off)]
            }
        }
    }

    fn const_int(&self, v: Value) -> Option<i128> {
        let ConstVal::Bits(b) = self.st.consts.get(&v)? else {
            return if matches!(self.st.consts.get(&v)?, ConstVal::Zero) {
                Some(0)
            } else {
                None
            };
        };
        let w = match classify(self.ctx, v.get_type(self.ctx)) {
            TyK::Int(w) => w,
            _ => 32,
        };
        let b = *b;
        Some(if w < 128 && (b >> (w - 1)) & 1 == 1 {
            (b | (!0u128 << w)) as i128
        } else {
            b as i128
        })
    }

    fn set(&mut self, op: Ptr<Operation>, vals: Vals) {
        let r = op.deref(self.ctx).get_result(0);
        self.vals.insert(r, vals);
    }

    fn set1(&mut self, op: Ptr<Operation>, v: WV) {
        self.set(op, smallvec![v]);
    }

    fn res_ty(&self, op: Ptr<Operation>) -> TypeHandle {
        op.deref(self.ctx).get_result(0).get_type(self.ctx)
    }

    /// Bit width of an int/pointer type, or of a vector's element.
    fn width(&self, t: TypeHandle) -> u32 {
        match classify(self.ctx, t) {
            TyK::Int(w) => w,
            TyK::Ptr => 32,
            TyK::Vector(e, _) => self.width(e),
            k => panic!("int width of {k:?}"),
        }
    }

    /// Call a C-ABI runtime function. As in rustc's wasm32 C ABI, a 128-bit
    /// result is returned through a pointer passed as the first argument.
    fn call_named(&mut self, name: &str, args: &[WV], params: &[WT], rets: &[WT]) -> Vals {
        if rets == [WT::I64, WT::I64] {
            let ret = self.slot(16, 16);
            let ps: Vec<WT> = std::iter::once(WT::I32)
                .chain(params.iter().copied())
                .collect();
            let i = self.o.libfunc(
                name,
                SignatureData {
                    params: ps,
                    returns: vec![],
                },
            );
            let a: Vec<WV> = std::iter::once(ret).chain(args.iter().copied()).collect();
            self.opn(
                O::Call {
                    function_index: Func::new(i),
                },
                &a,
                &[],
            );
            return smallvec![self.load(clt::I64, ret, 0), self.load(clt::I64, ret, 8)];
        }
        let i = self.o.libfunc(
            name,
            SignatureData {
                params: params.to_vec(),
                returns: rets.to_vec(),
            },
        );
        self.opn(
            O::Call {
                function_index: Func::new(i),
            },
            args,
            rets,
        )
    }

    fn lower(&mut self, f: Ptr<Operation>) {
        let ctx = self.ctx;
        let region = f.deref(ctx).get_region(0);
        let pblocks: Vec<Ptr<BasicBlock>> = region.deref(ctx).iter(ctx).collect();
        let entry = self.b.entry;
        self.blocks.insert(pblocks[0], entry);
        for pb in &pblocks[1..] {
            let wb = self.b.add_block();
            self.blocks.insert(*pb, wb);
            let args: Vec<Value> = pb.deref(ctx).arguments().collect();
            for a in args {
                let vs: Vals = wleaves(ctx, a.get_type(ctx))
                    .into_iter()
                    .map(|(_, t)| self.b.add_blockparam(wb, wty(t)))
                    .collect();
                self.vals.insert(a, vs);
            }
        }
        let params: Vec<WV> = self.b.blocks[entry].params.iter().map(|p| p.1).collect();
        let mut i = 0;
        let args: Vec<Value> = pblocks[0].deref(ctx).arguments().collect();
        for arg in args {
            let n = wleaves(ctx, arg.get_type(ctx)).len();
            self.vals.insert(arg, params[i..i + n].into());
            i += n;
        }
        for pb in rpo(ctx, self.st, &pblocks) {
            self.cur = self.blocks[&pb];
            self.cconst.clear();
            self.terminated = false;
            let ops: Vec<Ptr<Operation>> = pb.deref(ctx).iter(ctx).collect();
            for op in ops {
                if self.terminated {
                    self.cur = self.b.add_block();
                    self.cconst.clear();
                    self.terminated = false;
                }
                self.lower_op(op);
            }
            if !self.terminated {
                let c = self.cur;
                self.b.set_terminator(c, Terminator::Unreachable);
            }
        }
    }

    fn target(&mut self, pb: Ptr<BasicBlock>, args: &[Value]) -> BlockTarget {
        let args = args.iter().flat_map(|v| self.get(*v)).collect();
        BlockTarget {
            block: self.blocks[&pb],
            args,
        }
    }

    fn term(&mut self, t: Terminator) {
        let c = self.cur;
        self.b.set_terminator(c, t);
        self.terminated = true;
    }

    fn icmp(&mut self, pred: ICmpPredicateAttr, a: &[WV], b: &[WV], w: u32) -> WV {
        use ICmpPredicateAttr as P;
        if w == 128 {
            let lo_eq = self.op(O::I64Eq, &[a[0], b[0]], WT::I32);
            let hi_eq = self.op(O::I64Eq, &[a[1], b[1]], WT::I32);
            let eq = self.op(O::I32And, &[lo_eq, hi_eq], WT::I32);
            return match pred {
                P::EQ => eq,
                P::NE => self.op(O::I32Eqz, &[eq], WT::I32),
                _ => {
                    let (hs, lo) = match pred {
                        P::SLT => (O::I64LtS, O::I64LtU),
                        P::SLE => (O::I64LtS, O::I64LeU),
                        P::SGT => (O::I64GtS, O::I64GtU),
                        P::SGE => (O::I64GtS, O::I64GeU),
                        P::ULT => (O::I64LtU, O::I64LtU),
                        P::ULE => (O::I64LtU, O::I64LeU),
                        P::UGT => (O::I64GtU, O::I64GtU),
                        P::UGE => (O::I64GtU, O::I64GeU),
                        _ => unreachable!(),
                    };
                    let h = self.op(hs, &[a[1], b[1]], WT::I32);
                    let l = self.op(lo, &[a[0], b[0]], WT::I32);
                    self.sel(hi_eq, l, h)
                }
            };
        }
        let signed = matches!(pred, P::SLT | P::SLE | P::SGT | P::SGE);
        let (x, y) = if signed {
            (self.sext(a[0], w), self.sext(b[0], w))
        } else {
            (a[0], b[0])
        };
        let (o32, o64) = match pred {
            P::EQ => (O::I32Eq, O::I64Eq),
            P::NE => (O::I32Ne, O::I64Ne),
            P::SLT => (O::I32LtS, O::I64LtS),
            P::SLE => (O::I32LeS, O::I64LeS),
            P::SGT => (O::I32GtS, O::I64GtS),
            P::SGE => (O::I32GeS, O::I64GeS),
            P::ULT => (O::I32LtU, O::I64LtU),
            P::ULE => (O::I32LeU, O::I64LeU),
            P::UGT => (O::I32GtU, O::I64GtU),
            P::UGE => (O::I32GeU, O::I64GeU),
        };
        let o = if w > 32 { o64 } else { o32 };
        self.op(o, &[x, y], WT::I32)
    }

    fn fcmp(&mut self, pred: FCmpPredicateAttr, a: WV, b: WV) -> WV {
        use FCmpPredicateAttr as P;
        let f64 = self.ty_of(a) == WT::F64;
        let c = |o: &str| -> O {
            match (o, f64) {
                ("eq", false) => O::F32Eq,
                ("ne", false) => O::F32Ne,
                ("lt", false) => O::F32Lt,
                ("le", false) => O::F32Le,
                ("gt", false) => O::F32Gt,
                ("ge", false) => O::F32Ge,
                ("eq", true) => O::F64Eq,
                ("ne", true) => O::F64Ne,
                ("lt", true) => O::F64Lt,
                ("le", true) => O::F64Le,
                ("gt", true) => O::F64Gt,
                _ => O::F64Ge,
            }
        };
        let cmp = |s: &mut Self, o: &str, x: WV, y: WV| s.op(c(o), &[x, y], WT::I32);
        let not = |s: &mut Self, x: WV| s.op(O::I32Eqz, &[x], WT::I32);
        match pred {
            P::False => self.i32c(0),
            P::True => self.i32c(1),
            P::OEQ => cmp(self, "eq", a, b),
            P::OGT => cmp(self, "gt", a, b),
            P::OGE => cmp(self, "ge", a, b),
            P::OLT => cmp(self, "lt", a, b),
            P::OLE => cmp(self, "le", a, b),
            P::UNE => cmp(self, "ne", a, b),
            P::ONE | P::UEQ => {
                let l = cmp(self, "lt", a, b);
                let g = cmp(self, "gt", a, b);
                let one = self.op(O::I32Or, &[l, g], WT::I32);
                if matches!(pred, P::ONE) {
                    one
                } else {
                    not(self, one)
                }
            }
            P::ORD | P::UNO => {
                let x = cmp(self, "eq", a, a);
                let y = cmp(self, "eq", b, b);
                let ord = self.op(O::I32And, &[x, y], WT::I32);
                if matches!(pred, P::ORD) {
                    ord
                } else {
                    not(self, ord)
                }
            }
            P::UGT => {
                let r = cmp(self, "le", a, b);
                not(self, r)
            }
            P::UGE => {
                let r = cmp(self, "lt", a, b);
                not(self, r)
            }
            P::ULT => {
                let r = cmp(self, "ge", a, b);
                not(self, r)
            }
            P::ULE => {
                let r = cmp(self, "gt", a, b);
                not(self, r)
            }
        }
    }

    fn lower_op(&mut self, op: Ptr<Operation>) {
        let ctx = self.ctx;
        let id = Operation::get_opid(op, ctx);
        let (opnds, succs): (Vec<Value>, Vec<Ptr<BasicBlock>>) = {
            let o = op.deref(ctx);
            (o.operands().collect(), o.successors().collect())
        };
        macro_rules! is {
            ($t:ty) => {
                id == <$t>::get_opid_static()
            };
        }
        // Lane-wise integer op; `fix` re-normalizes narrow results.
        macro_rules! intop {
            ($o32:expr, $o64:expr, $pre:ident, $fix:expr) => {{
                let w = self.width(self.res_ty(op));
                if w == 128 {
                    return self.i128_op(op, id, &opnds);
                }
                let a = self.get(opnds[0]);
                let b = self.get(opnds[1]);
                let mut r = Vals::new();
                for (x, y) in a.into_iter().zip(b) {
                    let (x, y) = self.$pre(x, y, w);
                    let v = self.ib(w, $o32, $o64, x, y);
                    r.push(if $fix { self.norm(v, w) } else { v });
                }
                self.set(op, r);
            }};
        }
        macro_rules! fop {
            ($o32:expr, $o64:expr) => {{
                let a = self.get(opnds[0]);
                let b = self.get(opnds[1]);
                let r = a
                    .into_iter()
                    .zip(b)
                    .map(|(x, y)| {
                        let f64 = self.ty_of(x) == WT::F64;
                        self.op(
                            if f64 { $o64 } else { $o32 },
                            &[x, y],
                            if f64 { WT::F64 } else { WT::F32 },
                        )
                    })
                    .collect();
                self.set(op, r);
            }};
        }

        if is!(ReturnOp) {
            let values = match opnds.first() {
                Some(v) => self.get(*v).into_vec(),
                None => vec![],
            };
            let (sp, sp0) = (self.o.sp, self.sp0);
            self.op0(O::GlobalSet { global_index: sp }, &[sp0]);
            self.term(Terminator::Return { values });
        } else if is!(UnreachableOp) {
            self.term(Terminator::Unreachable);
        } else if is!(BrOp) {
            let t = self.target(succs[0], &opnds);
            self.term(Terminator::Br { target: t });
        } else if is!(CondBrOp) {
            let c = self.get1(opnds[0]);
            let cb = Operation::get_op::<CondBrOp>(op, ctx).unwrap();
            let ta = cb.get_true_dest_operands(ctx);
            let ea = cb.get_false_dest_operands(ctx);
            let if_true = self.target(succs[0], &ta);
            let if_false = self.target(succs[1], &ea);
            self.term(Terminator::CondBr {
                cond: c,
                if_true,
                if_false,
            });
        } else if is!(ICmpOp) {
            let pred = Operation::get_op::<ICmpOp>(op, ctx).unwrap().predicate(ctx);
            let w = self.width(opnds[0].get_type(ctx));
            let a = self.get(opnds[0]);
            let b = self.get(opnds[1]);
            let r = if w == 128 {
                smallvec![self.icmp(pred, &a, &b, w)]
            } else {
                a.iter()
                    .zip(b.iter())
                    .map(|(x, y)| self.icmp(pred.clone(), &[*x], &[*y], w))
                    .collect()
            };
            self.set(op, r);
        } else if is!(FCmpOp) {
            let pred = Operation::get_op::<FCmpOp>(op, ctx).unwrap().predicate(ctx);
            let a = self.get(opnds[0]);
            let b = self.get(opnds[1]);
            let r = a
                .iter()
                .zip(b.iter())
                .map(|(x, y)| self.fcmp(pred.clone(), *x, *y))
                .collect();
            self.set(op, r);
        } else if is!(AddOp) {
            intop!(O::I32Add, O::I64Add, raw2, true)
        } else if is!(SubOp) {
            intop!(O::I32Sub, O::I64Sub, raw2, true)
        } else if is!(MulOp) {
            intop!(O::I32Mul, O::I64Mul, raw2, true)
        } else if is!(AndOp) {
            intop!(O::I32And, O::I64And, raw2, false)
        } else if is!(OrOp) {
            intop!(O::I32Or, O::I64Or, raw2, false)
        } else if is!(XorOp) {
            intop!(O::I32Xor, O::I64Xor, raw2, false)
        } else if is!(ShlOp) {
            intop!(O::I32Shl, O::I64Shl, raw2, true)
        } else if is!(LShrOp) {
            intop!(O::I32ShrU, O::I64ShrU, raw2, false)
        } else if is!(AShrOp) {
            intop!(O::I32ShrS, O::I64ShrS, sext1, true)
        } else if is!(UDivOp) {
            intop!(O::I32DivU, O::I64DivU, raw2, false)
        } else if is!(URemOp) {
            intop!(O::I32RemU, O::I64RemU, raw2, false)
        } else if is!(SDivOp) {
            intop!(O::I32DivS, O::I64DivS, sext2, true)
        } else if is!(SRemOp) {
            intop!(O::I32RemS, O::I64RemS, sext2, true)
        } else if is!(FAddOp) {
            fop!(O::F32Add, O::F64Add)
        } else if is!(FSubOp) {
            fop!(O::F32Sub, O::F64Sub)
        } else if is!(FMulOp) {
            fop!(O::F32Mul, O::F64Mul)
        } else if is!(FDivOp) {
            fop!(O::F32Div, O::F64Div)
        } else if is!(FRemOp) {
            let a = self.get1(opnds[0]);
            let b = self.get1(opnds[1]);
            let t = self.ty_of(a);
            let r = self.call_named(
                if t == WT::F32 { "fmodf" } else { "fmod" },
                &[a, b],
                &[t, t],
                &[t],
            )[0];
            self.set1(op, r);
        } else if is!(FNegOp) {
            let a = self.get(opnds[0]);
            let r = a
                .into_iter()
                .map(|x| {
                    let t = self.ty_of(x);
                    self.op(if t == WT::F64 { O::F64Neg } else { O::F32Neg }, &[x], t)
                })
                .collect();
            self.set(op, r);
        } else if is!(TruncOp) || is!(ZExtOp) || is!(SExtOp) || is!(PtrToIntOp) || is!(IntToPtrOp) {
            let sw = self.width(opnds[0].get_type(ctx));
            let dw = self.width(self.res_ty(op));
            let xs = self.get(opnds[0]);
            let signed = is!(SExtOp);
            let lanes = if sw == 128 { 1 } else { xs.len() };
            let mut r = Vals::new();
            for l in 0..lanes {
                let x = if sw == 128 { xs[0] } else { xs[l] };
                let hi_in = if sw == 128 { Some(xs[1]) } else { None };
                let x = if signed { self.sext(x, sw.min(64)) } else { x };
                if dw == 128 {
                    let lo = self.to_wide(x, true, signed);
                    let hi = match hi_in {
                        Some(h) => h,
                        None if signed => {
                            let s = self.i64c(63);
                            self.op(O::I64ShrS, &[lo, s], WT::I64)
                        }
                        None => self.i64c(0),
                    };
                    r.push(lo);
                    r.push(hi);
                } else {
                    let v = self.to_wide(x, dw > 32, signed);
                    r.push(self.norm(v, dw));
                }
            }
            self.set(op, r);
        } else if is!(FPTruncOp) || is!(FPExtOp) {
            let x = self.get(opnds[0]);
            let from = wleaves(ctx, opnds[0].get_type(ctx));
            let to = wleaves(ctx, self.res_ty(op));
            let fk = |s: &[(u64, ClType)]| match (s.len(), s[0].1) {
                (2, _) => "tf",
                (_, clt::I16) => "hf",
                (_, clt::F32) => "sf",
                _ => "df",
            };
            let (f, t) = (fk(&from), fk(&to));
            let r: Vals = match (f, t) {
                ("df", "sf") => smallvec![self.op(O::F32DemoteF64, &[x[0]], WT::F32)],
                ("sf", "df") => smallvec![self.op(O::F64PromoteF32, &[x[0]], WT::F64)],
                _ => {
                    let name = format!(
                        "__{}{f}{t}2",
                        if is!(FPTruncOp) { "trunc" } else { "extend" }
                    );
                    let ps: Vec<WT> = from.iter().map(|l| wty(l.1)).collect();
                    let rs: Vec<WT> = to.iter().map(|l| wty(l.1)).collect();
                    self.call_named(&name, &x, &ps, &rs)
                }
            };
            self.set(op, r);
        } else if is!(FPToUIOp) || is!(FPToSIOp) {
            let x = self.get1(opnds[0]);
            let w = self.width(self.res_ty(op));
            let r = self.fcvt_sat(is!(FPToSIOp), w, x);
            self.set(op, r);
        } else if is!(UIToFPOp) || is!(SIToFPOp) {
            let xs = self.get(opnds[0]);
            let w = self.width(opnds[0].get_type(ctx));
            let signed = is!(SIToFPOp);
            let t = wty(wleaves(ctx, self.res_ty(op))[0].1);
            let f32 = t == WT::F32;
            let r = if w == 128 {
                let name = match (signed, f32) {
                    (true, true) => "__floattisf",
                    (true, false) => "__floattidf",
                    (false, true) => "__floatuntisf",
                    (false, false) => "__floatuntidf",
                };
                self.call_named(name, &xs, &[WT::I64, WT::I64], &[t])[0]
            } else {
                let x = if signed { self.sext(xs[0], w) } else { xs[0] };
                let o = match (w > 32, signed, f32) {
                    (false, true, true) => O::F32ConvertI32S,
                    (false, false, true) => O::F32ConvertI32U,
                    (true, true, true) => O::F32ConvertI64S,
                    (true, false, true) => O::F32ConvertI64U,
                    (false, true, false) => O::F64ConvertI32S,
                    (false, false, false) => O::F64ConvertI32U,
                    (true, true, false) => O::F64ConvertI64S,
                    (true, false, false) => O::F64ConvertI64U,
                };
                self.op(o, &[x], t)
            };
            self.set1(op, r);
        } else if is!(BitcastOp) || is!(AddrSpaceCastOp) || is!(FreezeOp) {
            let xs = self.get(opnds[0]);
            let sl = wleaves(ctx, opnds[0].get_type(ctx));
            let dl = wleaves(ctx, self.res_ty(op));
            let r: Vals = if sl.iter().map(|l| l.1).eq(dl.iter().map(|l| l.1)) {
                xs
            } else if xs.len() == 1 && dl.len() == 1 && sl[0].1.bits() == dl[0].1.bits() {
                let o = match (self.ty_of(xs[0]), wty(dl[0].1)) {
                    (WT::I32, WT::F32) => O::F32ReinterpretI32,
                    (WT::F32, WT::I32) => O::I32ReinterpretF32,
                    (WT::I64, WT::F64) => O::F64ReinterpretI64,
                    (WT::F64, WT::I64) => O::I64ReinterpretF64,
                    (a, b) => panic!("bitcast {a} -> {b}"),
                };
                smallvec![self.op(o, &[xs[0]], wty(dl[0].1))]
            } else {
                let (sz, _) = size_align(ctx, self.res_ty(op));
                let slot = self.slot(sz.max(16), 16);
                for (x, (o, t)) in xs.iter().zip(sl) {
                    self.store(t, *x, slot, o);
                }
                dl.iter().map(|(o, t)| self.load(*t, slot, *o)).collect()
            };
            self.set(op, r);
        } else if is!(AllocaOp) {
            let res = op.deref(ctx).get_result(0);
            let (size, align) = self.st.allocas[&res];
            let p = self.slot(size, align);
            self.set1(op, p);
        } else if is!(LoadOp) || is!(AtomicLoadOp) {
            let p = self.get1(opnds[0]);
            let r = wleaves(ctx, self.res_ty(op))
                .into_iter()
                .map(|(o, t)| self.load(t, p, o))
                .collect();
            self.set(op, r);
        } else if is!(StoreOp) || is!(AtomicStoreOp) {
            let vs = self.get(opnds[0]);
            let p = self.get1(opnds[1]);
            for (v, (o, t)) in vs.into_iter().zip(wleaves(ctx, opnds[0].get_type(ctx))) {
                self.store(t, v, p, o);
            }
        } else if is!(AtomicRmwOp) {
            // Single-threaded: a plain load / op / store.
            let p = self.get1(opnds[0]);
            let v = self.get1(opnds[1]);
            let w = self.width(opnds[1].get_type(ctx));
            let t = wleaves(ctx, opnds[1].get_type(ctx))[0].1;
            let old = self.load(t, p, 0);
            let lt = |s: &mut Self, signed: bool| {
                let (x, y) = if signed {
                    (s.sext(old, w), s.sext(v, w))
                } else {
                    (old, v)
                };
                let o = match (w > 32, signed) {
                    (false, true) => O::I32LtS,
                    (false, false) => O::I32LtU,
                    (true, true) => O::I64LtS,
                    (true, false) => O::I64LtU,
                };
                s.op(o, &[x, y], WT::I32)
            };
            let new = match self.st.rmw[&op] {
                AtomicRmwBinOp::AtomicXchg => v,
                AtomicRmwBinOp::AtomicAdd => self.ib(w, O::I32Add, O::I64Add, old, v),
                AtomicRmwBinOp::AtomicSub => self.ib(w, O::I32Sub, O::I64Sub, old, v),
                AtomicRmwBinOp::AtomicAnd => self.ib(w, O::I32And, O::I64And, old, v),
                AtomicRmwBinOp::AtomicNand => {
                    let a = self.ib(w, O::I32And, O::I64And, old, v);
                    let m = self.ic(w, u64::MAX);
                    self.ib(w, O::I32Xor, O::I64Xor, a, m)
                }
                AtomicRmwBinOp::AtomicOr => self.ib(w, O::I32Or, O::I64Or, old, v),
                AtomicRmwBinOp::AtomicXor => self.ib(w, O::I32Xor, O::I64Xor, old, v),
                AtomicRmwBinOp::AtomicMax | AtomicRmwBinOp::AtomicUMax => {
                    let signed = matches!(self.st.rmw[&op], AtomicRmwBinOp::AtomicMax);
                    let c = lt(self, signed);
                    self.sel(c, v, old)
                }
                AtomicRmwBinOp::AtomicMin | AtomicRmwBinOp::AtomicUMin => {
                    let signed = matches!(self.st.rmw[&op], AtomicRmwBinOp::AtomicMin);
                    let c = lt(self, signed);
                    self.sel(c, old, v)
                }
            };
            self.store(t, new, p, 0);
            self.set1(op, old);
        } else if is!(AtomicCmpxchgOp) {
            let p = self.get1(opnds[0]);
            let c = self.get1(opnds[1]);
            let n = self.get1(opnds[2]);
            let w = self.width(opnds[1].get_type(ctx));
            let t = wleaves(ctx, opnds[1].get_type(ctx))[0].1;
            let old = self.load(t, p, 0);
            let ok = self.op(if w > 32 { O::I64Eq } else { O::I32Eq }, &[old, c], WT::I32);
            let new = self.sel(ok, n, old);
            self.store(t, new, p, 0);
            self.set(op, smallvec![old, ok]);
        } else if is!(FenceOp) {
        } else if is!(GetElementPtrOp) {
            self.lower_gep(op, opnds[0]);
        } else if is!(CallOp) {
            self.lower_call(op);
        } else if is!(CallIntrinsicOp) {
            self.lower_intrinsic(op, &opnds);
        } else if is!(SelectOp) {
            let c = self.get(opnds[0]);
            let a = self.get(opnds[1]);
            let b = self.get(opnds[2]);
            let r = a
                .iter()
                .zip(b.iter())
                .enumerate()
                .map(|(i, (x, y))| {
                    let ci = if c.len() == 1 { c[0] } else { c[i] };
                    self.sel(ci, *x, *y)
                })
                .collect();
            self.set(op, r);
        } else if is!(ExtractValueOp) {
            let idx = Operation::get_op::<ExtractValueOp>(op, ctx)
                .unwrap()
                .indices(ctx);
            let (s, n) = wleaf_range(ctx, opnds[0].get_type(ctx), &idx);
            let a = self.get(opnds[0]);
            self.set(op, a[s..s + n].into());
        } else if is!(InsertValueOp) {
            let idx = Operation::get_op::<InsertValueOp>(op, ctx)
                .unwrap()
                .indices(ctx);
            let (s, n) = wleaf_range(ctx, opnds[0].get_type(ctx), &idx);
            let mut a = self.get(opnds[0]);
            let v = self.get(opnds[1]);
            a.drain(s..s + n);
            a.insert_many(s, v);
            self.set(op, a);
        } else if is!(ExtractElementOp) {
            let a = self.get(opnds[0]);
            let r = match self.const_int(opnds[1]) {
                Some(i) => a[i as usize],
                None => {
                    let (p, es, t) = self.spill_vec(opnds[0], &a);
                    let addr = self.dyn_index(p, opnds[1], es);
                    self.load(t, addr, 0)
                }
            };
            self.set1(op, r);
        } else if is!(InsertElementOp) {
            let mut a = self.get(opnds[0]);
            let e = self.get1(opnds[1]);
            match self.const_int(opnds[2]) {
                Some(i) => a[i as usize] = e,
                None => {
                    let (p, es, t) = self.spill_vec(opnds[0], &a);
                    let addr = self.dyn_index(p, opnds[2], es);
                    self.store(t, e, addr, 0);
                    for (i, x) in a.iter_mut().enumerate() {
                        *x = self.load(t, p, i as u64 * es);
                    }
                }
            }
            self.set(op, a);
        } else if is!(UndefOp) || is!(PoisonOp) || is!(ZeroOp) {
            let r = wleaves(ctx, self.res_ty(op))
                .into_iter()
                .map(|(_, t)| self.leafc(t, 0))
                .collect();
            self.set(op, r);
        } else {
            panic!("pliron->wasm: unsupported op {id}");
        }
    }

    fn raw2(&mut self, x: WV, y: WV, _w: u32) -> (WV, WV) {
        (x, y)
    }

    fn sext1(&mut self, x: WV, y: WV, w: u32) -> (WV, WV) {
        (self.sext(x, w), y)
    }

    fn sext2(&mut self, x: WV, y: WV, w: u32) -> (WV, WV) {
        (self.sext(x, w), self.sext(y, w))
    }

    /// 128-bit integer arithmetic on (lo, hi) pairs.
    fn i128_op(&mut self, op: Ptr<Operation>, id: pliron::op::OpId, opnds: &[Value]) {
        macro_rules! is {
            ($t:ty) => {
                id == <$t>::get_opid_static()
            };
        }
        let a = self.get(opnds[0]);
        let b = self.get(opnds[1]);
        let i64s = [WT::I64; 4];
        let r: Vals = if is!(AndOp) || is!(OrOp) || is!(XorOp) {
            let o = if is!(AndOp) {
                O::I64And
            } else if is!(OrOp) {
                O::I64Or
            } else {
                O::I64Xor
            };
            smallvec![
                self.op(o, &[a[0], b[0]], WT::I64),
                self.op(o, &[a[1], b[1]], WT::I64)
            ]
        } else if is!(AddOp) {
            self.add128(&a, &b)
        } else if is!(SubOp) {
            self.sub128(&a, &b)
        } else if is!(MulOp) {
            let (lo, h) = self.mul64(a[0], b[0]);
            let x = self.op(O::I64Mul, &[a[0], b[1]], WT::I64);
            let y = self.op(O::I64Mul, &[a[1], b[0]], WT::I64);
            let h = self.op(O::I64Add, &[h, x], WT::I64);
            smallvec![lo, self.op(O::I64Add, &[h, y], WT::I64)]
        } else if is!(ShlOp) || is!(LShrOp) || is!(AShrOp) {
            self.shift128(id, &a, b[0])
        } else {
            let name = if is!(UDivOp) {
                "__udivti3"
            } else if is!(SDivOp) {
                "__divti3"
            } else if is!(URemOp) {
                "__umodti3"
            } else if is!(SRemOp) {
                "__modti3"
            } else {
                panic!("i128 {id}")
            };
            self.call_named(name, &[a[0], a[1], b[0], b[1]], &i64s, &[WT::I64, WT::I64])
        };
        self.set(op, r);
    }

    /// Full 64x64 -> 128-bit unsigned product as (lo, hi).
    fn mul64(&mut self, a: WV, b: WV) -> (WV, WV) {
        let m = self.i64c(0xffff_ffff);
        let s = self.i64c(32);
        let al = self.op(O::I64And, &[a, m], WT::I64);
        let ah = self.op(O::I64ShrU, &[a, s], WT::I64);
        let bl = self.op(O::I64And, &[b, m], WT::I64);
        let bh = self.op(O::I64ShrU, &[b, s], WT::I64);
        let p0 = self.op(O::I64Mul, &[al, bl], WT::I64);
        let p1 = self.op(O::I64Mul, &[al, bh], WT::I64);
        let p2 = self.op(O::I64Mul, &[ah, bl], WT::I64);
        let p3 = self.op(O::I64Mul, &[ah, bh], WT::I64);
        let p0h = self.op(O::I64ShrU, &[p0, s], WT::I64);
        let p1l = self.op(O::I64And, &[p1, m], WT::I64);
        let p2l = self.op(O::I64And, &[p2, m], WT::I64);
        let mid = self.op(O::I64Add, &[p0h, p1l], WT::I64);
        let mid = self.op(O::I64Add, &[mid, p2l], WT::I64);
        let p0l = self.op(O::I64And, &[p0, m], WT::I64);
        let midl = self.op(O::I64Shl, &[mid, s], WT::I64);
        let lo = self.op(O::I64Or, &[p0l, midl], WT::I64);
        let p1h = self.op(O::I64ShrU, &[p1, s], WT::I64);
        let p2h = self.op(O::I64ShrU, &[p2, s], WT::I64);
        let midh = self.op(O::I64ShrU, &[mid, s], WT::I64);
        let hi = self.op(O::I64Add, &[p3, p1h], WT::I64);
        let hi = self.op(O::I64Add, &[hi, p2h], WT::I64);
        let hi = self.op(O::I64Add, &[hi, midh], WT::I64);
        (lo, hi)
    }

    /// Branchless 128-bit shl/lshr/ashr by `amt` (low word of the i128 amount).
    fn shift128(&mut self, id: pliron::op::OpId, a: &[WV], amt: WV) -> Vals {
        let (lo, hi) = (a[0], a[1]);
        let c63 = self.i64c(63);
        let c64 = self.i64c(64);
        let c1 = self.i64c(1);
        let k = self.op(O::I64And, &[amt, c63], WT::I64);
        let inv = self.op(O::I64Sub, &[c63, k], WT::I64);
        let big = self.op(O::I64And, &[amt, c64], WT::I64);
        let big = self.op(O::I64Eqz, &[big], WT::I32);
        let big = self.op(O::I32Eqz, &[big], WT::I32);
        if id == ShlOp::get_opid_static() {
            let lo_sh = self.op(O::I64Shl, &[lo, k], WT::I64);
            let h = self.op(O::I64Shl, &[hi, k], WT::I64);
            let c = self.op(O::I64ShrU, &[lo, c1], WT::I64);
            let c = self.op(O::I64ShrU, &[c, inv], WT::I64);
            let hi_sh = self.op(O::I64Or, &[h, c], WT::I64);
            let z = self.i64c(0);
            smallvec![self.sel(big, z, lo_sh), self.sel(big, lo_sh, hi_sh)]
        } else {
            let arith = id == AShrOp::get_opid_static();
            let l = self.op(O::I64ShrU, &[lo, k], WT::I64);
            let c = self.op(O::I64Shl, &[hi, c1], WT::I64);
            let c = self.op(O::I64Shl, &[c, inv], WT::I64);
            let lo_sh = self.op(O::I64Or, &[l, c], WT::I64);
            let hi_sh = self.op(
                if arith { O::I64ShrS } else { O::I64ShrU },
                &[hi, k],
                WT::I64,
            );
            let fill = if arith {
                self.op(O::I64ShrS, &[hi, c63], WT::I64)
            } else {
                self.i64c(0)
            };
            smallvec![self.sel(big, hi_sh, lo_sh), self.sel(big, fill, hi_sh)]
        }
    }

    fn add128(&mut self, a: &[WV], b: &[WV]) -> Vals {
        let lo = self.op(O::I64Add, &[a[0], b[0]], WT::I64);
        let c = self.op(O::I64LtU, &[lo, a[0]], WT::I32);
        let c = self.op(O::I64ExtendI32U, &[c], WT::I64);
        let hi = self.op(O::I64Add, &[a[1], b[1]], WT::I64);
        let hi = self.op(O::I64Add, &[hi, c], WT::I64);
        smallvec![lo, hi]
    }

    fn sub128(&mut self, a: &[WV], b: &[WV]) -> Vals {
        let lo = self.op(O::I64Sub, &[a[0], b[0]], WT::I64);
        let c = self.op(O::I64LtU, &[a[0], b[0]], WT::I32);
        let c = self.op(O::I64ExtendI32U, &[c], WT::I64);
        let hi = self.op(O::I64Sub, &[a[1], b[1]], WT::I64);
        let hi = self.op(O::I64Sub, &[hi, c], WT::I64);
        smallvec![lo, hi]
    }

    fn spill_vec(&mut self, v: Value, a: &Vals) -> (WV, u64, ClType) {
        let (sz, al) = size_align(self.ctx, v.get_type(self.ctx));
        let p = self.slot(sz, al);
        let lv = wleaves(self.ctx, v.get_type(self.ctx));
        let es = sz / a.len() as u64;
        for (x, (o, t)) in a.iter().zip(&lv) {
            self.store(*t, *x, p, *o);
        }
        (p, es, lv[0].1)
    }

    /// `base + idx * scale` with the index converted to i32.
    fn dyn_index(&mut self, base: WV, idx: Value, scale: u64) -> WV {
        let w = self.width(idx.get_type(self.ctx));
        let xs = self.get(idx);
        let x = if w > 64 { xs[0] } else { self.sext(xs[0], w) };
        let x = self.to_wide(x, false, true);
        let x = if scale == 1 {
            x
        } else {
            let s = self.i32c(scale as u32);
            self.op(O::I32Mul, &[x, s], WT::I32)
        };
        self.op(O::I32Add, &[base, x], WT::I32)
    }

    fn lower_gep(&mut self, op: Ptr<Operation>, base: Value) {
        use pliron_llvm::ops::GepIndex;
        let ctx = self.ctx;
        let gep = Operation::get_op::<GetElementPtrOp>(op, ctx).unwrap();
        let idxs = gep.indices(ctx);
        let mut cur = gep.src_elem_type(ctx);
        let mut addr = self.get1(base);
        for (k, idx) in idxs.iter().enumerate() {
            let c = match idx {
                GepIndex::Constant(c) => Some(*c as i128),
                GepIndex::Value(v) => self.const_int(*v),
            };
            let scale;
            if k == 0 {
                scale = size_align(ctx, cur).0;
            } else {
                match classify(ctx, cur) {
                    TyK::Struct(fs, packed) => {
                        let i = c.expect("struct GEP index must be constant") as usize;
                        let offs = struct_offsets(ctx, &fs, packed).0;
                        addr = self.add_imm(addr, offs[i] as i64);
                        cur = fs[i];
                        continue;
                    }
                    TyK::Array(e, _) | TyK::Vector(e, _) => {
                        cur = e;
                        scale = size_align(ctx, e).0;
                    }
                    k => panic!("GEP into {k:?}"),
                }
            }
            match (c, idx) {
                (Some(c), _) => addr = self.add_imm(addr, (c as i64).wrapping_mul(scale as i64)),
                (None, GepIndex::Value(v)) => addr = self.dyn_index(addr, *v, scale),
                _ => unreachable!(),
            }
        }
        self.set1(op, addr);
    }

    fn lower_call(&mut self, op: Ptr<Operation>) {
        let ctx = self.ctx;
        let call = Operation::get_op::<CallOp>(op, ctx).unwrap();
        assert!(
            !self.st.invokes.contains_key(&op),
            "invoke on wasm (needs panic=abort)"
        );
        let info = &self.st.calls[&op];
        let fn_ty = info.fn_ty;
        let exts = info.exts.clone();
        let sigd = wsig(ctx, fn_ty);
        let rets = sigd.returns.clone();
        let mut wargs = Vec::new();
        let (nfixed, var_arg) = match classify(ctx, fn_ty) {
            TyK::Func(_, a, v) => (a.len(), v),
            _ => unreachable!(),
        };
        let mut va: Vec<WV> = Vec::new();
        for (i, a) in call.args(ctx).into_iter().enumerate() {
            if var_arg && i >= nfixed {
                va.extend(self.get(a));
                continue;
            }
            let vs = self.get(a);
            if let Some(ArgExt::ByVal(n)) = exts.params.get(i).copied() {
                let dst = self.slot(n as u64, 16);
                let len = self.i32c(n);
                let mem = self.o.mem;
                self.op0(
                    O::MemoryCopy {
                        dst_mem: mem,
                        src_mem: mem,
                    },
                    &[dst, vs[0], len],
                );
                wargs.push(dst);
            } else {
                wargs.extend(vs);
            }
        }
        if var_arg {
            // Each variadic argument at its natural alignment, as clang does.
            let mut off = 0u64;
            let lay: Vec<(ClType, u64)> = va
                .iter()
                .map(|&v| {
                    let (t, n) = match self.ty_of(v) {
                        WT::I32 => (clt::I32, 4),
                        WT::I64 => (clt::I64, 8),
                        WT::F32 => (clt::F32, 4),
                        WT::F64 => (clt::F64, 8),
                        t => panic!("variadic argument of type {t:?}"),
                    };
                    off = off.next_multiple_of(n);
                    let at = off;
                    off += n;
                    (t, at)
                })
                .collect();
            let buf = self.slot(off.max(1), 16);
            for (&v, &(t, at)) in va.iter().zip(&lay) {
                self.store(t, v, buf, at);
            }
            wargs.push(buf);
        }
        // core::arch::wasm32 memory intrinsics are declared as `llvm.*`
        // link_name functions, which become `__pliron_llvm_*` symbols.
        if let CallOpCallable::Direct(ident) = call.callee(ctx) {
            let st = self.st;
            let sym = &st.ident_to_sym[&ident.to_string()];
            let mem = self.o.mem;
            if sym.starts_with("__pliron_llvm_wasm_memory_grow") {
                let v = self.op(O::MemoryGrow { mem }, &[wargs[1]], WT::I32);
                return self.set(op, smallvec![v]);
            }
            if sym.starts_with("__pliron_llvm_wasm_memory_size") {
                let v = self.op(O::MemorySize { mem }, &[], WT::I32);
                return self.set(op, smallvec![v]);
            }
        }
        let direct = match call.callee(ctx) {
            CallOpCallable::Direct(ident) => {
                let sym = self.st.ident_to_sym[&ident.to_string()].clone();
                match self.o.fidx.get(&sym).copied() {
                    Some(i) if self.o.funcs[i].ty == Some(fn_ty) => Ok(i),
                    _ => Err(self.mat(fn_ty, ConstVal::Sym { sym, off: 0 })[0]),
                }
            }
            CallOpCallable::Indirect(v) => Err(self.get1(v)),
        };
        let rs = match direct {
            Ok(i) => self.opn(
                O::Call {
                    function_index: Func::new(i),
                },
                &wargs,
                &rets,
            ),
            Err(callee) => {
                let sig_index = self.o.sig(sigd);
                wargs.push(callee);
                let table_index = self.o.table;
                self.opn(
                    O::CallIndirect {
                        sig_index,
                        table_index,
                    },
                    &wargs,
                    &rets,
                )
            }
        };
        if op.deref(ctx).get_num_results() > 0 {
            self.set(op, rs);
        }
    }

    fn fcvt_sat(&mut self, signed: bool, w: u32, x: WV) -> Vals {
        let f64 = self.ty_of(x) == WT::F64;
        if w == 128 {
            let name = match (signed, f64) {
                (true, false) => "__fixsfti",
                (true, true) => "__fixdfti",
                (false, false) => "__fixunssfti",
                (false, true) => "__fixunsdfti",
            };
            let ft = self.ty_of(x);
            return self.call_named(name, &[x], &[ft], &[WT::I64, WT::I64]);
        }
        let wide = w > 32;
        let o = match (wide, signed, f64) {
            (false, true, false) => O::I32TruncSatF32S,
            (false, false, false) => O::I32TruncSatF32U,
            (false, true, true) => O::I32TruncSatF64S,
            (false, false, true) => O::I32TruncSatF64U,
            (true, true, false) => O::I64TruncSatF32S,
            (true, false, false) => O::I64TruncSatF32U,
            (true, true, true) => O::I64TruncSatF64S,
            (true, false, true) => O::I64TruncSatF64U,
        };
        let r = self.op(o, &[x], if wide { WT::I64 } else { WT::I32 });
        if w == 32 || w == 64 {
            return smallvec![r];
        }
        assert!(w < 32, "fptoint to i{w}");
        let r = if signed {
            let lo = self.i32c((-(1i64 << (w - 1))) as u32);
            let hi = self.i32c(((1i64 << (w - 1)) - 1) as u32);
            let c = self.op(O::I32LtS, &[r, lo], WT::I32);
            let r = self.sel(c, lo, r);
            let c = self.op(O::I32GtS, &[r, hi], WT::I32);
            self.sel(c, hi, r)
        } else {
            let hi = self.i32c(((1u64 << w) - 1) as u32);
            let c = self.op(O::I32GtU, &[r, hi], WT::I32);
            self.sel(c, hi, r)
        };
        smallvec![self.norm(r, w)]
    }

    fn bswap(&mut self, x: WV, w: u32) -> WV {
        let wide = w > 32;
        let sh = |s: &mut Self, x: WV, k: u64, left: bool| {
            let c = s.ic(w, k);
            let o = match (wide, left) {
                (false, true) => O::I32Shl,
                (false, false) => O::I32ShrU,
                (true, true) => O::I64Shl,
                (true, false) => O::I64ShrU,
            };
            s.op(o, &[x, c], s.ty_of(x))
        };
        let and = |s: &mut Self, x: WV, m: u64| {
            let c = s.ic(w, m);
            s.ib(w, O::I32And, O::I64And, x, c)
        };
        let or = |s: &mut Self, x: WV, y: WV| s.ib(w, O::I32Or, O::I64Or, x, y);
        // Swap adjacent groups of `k` bits selected by `m`.
        let swap = |s: &mut Self, x: WV, k: u64, m: u64| {
            let a = and(s, x, m);
            let a = sh(s, a, k, true);
            let b = sh(s, x, k, false);
            let b = and(s, b, m);
            or(s, a, b)
        };
        match w {
            8 => x,
            16 => {
                let r = swap(self, x, 8, 0x00ff);
                self.norm(r, 16)
            }
            32 => {
                let x = swap(self, x, 8, 0x00ff_00ff);
                let c = self.i32c(16);
                self.op(O::I32Rotl, &[x, c], WT::I32)
            }
            64 => {
                let x = swap(self, x, 8, 0x00ff_00ff_00ff_00ff);
                let x = swap(self, x, 16, 0x0000_ffff_0000_ffff);
                let c = self.i64c(32);
                self.op(O::I64Rotl, &[x, c], WT::I64)
            }
            w => panic!("bswap i{w}"),
        }
    }

    fn bitrev(&mut self, x: WV, w: u32) -> WV {
        let wide = w > 32;
        let mut x = x;
        for (k, m) in [
            (1u64, 0x5555_5555_5555_5555u64),
            (2, 0x3333_3333_3333_3333),
            (4, 0x0f0f_0f0f_0f0f_0f0f),
        ] {
            let m = if wide { m } else { m & 0xffff_ffff };
            let c = self.ic(w, k);
            let mm = self.ic(w, m);
            let (shl, shr, and, or) = if wide {
                (O::I64Shl, O::I64ShrU, O::I64And, O::I64Or)
            } else {
                (O::I32Shl, O::I32ShrU, O::I32And, O::I32Or)
            };
            let t = self.ty_of(x);
            let a = self.op(and, &[x, mm], t);
            let a = self.op(shl, &[a, c], t);
            let b = self.op(shr, &[x, c], t);
            let b = self.op(and, &[b, mm], t);
            x = self.op(or, &[a, b], t);
        }
        if w == 8 {
            self.norm(x, 8)
        } else {
            self.bswap(x, w)
        }
    }

    fn lower_intrinsic(&mut self, op: Ptr<Operation>, opnds: &[Value]) {
        let name = self.st.intrinsics[&op].clone();
        let name = name.as_str();
        let ctx = self.ctx;
        let w = |s: &Self, i: usize| s.width(opnds[i].get_type(ctx));
        match name {
            "llvm.memcpy" | "llvm.memmove" | "llvm.memset" => {
                let d = self.get1(opnds[0]);
                let s = self.get1(opnds[1]);
                let n = self.get(opnds[2]);
                let n = self.to_wide(n[0], false, false);
                let mem = self.o.mem;
                if name == "llvm.memset" {
                    self.op0(O::MemoryFill { mem }, &[d, s, n]);
                } else {
                    self.op0(
                        O::MemoryCopy {
                            dst_mem: mem,
                            src_mem: mem,
                        },
                        &[d, s, n],
                    );
                }
                return;
            }
            "llvm.trap" => {
                self.term(Terminator::Unreachable);
                return;
            }
            _ => {}
        }
        let a: Vec<Vals> = opnds.iter().map(|v| self.get(*v)).collect();
        let a0 = a[0][0];
        let r: Vals = match name {
            "llvm.ctpop" | "llvm.ctlz" | "llvm.cttz" if w(self, 0) == 128 => {
                let (lo, hi) = (a[0][0], a[0][1]);
                let r = match name {
                    "llvm.ctpop" => {
                        let x = self.op(O::I64Popcnt, &[lo], WT::I64);
                        let y = self.op(O::I64Popcnt, &[hi], WT::I64);
                        self.op(O::I64Add, &[x, y], WT::I64)
                    }
                    _ => {
                        let (first, second, o) = if name == "llvm.ctlz" {
                            (hi, lo, O::I64Clz)
                        } else {
                            (lo, hi, O::I64Ctz)
                        };
                        let z = self.op(O::I64Eqz, &[first], WT::I32);
                        let f = self.op(o, &[first], WT::I64);
                        let s = self.op(o, &[second], WT::I64);
                        let c = self.i64c(64);
                        let s = self.op(O::I64Add, &[s, c], WT::I64);
                        self.sel(z, s, f)
                    }
                };
                smallvec![r, self.i64c(0)]
            }
            "llvm.ctpop" => {
                let w = w(self, 0);
                smallvec![if w > 32 {
                    self.op(O::I64Popcnt, &[a0], WT::I64)
                } else {
                    self.op(O::I32Popcnt, &[a0], WT::I32)
                }]
            }
            "llvm.ctlz" => {
                let w = w(self, 0);
                let (o, k) = if w > 32 {
                    (O::I64Clz, 64)
                } else {
                    (O::I32Clz, 32)
                };
                let t = self.ty_of(a0);
                let r = self.op(o, &[a0], t);
                smallvec![if w == k {
                    r
                } else {
                    let c = self.ic(w, (k - w) as u64);
                    self.ib(w, O::I32Sub, O::I64Sub, r, c)
                }]
            }
            "llvm.cttz" => {
                let w = w(self, 0);
                let (o, k) = if w > 32 {
                    (O::I64Ctz, 64)
                } else {
                    (O::I32Ctz, 32)
                };
                let x = if w == k {
                    a0
                } else {
                    let c = self.ic(w, 1u64 << w);
                    self.ib(w, O::I32Or, O::I64Or, a0, c)
                };
                let t = self.ty_of(x);
                smallvec![self.op(o, &[x], t)]
            }
            "llvm.bswap" if w(self, 0) == 128 => {
                let lo = self.bswap(a[0][1], 64);
                let hi = self.bswap(a[0][0], 64);
                smallvec![lo, hi]
            }
            "llvm.bswap" => {
                let w = w(self, 0);
                smallvec![self.bswap(a0, w)]
            }
            "llvm.bitreverse" if w(self, 0) == 128 => {
                let lo = self.bitrev(a[0][1], 64);
                let hi = self.bitrev(a[0][0], 64);
                smallvec![lo, hi]
            }
            "llvm.bitreverse" => {
                let w = w(self, 0);
                smallvec![self.bitrev(a0, w)]
            }
            "llvm.fshl" | "llvm.fshr" => {
                let w = w(self, 0);
                assert!(w <= 64, "funnel shift i{w}");
                let (x, y) = (a[0][0], a[1][0]);
                let m = self.ic(w, w as u64 - 1);
                let s = self.ib(w, O::I32And, O::I64And, a[2][0], m);
                let left = name == "llvm.fshl";
                if x == y && (w == 32 || w == 64) {
                    let o = match (w > 32, left) {
                        (false, true) => O::I32Rotl,
                        (false, false) => O::I32Rotr,
                        (true, true) => O::I64Rotl,
                        (true, false) => O::I64Rotr,
                    };
                    let t = self.ty_of(x);
                    smallvec![self.op(o, &[x, s], t)]
                } else {
                    let wv = self.ic(w, w as u64);
                    let inv = self.ib(w, O::I32Sub, O::I64Sub, wv, s);
                    let (hi, lo) = if left {
                        (
                            self.ib(w, O::I32Shl, O::I64Shl, x, s),
                            self.ib(w, O::I32ShrU, O::I64ShrU, y, inv),
                        )
                    } else {
                        (
                            self.ib(w, O::I32Shl, O::I64Shl, x, inv),
                            self.ib(w, O::I32ShrU, O::I64ShrU, y, s),
                        )
                    };
                    let comb = self.ib(w, O::I32Or, O::I64Or, hi, lo);
                    let comb = self.norm(comb, w);
                    let zero = self.ic(w, 0);
                    let z = self.op(
                        if w > 32 { O::I64Eq } else { O::I32Eq },
                        &[s, zero],
                        WT::I32,
                    );
                    let keep = if left { x } else { y };
                    smallvec![self.sel(z, keep, comb)]
                }
            }
            n if n.ends_with(".with.overflow") => {
                let w = w(self, 0);
                let (r, of) = self.overflow_op(n, w, &a[0], &a[1]);
                let mut v = r;
                v.push(of);
                v
            }
            "llvm.fptoui.sat" | "llvm.fptosi.sat" => {
                let w = self.width(self.res_ty(op));
                self.fcvt_sat(name == "llvm.fptosi.sat", w, a0)
            }
            _ => {
                let t = self.ty_of(a0);
                let f64 = t == WT::F64;
                let un = |o32: O, o64: O| if f64 { o64 } else { o32 };
                let native = match name {
                    "llvm.sqrt" => Some(un(O::F32Sqrt, O::F64Sqrt)),
                    "llvm.fabs" => Some(un(O::F32Abs, O::F64Abs)),
                    "llvm.floor" => Some(un(O::F32Floor, O::F64Floor)),
                    "llvm.ceil" => Some(un(O::F32Ceil, O::F64Ceil)),
                    "llvm.trunc" => Some(un(O::F32Trunc, O::F64Trunc)),
                    "llvm.roundeven" => Some(un(O::F32Nearest, O::F64Nearest)),
                    "llvm.copysign" => Some(un(O::F32Copysign, O::F64Copysign)),
                    "llvm.minimum" => Some(un(O::F32Min, O::F64Min)),
                    "llvm.maximum" => Some(un(O::F32Max, O::F64Max)),
                    _ => None,
                };
                let args: Vec<WV> = a.iter().map(|v| v[0]).collect();
                if let Some(o) = native {
                    smallvec![self.op(o, &args, t)]
                } else if name == "llvm.fmuladd" {
                    let m = self.op(un(O::F32Mul, O::F64Mul), &args[..2], t);
                    smallvec![self.op(un(O::F32Add, O::F64Add), &[m, args[2]], t)]
                } else if matches!(
                    name,
                    "llvm.fma"
                        | "llvm.round"
                        | "llvm.sin"
                        | "llvm.cos"
                        | "llvm.exp"
                        | "llvm.exp2"
                        | "llvm.log"
                        | "llvm.log2"
                        | "llvm.log10"
                        | "llvm.pow"
                        | "llvm.minnum"
                        | "llvm.maxnum"
                ) && matches!(t, WT::F32 | WT::F64)
                {
                    let base = match &name["llvm.".len()..] {
                        "minnum" => "fmin",
                        "maxnum" => "fmax",
                        b => b,
                    };
                    let f = if f64 {
                        base.to_string()
                    } else {
                        format!("{base}f")
                    };
                    let ps = vec![t; args.len()];
                    self.call_named(&f, &args, &ps, &[t])
                } else {
                    panic!("pliron->wasm: unsupported intrinsic {name}")
                }
            }
        };
        self.set(op, r);
    }

    /// `llvm.{s,u}{add,sub,mul}.with.overflow`: (result leaves, overflow bit).
    fn overflow_op(&mut self, n: &str, w: u32, a: &[WV], b: &[WV]) -> (Vals, WV) {
        let signed = n.starts_with("llvm.s");
        let kind = &n["llvm.s".len()..n.len() - ".with.overflow".len()];
        if w == 128 {
            return match kind {
                "add" | "sub" => {
                    let r = if kind == "add" {
                        self.add128(a, b)
                    } else {
                        self.sub128(a, b)
                    };
                    let of = if signed {
                        // add: (a^r)&(b^r) < 0; sub: (a^b)&(a^r) < 0, on the high word.
                        let x = self.op(O::I64Xor, &[a[1], r[1]], WT::I64);
                        let y = if kind == "add" {
                            self.op(O::I64Xor, &[b[1], r[1]], WT::I64)
                        } else {
                            self.op(O::I64Xor, &[a[1], b[1]], WT::I64)
                        };
                        let m = self.op(O::I64And, &[x, y], WT::I64);
                        let z = self.i64c(0);
                        self.op(O::I64LtS, &[m, z], WT::I32)
                    } else if kind == "add" {
                        self.icmp(ICmpPredicateAttr::ULT, &r, a, 128)
                    } else {
                        self.icmp(ICmpPredicateAttr::ULT, a, b, 128)
                    };
                    (r, of)
                }
                _ => {
                    let slot = self.slot(4, 4);
                    let f = if signed {
                        "__rust_i128_mulo"
                    } else {
                        "__rust_u128_mulo"
                    };
                    let r = self.call_named(
                        f,
                        &[a[0], a[1], b[0], b[1], slot],
                        &[WT::I64, WT::I64, WT::I64, WT::I64, WT::I32],
                        &[WT::I64, WT::I64],
                    );
                    let o = self.load(clt::I32, slot, 0);
                    let z = self.i32c(0);
                    (r, self.op(O::I32Ne, &[o, z], WT::I32))
                }
            };
        }
        let (x, y) = (a[0], b[0]);
        if w < 32 {
            // Exact in i32: compare the full result against its truncation.
            let (x, y) = if signed {
                (self.sext(x, w), self.sext(y, w))
            } else {
                (x, y)
            };
            let o = match kind {
                "add" => O::I32Add,
                "sub" => O::I32Sub,
                _ => O::I32Mul,
            };
            let full = self.op(o, &[x, y], WT::I32);
            let r = self.norm(full, w);
            let back = if signed { self.sext(r, w) } else { r };
            let of = self.op(O::I32Ne, &[back, full], WT::I32);
            return (smallvec![r], of);
        }
        assert!(w == 32 || w == 64, "overflow op on i{w}");
        let wide = w == 64;
        let zero = self.ic(w, 0);
        let lt_s = if wide { O::I64LtS } else { O::I32LtS };
        let lt_u = if wide { O::I64LtU } else { O::I32LtU };
        match kind {
            "add" | "sub" => {
                let r = if kind == "add" {
                    self.ib(w, O::I32Add, O::I64Add, x, y)
                } else {
                    self.ib(w, O::I32Sub, O::I64Sub, x, y)
                };
                let of = if signed {
                    let p = self.ib(w, O::I32Xor, O::I64Xor, x, r);
                    let q = if kind == "add" {
                        self.ib(w, O::I32Xor, O::I64Xor, y, r)
                    } else {
                        self.ib(w, O::I32Xor, O::I64Xor, x, y)
                    };
                    let m = self.ib(w, O::I32And, O::I64And, p, q);
                    self.op(lt_s, &[m, zero], WT::I32)
                } else if kind == "add" {
                    self.op(lt_u, &[r, x], WT::I32)
                } else {
                    self.op(lt_u, &[x, y], WT::I32)
                };
                (smallvec![r], of)
            }
            _ if !wide => {
                let xw = self.to_wide(x, true, signed);
                let yw = self.to_wide(y, true, signed);
                let p = self.op(O::I64Mul, &[xw, yw], WT::I64);
                let r = self.op(O::I32WrapI64, &[p], WT::I32);
                let back = self.to_wide(r, true, signed);
                let of = self.op(O::I64Ne, &[back, p], WT::I32);
                (smallvec![r], of)
            }
            _ if signed => {
                let slot = self.slot(4, 4);
                let r = self.call_named(
                    "__mulodi4",
                    &[x, y, slot],
                    &[WT::I64, WT::I64, WT::I32],
                    &[WT::I64],
                )[0];
                let o = self.load(clt::I32, slot, 0);
                let z = self.i32c(0);
                (smallvec![r], self.op(O::I32Ne, &[o, z], WT::I32))
            }
            _ => {
                let (lo, hi) = self.mul64(x, y);
                let z = self.i64c(0);
                let of = self.op(O::I64Ne, &[hi, z], WT::I32);
                (smallvec![lo], of)
            }
        }
    }
}
