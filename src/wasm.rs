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
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
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
    /// panic=unwind: emulated EH (see `FL::eh_check`).
    unwind: bool,
    /// `PLIRON_WASM_TRIP` shared loop-trip counter (created lazily).
    trip: waffle::Global,
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

/// LLVM's `generic` wasm CPU features, adjusted by `-Ctarget-cpu=mvp` and
/// `-Ctarget-feature`. Tools like wasm-bindgen read them from `target_features`.
pub fn target_features(
    target: &rustc_target::spec::Target,
    opts: &rustc_session::config::Options,
) -> Vec<String> {
    let mut f: Vec<String> = if opts.cg.target_cpu.as_deref() == Some("mvp") {
        vec![]
    } else {
        [
            "bulk-memory",
            "multivalue",
            "mutable-globals",
            "nontrapping-fptoint",
            "reference-types",
            "sign-ext",
        ]
        .map(String::from)
        .to_vec()
    };
    let cg = opts.cg.target_feature.as_str();
    for t in target.features.split(',').chain(cg.split(',')).map(str::trim) {
        if let Some(n) = t.strip_prefix('+') {
            if !f.iter().any(|x| x == n) {
                f.push(n.into());
            }
        } else if let Some(n) = t.strip_prefix('-') {
            f.retain(|x| x != n);
        }
    }
    f
}

pub fn lower_to_wasm(
    ctx: &Context,
    st: &State<'_>,
    name: &str,
    features: &[String],
    unwind: bool,
) -> Vec<u8> {
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
        unwind,
        trip: waffle::Global::invalid(),
    };
    o.sp = o.import_global("env", "__stack_pointer", true);

    for (n, f) in &st.funcs {
        if st.dead_fns.contains(n) {
            continue;
        }
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
        if !has_body(ctx, f.op) || st.dead_fns.contains(n) {
            continue;
        }
        defined += 1;
        let i = o.fidx[n];
        let sig = o.funcs[i].sig;
        let body = match guarded(|| {
            let mut fl = FL::new(&mut o, sig);
            fl.name = n.clone();
            fl.lower(f.op);
            fl.finish()
        }) {
            Ok(b) => b,
            Err(e) => {
                stubs += 1;
                if let Ok(f) = std::env::var("PLIRON_WASM_STUBLOG") {
                    use std::fmt::Write as _;
                    let mut s = String::new();
                    writeln!(s, "stub {n}: {e}").unwrap();
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(f)
                        .map(|mut x| std::io::Write::write_all(&mut x, s.as_bytes()));
                }
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
                    .unwrap_or_else(|| ("env".into(), crate::obj_sym(&d.name).to_string()));
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
                        if let Ok(f) = std::env::var("PLIRON_WASM_STUBLOG") {
                            let _ = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(f)
                                .map(|mut x| {
                                    std::io::Write::write_all(
                                        &mut x,
                                        format!("stub {} (backend): {e}\n", d.name).as_bytes(),
                                    )
                                });
                        }
                        if verbose {
                            eprintln!("pliron-wasm: stub {} (backend): {e}", d.name);
                        }
                        stub(&o.m, d.sig).compile().unwrap().into_raw_body()
                    }
                };
                o.m.funcs
                    .push(FuncDecl::Compiled(d.sig, d.name.clone(), bytes));
                o.m.exports.push(Export {
                    name: crate::obj_sym(&d.name).to_string(),
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
    if std::env::var_os("PLIRON_WASM_PEEP")
        .map(|v| v != "0")
        .unwrap_or(true)
    {
        wpeep(&mut bytes);
    }
    custom_section(&mut bytes, "pliron.link", &link_section(ctx, st));
    let mut tf = Vec::new();
    leb(&mut tf, features.len() as u32);
    for f in features {
        tf.push(b'+');
        leb(&mut tf, f.len() as u32);
        tf.extend_from_slice(f.as_bytes());
    }
    custom_section(&mut bytes, "target_features", &tf);
    // `#[link_section = "name"]` statics are wasm custom sections, which the
    // linker concatenates (wasm-bindgen's `__wasm_bindgen_unstable` metadata).
    for (n, g) in &st.globals {
        if let (Some(sec), Some(init)) = (custom_section_name(g), g.init) {
            let (size, _) = size_align(ctx, init.get_type(ctx));
            let mut data = vec![0u8; size as usize];
            let mut relocs = Vec::new();
            write_const(ctx, st, init, 0, &mut data, &mut relocs);
            assert!(relocs.is_empty(), "pliron-wasm: {n}: relocations in custom section {sec}");
            custom_section(&mut bytes, sec, &data);
        }
    }
    bytes
}

fn custom_section_name(g: &crate::context::GlobalInfo) -> Option<&str> {
    g.section.as_deref().filter(|s| !s.starts_with('.'))
}

fn stub(m: &Module, sig: Signature) -> FunctionBody {
    let mut b = FunctionBody::new(m, sig);
    let e = b.entry;
    b.set_terminator(e, Terminator::Unreachable);
    b
}

/// Read a u32 LEB128 at `*p`, advancing past it.
fn urd(b: &[u8], p: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    let mut s = 0u32;
    loop {
        let c = *b.get(*p)?;
        *p += 1;
        if s < 32 {
            v |= ((c & 0x7f) as u32) << s;
        }
        if c & 0x80 == 0 {
            return Some(v);
        }
        s += 7;
        if s > 35 {
            return None;
        }
    }
}

/// Skip a signed LEB128 at `*p`.
fn sleb_skip(b: &[u8], p: &mut usize) -> Option<()> {
    loop {
        let c = *b.get(*p)?;
        *p += 1;
        if c & 0x80 == 0 {
            return Some(());
        }
    }
}

/// Skip `n` bytes at `*p`.
fn nskip(b: &[u8], p: &mut usize, n: usize) -> Option<()> {
    if b.len() - *p < n {
        return None;
    }
    *p += n;
    Some(())
}

#[derive(Clone, Copy)]
enum WPeek {
    Get(u32),
    Set(u32),
    Tee(u32),
    Other,
}

/// Decode one function body into instructions; rewrite the
/// local-shuffle noise waffle's SSA->locals lowering leaves behind:
/// `local.get x; local.set x` is a no-op pair, `local.set x; local.get
/// x` fuses to `local.tee x`, and `local.tee x; local.set x` is just
/// `local.set x`. All three edits keep the operand stack and locals
/// identical and only span adjacent instructions, so structured
/// control flow is unaffected (branch targets are label depths, not
/// byte offsets). Returns None — caller keeps the original bytes — on
/// any opcode it can't decode.
fn peep_body(b: &[u8]) -> Option<Vec<u8>> {
    // Locals prefix: vec of (count, valtype); copied verbatim.
    let mut p = 0usize;
    let nl = urd(b, &mut p)?;
    for _ in 0..nl {
        urd(b, &mut p)?;
        // Single-byte valtypes only (numeric/v128/funcref/externref);
        // typed refs (0x63/0x64 + heaptype) need a longer decode — bail.
        let t = *b.get(p)?;
        if !matches!(t, 0x6f | 0x70 | 0x78..=0x7f) {
            return None;
        }
        p += 1;
    }
    let locals_end = p;
    // Instruction stream: (kind, byte range). `depth` tracks structured
    // constructs so the scan stops on the `end` that closes the body.
    let mut ins: Vec<(WPeek, usize, usize)> = Vec::new();
    let mut depth = 0i32;
    loop {
        if p >= b.len() {
            return None;
        }
        let s = p;
        let op = b[p];
        p += 1;
        let k = match op {
            0x00 | 0x01 | 0x05 | 0x0f | 0x1a | 0x1b | 0xd1 => WPeek::Other,
            0x02 | 0x03 | 0x04 => {
                depth += 1;
                sleb_skip(b, &mut p)?; // blocktype: valtype or type index
                WPeek::Other
            }
            0x0b => {
                depth -= 1;
                ins.push((WPeek::Other, s, p));
                if depth < 0 {
                    break; // `end` closing the function body itself
                }
                continue;
            }
            0x0c | 0x0d | 0x10 | 0x12 | 0x25 | 0x26 | 0x3f | 0x40 | 0xd2 => {
                urd(b, &mut p)?;
                WPeek::Other
            }
            0x0e => {
                // br_table: n targets + default.
                let n = urd(b, &mut p)?;
                for _ in 0..=n {
                    urd(b, &mut p)?;
                }
                WPeek::Other
            }
            0x11 | 0x13 => {
                urd(b, &mut p)?;
                urd(b, &mut p)?;
                WPeek::Other
            }
            0x1c => {
                // select t: vec of valtypes. Multi-byte valtypes (typed
                // refs) aren't emitted here; bail if one shows up.
                let n = urd(b, &mut p)?;
                for _ in 0..n {
                    let t = *b.get(p)?;
                    if t == 0x63 || t == 0x64 {
                        return None;
                    }
                    p += 1;
                }
                WPeek::Other
            }
            0x20 => WPeek::Get(urd(b, &mut p)?),
            0x21 => WPeek::Set(urd(b, &mut p)?),
            0x22 => WPeek::Tee(urd(b, &mut p)?),
            0x23 | 0x24 => {
                urd(b, &mut p)?;
                WPeek::Other
            }
            0x28..=0x3e => {
                urd(b, &mut p)?; // align
                urd(b, &mut p)?; // offset
                WPeek::Other
            }
            0x41 | 0x42 => {
                sleb_skip(b, &mut p)?;
                WPeek::Other
            }
            0x43 => {
                nskip(b, &mut p, 4)?;
                WPeek::Other
            }
            0x44 => {
                nskip(b, &mut p, 8)?;
                WPeek::Other
            }
            0x45..=0xc4 => WPeek::Other,
            0xd0 => {
                sleb_skip(b, &mut p)?; // ref.null heaptype
                WPeek::Other
            }
            0xfc => {
                let sub = urd(b, &mut p)?;
                match sub {
                    0..=7 => {}
                    8 | 10 | 12 | 14 => {
                        urd(b, &mut p)?;
                        urd(b, &mut p)?;
                    }
                    9 | 11 | 13 | 15 | 16 | 17 => {
                        urd(b, &mut p)?;
                    }
                    _ => return None,
                }
                WPeek::Other
            }
            _ => return None, // 0xfb GC, 0xfd SIMD, 0xfe atomics, EH ops
        };
        ins.push((k, s, p));
    }
    if p != b.len() {
        return None; // trailing bytes after the body `end`
    }
    // Coalesce adjacent pairs until none rewrite.
    loop {
        let mut out: Vec<(WPeek, usize, usize)> = Vec::with_capacity(ins.len());
        let mut changed = false;
        let mut i = 0;
        while i < ins.len() {
            let pair = (ins[i].0, ins.get(i + 1).map(|x| x.0));
            match pair {
                (WPeek::Get(a), Some(WPeek::Set(c))) if a == c => {
                    changed = true;
                    i += 2;
                    continue;
                }
                (WPeek::Set(a), Some(WPeek::Get(c))) if a == c => {
                    out.push((WPeek::Tee(a), ins[i].1, ins[i].2));
                    changed = true;
                    i += 2;
                    continue;
                }
                (WPeek::Tee(a), Some(WPeek::Set(c))) if a == c => {
                    out.push((WPeek::Set(a), ins[i].1, ins[i].2));
                    changed = true;
                    i += 2;
                    continue;
                }
                _ => {}
            }
            out.push(ins[i]);
            i += 1;
        }
        ins = out;
        if !changed {
            break;
        }
    }
    let mut nb = Vec::with_capacity(p);
    nb.extend_from_slice(&b[..locals_end]);
    for &(k, s, e) in &ins {
        match k {
            WPeek::Get(n) => {
                nb.push(0x20);
                leb(&mut nb, n);
            }
            WPeek::Set(n) => {
                nb.push(0x21);
                leb(&mut nb, n);
            }
            WPeek::Tee(n) => {
                nb.push(0x22);
                leb(&mut nb, n);
            }
            WPeek::Other => nb.extend_from_slice(&b[s..e]),
        }
    }
    Some(nb)
}

/// Module-level driver for `peep_body`: walks sections, rewrites each
/// code-section function body. Leaves the module untouched on any
/// malformed section layout.
fn wpeep(bytes: &mut Vec<u8>) {
    if bytes.len() < 8 || &bytes[..8] != b"\0asm\x01\0\0\0" {
        return;
    }
    let mut out = bytes[..8].to_vec();
    let mut p = 8usize;
    while p < bytes.len() {
        let id = bytes[p];
        p += 1;
        let Some(sz) = urd(bytes, &mut p) else {
            return;
        };
        let (s, e) = (p, p.saturating_add(sz as usize));
        if e > bytes.len() {
            return;
        }
        out.push(id);
        if id != 10 {
            leb(&mut out, sz);
            out.extend_from_slice(&bytes[s..e]);
            p = e;
            continue;
        }
        // Code section: vec of (size, body).
        let mut body_out = Vec::new();
        let mut q = s;
        let Some(nf) = urd(bytes, &mut q) else {
            return;
        };
        leb(&mut body_out, nf);
        for _ in 0..nf {
            let Some(bsz) = urd(bytes, &mut q) else {
                return;
            };
            let be = q + bsz as usize;
            if be > e {
                return;
            }
            let nb = peep_body(&bytes[q..be]).unwrap_or_else(|| bytes[q..be].to_vec());
            leb(&mut body_out, nb.len() as u32);
            body_out.extend_from_slice(&nb);
            q = be;
        }
        if q != e {
            return; // trailing bytes inside the code section
        }
        leb(&mut out, body_out.len() as u32);
        out.extend_from_slice(&body_out);
        p = e;
    }
    *bytes = out;
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
        .filter(|(n, f)| has_body(ctx, f.op) && !st.dead_fns.contains(*n))
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
        .filter(|(n, g)| {
            g.init.is_some() && !st.funcs.contains_key(*n) && custom_section_name(g).is_none()
        })
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

/// SSA tracking for `st.promoted` allocas. Waffle IR has no mutable locals,
/// so each promoted alloca leaf is a virtual variable whose block-param
/// wiring is built lazily — the same algorithm waffle's own wasm frontend
/// uses for locals (`frontend.rs::LocalTracker`), keyed on our leaf indices.
#[derive(Default)]
struct Promo {
    /// Alloca result -> (first leaf index, leaf count).
    slots: FxHashMap<Value, (u32, u32)>,
    /// Wasm type per leaf slot.
    tys: Vec<WT>,
    /// Live-out leaf values of a block (or current in-block values).
    map: FxHashMap<WBlock, FxHashMap<u32, WV>>,
    /// Blocks whose predecessor set is final.
    sealed: FxHashSet<WBlock>,
    /// Waffle blocks that map to a pliron block (vs. internal continuations).
    mapped: FxHashSet<WBlock>,
    /// Waffle blocks with their terminator set.
    finished: FxHashSet<WBlock>,
    /// Phi placeholders awaiting the block's seal.
    incomplete: FxHashMap<WBlock, Vec<(u32, WV)>>,
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
    promo: Promo,
    terminated: bool,
    sp0: WV,
    fp: WV,
    frame_c: WV,
    mask_c: WV,
    frame: u64,
    frame_align: u64,
    /// Shared "an unwind is in flight" epilogue: restores SP and returns
    /// dummy values so the caller sees the flag and keeps unwinding.
    epi: Option<WBlock>,
    /// This function's wasm result types, for the epilogue's dummy values.
    rets: Vec<WT>,
    /// Hidden buffer-pointer param of a C-variadic function (`pliron.va.buf`).
    va_buf: Option<WV>,
    name: String,
}

impl<'o, 'a, 'tcx> FL<'o, 'a, 'tcx> {
    fn new(o: &'o mut Obj<'a, 'tcx>, sig: Signature) -> Self {
        let rets = o.m.signatures[sig].returns.clone();
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
            promo: Promo::default(),
            terminated: false,
            epi: None,
            rets,
            sp0: WV::invalid(),
            fp: WV::invalid(),
            frame_c: WV::invalid(),
            mask_c: WV::invalid(),
            frame: 0,
            frame_align: 16,
            va_buf: None,
            name: String::new(),
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
        // Patch the frame placeholders before optimizing: GVN would otherwise
        // alias same-valued consts onto frame_c/mask_c, and the rewrite below
        // would corrupt them.
        let frame = crate::types::align_to(self.frame, self.frame_align) as u32;
        let mask = !(self.frame_align as u32 - 1);
        for (v, c) in [(self.frame_c, frame), (self.mask_c, mask)] {
            if let ValueDef::Operator(op, ..) = &mut self.b.values[v] {
                *op = O::I32Const { value: c };
            }
        }
        self.b.optimize(&waffle::OptOptions::default());
        let dbg = std::env::var("PLIRON_WASM_LOOPS")
            .map(|f| f.is_empty() || self.name.contains(&f))
            .unwrap_or(false);
        if dbg {
            eprintln!("==== pre-loopopt {} ====\n{}", self.name, self.b.display("  ", None));
        }
        if std::env::var("PLIRON_WASM_WLOOP").map_or(true, |v| v != "0") {
            wloop_opt(&mut self.b, self.o.sp);
        }
        if dbg {
            eprintln!("==== post-loopopt {} ====\n{}", self.name, self.b.display("  ", None));
        }
        self.b.optimize(&waffle::OptOptions::default());
        if let Some(lim) = std::env::var("PLIRON_WASM_TRIP")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            if self.o.trip == waffle::Global::invalid() {
                // Resolved by the linker to one shared mutable global.
                self.o.trip = self.o.import_global("env", "__pliron_trip", true);
            }
            wtrip_guard(&mut self.b, self.o.trip, lim);
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

    /// Read a promoted-alloca leaf in the current block.
    fn pget(&mut self, loc: u32) -> WV {
        let at = self.cur;
        self.pget_in(at, loc)
    }

    /// Write a promoted-alloca leaf in the current block.
    fn pset(&mut self, loc: u32, v: WV) {
        self.promo.map.entry(self.cur).or_default().insert(loc, v);
    }

    /// The leaf's value at `at`: the in-block map for an open block, the
    /// recorded end map once it is done, else a phi resolved at seal time.
    fn pget_in(&mut self, at: WBlock, loc: u32) -> WV {
        if (at == self.cur || self.promo.sealed.contains(&at))
            && let Some(&v) = self.promo.map.get(&at).and_then(|m| m.get(&loc))
        {
            return v;
        }
        let ty = self.promo.tys[loc as usize];
        if self.promo.sealed.contains(&at) {
            if self.b.blocks[at].preds.is_empty() {
                return self.pzero(at, ty);
            }
            let ph = self.b.add_placeholder(ty);
            self.promo.map.entry(at).or_default().insert(loc, ph);
            self.phi(at, loc, ph);
            return ph;
        }
        if let Some(&v) = self.promo.map.get(&at).and_then(|m| m.get(&loc)) {
            return v;
        }
        let ph = self.b.add_placeholder(ty);
        self.promo.map.entry(at).or_default().insert(loc, ph);
        self.promo.incomplete.entry(at).or_default().push((loc, ph));
        ph
    }

    /// A default (uninitialized) leaf value emitted into a pred-less block.
    fn pzero(&mut self, at: WBlock, ty: WT) -> WV {
        let o = match ty {
            WT::I32 => O::I32Const { value: 0 },
            WT::I64 => O::I64Const { value: 0 },
            WT::F32 => O::F32Const { value: 0 },
            WT::F64 => O::F64Const { value: 0 },
            WT::V128 => O::V128Const { value: 0 },
            t => panic!("promoted leaf type {t:?}"),
        };
        self.b.add_op(at, o, &[], &[ty])
    }

    /// `wb`'s predecessor set is final: resolve its phi placeholders.
    fn pseal(&mut self, wb: WBlock) {
        if !self.promo.sealed.insert(wb) {
            return;
        }
        for (loc, ph) in self.promo.incomplete.remove(&wb).unwrap_or_default() {
            self.phi(wb, loc, ph);
        }
    }

    /// Fill leaf phi `ph` in `wb` from each pred's live-out value, or fold it
    /// to an alias when all preds agree.
    fn phi(&mut self, wb: WBlock, loc: u32, ph: WV) {
        let preds = self.b.blocks[wb].preds.clone();
        let mut results = Vec::with_capacity(preds.len());
        for pred in preds {
            results.push(self.pget_in(pred, loc));
        }
        let mut non_self = results.iter().filter(|&&v| v != ph);
        let alias = match non_self.next() {
            None => None,
            Some(&first)
                if non_self.all(|&v| v == first) && self.b.resolve_alias(first) != ph =>
            {
                Some(first)
            }
            Some(_) => None,
        };
        if let Some(v) = alias {
            self.b.set_alias(ph, v);
        } else {
            self.b.replace_placeholder_with_blockparam(wb, ph);
            for (i, result) in results.into_iter().enumerate() {
                let pred = self.b.blocks[wb].preds[i];
                let index = self.b.blocks[wb].pos_in_pred_succ[i];
                self.b.blocks[pred].terminator.update_target(index, |t| {
                    t.args.push(result);
                });
            }
        }
    }

    /// Seal an internal (non-pliron) waffle block once its preds — which are
    /// final as soon as they exist — have all been emitted.
    fn maybe_seal(&mut self, wb: WBlock) {
        if self.promo.sealed.contains(&wb)
            || self.promo.mapped.contains(&wb)
            || Some(wb) == self.epi
        {
            return;
        }
        if self
            .b
            .blocks[wb]
            .preds
            .iter()
            .all(|p| self.promo.finished.contains(p))
        {
            self.pseal(wb);
        }
    }

    /// Reinterpret `v` as another same-width wasm container type.
    fn reinterp(&mut self, v: WV, to: WT) -> WV {
        let from = self.ty_of(v);
        if from == to {
            return v;
        }
        let o = match (from, to) {
            (WT::I32, WT::F32) => O::F32ReinterpretI32,
            (WT::F32, WT::I32) => O::I32ReinterpretF32,
            (WT::I64, WT::F64) => O::F64ReinterpretI64,
            (WT::F64, WT::I64) => O::I64ReinterpretF64,
            (a, b) => panic!("reinterp {a:?} -> {b:?}"),
        };
        self.op(o, &[v], to)
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
        if i < params.len() {
            // Variadic functions carry one extra hidden param (see wsig).
            self.va_buf = Some(params[i]);
        }
        for op in crate::sroa::allocas(ctx, f) {
            let a = op.deref(ctx).get_result(0);
            if let Some(&ty) = self.st.promoted.get(&a) {
                let base = self.promo.tys.len() as u32;
                let n = wleaves(ctx, ty).len() as u32;
                self.promo.tys.extend(wleaves(ctx, ty).iter().map(|&(_, t)| wty(t)));
                self.promo.slots.insert(a, (base, n));
            }
        }
        self.promo.mapped = self.blocks.values().copied().collect();
        // Pliron-level predecessors: a mapped block's waffle `preds` is only
        // complete once every pliron predecessor has finished emitting.
        let succ = |b: Ptr<BasicBlock>| -> Vec<Ptr<BasicBlock>> {
            b.deref(ctx)
                .iter(ctx)
                .flat_map(|op| {
                    let mut v: Vec<_> = op.deref(ctx).successors().collect();
                    v.extend(self.st.invokes.get(&op).map(|&(l, _)| l));
                    v
                })
                .collect()
        };
        let mut ppred: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
        for &pb in &pblocks {
            for sb in succ(pb) {
                ppred.entry(sb).or_default().push(pb);
            }
        }
        let mut pdone: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
        for pb in rpo(ctx, self.st, &pblocks) {
            self.cur = self.blocks[&pb];
            self.cconst.clear();
            self.terminated = false;
            if ppred
                .get(&pb)
                .is_none_or(|ps| ps.iter().all(|p| pdone.contains(p)))
            {
                self.pseal(self.cur);
            }
            let ops: Vec<Ptr<Operation>> = pb.deref(ctx).iter(ctx).collect();
            for op in ops {
                if self.terminated {
                    let wb = self.b.add_block();
                    self.cur = wb;
                    self.maybe_seal(wb);
                    self.cconst.clear();
                    self.terminated = false;
                }
                self.lower_op(op);
            }
            if !self.terminated {
                let c = self.cur;
                self.b.set_terminator(c, Terminator::Unreachable);
                self.promo.finished.insert(c);
            }
            pdone.insert(pb);
            for sb in succ(pb) {
                if let Some(&wb) = self.blocks.get(&sb)
                    && ppred[&sb].iter().all(|p| pdone.contains(p))
                {
                    self.pseal(wb);
                }
            }
        }
        // Unsealed leftovers (e.g. the unwind epilogue, which gathers preds
        // over the whole function): all preds exist now.
        while let Some(&wb) = self.promo.incomplete.keys().next() {
            self.pseal(wb);
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
        self.promo.finished.insert(c);
        self.terminated = true;
    }

    /// Emulated EH storage: `__pliron_eh` is an 8-byte data symbol synthesized
    /// by the linker; +0 is the "unwind in flight" flag, +4 the exception ptr.
    fn eh_addr(&mut self) -> WV {
        let g = self.o.got("__pliron_eh");
        self.op(O::GlobalGet { global_index: g }, &[], WT::I32)
    }

    fn eh_flag(&mut self) -> WV {
        let p = self.eh_addr();
        self.load(clt::I32, p, 0)
    }

    fn zero_of(&mut self, t: WT) -> WV {
        self.op(
            match t {
                WT::I32 => O::I32Const { value: 0 },
                WT::I64 => O::I64Const { value: 0 },
                WT::F32 => O::F32Const { value: 0 },
                WT::F64 => O::F64Const { value: 0 },
                WT::V128 => O::V128Const { value: 0 },
                t => panic!("eh epilogue return type {t:?}"),
            },
            &[],
            t,
        )
    }

    /// The shared epilogue reached when an unwind propagates out of this
    /// function: restore the stack pointer and return dummy values; the
    /// caller's own flag check keeps unwinding.
    fn unwind_epilogue(&mut self) -> WBlock {
        if let Some(b) = self.epi {
            return b;
        }
        let b = self.b.add_block();
        let (sp, sp0) = (self.o.sp, self.sp0);
        let save = std::mem::replace(&mut self.cur, b);
        self.op0(O::GlobalSet { global_index: sp }, &[sp0]);
        let values: Vec<WV> = self.rets.clone().iter().map(|&t| self.zero_of(t)).collect();
        self.b.set_terminator(b, Terminator::Return { values });
        self.promo.finished.insert(b);
        self.cur = save;
        self.epi = Some(b);
        b
    }

    /// panic=unwind check after a call: if the callee started an unwind, go to
    /// `landing` (the invoke's cleanup pad) or propagate out of this function.
    fn eh_check(&mut self, landing: Option<Ptr<BasicBlock>>) {
        if !self.o.unwind {
            return;
        }
        let f = self.eh_flag();
        let cont = self.b.add_block();
        let if_true = match landing {
            Some(pb) => BlockTarget {
                block: self.blocks[&pb],
                args: vec![],
            },
            None => BlockTarget {
                block: self.unwind_epilogue(),
                args: vec![],
            },
        };
        let c = self.cur;
        self.b.set_terminator(
            c,
            Terminator::CondBr {
                cond: f,
                if_true,
                if_false: BlockTarget {
                    block: cont,
                    args: vec![],
                },
            },
        );
        self.promo.finished.insert(c);
        self.cur = cont;
        self.maybe_seal(cont);
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
        // f16 arrives as raw i16 bits and f128 as an i64 pair: neither can go
        // through scalar wasm float ops, and this sysroot has no hf/tf
        // builtins to call. Panic here becomes an `unreachable` stub.
        let int_float_ty = |t: TypeHandle| {
            wleaves(ctx, t)
                .iter()
                .any(|l| !matches!(l.1, clt::F32 | clt::F64))
        };
        macro_rules! fop {
            ($o32:expr, $o64:expr) => {{
                if int_float_ty(opnds[0].get_type(ctx)) {
                    panic!("f16/f128 arithmetic on wasm");
                }
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
            if int_float_ty(opnds[0].get_type(ctx)) {
                panic!("f16/f128 compare on wasm");
            }
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
            if int_float_ty(opnds[0].get_type(ctx)) {
                panic!("f16/f128 rem on wasm");
            }
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
            let ls = wleaves(ctx, opnds[0].get_type(ctx));
            let r = if ls.len() == 2 {
                // f128 (lo, hi) i64 pair: flip the sign bit in hi.
                let s = self.i64c(1u64 << 63);
                smallvec![a[0], self.op(O::I64Xor, &[a[1], s], WT::I64)]
            } else if ls[0].1 == clt::I16 {
                // f16: raw bits zero-extended in an i32; flip bit 15.
                let s = self.i32c(1 << 15);
                smallvec![self.op(O::I32Xor, &[a[0], s], WT::I32)]
            } else {
                a.into_iter()
                    .map(|x| {
                        let t = self.ty_of(x);
                        self.op(if t == WT::F64 { O::F64Neg } else { O::F32Neg }, &[x], t)
                    })
                    .collect()
            };
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
                // hf/tf conversions call __extend*/__trunc* builtins that this
                // sysroot lacks; a stub traps only when actually invoked.
                ("hf", _) | (_, "hf") | ("tf", _) | (_, "tf") => {
                    panic!("f16/f128 conversion on wasm")
                }
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
            if int_float_ty(opnds[0].get_type(ctx)) {
                panic!("f16/f128 to int on wasm");
            }
            let x = self.get1(opnds[0]);
            let w = self.width(self.res_ty(op));
            let r = self.fcvt_sat(is!(FPToSIOp), w, x);
            self.set(op, r);
        } else if is!(UIToFPOp) || is!(SIToFPOp) {
            if int_float_ty(self.res_ty(op)) {
                panic!("int to f16/f128 on wasm");
            }
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
            if !self.promo.slots.contains_key(&res) {
                let (size, align) = self.st.allocas[&res];
                let p = self.slot(size, align);
                self.set1(op, p);
            }
        } else if is!(LoadOp) && self.promo.slots.contains_key(&opnds[0]) {
            let (base, _) = self.promo.slots[&opnds[0]];
            let r: Vals = wleaves(ctx, self.res_ty(op))
                .iter()
                .enumerate()
                .map(|(k, &(_, t))| {
                    let v = self.pget(base + k as u32);
                    self.reinterp(v, wty(t))
                })
                .collect();
            self.set(op, r);
        } else if is!(StoreOp) && self.promo.slots.contains_key(&opnds[1]) {
            let (base, _) = self.promo.slots[&opnds[1]];
            let vs = self.get(opnds[0]);
            for (k, x) in vs.into_iter().enumerate() {
                let slot = base + k as u32;
                let x = self.reinterp(x, self.promo.tys[slot as usize]);
                self.pset(slot, x);
            }
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
        // panic=unwind only: where an in-flight unwind is delivered.
        let landing = self.st.invokes.get(&op).map(|&(b, _)| b);
        if landing.is_some() && !self.o.unwind {
            panic!("invoke on wasm needs panic=unwind");
        }
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
        let mut va_args: Vec<Value> = Vec::new();
        for (i, a) in call.args(ctx).into_iter().enumerate() {
            if var_arg && i >= nfixed {
                va_args.push(a);
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
            // SysV overflow-area layout, matching va_start/va_arg's tag walk:
            // each vararg in an 8-byte slot aligned to its alignment (<=16).
            let mut off = 0u64;
            let mut plan = Vec::new();
            for &a in &va_args {
                let ty = a.get_type(ctx);
                let (size, align) = size_align(ctx, ty);
                off = off.next_multiple_of(align.clamp(8, 16));
                let slot = off;
                off += size.max(8).next_multiple_of(8);
                plan.push((a, ty, slot));
            }
            let buf = self.slot(off.max(1), 16);
            for (a, ty, slot) in plan {
                for ((lo, lt), v) in leaves(ctx, ty).into_iter().zip(self.get(a)) {
                    self.store(lt, v, buf, slot + lo);
                }
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
            // wasm EH intrinsics on the emulated-EH flag/exception words.
            if sym == "__pliron_llvm_wasm_throw" || sym == "llvm.wasm.throw" {
                let p = self.eh_addr();
                self.store(clt::I32, wargs[1], p, 4);
                let one = self.i32c(1);
                self.store(clt::I32, one, p, 0);
                self.eh_check(None);
                return;
            }
            if sym == "__pliron_llvm_wasm_rethrow" || sym == "llvm.wasm.rethrow" {
                let p = self.eh_addr();
                let one = self.i32c(1);
                self.store(clt::I32, one, p, 0);
                self.eh_check(None);
                return;
            }
            if sym == "__pliron_llvm_wasm_get_exception" || sym == "llvm.wasm.get_exception" {
                let p = self.eh_addr();
                let v = self.load(clt::I32, p, 4);
                return self.set(op, smallvec![v]);
            }
            if sym == "__pliron_llvm_wasm_get_ehselector"
                || sym == "__pliron_llvm_wasm_landingpad_index"
                || sym == "llvm.wasm.get_ehselector"
                || sym == "llvm.wasm.landingpad_index"
            {
                let v = self.i32c(0);
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
        self.eh_check(landing);
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
            // Landing-pad read of the in-flight exception: delivers the
            // exception pointer and clears the unwind flag so the handler's
            // own calls work normally.
            "pliron.eh.exn" => {
                let p = self.eh_addr();
                let exn = self.load(clt::I32, p, 4);
                let z = self.i32c(0);
                self.store(clt::I32, z, p, 0);
                self.set(op, smallvec![exn]);
                return;
            }
            "pliron.va.buf" => {
                let v = self.va_buf.expect("pliron.va.buf in a non-variadic function");
                self.set(op, smallvec![v]);
                return;
            }
            // Rethrow: the unwind continues out of this function.
            "pliron.eh.rethrow" => {
                let p = self.eh_addr();
                let one = self.i32c(1);
                self.store(clt::I32, one, p, 0);
                let (sp, sp0) = (self.o.sp, self.sp0);
                self.op0(O::GlobalSet { global_index: sp }, &[sp0]);
                let values: Vec<WV> = self.rets.clone().iter().map(|&t| self.zero_of(t)).collect();
                self.term(Terminator::Return { values });
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
            "llvm.fshl" | "llvm.fshr" if w(self, 0) == 128 => {
                let c127 = self.i64c(127);
                let s = self.op(O::I64And, &[a[2][0], c127], WT::I64);
                let c128 = self.i64c(128);
                let inv = self.op(O::I64Sub, &[c128, s], WT::I64);
                let left = name == "llvm.fshl";
                let (sx, sy) = if left { (s, inv) } else { (inv, s) };
                let (x, y) = ([a[0][0], a[0][1]], [a[1][0], a[1][1]]);
                let hi = self.shift128(ShlOp::get_opid_static(), &x, sx);
                let lo = self.shift128(LShrOp::get_opid_static(), &y, sy);
                let c0 = self.op(O::I64Or, &[hi[0], lo[0]], WT::I64);
                let c1 = self.op(O::I64Or, &[hi[1], lo[1]], WT::I64);
                let z = self.op(O::I64Eqz, &[s], WT::I32);
                let keep = if left { x } else { y };
                smallvec![self.sel(z, keep[0], c0), self.sel(z, keep[1], c1)]
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

/// Natural loops of the body: (header, member-blocks) pairs.
fn wloops(b: &FunctionBody, cfg: &waffle::cfg::CFGInfo) -> Vec<(WBlock, FxHashSet<WBlock>)> {
    let mut out = Vec::new();
    for h in b.blocks.iter() {
        let latch: Vec<WBlock> = b.blocks[h]
            .preds
            .iter()
            .copied()
            .filter(|&p| p == h || cfg.dominates(h, p))
            .collect();
        if latch.is_empty() {
            continue;
        }
        // Blocks reaching a latch without crossing the header.
        let mut body_set: FxHashSet<WBlock> = [h].into_iter().collect();
        let mut wl = latch;
        while let Some(x) = wl.pop() {
            if body_set.insert(x) {
                wl.extend(b.blocks[x].preds.iter().copied());
            }
        }
        out.push((h, body_set));
    }
    out
}

/// Edges into `h` from outside the loop: (pred, target-index) pairs.
fn outside_edges(b: &FunctionBody, h: WBlock, inloop: &FxHashSet<WBlock>) -> Vec<(WBlock, usize)> {
    b.blocks[h]
        .preds
        .iter()
        .enumerate()
        .filter(|(_, p)| !inloop.contains(p))
        .map(|(j, _)| (b.blocks[h].preds[j], b.blocks[h].pos_in_pred_succ[j]))
        .collect()
}

/// A loop header with several entry edges gets a forwarding preheader:
/// `pre` params mirror `h`'s, its terminator forwards them, and all
/// outside edges are retargeted to `pre`.
fn mk_pre(b: &mut FunctionBody, h: WBlock, inloop: &FxHashSet<WBlock>) -> WBlock {
    // Collect outside edges before pre's own edge exists.
    let edges = outside_edges(b, h, inloop);
    let pre = b.add_block();
    let fwd: Vec<WV> = b.blocks[h]
        .params
        .clone()
        .into_iter()
        .map(|(t, _)| b.add_blockparam(pre, t))
        .collect();
    for (pr, tidx) in edges {
        b.blocks[pr].terminator.update_target(tidx, |t| t.block = pre);
    }
    b.set_terminator(
        pre,
        Terminator::Br {
            target: BlockTarget { block: h, args: fwd },
        },
    );
    b.recompute_edges();
    pre
}

/// Loop optimizations on a finished waffle body: hoists speculatable
/// loop-invariant ops (and immutable-global reads) to each loop's
/// preheader, and strength-reduces `base + iv*K` addressing into
/// induction-variable block params. The wasm path bypasses the CLIF
/// pipeline, so this fills in for licm/indvars there.
fn wloop_opt(b: &mut FunctionBody, sp: waffle::Global) {
    // Merge multi-entry loop headers so every loop has a preheader.
    {
        let cfg = waffle::cfg::CFGInfo::new(b);
        for (h, inloop) in wloops(b, &cfg) {
            let mut preds: Vec<WBlock> = outside_edges(b, h, &inloop)
                .into_iter()
                .map(|(p, _)| p)
                .collect();
            preds.sort();
            preds.dedup();
            if preds.len() > 1 {
                mk_pre(b, h, &inloop);
            }
        }
    }
    let mut cfg = waffle::cfg::CFGInfo::new(b);
    // LICM/indvars move and create insts, so track defining blocks in a
    // live map rather than the (stale) CFGInfo.
    let mut defb = cfg.def_block.clone();
    let loops = wloops(b, &cfg);
    for (h, inloop) in &loops {
        let outside = outside_edges(b, *h, inloop);
        if outside.is_empty() {
            continue;
        }
        let mut preds: Vec<WBlock> = outside.iter().map(|&(p, _)| p).collect();
        preds.sort();
        preds.dedup();
        if let [pre] = preds[..] {
            loop_licm(b, &cfg, &mut defb, inloop, pre, sp);
        }
        // Indvars to a fixed point: each new param may itself be the
        // `pv` of a `mul(pv, K)` addressing site (e.g. `idx*4` where
        // idx already strides), needing another pass to strength-reduce.
        for _ in 0..4 {
            let n0 = b.blocks[*h].params.len();
            loop_indvars(b, &mut defb, inloop, *h);
            if b.blocks[*h].params.len() == n0 {
                break;
            }
        }
        // Loop-closed exits: funnel leaked loop values through exit
        // block params so cloning (wbcheck/unroll) is legal.
        if std::env::var_os("PLIRON_WASM_SEAL")
            .map(|v| v != "0")
            .unwrap_or(true)
        {
            wseal_exits(b, &mut defb, &cfg, inloop, *h);
        }
    }
    // Version loops on provably-passing bounds checks; the fast clones
    // join the unroll worklist.
    let mut extra: Vec<(WBlock, FxHashSet<WBlock>)> = Vec::new();
    for (h, inloop) in &loops {
        if let Some(cl) = wbcheck(b, &mut defb, &cfg, inloop, *h) {
            extra.push(cl);
        }
    }
    // wbcheck adds blocks; refresh CFG so clones have rpo/dominance info.
    if !extra.is_empty() {
        cfg = waffle::cfg::CFGInfo::new(b);
    }
    // Unroll innermost loops (bodies containing no other loop header).
    let unr = std::env::var("PLIRON_WASM_UNROLL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    for (h, inloop) in loops.iter().chain(extra.iter()) {
        let innermost = loops
            .iter()
            .all(|(h2, _)| h2 == h || !inloop.contains(h2));
        if innermost {
            wunroll(b, &mut defb, &cfg, inloop, *h, unr);
        }
    }
    wdce(b);
}

/// `PLIRON_WASM_TRIP=n`: splice a shared i32 counter check onto every
/// latch edge. A loop spinning past `n` back-edges traps `unreachable`
/// instead of hanging, and the trap names the function.
fn wtrip_guard(b: &mut FunctionBody, g: waffle::Global, limit: u64) {
    let cfg = waffle::cfg::CFGInfo::new(b);
    let mut edges = Vec::new();
    for (h, inloop) in wloops(b, &cfg) {
        for (j, &p) in b.blocks[h].preds.iter().enumerate() {
            if inloop.contains(&p) {
                edges.push((h, p, b.blocks[h].pos_in_pred_succ[j]));
            }
        }
    }
    let mut trap = None;
    for (h, p, tidx) in edges {
        let chk = b.add_block();
        let fwd: Vec<WV> = b.blocks[h]
            .params
            .clone()
            .into_iter()
            .map(|(t, _)| b.add_blockparam(chk, t))
            .collect();
        let c = b.add_op(chk, O::GlobalGet { global_index: g }, &[], &[WT::I32]);
        let one = b.add_op(chk, O::I32Const { value: 1 }, &[], &[WT::I32]);
        let c2 = b.add_op(chk, O::I32Add, &[c, one], &[WT::I32]);
        b.add_op(chk, O::GlobalSet { global_index: g }, &[c2], &[]);
        let lim = b.add_op(chk, O::I32Const { value: limit as u32 }, &[], &[WT::I32]);
        let over = b.add_op(chk, O::I32GeU, &[c, lim], &[WT::I32]);
        let t = *trap.get_or_insert_with(|| {
            let t = b.add_block();
            b.set_terminator(t, Terminator::Unreachable);
            t
        });
        b.set_terminator(
            chk,
            Terminator::CondBr {
                cond: over,
                if_true: BlockTarget {
                    block: t,
                    args: vec![],
                },
                if_false: BlockTarget { block: h, args: fwd },
            },
        );
        b.blocks[p].terminator.update_target(tidx, |t| t.block = chk);
    }
    b.recompute_edges();
}

/// Move pure ops and non-SP global reads with all-invariant args to `pre`.
fn loop_licm(
    b: &mut FunctionBody,
    cfg: &waffle::cfg::CFGInfo,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    inloop: &FxHashSet<WBlock>,
    pre: WBlock,
    sp: waffle::Global,
) {
    // Process in RPO order so defs hoist before their users.
    let mut order: Vec<WBlock> = inloop.iter().copied().collect();
    order.sort_by_key(|&lb| cfg.rpo_pos[lb]);
    loop {
        let mut moved = false;
        for &lb in &order {
            let mut i = 0;
            while i < b.blocks[lb].insts.len() {
                let v = b.blocks[lb].insts[i];
                let hoist = match b.values[v] {
                    ValueDef::Operator(op, args, _) => {
                        let safe = match op {
                            O::GlobalGet { global_index } => global_index != sp,
                            _ => op.is_pure(),
                        };
                        safe && b.arg_pool[args].iter().all(|&a| {
                            !inloop.contains(&defb[b.resolve_alias(a)])
                        })
                    }
                    _ => false,
                };
                if hoist {
                    b.blocks[lb].insts.remove(i);
                    b.blocks[pre].insts.push(v);
                    defb[v] = pre;
                    moved = true;
                } else {
                    i += 1;
                }
            }
        }
        if !moved {
            break;
        }
    }
}

/// Constant value of `v`, if it is an int-const op.
fn wconst(b: &FunctionBody, v: WV) -> Option<i64> {
    match b.values[b.resolve_alias(v)] {
        ValueDef::Operator(O::I32Const { value }, ..) => Some(value as i32 as i64),
        ValueDef::Operator(O::I64Const { value }, ..) => Some(value as i64),
        _ => None,
    }
}

/// Is `v` loop-invariant: defined outside the loop, a constant, or a
/// pure op whose operands all are? In-loop defs of pure ops (e.g. a
/// `mul` of two invariants left on a latch edge by strength reduction)
/// count too — the value is still identical every iteration.
fn winv(
    b: &FunctionBody,
    defb: &waffle::entity::PerEntity<WV, WBlock>,
    inloop: &FxHashSet<WBlock>,
    v: WV,
) -> bool {
    let v = b.resolve_alias(v);
    match b.values[v] {
        ValueDef::Operator(
            O::I32Const { .. } | O::I64Const { .. } | O::F32Const { .. } | O::F64Const { .. },
            ..,
        ) => true,
        ValueDef::Operator(op, aa, _) => {
            !inloop.contains(&defb[v])
                || (op.is_pure()
                    && b.arg_pool[aa]
                        .iter()
                        .all(|&a| winv(b, defb, inloop, a)))
        }
        _ => !inloop.contains(&defb[v]),
    }
}

/// Materialize a `winv`-invariant value inside `blk`: outside defs are
/// used as-is; an in-loop pure op is re-emitted with recursively
/// materialized args so the result is dominated where it's used.
fn mat_inv(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    inloop: &FxHashSet<WBlock>,
    blk: WBlock,
    v: WV,
) -> WV {
    let v = b.resolve_alias(v);
    let (op, aa, tt) = match b.values[v] {
        ValueDef::Operator(op, aa, tt) if inloop.contains(&defb[v]) && op.is_pure() => {
            (op, aa, tt)
        }
        _ => return v,
    };
    let mut args: Vec<WV> = Vec::with_capacity(b.arg_pool[aa].len());
    for i in 0..b.arg_pool[aa].len() {
        let a = b.arg_pool[aa][i];
        args.push(mat_inv(b, defb, inloop, blk, a));
    }
    let tys: Vec<WT> = b.type_pool[tt].to_vec();
    let nv = b.add_op(blk, op, &args, &tys);
    defb[nv] = blk;
    nv
}

/// The value arriving for param `pidx` of `h` on edge `tidx` of `pred`.
fn edge_arg(b: &mut FunctionBody, pred: WBlock, tidx: usize, pidx: usize) -> WV {
    b.blocks[pred].terminator.visit_target(tidx, |t| t.args[pidx])
}

/// Push `v` as an extra block-arg on the `tidx`-th target of `pred`.
fn edge_push(b: &mut FunctionBody, pred: WBlock, tidx: usize, v: WV) {
    b.blocks[pred].terminator.update_target(tidx, |t| t.args.push(v));
}

/// Turn `add(base, iv*K)` addresses into `p += step*K` induction params.
fn loop_indvars(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    inloop: &FxHashSet<WBlock>,
    h: WBlock,
) {
    let params = b.blocks[h].params.clone();
    for (pidx, &(pty, pv)) in params.iter().enumerate() {
        if pty != WT::I32 {
            continue;
        }
        // Incoming edges into h: (pred, target-index, arg for pidx).
        let mut edges = Vec::new();
        let preds = b.blocks[h].preds.clone();
        for (j, &pr) in preds.iter().enumerate() {
            let tidx = b.blocks[h].pos_in_pred_succ[j];
            let arg = edge_arg(b, pr, tidx, pidx);
            let arg = b.resolve_alias(arg);
            edges.push((pr, tidx, arg));
        }
        // Each in-loop edge must feed `pv + c` (or pv itself: step 0).
        let mut steps: Vec<Option<WV>> = Vec::new();
        let mut ok = true;
        for &(pr, _, arg) in &edges {
            if !inloop.contains(&pr) {
                steps.push(None);
                continue;
            }
            if arg == pv {
                steps.push(Some(WV::invalid()));
                continue;
            }
            let c = match b.values[arg] {
                ValueDef::Operator(O::I32Add, aa, _) => {
                    let &[x, y, ..] = &b.arg_pool[aa][..] else {
                        ok = false;
                        break;
                    };
                    let (x, y) = (b.resolve_alias(x), b.resolve_alias(y));
                    if x == pv && winv(b, defb, inloop, y) {
                        Some(y)
                    } else if y == pv && winv(b, defb, inloop, x) {
                        Some(x)
                    } else {
                        ok = false;
                        break;
                    }
                }
                _ => {
                    ok = false;
                    break;
                }
            };
            steps.push(c);
        }
        if !ok {
            continue;
        }
        // Addressing sites: `mul(<affine pv>, K)` feeding `add(base, m)`,
        // where <affine pv> is `pv` itself or `pv + off` for invariant
        // off (`base + (pv+off)*K` still strides by K per step of pv).
        let mut sites: Vec<(WV, WV, WV, Option<WV>)> = Vec::new(); // (add, base, K, off)
        let affine_pv = |b: &FunctionBody, x: WV, pv: WV| -> Option<Option<WV>> {
            if x == pv {
                return Some(None);
            }
            if let ValueDef::Operator(O::I32Add, xa, _) = b.values[x] {
                let &[p, q, ..] = &b.arg_pool[xa][..] else {
                    return None;
                };
                let (p, q) = (b.resolve_alias(p), b.resolve_alias(q));
                if p == pv && winv(b, defb, inloop, q) {
                    return Some(Some(q));
                }
                if q == pv && winv(b, defb, inloop, p) {
                    return Some(Some(p));
                }
            }
            None
        };
        for &lb in inloop.iter() {
            for &inst in &b.blocks[lb].insts {
                if let ValueDef::Operator(O::I32Add, aa, _) = b.values[inst] {
                    let &[x, y, ..] = &b.arg_pool[aa][..] else { continue };
                    for (m, base) in [(x, y), (y, x)] {
                        let m = b.resolve_alias(m);
                        let base = b.resolve_alias(base);
                        if !winv(b, defb, inloop, base) {
                            continue;
                        }
                        if let ValueDef::Operator(O::I32Mul, mm, _) = b.values[m] {
                            let &[u, k, ..] = &b.arg_pool[mm][..] else { continue };
                            let (u, k) = (b.resolve_alias(u), b.resolve_alias(k));
                            for (x, kk) in [(u, k), (k, u)] {
                                if let Some(off) = affine_pv(b, x, pv) {
                                    if winv(b, defb, inloop, kk) {
                                        sites.push((inst, base, kk, off));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        for (add_v, base, k, off) in sites {
            let p = b.add_blockparam(h, WT::I32);
            defb[p] = h;
            for (i, &(pr, tidx, arg)) in edges.iter().enumerate() {
                if !inloop.contains(&pr) {
                    // Preheader edge: p = base + (arg + off)*K.
                    let arg = match off {
                        Some(off) => {
                            let off = mat_inv(b, defb, inloop, pr, off);
                            let t = b.add_op(pr, O::I32Add, &[arg, off], &[WT::I32]);
                            defb[t] = pr;
                            t
                        }
                        None => arg,
                    };
                    let k = mat_inv(b, defb, inloop, pr, k);
                    let base = mat_inv(b, defb, inloop, pr, base);
                    let m = b.add_op(pr, O::I32Mul, &[arg, k], &[WT::I32]);
                    let init = b.add_op(pr, O::I32Add, &[base, m], &[WT::I32]);
                    defb[m] = pr;
                    defb[init] = pr;
                    edge_push(b, pr, tidx, init);
                } else {
                    // Latch edge: p += c*K (p itself when the step is 0).
                    let next = match steps[i] {
                        None => unreachable!(),
                        Some(c) if !c.is_valid() => p,
                        Some(c) => {
                            let c = mat_inv(b, defb, inloop, pr, c);
                            let k = mat_inv(b, defb, inloop, pr, k);
                            let inc = match (wconst(b, c), wconst(b, k)) {
                                (Some(c), Some(k)) => b.add_op(
                                    pr,
                                    O::I32Const {
                                        value: (c * k) as u32,
                                    },
                                    &[],
                                    &[WT::I32],
                                ),
                                _ => b.add_op(pr, O::I32Mul, &[c, k], &[WT::I32]),
                            };
                            let next = b.add_op(pr, O::I32Add, &[p, inc], &[WT::I32]);
                            defb[inc] = pr;
                            defb[next] = pr;
                            next
                        }
                    };
                    edge_push(b, pr, tidx, next);
                }
            }
            b.set_alias(add_v, p);
        }
    }
}

/// Drop now-dead speculatable insts (left behind by strength reduction).
fn wdce(b: &mut FunctionBody) {
    loop {
        let mut uses: FxHashMap<WV, u32> = FxHashMap::default();
        for blk in b.blocks.iter() {
            for &v in &b.blocks[blk].insts {
                match b.values[v] {
                    ValueDef::Operator(_, args, _) => {
                        for i in 0..args.len() {
                            let a = b.resolve_alias(b.arg_pool[args][i]);
                            *uses.entry(a).or_default() += 1;
                        }
                    }
                    ValueDef::PickOutput(src, ..) => {
                        let src = b.resolve_alias(src);
                        *uses.entry(src).or_default() += 1;
                    }
                    _ => {}
                }
            }
            b.blocks[blk]
                .terminator
                .visit_uses(|v| *uses.entry(b.resolve_alias(v)).or_default() += 1);
        }
        let mut removed = false;
        for blk in b.blocks.iter() {
            let insts = std::mem::take(&mut b.blocks[blk].insts);
            let keep: Vec<WV> = insts
                .into_iter()
                .filter(|&v| {
                    let dead = uses.get(&v).copied().unwrap_or(0) == 0
                        && match b.values[v] {
                            ValueDef::Operator(op, ..) => {
                                op.is_pure() || matches!(op, O::GlobalGet { .. })
                            }
                            ValueDef::Alias(_) | ValueDef::PickOutput(..) => true,
                            _ => false,
                        };
                    removed |= dead;
                    !dead
                })
                .collect();
            b.blocks[blk].insts = keep;
        }
        if !removed {
            break;
        }
    }
}

/// Unroll a loop `copies` times by cloning all of its blocks `copies-1`
/// times and chaining the copies through their (cloned) headers. Because
/// the loop header — including its exit test — is cloned, each copy
/// re-checks the condition and no epilogue is needed:
/// `body → h₁ → body₁ → h₂ → … → h`. Runs after licm/indvars so the
/// cloned body is already lean.
fn wunroll(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    cfg: &waffle::cfg::CFGInfo,
    inloop: &FxHashSet<WBlock>,
    h: WBlock,
    copies: usize,
) {
    let verbose = std::env::var_os("PLIRON_WASM_VERBOSE").is_some();
    // Size limits: unrolling is a tradeoff, not a free win. Scale the
    // copy count down for larger bodies — a 64-inst loop gets 2 copies,
    // a tiny one gets the full 8.
    let insts: usize = inloop.iter().map(|&lb| b.blocks[lb].insts.len()).sum();
    let copies = copies.min((256 / insts.max(1)).max(2));
    if copies < 2 || inloop.len() > 16 || insts > 64 {
        if verbose {
            eprintln!("wunroll: {h} skipped size blocks={} insts={insts}", inloop.len());
        }
        return;
    }
    if !loop_cloneable(b, defb, inloop, h) {
        return;
    }
    let mut order: Vec<WBlock> = inloop.iter().copied().collect();
    order.sort_by_key(|&lb| cfg.rpo_pos[lb]);
    // Build the copies.
    let mut heads = vec![h];
    let mut copies_blocks: Vec<Vec<WBlock>> = Vec::new();
    for _ in 1..copies {
        let (bmap, _) = clone_loop(b, defb, &order);
        heads.push(bmap[&h]);
        copies_blocks.push(order.iter().map(|lb| bmap[lb]).collect());
    }
    // Chain: copy k's back-edges to its own header go to copy k+1's header
    // (the last copy's go back to the original h).
    for k in 0..copies {
        let srcs: &[WBlock] = if k == 0 {
            &order
        } else {
            &copies_blocks[k - 1]
        };
        let next = heads[(k + 1) % copies];
        for &lb in srcs {
            b.blocks[lb].terminator.update_targets(|t| {
                if t.block == heads[k] {
                    t.block = next;
                }
            });
        }
    }
    b.recompute_edges();
}

/// `unreachable`-terminated blocks of ordinary ops are pure sinks (panic
/// paths): a loop clone may point at its own copy of the sink, so values
/// leaking into them stay dominated.
fn cloneable_sink(b: &FunctionBody, blk: WBlock) -> bool {
    matches!(b.blocks[blk].terminator, Terminator::Unreachable)
        && b.blocks[blk]
            .insts
            .iter()
            .all(|&v| {
                matches!(
                    b.values[v],
                    ValueDef::Operator(..) | ValueDef::Alias(_) | ValueDef::PickOutput(..)
                )
            })
}

/// Can this loop's blocks be safely cloned? Requires ordinary insts only,
/// supported terminators, and no loop-defined value used *inside* a
/// reachable non-sink outside block (edge args get remapped per copy;
/// dead blocks may carry dangling args and don't count).
fn loop_cloneable(
    b: &FunctionBody,
    defb: &waffle::entity::PerEntity<WV, WBlock>,
    inloop: &FxHashSet<WBlock>,
    h: WBlock,
) -> bool {
    let verbose = std::env::var_os("PLIRON_WASM_VERBOSE").is_some();
    for &lb in inloop {
        for &inst in &b.blocks[lb].insts {
            if !matches!(
                b.values[inst],
                ValueDef::Operator(..) | ValueDef::Alias(_) | ValueDef::PickOutput(..)
            ) {
                if verbose {
                    let k = match b.values[inst] {
                        ValueDef::PickOutput(..) => "pickoutput",
                        ValueDef::Placeholder(..) => "placeholder",
                        ValueDef::BlockParam(..) => "blockparam",
                        _ => "other",
                    };
                    eprintln!("wloop: {h} skipped exotic inst {inst} {k}");
                }
                return false;
            }
        }
        if !matches!(
            b.blocks[lb].terminator,
            Terminator::Br { .. } | Terminator::CondBr { .. } | Terminator::Select { .. }
        ) {
            if verbose {
                eprintln!("wloop: {h} skipped terminator in {lb}");
            }
            return false;
        }
    }
    let mut reach: FxHashSet<WBlock> = [WBlock::new(0)].into_iter().collect();
    {
        let mut wl = vec![WBlock::new(0)];
        while let Some(x) = wl.pop() {
            b.blocks[x].terminator.visit_targets(|t| {
                if reach.insert(t.block) {
                    wl.push(t.block);
                }
            });
        }
    }
    for blk in b.blocks.iter() {
        if inloop.contains(&blk) || !reach.contains(&blk) {
            continue;
        }
        let sink = cloneable_sink(b, blk);
        if !sink {
            for &v in &b.blocks[blk].insts {
                match b.values[v] {
                    ValueDef::Operator(_, aa, _) => {
                        if b.arg_pool[aa]
                            .iter()
                            .any(|&a| inloop.contains(&defb[b.resolve_alias(a)]))
                        {
                            if verbose {
                                eprintln!("wloop: {h} skipped leak {v} in {blk}");
                            }
                            return false;
                        }
                    }
                    ValueDef::PickOutput(src, ..) => {
                        if inloop.contains(&defb[b.resolve_alias(src)]) {
                            if verbose {
                                eprintln!("wloop: {h} skipped leak pick {v} in {blk}");
                            }
                            return false;
                        }
                    }
                    _ => {}
                }
            }
            // All terminator uses count as leaks: cond/select/return are
            // direct operands, and edge args on this block's outgoing
            // edges must be dominated here too.
            let mut leak = None;
            b.blocks[blk].terminator.visit_uses(|a| {
                let a = b.resolve_alias(a);
                if inloop.contains(&defb[a]) {
                    leak = Some(a);
                }
            });
            if let Some(a) = leak {
                if verbose {
                    eprintln!(
                        "wloop: {h} skipped leak via terminator {a} in {blk} defb={} term={:?}",
                        defb[a], b.blocks[blk].terminator
                    );
                }
                return false;
            }
        }
    }
    true
}

/// Rewrite every use of `v` inside `blk` — inst args, alias targets and
/// all terminator operands — to `np`.
fn rewrite_block_uses(b: &mut FunctionBody, blk: WBlock, v: WV, np: WV) {
    let insts = b.blocks[blk].insts.clone();
    for inst in insts {
        match b.values[inst] {
            ValueDef::Operator(_, aa, _) => {
                for ai in 0..b.arg_pool[aa].len() {
                    let a = b.arg_pool[aa][ai];
                    if b.resolve_alias(a) == v {
                        b.arg_pool[aa][ai] = np;
                    }
                }
            }
            ValueDef::Alias(a) if b.resolve_alias(a) == v => {
                b.values[inst] = ValueDef::Alias(np);
            }
            _ => {}
        }
    }
    let mut term = b.blocks[blk].terminator.clone();
    term.update_uses(|a| {
        if b.resolve_alias(*a) == v {
            *a = np;
        }
    });
    b.blocks[blk].terminator = term;
}

/// A value equal to `v` usable inside `blk`'s frame. Unreachable blocks
/// may use `v` directly (dead uses don't count); reachable blocks get a
/// *carrier* param fed by every incoming edge — in-loop blocks included.
/// Feeding a carrier pushes args into the preds' terminators. Only an
/// in-loop pred `v` dominates (or a dead pred, whose args dangle
/// harmlessly) may take `v` itself; any other pred gets its own carrier
/// on demand. Both halves of that condition are load-bearing:
/// domination keeps the pushed arg valid SSA (`v` dominating `blk` does
/// *not* imply it dominates `blk`'s preds — `entry→pr→cur→def(v)→blk`
/// is a legal shape), and the in-loop requirement keeps `loop_cloneable`
/// happy — an outside block dominated by the loop def is still an
/// outside block, and pushing `v` there plants a fresh leak. The
/// carrier is registered before its edges are fed, so CFG cycles
/// resolve to each other's params instead of recursing forever.
///
/// Feasibility is checked before any mutation: the pred-walk must bottom
/// out at dead blocks or dominated in-loop ones and can never cross the
/// entry block (its params are the function signature). If the walk
/// would reach entry, `v` is returned unsealed and cloning simply bails.
fn seal_value(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    cfg: &waffle::cfg::CFGInfo,
    inloop: &FxHashSet<WBlock>,
    reach: &FxHashSet<WBlock>,
    carriers: &mut FxHashMap<(WBlock, WV), WV>,
    blk: WBlock,
    v: WV,
) -> WV {
    let v = b.resolve_alias(v);
    if inloop.contains(&blk) || !reach.contains(&blk) {
        return v;
    }
    if let Some(&np) = carriers.get(&(blk, v)) {
        return np;
    }
    let Some(ty) = b.values[v].ty(&b.type_pool) else {
        // Multi-result/untyped defs can't be params; the leak stays and
        // cloning still bails, but the rest of the block's leaks seal.
        return v;
    };
    let dblk = defb[v];
    // Mirror of the carrier walk below, without mutating: infeasible iff
    // it would need a carrier on the entry block.
    {
        let mut seen: FxHashSet<WBlock> = [blk].into_iter().collect();
        let mut wl = vec![blk];
        while let Some(cur) = wl.pop() {
            for &pr in &b.blocks[cur].preds {
                if !seen.insert(pr)
                    || !reach.contains(&pr)
                    || (inloop.contains(&pr) && cfg.dominates(dblk, pr))
                    || carriers.contains_key(&(pr, v))
                {
                    continue;
                }
                if pr == WBlock::new(0) {
                    return v;
                }
                wl.push(pr);
            }
        }
    }
    let np = b.add_blockparam(blk, ty);
    defb[np] = blk;
    carriers.insert((blk, v), np);
    rewrite_block_uses(b, blk, v, np);
    let mut wl = vec![blk];
    while let Some(cur) = wl.pop() {
        let preds = b.blocks[cur].preds.clone();
        let poss = b.blocks[cur].pos_in_pred_succ.clone();
        for (j, pr) in preds.iter().enumerate() {
            let c = if !reach.contains(pr)
                || (inloop.contains(pr) && cfg.dominates(dblk, *pr))
            {
                v
            } else if let Some(&c) = carriers.get(&(*pr, v)) {
                c
            } else {
                let c = b.add_blockparam(*pr, ty);
                defb[c] = *pr;
                carriers.insert((*pr, v), c);
                rewrite_block_uses(b, *pr, v, c);
                wl.push(*pr);
                c
            };
            edge_push(b, *pr, poss[j], c);
        }
    }
    np
}

/// A value equal to `PickOutput(from, idx)` usable inside `blk`'s frame.
/// `from` is a multi-result def and can't itself be a param: blocks the
/// pick may be recomputed in (dead ones, or in-loop ones `from`
/// dominates — same rule as `seal_value`) get a fresh `PickOutput`;
/// other blocks get a carrier param fed recursively.
fn seal_pick(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    cfg: &waffle::cfg::CFGInfo,
    inloop: &FxHashSet<WBlock>,
    reach: &FxHashSet<WBlock>,
    carriers: &mut FxHashMap<(WBlock, WV, u32), WV>,
    blk: WBlock,
    from: WV,
    idx: u32,
    ty: WT,
) -> WV {
    let from = b.resolve_alias(from);
    let mkpick = |b: &mut FunctionBody,
                  defb: &mut waffle::entity::PerEntity<WV, WBlock>,
                  blk: WBlock| {
        let pv = b.add_value(ValueDef::PickOutput(from, idx, ty));
        defb[pv] = blk;
        b.blocks[blk].insts.push(pv);
        pv
    };
    if inloop.contains(&blk) || !reach.contains(&blk) {
        return mkpick(b, defb, blk);
    }
    if let Some(&np) = carriers.get(&(blk, from, idx)) {
        return np;
    }
    let dblk = defb[from];
    // Same feasibility pre-check as `seal_value`: the pred-walk must
    // never need a carrier on the entry block.
    {
        let mut seen: FxHashSet<WBlock> = [blk].into_iter().collect();
        let mut wl = vec![blk];
        while let Some(cur) = wl.pop() {
            for &pr in &b.blocks[cur].preds {
                if !seen.insert(pr)
                    || !reach.contains(&pr)
                    || (inloop.contains(&pr) && cfg.dominates(dblk, pr))
                    || carriers.contains_key(&(pr, from, idx))
                {
                    continue;
                }
                if pr == WBlock::new(0) {
                    return mkpick(b, defb, blk);
                }
                wl.push(pr);
            }
        }
    }
    let np = b.add_blockparam(blk, ty);
    defb[np] = blk;
    carriers.insert((blk, from, idx), np);
    let mut wl = vec![blk];
    while let Some(cur) = wl.pop() {
        let preds = b.blocks[cur].preds.clone();
        let poss = b.blocks[cur].pos_in_pred_succ.clone();
        for (j, pr) in preds.iter().enumerate() {
            let c = if !reach.contains(pr)
                || (inloop.contains(pr) && cfg.dominates(dblk, *pr))
            {
                mkpick(b, defb, *pr)
            } else if let Some(&c) = carriers.get(&(*pr, from, idx)) {
                c
            } else {
                let c = b.add_blockparam(*pr, ty);
                defb[c] = *pr;
                carriers.insert((*pr, from, idx), c);
                wl.push(*pr);
                c
            };
            edge_push(b, *pr, poss[j], c);
        }
    }
    np
}

/// Loop-closed exits: any in-loop value used inside a reachable outside
/// block is funneled through a new block param, fed by a carrier on
/// every incoming edge (see `seal_value`). Cloned loop copies remap the
/// edge args and stay dominated. `unreachable`-terminated pure sinks are
/// sealed too: a clone may keep its own copy of a directly-targeted
/// sink, but flow from a clone can also reach the *original* sink
/// through shared outside blocks — sealing the use is what keeps it
/// dominated on every incoming path.
fn wseal_exits(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    cfg: &waffle::cfg::CFGInfo,
    inloop: &FxHashSet<WBlock>,
    h: WBlock,
) {
    let verbose = std::env::var_os("PLIRON_WASM_VERBOSE").is_some();
    let mut reach: FxHashSet<WBlock> = [WBlock::new(0)].into_iter().collect();
    let mut wl = vec![WBlock::new(0)];
    while let Some(x) = wl.pop() {
        b.blocks[x].terminator.visit_targets(|t| {
            if reach.insert(t.block) {
                wl.push(t.block);
            }
        });
    }
    let mut carriers: FxHashMap<(WBlock, WV), WV> = FxHashMap::default();
    let mut pick_carriers: FxHashMap<(WBlock, WV, u32), WV> = FxHashMap::default();
    for blk in b.blocks.iter() {
        if inloop.contains(&blk) || !reach.contains(&blk) {
            continue;
        }
        // Leaked values: loop defs used by this block's insts or its
        // terminator (visit_uses covers cond/select/return and the edge
        // args, which evaluate in this block's frame).
        let mut leaks: FxHashSet<WV> = FxHashSet::default();
        // PickOutput sources can't become params (they must stay
        // multi-result defs); seal each output separately instead.
        let mut picks: Vec<(WV, WV, u32, WT)> = Vec::new();
        for &v in &b.blocks[blk].insts {
            match b.values[v] {
                ValueDef::Operator(_, aa, _) => {
                    for &a in b.arg_pool[aa].iter() {
                        let a = b.resolve_alias(a);
                        if inloop.contains(&defb[a]) {
                            leaks.insert(a);
                        }
                    }
                }
                ValueDef::Alias(a) => {
                    let a = b.resolve_alias(a);
                    if inloop.contains(&defb[a]) {
                        leaks.insert(a);
                    }
                }
                ValueDef::PickOutput(from, idx, ty) => {
                    let f = b.resolve_alias(from);
                    if inloop.contains(&defb[f]) {
                        picks.push((v, f, idx, ty));
                    }
                }
                _ => {}
            }
        }
        b.blocks[blk].terminator.visit_uses(|a| {
            let a = b.resolve_alias(a);
            if inloop.contains(&defb[a]) {
                leaks.insert(a);
            }
        });
        if leaks.is_empty() && picks.is_empty() {
            continue;
        }
        if verbose {
            eprintln!("wseal: {h} {blk} leaks={leaks:?} picks={picks:?}");
        }
        let mut leaks: Vec<WV> = leaks.into_iter().collect();
        leaks.sort();
        for &v in &leaks {
            let np = seal_value(b, defb, cfg, inloop, &reach, &mut carriers, blk, v);
            if np == v && verbose {
                eprintln!("wseal: {h} {blk} unsealable leak {v} {:?}", b.values[v]);
            }
        }
        for &(inst, from, idx, ty) in &picks {
            let np = seal_pick(
                b,
                defb,
                cfg,
                inloop,
                &reach,
                &mut pick_carriers,
                blk,
                from,
                idx,
                ty,
            );
            b.values[inst] = ValueDef::Alias(np);
        }
    }
}

fn remap_v(b: &FunctionBody, vmap: &FxHashMap<WV, WV>, v: WV) -> WV {
    let r = b.resolve_alias(v);
    vmap.get(&r).copied().unwrap_or(r)
}

/// Clone the blocks in `order` once: params, operator/alias insts, and
/// terminators are remapped into the copy; in-loop targets point at the
/// copy, exits keep their block, and cloneable sinks are cloned so their
/// leaked args stay dominated.
fn clone_loop(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    order: &[WBlock],
) -> (FxHashMap<WBlock, WBlock>, FxHashMap<WV, WV>) {
    let mut bmap: FxHashMap<WBlock, WBlock> = FxHashMap::default();
    let mut vmap: FxHashMap<WV, WV> = FxHashMap::default();
    for &lb in order {
        let nb = b.add_block();
        bmap.insert(lb, nb);
        for &(ty, pv) in b.blocks[lb].params.clone().iter() {
            let np = b.add_blockparam(nb, ty);
            defb[np] = nb;
            vmap.insert(pv, np);
        }
    }
    let mut aliases = Vec::new();
    for &lb in order {
        let nb = bmap[&lb];
        let insts = b.blocks[lb].insts.clone();
        for inst in insts {
            match b.values[inst] {
                ValueDef::Operator(op, aa, tt) => {
                    let args: Vec<WV> = b.arg_pool[aa]
                        .iter()
                        .map(|&a| remap_v(b, &vmap, a))
                        .collect();
                    let tys: Vec<WT> = b.type_pool[tt].to_vec();
                    let nv = b.add_op(nb, op, &args, &tys);
                    defb[nv] = nb;
                    vmap.insert(inst, nv);
                }
                ValueDef::PickOutput(from, idx, ty) => {
                    let nv = b.add_value(ValueDef::PickOutput(remap_v(b, &vmap, from), idx, ty));
                    b.blocks[nb].insts.push(nv);
                    defb[nv] = nb;
                    vmap.insert(inst, nv);
                }
                ValueDef::Alias(_) => aliases.push(inst),
                _ => {}
            }
        }
    }
    // Aliases may point at insts cloned in any order; remap them now that
    // every operator has a clone.
    for inst in aliases {
        if let ValueDef::Alias(a) = b.values[inst] {
            let nv = remap_v(b, &vmap, a);
            vmap.insert(inst, nv);
        }
    }
    let mut smap: FxHashMap<WBlock, WBlock> = FxHashMap::default();
    for &lb in order {
        let nb = bmap[&lb];
        let term = b.blocks[lb].terminator.clone();
        let mut remap_t = |b: &mut FunctionBody, t: &BlockTarget| {
            let blk = if let Some(&c) = bmap.get(&t.block) {
                c
            } else if cloneable_sink(b, t.block) {
                *smap
                    .entry(t.block)
                    .or_insert_with_key(|&tb| clone_sink(b, defb, &vmap, tb))
            } else {
                t.block
            };
            BlockTarget {
                block: blk,
                args: t.args.iter().map(|&a| remap_v(b, &vmap, a)).collect(),
            }
        };
        let nt = match term {
            Terminator::Br { target } => Terminator::Br {
                target: remap_t(b, &target),
            },
            Terminator::CondBr {
                cond,
                if_true,
                if_false,
            } => Terminator::CondBr {
                cond: remap_v(b, &vmap, cond),
                if_true: remap_t(b, &if_true),
                if_false: remap_t(b, &if_false),
            },
            Terminator::Select {
                value,
                targets,
                default,
            } => {
                let value = remap_v(b, &vmap, value);
                let targets: Vec<_> = targets.iter().map(|t| remap_t(b, t)).collect();
                let default = remap_t(b, &default);
                Terminator::Select {
                    value,
                    targets,
                    default,
                }
            }
            _ => Terminator::Unreachable,
        };
        b.set_terminator(nb, nt);
    }
    (bmap, vmap)
}

/// Loop versioning for in-loop unsigned bounds checks. Given a canonical
/// induction variable `i` bounded by a dominating `i <u n` exit test, an
/// in-loop check `idx <u len` (`idx = i + d`) always passes when the last
/// iteration's index is in bounds. The preheader computes
/// `hi = i0 + (n - i0 - 1) * step + d` in u64 (exact: it fits, since
/// `(n - i0 - 1) * step < 2^64` and wraps make `hi >= 2^32 > len` bail to
/// the slow path) and branches to a check-free clone when `i0 >=u n`
/// (zero trips) or `hi <u len`. The original loop stays as the slow path.
fn wbcheck(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    cfg: &waffle::cfg::CFGInfo,
    inloop: &FxHashSet<WBlock>,
    h: WBlock,
) -> Option<(WBlock, FxHashSet<WBlock>)> {
    let verbose = std::env::var_os("PLIRON_WASM_VERBOSE").is_some();
    macro_rules! bail {
        ($($a:tt)*) => {{
            if verbose {
                eprintln!("wbcheck: {h} {}", format_args!($($a)*));
            }
            return None;
        }};
    }
    // Single preheader that just forwards to h.
    let edges = outside_edges(b, h, inloop);
    let mut preds: Vec<WBlock> = edges.iter().map(|&(p, _)| p).collect();
    preds.sort();
    preds.dedup();
    let [pre] = preds[..] else { bail!("outside preds {preds:?}") };
    let pre_tidx = edges.iter().find(|&&(p, _)| p == pre)?.1;
    let Terminator::Br { target: pre_t } = &b.blocks[pre].terminator else {
        bail!("pre {pre} not a forwarder");
    };
    if pre_t.block != h {
        bail!("pre {pre} targets {}", pre_t.block);
    }
    // Latches: in-loop preds of h.
    let latches: Vec<(WBlock, usize)> = b.blocks[h]
        .preds
        .iter()
        .enumerate()
        .filter(|(_, p)| inloop.contains(p))
        .map(|(j, &p)| (p, b.blocks[h].pos_in_pred_succ[j]))
        .collect();
    if latches.is_empty() {
        bail!("no latches");
    }
    // Normalize a CondBr `cond` into (idx, bound, pass-is-true) for
    // `idx <u bound` / `idx >=u bound` shapes.
    let as_check = |b: &FunctionBody, cond: WV| -> Option<(WV, WV, bool)> {
        match b.values[b.resolve_alias(cond)] {
            ValueDef::Operator(O::I32LtU, aa, _) => {
                let &[x, y, ..] = &b.arg_pool[aa][..] else { return None };
                Some((b.resolve_alias(x), b.resolve_alias(y), true))
            }
            ValueDef::Operator(O::I32GeU, aa, _) => {
                let &[x, y, ..] = &b.arg_pool[aa][..] else { return None };
                Some((b.resolve_alias(x), b.resolve_alias(y), false))
            }
            _ => None,
        }
    };
    // Exit-test candidates: `iv <u n`, one arm leaves the loop, the block
    // dominates every latch so each iteration is gated by it. Several
    // blocks can match (a bounds check has the same shape); pick per
    // candidate the checks it dominates and use the first that yields
    // any — ordered by RPO so a dominating test wins.
    let mut cands: Vec<(WBlock, usize, WV)> = Vec::new();
    'cand: for &lb in inloop.iter() {
        let Terminator::CondBr {
            cond,
            if_true,
            if_false,
        } = &b.blocks[lb].terminator
        else {
            continue;
        };
        let Some((x, n, pass_true)) = as_check(b, *cond) else {
            continue;
        };
        let pass = if pass_true { if_true } else { if_false };
        let fail = if pass_true { if_false } else { if_true };
        if !inloop.contains(&pass.block) || inloop.contains(&fail.block) {
            continue;
        }
        let ValueDef::BlockParam(blk, pidx, _) = b.values[x] else {
            continue;
        };
        if blk != h || !winv(b, defb, inloop, n) {
            continue;
        }
        for &(la, _) in &latches {
            if !cfg.dominates(lb, la) {
                continue 'cand;
            }
        }
        cands.push((lb, pidx as usize, n));
    }
    cands.sort_by_key(|&(lb, _, _)| cfg.rpo_pos[lb]);
    let mut picked = None;
    for &(tb, pidx, n) in &cands {
        let pv = b.blocks[h].params[pidx].1;
        // Check sites: `idx <u len`, dominated by the exit test, idx
        // affine in pv with unit stride. The "pass" arm may stay in or
        // leave the loop — removing a proven-true edge is sound either
        // way.
        let mut checks: Vec<(WBlock, WV, Option<WV>, bool)> = Vec::new();
        for &lb in inloop.iter() {
            if lb == tb || !cfg.dominates(tb, lb) {
                continue;
            }
            let Terminator::CondBr {
                cond,
                if_true,
                if_false,
            } = &b.blocks[lb].terminator
            else {
                continue;
            };
            let Some((x, len, pass_true)) = as_check(b, *cond) else {
                continue;
            };
            let pass = if pass_true { if_true } else { if_false };
            if !inloop.contains(&pass.block) || !winv(b, defb, inloop, len) {
                continue;
            }
            let d = if x == pv {
                None
            } else {
                match b.values[x] {
                    ValueDef::Operator(O::I32Add, aa, _) => {
                        let &[u, v, ..] = &b.arg_pool[aa][..] else { continue };
                        let (u, v) = (b.resolve_alias(u), b.resolve_alias(v));
                        if u == pv && winv(b, defb, inloop, v) {
                            Some(v)
                        } else if v == pv && winv(b, defb, inloop, u) {
                            Some(u)
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                }
            };
            checks.push((lb, len, d, pass_true));
        }
        if !checks.is_empty() {
            picked = Some((tb, pidx, n, checks));
            break;
        }
    }
    let Some((_, pidx, n, checks)) = picked else {
        bail!("no exit test with checks ({} candidates)", cands.len());
    };
    let pv = b.blocks[h].params[pidx].1;
    if checks.len() > 8 {
        bail!("too many checks");
    }
    // pv must be affine on every latch edge with one common invariant
    // step (a latch feeding pv unchanged is step 0).
    let mut c: Option<WV> = None;
    for &(la, tidx) in &latches {
        let arg = edge_arg(b, la, tidx, pidx);
        let arg = b.resolve_alias(arg);
        if arg == pv {
            continue;
        }
        let step = match b.values[arg] {
            ValueDef::Operator(O::I32Add, aa, _) => {
                let &[x, y, ..] = &b.arg_pool[aa][..] else { return None };
                let (x, y) = (b.resolve_alias(x), b.resolve_alias(y));
                if x == pv && winv(b, defb, inloop, y) {
                    y
                } else if y == pv && winv(b, defb, inloop, x) {
                    x
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        match c {
            None => c = Some(step),
            Some(c) if c == step => {}
            _ => return None,
        }
    }
    let i0 = edge_arg(b, pre, pre_tidx, pidx);
    let i0 = b.resolve_alias(i0);
    if !loop_cloneable(b, defb, inloop, h) {
        bail!("not cloneable");
    }
    // Guard in the preheader, in u64: hi = i0 + (n - i0 - 1)*c + d.
    let mut pop = |b: &mut FunctionBody, op: O, args: &[WV], tys: &[WT]| {
        let v = b.add_op(pre, op, args, tys);
        defb[v] = pre;
        v
    };
    let i064 = pop(b, O::I64ExtendI32U, &[i0], &[WT::I64]);
    let n64 = pop(b, O::I64ExtendI32U, &[n], &[WT::I64]);
    let c64 = match c {
        Some(c) => pop(b, O::I64ExtendI32U, &[c], &[WT::I64]),
        None => pop(b, O::I64Const { value: 0 }, &[], &[WT::I64]),
    };
    let one = pop(b, O::I64Const { value: 1 }, &[], &[WT::I64]);
    let trip = pop(b, O::I64Sub, &[n64, i064], &[WT::I64]);
    let tm1 = pop(b, O::I64Sub, &[trip, one], &[WT::I64]);
    let span = pop(b, O::I64Mul, &[tm1, c64], &[WT::I64]);
    let last = pop(b, O::I64Add, &[i064, span], &[WT::I64]);
    let mut acc: Option<WV> = None;
    for &(_, len, d, _) in &checks {
        let len64 = pop(b, O::I64ExtendI32U, &[len], &[WT::I64]);
        let hi = match d {
            Some(d) => {
                let d64 = pop(b, O::I64ExtendI32U, &[d], &[WT::I64]);
                pop(b, O::I64Add, &[last, d64], &[WT::I64])
            }
            None => last,
        };
        let ok = pop(b, O::I64LtU, &[hi, len64], &[WT::I32]);
        acc = Some(match acc {
            None => ok,
            Some(a) => pop(b, O::I32And, &[a, ok], &[WT::I32]),
        });
    }
    let acc = acc.unwrap();
    let zt = pop(b, O::I32GeU, &[i0, n], &[WT::I32]);
    let guard = pop(b, O::I32Or, &[zt, acc], &[WT::I32]);
    // Fast copy: clone the loop, drop each proven check's branch.
    let mut order: Vec<WBlock> = inloop.iter().copied().collect();
    order.sort_by_key(|&lb| cfg.rpo_pos[lb]);
    let (bmap, _) = clone_loop(b, defb, &order);
    for &(cb, _, _, pass_true) in &checks {
        let nb = bmap[&cb];
        let Terminator::CondBr {
            if_true, if_false, ..
        } = b.blocks[nb].terminator.clone()
        else {
            continue;
        };
        let pass = if pass_true { if_true } else { if_false };
        b.blocks[nb].terminator = Terminator::Br { target: pass };
    }
    // Route the preheader edge: guard -> fast clone, else original.
    let args = match &b.blocks[pre].terminator {
        Terminator::Br { target } => target.args.clone(),
        _ => unreachable!(),
    };
    let fh = bmap[&h];
    b.blocks[pre].terminator = Terminator::CondBr {
        cond: guard,
        if_true: BlockTarget {
            block: fh,
            args: args.clone(),
        },
        if_false: BlockTarget { block: h, args },
    };
    b.recompute_edges();
    if verbose {
        eprintln!("wbcheck: {h} versioned {} checks -> {fh}", checks.len());
    }
    Some((fh, order.iter().map(|lb| bmap[lb]).collect()))
}

/// Clone an `unreachable`-terminated sink block (panic path) for a loop
/// copy, remapping loop values through `vmap` and its own params/insts
/// locally.
fn clone_sink(
    b: &mut FunctionBody,
    defb: &mut waffle::entity::PerEntity<WV, WBlock>,
    vmap: &FxHashMap<WV, WV>,
    blk: WBlock,
) -> WBlock {
    fn mapv(
        b: &FunctionBody,
        lmap: &FxHashMap<WV, WV>,
        vmap: &FxHashMap<WV, WV>,
        v: WV,
    ) -> WV {
        let r = b.resolve_alias(v);
        lmap.get(&r).or_else(|| vmap.get(&r)).copied().unwrap_or(r)
    }
    let nb = b.add_block();
    let mut lmap: FxHashMap<WV, WV> = FxHashMap::default();
    for &(ty, pv) in b.blocks[blk].params.clone().iter() {
        let np = b.add_blockparam(nb, ty);
        defb[np] = nb;
        lmap.insert(pv, np);
    }
    let insts = b.blocks[blk].insts.clone();
    let mut aliases = Vec::new();
    for inst in insts {
        match b.values[inst] {
            ValueDef::Operator(op, aa, tt) => {
                let args: Vec<WV> = b.arg_pool[aa]
                    .iter()
                    .map(|&a| mapv(b, &lmap, vmap, a))
                    .collect();
                let tys: Vec<WT> = b.type_pool[tt].to_vec();
                let nv = b.add_op(nb, op, &args, &tys);
                defb[nv] = nb;
                lmap.insert(inst, nv);
            }
            ValueDef::PickOutput(from, idx, ty) => {
                let nv = b.add_value(ValueDef::PickOutput(
                    mapv(b, &lmap, vmap, from),
                    idx,
                    ty,
                ));
                b.blocks[nb].insts.push(nv);
                defb[nv] = nb;
                lmap.insert(inst, nv);
            }
            ValueDef::Alias(_) => aliases.push(inst),
            _ => {}
        }
    }
    for inst in aliases {
        if let ValueDef::Alias(a) = b.values[inst] {
            let nv = mapv(b, &lmap, vmap, a);
            lmap.insert(inst, nv);
        }
    }
    b.set_terminator(nb, Terminator::Unreachable);
    nb
}
