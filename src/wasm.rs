//! Optimized Cranelift IR -> waffle IR -> wasm32 "object" module.
//!
//! `lower_to_wasm_clif` runs FnLower + the pliron-clif-opt suite (same as
//! the native backend) via `lower::lower_clif_wasm`, then `clif_emit`
//! translates each function's optimized CLIF to waffle SSA.
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
use pliron::context::Context;
use pliron::r#type::{TypeHandle, Typed};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::{SmallVec, smallvec};
use waffle::entity::EntityRef;
use waffle::{
    Block as WBlock, BlockTarget, Export, ExportKind, Func, FuncDecl, FunctionBody, Global,
    GlobalData, Import, ImportKind, Memory, MemoryArg, MemoryData, Module, Operator as O,
    Signature, SignatureData, Table, TableData, Terminator, Type as WT, Value as WV, ValueDef,
};

use crate::context::State;
use crate::has_body;
use crate::lower::write_const;
use crate::types::{TyK, classify, leaves, size_align};

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
        // Native 64/128-bit vectors: one v128 (a 64-bit vector's upper half
        // is don't-care).
        t if t.is_vector() && matches!(t.bits(), 64 | 128) => WT::V128,
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

thread_local!(static QUIET: Cell<bool> = const { Cell::new(false) });

/// Diagnose malformed targets before waffle's optimizer ICEs on
/// them: every branch's arg count must equal the target's params.
/// Debug-only: release skips it — it ran per splice and per function,
/// and a real arity bug lands as malformed wasm at instantiation anyway.
fn check_targets(b: &FunctionBody, name: &str) {
    if cfg!(debug_assertions) {
        for bi in b.blocks.iter() {
            b.blocks[bi].terminator.visit_targets(|t| {
                assert_eq!(
                    t.args.len(),
                    b.blocks[t.block].params.len(),
                    "{name}: block{bi:?} -> {:?} args {} != params {}",
                    t.block,
                    t.args.len(),
                    b.blocks[t.block].params.len()
                );
            });
        }
    }
}

/// Run `f`, turning a panic into `Err(message)` without printing it.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |i| {
            if !QUIET.with(|q| q.get()) || crate::env_set("PLIRON_WASM_PANIC") {
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

/// `f(0..n)` on all cores (`PLIRON_PAR` sizes it, `=0` runs inline),
/// results in index order. For per-function work that reads only its own
/// input — the module is assembled afterwards in the serial order.
fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let par = crate::env_get("PLIRON_PAR").and_then(|v| v.parse::<usize>().ok());
    let threads = if par == Some(0) {
        1
    } else {
        par.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
            .clamp(1, n.max(1))
    };
    if threads <= 1 {
        return (0..n).map(f).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<T>>> = (0..n).map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    *slots[i].lock().unwrap() = Some(f(i));
                }
            });
        }
    });
    slots.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
}

struct FDecl {
    name: String,
    sig: Signature,
    body: Option<FunctionBody>,
    /// `#[inline(always)]`: splice at every callsite regardless of
    /// size (LLVM's AlwaysInliner parity). `#[inline(never)]` blocks
    /// inlining entirely.
    always: bool,
    /// `#[inline]` hint (LLVM `inlinehint`): raised size budget.
    hint: bool,
    no_inline: bool,
}

struct Obj {
    m: Module<'static>,
    sigs: FxHashMap<(Vec<WT>, Vec<WT>), Signature>,
    funcs: Vec<FDecl>,
    fidx: FxHashMap<String, usize>,
    got: FxHashMap<(bool, String), Global>,
    sp: Global,
    table: Table,
    mem: Memory,
}

impl Obj {
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

    /// `GOT.func.<sym>` (table index) or `GOT.mem.<sym>` (address); the
    /// func/data class comes from the reference namespace even when the
    /// name isn't in st.funcs (`\x01` verbatim syms, libcalls, extern data).
    fn got_at(&mut self, func: bool, sym: &str) -> Global {
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
            body: None,
            always: false,
            hint: false,
            no_inline: false,
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
    for t in target
        .features
        .split(',')
        .chain(cg.split(','))
        .map(str::trim)
    {
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

/// What the module tail reads from the pliron `Context`: the `pliron.link`
/// section and the `#[link_section]` custom sections. Serialized before
/// CLIF lowering so `PLIRON_LOWERFREE` can drop the `Context` once phase
/// (A) is done (pure functions of the `Context` and `State`, which
/// lowering doesn't change: the bytes are the same either way).
struct ModTail {
    /// `pliron.link` function entries `(name, flags)`; serialized at emit
    /// time so `PLIRON_WASM_GC` can leave dropped functions out.
    link_funcs: Vec<(String, u8)>,
    /// `pliron.link` data entries, already serialized (count first).
    link_data: Vec<u8>,
    /// `PLIRON_WASM_GC`: functions whose address escapes at the IR level
    /// (`address_taken`: `Sym` consts in global initializers — the data
    /// relocations above — and `llvm.addressof` ops). Empty when off.
    taken: FxHashSet<String>,
    custom: Vec<(String, Vec<u8>)>,
}

impl ModTail {
    fn new(ctx: &Context, st: &State) -> Self {
        let link_funcs = link_funcs(ctx, st);
        let link_data = link_data(ctx, st);
        let taken = if pliron_cfg::env_is("PLIRON_WASM_GC", "1") {
            pliron_ir_opt::address_taken(ctx, st)
        } else {
            FxHashSet::default()
        };
        // `#[link_section = "name"]` statics are wasm custom sections, which
        // the linker concatenates (wasm-bindgen's `__wasm_bindgen_unstable`
        // metadata).
        let mut custom = Vec::new();
        for (n, g) in &st.globals {
            if let (Some(sec), Some(init)) = (custom_section_name(g), g.init) {
                let (size, _) = size_align(ctx, init.get_type(ctx));
                let mut data = vec![0u8; size as usize];
                let mut relocs = Vec::new();
                write_const(ctx, st, init, 0, &mut data, &mut relocs);
                assert!(
                    relocs.is_empty(),
                    "pliron-wasm: {n}: relocations in custom section {sec}"
                );
                custom.push((sec.to_string(), data));
            }
        }
        ModTail {
            link_funcs,
            link_data,
            taken,
            custom,
        }
    }

    /// The `pliron.link` payload without the functions in `dropped`.
    fn link(&self, dropped: &FxHashSet<String>) -> Vec<u8> {
        let mut out = Vec::new();
        let fs: Vec<_> = self
            .link_funcs
            .iter()
            .filter(|(n, _)| !dropped.contains(n))
            .collect();
        put_u32(&mut out, fs.len() as u32);
        for (n, flags) in fs {
            put_str(&mut out, n);
            out.push(*flags);
        }
        out.extend_from_slice(&self.link_data);
        out
    }
}

/// Module tail: waffle-IR inlining, function
/// index remap (imports first), per-body wasm emit with stub fallback,
/// exports, and the `pliron.link`/`target_features`/custom sections.
fn emit_obj(
    mut o: Obj,
    tail: ModTail,
    st: &State,
    name: &str,
    features: &[String],
    mut stubs: u32,
    defined: u32,
    verbose: bool,
) -> Vec<u8> {
    // Waffle-IR inlining: splice small / single-callsite callees into
    // callers while `Call` targets still index `o.funcs`. Callees whose
    // last call edge disappears are dropped by wasm-ld's GC â€” this is
    // the main lever on the 4x function-count gap vs stock.
    let t0 = std::time::Instant::now();
    if crate::env_get("PLIRON_WASM_INLINE") != Some("0") {
        winline(&mut o.funcs, verbose, o.sp);
    }
    pliron_cfg::trace("*wasm-winline*", t0.elapsed().as_secs_f64() * 1e3, name);
    pliron_cfg::mark(&format!("wasm-winline {name}"));
    let t0 = std::time::Instant::now();
    // `PLIRON_WASM_GC=1`: drop `Local` functions nothing in this module
    // references any more — mostly callees `winline` spliced into every
    // caller. LLVM's GlobalDCE removes these before the object is
    // written; without it the object (and an rlib's members) carries
    // bodies the final link's GC drops anyway.
    let dropped = if pliron_cfg::env_is("PLIRON_WASM_GC", "1") {
        wgc(&mut o, &tail.taken, st, name)
    } else {
        FxHashSet::default()
    };

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
    let mut decls: Vec<FDecl> = order.iter().map(|&old| decls[old].take().unwrap()).collect();
    for d in &mut decls {
        if let Some(b) = d.body.as_mut() {
            for v in b.values.values_mut() {
                if let ValueDef::Operator(O::Call { function_index }, ..) = v {
                    *function_index = Func::new(remap[function_index.index()]);
                }
            }
        }
    }
    // Bodies compile independently (waffle's backend reads only the body).
    let compiled: Vec<Option<Result<Vec<u8>, String>>> = par_map(decls.len(), |i| {
        decls[i].body.as_ref().map(|b| match guarded(|| b.compile().map_err(|e| e.to_string())) {
            Ok(Ok(c)) => Ok(c.into_raw_body()),
            Ok(Err(e)) | Err(e) => Err(e),
        })
    });
    // `PLIRON_WASM_SIZELOG=1`: one `wsize <insts> <bytes> <name>` line per
    // body on stderr — waffle IR size against emitted bytes.
    if crate::env_set("PLIRON_WASM_SIZELOG") {
        for (d, c) in decls.iter().zip(&compiled) {
            if let (Some(b), Some(Ok(bytes))) = (d.body.as_ref(), c) {
                let insts: usize = b.blocks.values().map(|d| d.insts.len()).sum();
                eprintln!("wsize {insts} {} {}", bytes.len(), d.name);
            }
        }
    }
    for (d, c) in decls.into_iter().zip(compiled) {
        let f = Func::new(o.m.funcs.len());
        match c {
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
            Some(r) => {
                let bytes = match r {
                    Ok(c) => c,
                    Err(e) => {
                        stubs += 1;
                        if let Some(f) = crate::env_get("PLIRON_WASM_STUBLOG") {
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
                            if pliron_cfg::tracing_on() {
                                tracing::debug!(target: "pliron::wasm","pliron-wasm: stub {} (backend): {e}", d.name)
                            };
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
    pliron_cfg::trace("*wasm-compile*", t0.elapsed().as_secs_f64() * 1e3, name);
    pliron_cfg::mark(&format!("wasm-compile {name}"));
    let t0 = std::time::Instant::now();
    let mut bytes =
        o.m.to_wasm_bytes()
            .unwrap_or_else(|e| panic!("pliron-wasm: {name}: {e}"));
    pliron_cfg::trace("*wasm-bytes*", t0.elapsed().as_secs_f64() * 1e3, name);
    pliron_cfg::mark(&format!("wasm-bytes {name}"));
    let t0 = std::time::Instant::now();
    if crate::pass_enabled("PLIRON_WASM_PEEP") {
        wpeep(&mut bytes, name);
    }
    pliron_cfg::trace("*wasm-peep*", t0.elapsed().as_secs_f64() * 1e3, name);
    pliron_cfg::mark(&format!("wasm-peep {name}"));
    custom_section(&mut bytes, "pliron.link", &tail.link(&dropped));
    let mut tf = Vec::new();
    leb(&mut tf, features.len() as u32);
    for f in features {
        tf.push(b'+');
        leb(&mut tf, f.len() as u32);
        tf.extend_from_slice(f.as_bytes());
    }
    custom_section(&mut bytes, "target_features", &tf);
    for (sec, data) in &tail.custom {
        custom_section(&mut bytes, sec, data);
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

/// `core::arch::wasm32` / `llvm.*` vector intrinsics arrive as calls to
/// `__pliron_llvm_*` symbols (see `llvm_intrinsic_stub`); wasm covers them
/// natively. Returns (op, nargs, result type); scalar ops return i32.
fn vw_intrinsic(sym: &str) -> Option<(O, usize, WT)> {
    let n = sym
        .strip_prefix("__pliron_llvm_")
        .or_else(|| sym.strip_prefix("llvm."))
        .unwrap_or(sym);
    let n: String = n.chars().map(|c| if c == '.' { '_' } else { c }).collect();
    Some(match n.as_str() {
        "wasm_swizzle" => (O::I8x16Swizzle, 2, WT::V128),
        "wasm_bitselect_v16i8" => (O::V128Bitselect, 3, WT::V128),
        "wasm_anytrue_v16i8" => (O::V128AnyTrue, 1, WT::I32),
        "wasm_alltrue_v16i8" => (O::I8x16AllTrue, 1, WT::I32),
        "wasm_alltrue_v8i16" => (O::I16x8AllTrue, 1, WT::I32),
        "wasm_alltrue_v4i32" => (O::I32x4AllTrue, 1, WT::I32),
        "wasm_alltrue_v2i64" => (O::I64x2AllTrue, 1, WT::I32),
        "wasm_bitmask_v16i8" => (O::I8x16Bitmask, 1, WT::I32),
        "wasm_bitmask_v8i16" => (O::I16x8Bitmask, 1, WT::I32),
        "wasm_bitmask_v4i32" => (O::I32x4Bitmask, 1, WT::I32),
        "wasm_bitmask_v2i64" => (O::I64x2Bitmask, 1, WT::I32),
        "wasm_avgr_unsigned_v16i8" => (O::I8x16AvgrU, 2, WT::V128),
        "wasm_avgr_unsigned_v8i16" => (O::I16x8AvgrU, 2, WT::V128),
        "wasm_q15mulr_sat_signed" => (O::I16x8Q15MulrSatS, 2, WT::V128),
        "wasm_extadd_pairwise_signed_v8i16" => (O::I16x8ExtAddPairwiseI8x16S, 1, WT::V128),
        "wasm_extadd_pairwise_unsigned_v8i16" => (O::I16x8ExtAddPairwiseI8x16U, 1, WT::V128),
        "wasm_extadd_pairwise_signed_v4i32" => (O::I32x4ExtAddPairwiseI16x8S, 1, WT::V128),
        "wasm_extadd_pairwise_unsigned_v4i32" => (O::I32x4ExtAddPairwiseI16x8U, 1, WT::V128),
        "wasm_dot" => (O::I32x4DotI16x8S, 2, WT::V128),
        "nearbyint_v4f32" => (O::F32x4Nearest, 1, WT::V128),
        "nearbyint_v2f64" => (O::F64x2Nearest, 1, WT::V128),
        "minimum_v4f32" => (O::F32x4Min, 2, WT::V128),
        "minimum_v2f64" => (O::F64x2Min, 2, WT::V128),
        "maximum_v4f32" => (O::F32x4Max, 2, WT::V128),
        "maximum_v2f64" => (O::F64x2Max, 2, WT::V128),
        _ => return None,
    })
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

#[derive(Clone, Copy, Debug)]
enum WPeek {
    Get(u32),
    Set(u32),
    Tee(u32),
    /// Control-flow instruction: anything that can change which
    /// instruction executes next (or how the operand stack's shape is
    /// interpreted at structured boundaries).
    Ctl,
    /// Deleted by the rewrite passes; emits no bytes.
    Del,
    /// `local.set` proven dead; emits `drop` to keep the stack effect.
    Drop,
    /// A `local.get l; local.set l` self-copy deleted by the adjacent-pair
    /// coalescing. Emits no bytes â€” the pair is a real no-op â€” but it both
    /// *reads* l and *writes back the observed value*, and later rewrites
    /// (the copy-forward fixpoint below) move defs between locals, which
    /// can change what l holds at this point. Keeping the marker makes the
    /// analysis see the read and the reaffirming write it would otherwise
    /// lose track of.
    Nop(u32),
    /// A `br_if` produced by the back-edge `if` fold (the loop's
    /// `if c { copies; br L } else { .. }` restructure): emits
    /// `0x0d` + the (already depth-adjusted) target.
    BrIf(u32),
    /// A `br` whose label was retargeted `d-1` for the `if`-level the
    /// fold removed (else-arm control flow under the same fold): emits
    /// `0x0c` + the adjusted target.
    Br(u32),
    /// A synthesized `i32.eqz` (`0x45`) — emitted by the `bri2` fold to
    /// invert an `if` condition into a `br_if` test. `s`/`e` carry no
    /// real byte range; passes that byte-decode `Other`/`Ctl` must
    /// never see it reach those arms.
    Eqz,
    Other,
}

/// Decode one function body into instructions; rewrite the
/// local-shuffle noise waffle's SSA->locals lowering leaves behind:
/// `local.get x; local.set x` is a no-op pair, `local.set x; local.get
/// x` fuses to `local.tee x`, and `local.tee x; local.set x` is just
/// `local.set x`. All three edits keep the operand stack and locals
/// identical and only span adjacent instructions, so structured
/// control flow is unaffected (branch targets are label depths, not
/// byte offsets). Returns None â€” caller keeps the original bytes â€” on
/// any opcode it can't decode. `fsig`/`tsig` give (pops, pushes) per
/// function index and per type index so calls can take part in
/// stack-effect tracking.
#[cfg(test)]
fn peep_body(b: &[u8], fsig: &[(u32, u32)], tsig: &[(u32, u32)], nparams: u32) -> Option<Vec<u8>> {
    peep_body_opts(b, fsig, tsig, nparams, &PeepOpts::from_env())
}

#[path = "wpeep_audit.rs"]
mod wpeep_audit;

/// `wpeep` switches. `from_env`: `PLIRON_WASM_AUDIT=1` audits every
/// copy-forward round with the Datalog checker in `wpeep_audit` and
/// reverts rounds that fail it (`=panic` panics instead);
/// `PLIRON_WASM_PEEP_PATHCHECK=0` drops the straight-line requirement
/// (dab149b) — only for demonstrating that the audit catches that class.
struct PeepOpts {
    audit: bool,
    audit_panic: bool,
    /// Module#body tag for audit diagnostics (set by the caller).
    tag: String,
    pathcheck: bool,
    violations: std::cell::Cell<u32>,
    /// Rounds over the audit's budget, left unaudited.
    skipped: std::cell::Cell<u32>,
    /// `PLIRON_WASM_TRACE_BODY=<k>` matched this body: dump `ins` after
    /// each rewrite phase.
    trace: bool,
    /// Dead stack-value flush (`PLIRON_WASM_DDROP`).
    ddrop: bool,
    /// Dead-else arm flush (`PLIRON_WASM_DELSE`).
    delse: bool,
    /// `if {br K} else {A}` → `br_if` flush (`PLIRON_WASM_BRI`).
    bri: bool,
    /// `if {A} else {br K}` → `i32.eqz; br_if` flush (`PLIRON_WASM_BRI2`).
    bri2: bool,
    /// `br K` before its K+1 enclosing `end`s (`PLIRON_WASM_BTR=0` off).
    btr: bool,
}

impl PeepOpts {
    fn from_env() -> Self {
        let a = crate::env_get("PLIRON_WASM_AUDIT");
        PeepOpts {
            audit: a.is_some_and(|v| v != "0"),
            audit_panic: a == Some("panic"),
            tag: String::new(),
            pathcheck: crate::pass_enabled("PLIRON_WASM_PEEP_PATHCHECK"),
            violations: std::cell::Cell::new(0),
            skipped: std::cell::Cell::new(0),
            trace: false,
            ddrop: crate::env_get("PLIRON_WASM_DDROP") != Some("0"),
            delse: crate::env_get("PLIRON_WASM_DELSE") != Some("0"),
            bri: crate::env_get("PLIRON_WASM_BRI") != Some("0"),
            bri2: crate::env_get("PLIRON_WASM_BRI2") != Some("0"),
            btr: crate::env_get("PLIRON_WASM_BTR") != Some("0"),
        }
    }
}

fn wpeek_name(k: WPeek) -> String {
    match k {
        WPeek::Get(l) => format!("get {l}"),
        WPeek::Set(l) => format!("set {l}"),
        WPeek::Tee(l) => format!("tee {l}"),
        WPeek::Drop => "drop".into(),
        WPeek::Del => "DEL".into(),
        WPeek::Nop(l) => format!("nop {l}"),
        WPeek::Br(d) => format!("br {d}"),
        WPeek::BrIf(d) => format!("br_if {d}"),
        WPeek::Ctl => "ctl".into(),
        WPeek::Eqz => "eqz".into(),
        WPeek::Other => "other".into(),
    }
}

fn peep_body_opts(
    b: &[u8],
    fsig: &[(u32, u32)],
    tsig: &[(u32, u32)],
    nparams: u32,
    opts: &PeepOpts,
) -> Option<Vec<u8>> {
    macro_rules! wtrace {
        ($tag:expr, $ins:expr) => {
            if opts.trace {
                wdump($tag, &$ins, b);
            }
        };
    }
    fn wdump(tag: &str, ins: &[(WPeek, usize, usize, usize)], b: &[u8]) {
        eprintln!("--- {tag} ---");
        for (i, &(k, s, _e, _)) in ins.iter().enumerate() {
            eprintln!("{i:4} {} @{s} [{:02x}]", wpeek_name(k), b[s]);
        }
    }
    // Locals prefix: vec of (count, valtype). waffle emits one group per
    // local; re-emit adjacent same-type runs as a single group. This is
    // index-preserving (merge only, never reorder), so it needs no other
    // fixups and costs two bytes per eliminated group.
    let mut p = 0usize;
    let nl = urd(b, &mut p)?;
    let mut groups: Vec<(u32, u8)> = Vec::new();
    for _ in 0..nl {
        let c = urd(b, &mut p)?;
        // Single-byte valtypes only (numeric/v128/funcref/externref);
        // typed refs (0x63/0x64 + heaptype) need a longer decode â€” bail.
        let t = *b.get(p)?;
        if !matches!(t, 0x6f | 0x70 | 0x78..=0x7f) {
            return None;
        }
        p += 1;
        match groups.last_mut() {
            Some((gc, gt)) if *gt == t => *gc += c,
            _ => groups.push((c, t)),
        }
    }
    // Instruction stream: (kind, byte range). `depth` tracks structured
    // constructs so the scan stops on the `end` that closes the body.
    // `stack` additionally records each instruction's structured
    // context â€” the enclosing (construct, arm) pairs â€” so the rewrite
    // pass can tell when a use/def lives on a different `if` arm than
    // the producer (and can therefore never observe its value).
    let mut ins: Vec<(WPeek, usize, usize, usize)> = Vec::new();
    let mut ctx: Vec<Vec<(u32, u8)>> = Vec::new();
    // Opcode per decode-time ins index: ctx entries record the
    // construct's decode index, which coalescing below makes stale as
    // an `ins` index â€” `dop` keeps it resolvable.
    let mut dop: Vec<u8> = Vec::new();
    let mut stack: Vec<(u32, u8)> = Vec::new();
    let mut depth = 0i32;
    loop {
        if p >= b.len() {
            return None;
        }
        let s = p;
        let op = b[p];
        p += 1;
        let k = match op {
            0x01 | 0x1a | 0x1b | 0xd1 => WPeek::Other,
            0x00 | 0x0f => WPeek::Ctl,
            0x05 => {
                // else: belongs to the `if` at the top of the stack;
                // after it, that arm is the else-arm.
                ctx.push(stack.clone());
                if let Some(top) = stack.last_mut() {
                    top.1 = 1;
                }
                dop.push(op);
                ins.push((WPeek::Ctl, s, p, ctx.len() - 1));
                continue;
            }
            0x02 | 0x03 | 0x04 => {
                depth += 1;
                sleb_skip(b, &mut p)?; // blocktype: valtype or type index
                ctx.push(stack.clone());
                stack.push((ins.len() as u32, 0));
                dop.push(op);
                ins.push((WPeek::Ctl, s, p, ctx.len() - 1));
                continue;
            }
            0x0b => {
                depth -= 1;
                ctx.push(stack.clone());
                dop.push(op);
                ins.push((WPeek::Ctl, s, p, ctx.len() - 1));
                stack.pop();
                if depth < 0 {
                    break; // `end` closing the function body itself
                }
                continue;
            }
            0x0c | 0x0d | 0x10 | 0x12 | 0x14 => {
                urd(b, &mut p)?;
                WPeek::Ctl
            }
            0x11 | 0x13 => {
                urd(b, &mut p)?;
                urd(b, &mut p)?;
                WPeek::Ctl
            }
            0x25 | 0x26 | 0x3f | 0x40 | 0xd2 => {
                urd(b, &mut p)?;
                WPeek::Other
            }
            0x0e => {
                // br_table: n targets + default.
                let n = urd(b, &mut p)?;
                for _ in 0..=n {
                    urd(b, &mut p)?;
                }
                WPeek::Ctl
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
        ctx.push(stack.clone());
        dop.push(op);
        ins.push((k, s, p, ctx.len() - 1));
    }
    if p != b.len() {
        return None; // trailing bytes after the body `end`
    }
    wtrace!("decode", ins);
    // Coalesce adjacent pairs until none rewrite.
    loop {
        let mut out: Vec<(WPeek, usize, usize, usize)> = Vec::with_capacity(ins.len());
        let mut changed = false;
        let mut i = 0;
        while i < ins.len() {
            // Pair detection skips Nop markers: deleted self-copies are
            // stack-transparent, so `get a; <nop>; set a` is still a
            // no-op pair. Skipped markers are re-emitted either way.
            let mut j = i + 1;
            while j < ins.len() && matches!(ins[j].0, WPeek::Nop(_)) {
                j += 1;
            }
            let pair = (ins[i].0, ins.get(j).map(|x| x.0));
            match pair {
                (WPeek::Get(a), Some(WPeek::Set(c))) if a == c => {
                    out.push((WPeek::Nop(a), ins[i].1, ins[i].2, ins[i].3));
                    out.extend_from_slice(&ins[i + 1..j]);
                    changed = true;
                    i = j + 1;
                    continue;
                }
                (WPeek::Set(a), Some(WPeek::Get(c))) if a == c => {
                    out.push((WPeek::Tee(a), ins[i].1, ins[i].2, ins[i].3));
                    out.extend_from_slice(&ins[i + 1..j]);
                    changed = true;
                    i = j + 1;
                    continue;
                }
                (WPeek::Tee(a), Some(WPeek::Set(c))) if a == c => {
                    out.push((WPeek::Set(a), ins[i].1, ins[i].2, ins[i].3));
                    out.extend_from_slice(&ins[i + 1..j]);
                    changed = true;
                    i = j + 1;
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
    wtrace!("coalesce", ins);
    // Materialization forwarding: waffle's treeify only inlines pure
    // ops, so every load / non-pure result gets `P; local.set t` then a
    // later `local.get t` â€” LLVM instead keeps such values on the
    // operand stack, which is why its loops are ~40% shorter. When t is
    // defined exactly once (that set) and read exactly once (a later
    // `local.get`), we can replay P at the use site and drop both local
    // ops. P must be a self-contained stack unit â€” net effect +1 and
    // never consuming values pushed before it â€” composed only of
    // "transparent" instrs: no control flow, no local/global writes,
    // no stores/calls (loads and pure ops may reorder freely; a load
    // moving across another load changes only trap order, which is
    // unobservable). Everything between a forwarded set and its use
    // must be transparent too â€” an `else`, a `local.set` of another
    // local, or a store between them makes the move unsafe (the use
    // could observe a different local value, or a load could cross a
    // store). Stack shape is preserved exactly: each producer replayed
    // at its use contributes the +1 the deleted `local.get` provided.
    if crate::env_get("PLIRON_WASM_FWD") != Some("0") {
        // Stack (pops, pushes) for instrs allowed inside producers and
        // in set->use gaps. None = opaque (barrier).
        let tr = |i: usize| -> Option<(u32, u32)> {
            match ins[i].0 {
                WPeek::Get(_) => Some((0, 1)),
                WPeek::Other => {
                    let op = b[ins[i].1];
                    Some(match op {
                        0x01 => (0, 0),                      // nop
                        0x1a => (1, 0),                      // drop
                        0x1b | 0x1c => (3, 1),               // select
                        0x23 => (0, 1),                      // global.get
                        0x28..=0x35 => (1, 1),               // loads
                        0x41..=0x44 => (0, 1),               // consts
                        0x45 | 0x50 => (1, 1),               // eqz
                        0x46..=0x4f | 0x51..=0x66 => (2, 1), // compares
                        0x67..=0x69 | 0x79..=0x7b | 0x8b..=0x91 | 0x99..=0x9f => {
                            (1, 1)
                        }
                        0x6a..=0x78 | 0x7c..=0x8a | 0x92..=0x98 | 0xa0..=0xa6 => {
                            (2, 1)
                        }
                        0xa7..=0xc4 => (1, 1), // conversions
                        0xd1 | 0xd2 => (0, 1), // ref.null / ref.func
                        _ => return None,
                    })
                }
                _ => None,
            }
        };
        let mut uses: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
        let mut defs: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
        for (i, &(k, _, _, _)) in ins.iter().enumerate() {
            match k {
                WPeek::Get(l) => uses.entry(l).or_default().push(i),
                WPeek::Set(l) => defs.entry(l).or_default().push(i),
                WPeek::Tee(l) => {
                    uses.entry(l).or_default().push(i);
                    defs.entry(l).or_default().push(i);
                }
                _ => {}
            }
        }
        // Forwardable `local.set t` at i: u is the ONLY use of t in the
        // window (i, next def of t) â€” so deleting the set cannot leave
        // a stale read: uses before i read an earlier def, uses after
        // the next def read a newer one, and we replace u itself with
        // the producer. Multi-def locals (temps reused across unrolled
        // copies) forward fine under this rule.
        // Instruction a and b sit in different arms of the same `if`
        // when their enclosing-construct lists first differ by arm.
        let arm_div = |a: usize, b2: usize| -> bool {
            let (a, b2) = (&ctx[ins[a].3], &ctx[ins[b2].3]);
            for k in 0..a.len().min(b2.len()) {
                if a[k].0 != b2[k].0 {
                    return false;
                }
                if a[k].1 != b2[k].1 {
                    return true;
                }
            }
            false
        };
        // A later def d of a local provably rewrites the value before
        // use u reads it only when d's context encloses u's (same
        // construct path and arm â€” it can't be skipped) and no
        // branch-family instruction between them can hop over d. A def
        // on a conditional arm never covers: paths that skip it still
        // observe the earlier value.
        let covers = |d: usize, u: usize| -> bool {
            let (cd, cu) = (&ctx[ins[d].3], &ctx[ins[u].3]);
            cd.len() <= cu.len()
                && cu[..cd.len()] == cd[..]
                && !(d + 1..u).any(|k| {
                    matches!(ins[k].0, WPeek::Ctl) && matches!(b[ins[k].1], 0x0c | 0x0d | 0x0e)
                })
        };
        // Shared enclosing `loop`: a back-edge inside it can carry a
        // value between sibling if-arms on the next iteration, so
        // arm-divergence does not make positions unreachable across one.
        let shr_lp = |a: usize, b2: usize| {
            ctx[ins[a].3]
                .iter()
                .any(|e| dop[e.0 as usize] == 0x03 && ctx[ins[b2].3].contains(e))
        };
        let fwd = |i: usize, del: &[bool]| -> Option<(u32, usize)> {
            let WPeek::Set(t) = ins[i].0 else { return None };
            if del[i] || !defs.get(&t).map(|v| v.contains(&i)).unwrap_or(false) {
                return None;
            }
            // A read positioned before the set can observe its value
            // through a `loop` back-edge feeding the next iteration â€”
            // that makes the set unforwardable outright (the producer
            // can never be moved backwards to it).
            if uses
                .get(&t)
                .map(|v| v.iter().any(|&u| u < i && !del[u] && shr_lp(i, u)))
                .unwrap_or(false)
            {
                return None;
            }
            // The use must be the *only* later-positioned reader that
            // can observe this set's value: any use not covered by a
            // following def (conditional defs don't cover â€” paths that
            // skip them still see this value). A read inside a
            // conditional arm counts as a reader too â€” it just can't
            // serve as the splice target below.
            let w: Vec<usize> = uses
                .get(&t)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|&u| {
                            if del[u] || u <= i || (arm_div(i, u) && !shr_lp(i, u)) {
                                return false;
                            }
                            !defs
                                .get(&t)
                                .map(|v| {
                                    v.iter().any(|&d| d > i && d < u && !del[d] && covers(d, u))
                                })
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let [u] = w.as_slice() else { return None };
            // Splice-target validity: every path reaching u must pass
            // through i, and the producer can't gain conditionality â€”
            // an `if` construct on either side of the context LCP
            // means some path observes a different value (or the
            // producer, possibly trapping, no longer always runs).
            {
                let (ci, cu) = (&ctx[ins[i].3], &ctx[ins[*u].3]);
                let lcp = ci.iter().zip(cu.iter()).take_while(|(a, b)| a == b).count();
                if ci[lcp..]
                    .iter()
                    .chain(&cu[lcp..])
                    .any(|e| dop[e.0 as usize] == 0x04)
                {
                    return None;
                }
            }
            if !matches!(ins[*u].0, WPeek::Get(_)) {
                return None;
            }
            Some((t, *u))
        };
        // Minimal self-contained producer suffix of `slice` (instrs in
        // [lo, hi)): walk the stack need backwards; the suffix starts
        // where the need first reaches 0 with no negative dip.
        let prod_start = |lo: usize, hi: usize| -> Option<usize> {
            let mut need: i32 = 1;
            for k in (lo..hi).rev() {
                let (p, q) = tr(k)?;
                need -= q as i32;
                if need < 0 {
                    return None;
                }
                need += p as i32;
                if need == 0 {
                    return Some(k);
                }
            }
            if need == 0 { Some(lo) } else { None }
        };
        let verbose = pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::wasm", tracing::Level::DEBUG);
        let mut del = vec![false; ins.len()]; // forwarded sets + moved producers
        let mut splice: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
        let mut i = 0;
        while i < ins.len() {
            if fwd(i, &del).is_none() {
                i += 1;
                continue;
            }
            // Chain: [transparent run][fwd set]+. The transparent run
            // before each set yields its producer suffix.
            let mut segsets: Vec<(usize, u32, usize)> = Vec::new(); // (set, t, use)
            let mut j = i;
            loop {
                match fwd(j, &del) {
                    Some((t, u)) => {
                        segsets.push((j, t, u));
                        j += 1;
                    }
                    None => break,
                }
                // Instrs until the following set must be transparent;
                // anything else ends the segment.
                while j < ins.len()
                    && !matches!(ins[j].0, WPeek::Set(_))
                    && !del[j]
                    && tr(j).is_some()
                {
                    j += 1;
                }
                if j >= ins.len() || !matches!(ins[j].0, WPeek::Set(_)) {
                    break;
                }
            }
            // Largest prefix of segsets whose uses all land after its
            // last set inside a transparent tail. A later set whose
            // producer would swallow earlier uses (e.g. the
            // loop-carried accumulator's) just truncates the chain.
            let mut k = segsets.len();
            let mut pvec: Vec<(usize, usize)> = Vec::new();
            let mut umax = 0usize;
            while k > 0 {
                let last = segsets[k - 1].0;
                umax = segsets[..k].iter().map(|&(_, _, u)| u).max().unwrap();
                let tail_ok = segsets[..k].iter().all(|&(_, _, u)| u > last)
                    && (last + 1..=umax).all(|k2| tr(k2).is_some());
                if !tail_ok {
                    k -= 1;
                    continue;
                }
                // Producer slice per set: self-contained suffix of the
                // transparent run since the previous set (or before
                // the first). Must not contain a use position or an
                // already-moved/deleted instr.
                let mut plo = {
                    let mut lo = segsets[0].0;
                    while lo > 0 && tr(lo - 1).is_some() {
                        lo -= 1;
                    }
                    lo
                };
                let mut good = true;
                pvec.clear();
                // A producer that reads a forwarded temp would observe
                // its deleted set's value: replaying it at the use site
                // would see the pre-segment local instead.
                let temps: FxHashSet<u32> = segsets[..k].iter().map(|&(_, t, _)| t).collect();
                for &(s, _, _) in &segsets[..k] {
                    match prod_start(plo, s) {
                        Some(ps)
                            if (ps..s).all(|m| !del[m] && !splice.contains_key(&m))
                                && !(ps..s).any(|m| {
                                    matches!(ins[m].0, WPeek::Get(l2) if temps.contains(&l2))
                                })
                                // Nor may it read a local whose def was
                                // already deleted by an earlier
                                // forwarding â€” the producer would replay
                                // against the pre-segment value.
                                && !(ps..s).any(|m| {
                                    matches!(ins[m].0, WPeek::Get(l2) | WPeek::Tee(l2)
                                        if defs.get(&l2).map(|v| {
                                            v.iter().any(|&d| del[d])
                                        }).unwrap_or(false))
                                }) =>
                        {
                            pvec.push((ps, s));
                        }
                        _ => {
                            good = false;
                            break;
                        }
                    }
                    plo = s + 1;
                }
                if good {
                    break;
                }
                k -= 1;
            }
            if k == 0 {
                i += 1;
                continue;
            }
            if verbose {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::wasm","wsched: fwd {k} mats at set@{}", ins[segsets[0].0].1)
                };
            }
            for (m, &(s, _, u)) in segsets[..k].iter().enumerate() {
                let (ps, pe) = pvec[m];
                for m2 in ps..pe {
                    del[m2] = true;
                }
                del[s] = true;
                splice.insert(u, (ps..pe).collect());
            }
            i = umax + 1;
        }
        if !splice.is_empty() {
            let mut ni: Vec<(WPeek, usize, usize, usize)> = Vec::with_capacity(ins.len());
            for (i, &e) in ins.iter().enumerate() {
                if del[i] {
                    continue;
                }
                if let Some(pr) = splice.get(&i) {
                    for &m in pr {
                        ni.push(ins[m]);
                    }
                    continue;
                }
                ni.push(e);
            }
            ins = ni;
        }
    }
    wtrace!("fwd", ins);
    // Store-forward parallel copies: waffle lowers `br blk(v1..vn)` by
    // computing the vi into scratch locals and then copying them onto
    // the block-param locals, e.g.
    //   local.get t1 .. local.get tn; local.set xn .. local.set x1
    // When ti is written exactly once and read only by that copy, the
    // producer can write xi directly and the copy pair disappears
    // (saves two instructions per copied param on every loop edge).
    // Pairing is by operand-stack order within a run of pure local ops:
    // the j-th `local.set` from the end pops the j-th `local.get`.
    //
    // The general case is a rotation: the "scratch" slots are often
    // other params' locals (`get 15;+4;set 14 | get 16;+256;set 15 |
    // get 17;+1024;set 16` then copies `15<-14,16<-15,17<-16`), so
    // pairwise retargeting can clobber a param another producer still
    // needs. Solve as a greatest fixpoint over the copy group: a pair
    // is retargetable iff every access to its destination local within
    // (producer, copy-set) is itself an instruction being rewritten,
    // and the temp has no other defs after its producer and no reads
    // after it other than the deleted copy-get.
    for _ in 0..4 {
        let mut uses: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
        let mut defs: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
        for (i, &(k, _, _, _)) in ins.iter().enumerate() {
            match k {
                WPeek::Get(l) => uses.entry(l).or_default().push(i),
                WPeek::Set(l) => defs.entry(l).or_default().push(i),
                WPeek::Nop(l) => {
                    // Deleted `get l; set l`: reads l and writes the same
                    // value back â€” a use plus an identity def. Without it
                    // in both maps the analysis could retarget a producer
                    // onto l (changing what the read would observe) or
                    // lose the reaffirming write a merge point relied on.
                    uses.entry(l).or_default().push(i);
                    defs.entry(l).or_default().push(i);
                }
                WPeek::Tee(l) => {
                    uses.entry(l).or_default().push(i);
                    defs.entry(l).or_default().push(i);
                }
                _ => {}
            }
        }
        let verbose = pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::wasm", tracing::Level::DEBUG);
        // Stack-order matching inside runs of pure local ops.
        let mut pairs: Vec<(usize, usize)> = Vec::new(); // (get idx, set idx)
        let mut pending: Vec<usize> = Vec::new();
        for (i, &(k, _, _, _)) in ins.iter().enumerate() {
            match k {
                WPeek::Get(_) | WPeek::Tee(_) => pending.push(i),
                // A deleted self-copy is stack-neutral: keep matching
                // through it exactly as if it weren't there.
                WPeek::Nop(_) => {}
                WPeek::Set(_) => {
                    if let Some(g) = pending.pop() {
                        pairs.push((g, i));
                    }
                }
                _ => pending.clear(),
            }
        }
        // Candidate rewrites: (producer idx, copy-get idx, copy-set idx,
        // temp local, dest local). A producer is the last set/tee of the
        // temp before the copy-get.
        let mut cand: Vec<(usize, usize, usize, u32, u32)> = Vec::new();
        for &(gi, si) in &pairs {
            let (WPeek::Get(t), WPeek::Set(x)) = (ins[gi].0, ins[si].0) else {
                continue;
            };
            if t == x {
                continue; // get x ... set x: stack-neutral no-op, leave it
            }
            // A Nop marker can never be the producer: it writes back the
            // value it read, so retargeting it would leave x unwritten.
            let Some(&p1) = defs.get(&t).and_then(|v| {
                v.iter()
                    .rev()
                    .find(|&&d| d < gi && !matches!(ins[d].0, WPeek::Nop(_)))
            }) else {
                continue;
            };
            // Retargeting makes the producer write x on *every* path from
            // p1, not just the ones through the copy-set. The fixpoint
            // below compares the sets of defs reaching each read, which
            // can't see a path that leaves (p1, si) early: the same
            // sources still reach, on different paths. So require
            // straight-line control: same structured context and no
            // branch in between, i.e. every path from p1 reaches si.
            // (A `br` out of an `if` arm between them kept a frame-slot
            // address in x on the exit path; the retarget overwrote it
            // with a finish_grow result — wasm std `words` miscompile.)
            if opts.pathcheck
                && (ctx[ins[p1].3] != ctx[ins[si].3]
                || (p1..si).any(|i| {
                    matches!(ins[i].0, WPeek::Ctl) && matches!(b[ins[i].1], 0x0c | 0x0d | 0x0e | 0x0f)
                }))
            {
                continue;
            }
            cand.push((p1, gi, si, t, x));
        }
        // Greatest fixpoint: drop candidates whose side conditions fail
        // with respect to the currently-surviving set.
        //
        // `arm_diverged(a, b)` is true when instructions a and b sit in
        // different arms of the same `if`, so b can never observe a's
        // writes (or vice versa): walking their enclosing-construct
        // lists outward, the first difference is an arm difference.
        // Any other first difference (different constructs, or one list
        // a prefix of the other) means sequential-or-nested control
        // flow, which we conservatively treat as reachable.
        let arm_div = |a: usize, b: usize| -> bool {
            let (a, b) = (&ctx[ins[a].3], &ctx[ins[b].3]);
            for k in 0..a.len().min(b.len()) {
                if a[k].0 != b[k].0 {
                    return false;
                }
                if a[k].1 != b[k].1 {
                    return true;
                }
            }
            false
        };
        // d dominates u (as a def covering it) when d's structured
        // context is a prefix of u's: d is on u's mandatory path, so no
        // path reaches u without first passing d's position. Branches
        // between them can't divert control into u except through d's
        // own region, so they don't affect covering.
        let covers = |d: usize, u: usize| -> bool {
            let (cd, cu) = (&ctx[ins[d].3], &ctx[ins[u].3]);
            cd.len() <= cu.len() && cu[..cd.len()] == cd[..]
        };
        // Back-edge reach: a def textually after a read still reaches it
        // when both sit inside a `loop` with a back-edge branch after
        // the def. loopbe[construct] = branch positions targeting the
        // loop (br to a loop label re-enters at its head).
        let mut loopbe: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
        for (i, &(k, s, _, ci)) in ins.iter().enumerate() {
            if !matches!(k, WPeek::Ctl) {
                continue;
            }
            let mut lbls: [u32; 2] = [u32::MAX; 2];
            let mut nl = 0usize;
            match b[s] {
                0x0c | 0x0d => {
                    let mut q = s + 1;
                    if let Some(l) = urd(b, &mut q) {
                        lbls[0] = l;
                        nl = 1;
                    }
                }
                0x0e => {
                    // br_table: every target counts.
                    let mut q = s + 1;
                    if let Some(n) = urd(b, &mut q) {
                        let mut all = Vec::with_capacity(n as usize + 1);
                        for _ in 0..=n {
                            if let Some(l) = urd(b, &mut q) {
                                all.push(l);
                            }
                        }
                        for l in all {
                            let c = &ctx[ci];
                            if (l as usize) < c.len() {
                                let e = c[c.len() - 1 - l as usize].0;
                                if dop[e as usize] == 0x03 {
                                    loopbe.entry(e).or_default().push(i);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            for &l in &lbls[..nl] {
                let c = &ctx[ci];
                if (l as usize) < c.len() {
                    let e = c[c.len() - 1 - l as usize].0;
                    if dop[e as usize] == 0x03 {
                        loopbe.entry(e).or_default().push(i);
                    }
                }
            }
        }
        // backedge(d,u): a shared loop e encloses both, and some
        // back-edge branch to e sits after d with no live def of the
        // local covering the branch â€” a dominating def hides d's value
        // from the next iteration.
        let backedge = |dl: &[usize],
                        extra: &[usize],
                        d: usize,
                        u: usize,
                        gone: &dyn Fn(usize) -> bool|
         -> bool {
            let (cd, cu) = (&ctx[ins[d].3], &ctx[ins[u].3]);
            cd.iter().any(|e| {
                if dop[e.0 as usize] != 0x03 || !cu.contains(e) {
                    return false;
                }
                loopbe
                    .get(&e.0)
                    .map(|bs| {
                        bs.iter().any(|&bpos| {
                            bpos > d
                                && !dl.iter().chain(extra.iter()).any(|&d2| {
                                    d2 > d && d2 < bpos && !gone(d2) && covers(d2, bpos)
                                })
                        })
                    })
                    .unwrap_or(false)
            })
        };
        // Reaching-def analysis over a def list (with optional extra
        // defs, e.g. a producer being retargeted onto this local).
        // `gone` marks defs a rewrite removes â€” they neither reach nor
        // cover. Forward (d < u): reaches unless arm-divergent or a
        // covering live def hides it. Backward (d > u): only via a
        // shared-loop back-edge.
        let reaches = |dl: &[usize], extra: &[usize], d: usize, u: usize, gone: &dyn Fn(usize) -> bool| -> bool {
            if arm_div(d, u) {
                // Sibling arms are disjoint on straight-line flow, but a
                // back-edge inside one arm re-enters the enclosing loop
                // and lands in the other arm on the next iteration
                // (core cgu.08#42: a `set x` in the then-arm reached
                // renamed else-arm reads via `br` to the loop head).
                return backedge(dl, extra, d, u, gone);
            }
            if d < u {
                !dl.iter().chain(extra.iter()).any(|&d2| {
                    d2 > d && d2 < u && !gone(d2) && covers(d2, u)
                })
            } else {
                backedge(dl, extra, d, u, gone)
            }
        };
        let obs = |dl: &[usize], extra: &[usize], u: usize, gone: &dyn Fn(usize) -> bool| -> Vec<usize> {
            let mut v: Vec<usize> = dl
                .iter()
                .chain(extra.iter())
                .copied()
                .filter(|&d| !gone(d) && reaches(dl, extra, d, u, gone))
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        // Defs of x inside the (p1, si) window per candidate: these
        // must relocate onto the temp local t, which the producer
        // retarget frees. (si itself is the copy-set, handled apart.)
        // Nop markers aren't relocatable â€” they emit no instruction and
        // their write is bound to the value x holds at their position.
        let xdefs: Vec<Vec<usize>> = cand
            .iter()
            .map(|&(p1, _, si, _, x)| {
                defs.get(&x)
                    .map(|v| {
                        v.iter()
                            .copied()
                            .filter(|&d| d > p1 && d < si && !matches!(ins[d].0, WPeek::Nop(_)))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        // Greatest fixpoint over observed-def sets: a candidate is
        // rewritten iff, in the post-rewrite program, every read keeps
        // observing the same value sources. The producer's local p1 and
        // the copy set si both denote the producer's value â€” normalize
        // them to one marker so sets compare by value source.
        const PSRC: usize = usize::MAX;
        let norm = |v: &mut Vec<usize>, p1: usize, si: usize| {
            for d in v.iter_mut() {
                if *d == p1 || *d == si {
                    *d = PSRC;
                }
            }
            v.sort_unstable();
            v.dedup();
        };
        let (live, acts) = {
            let mut live = vec![true; cand.len()];
            let mut acts: Vec<Option<(Vec<usize>, Vec<usize>)>> = vec![None; cand.len()];
            for _round in 0..16 {
                // def instructions the live candidates remove/retarget;
                // gone in the post-rewrite program.
                let mut mset: FxHashSet<usize> = FxHashSet::default();
                for (ci, &(p1, gi, si, _, _)) in cand.iter().enumerate() {
                    if live[ci] {
                        mset.extend([p1, gi, si]);
                        mset.extend(xdefs[ci].iter().copied());
                    }
                }
                let mut committed: FxHashSet<usize> = FxHashSet::default();
                let mut dropped = false;
                for ci in 0..cand.len() {
                    if !live[ci] {
                        continue;
                    }
                    let (p1, gi, si, t, x) = cand[ci];
                    let rel = &xdefs[ci];
                    let own = |d: usize| d == p1 || d == gi || d == si || rel.contains(&d);
                    let gone_o = |d: usize| mset.contains(&d) && !own(d);
                    // new-program views: x loses si and the relocated
                    // defs but gains p1; t loses p1 but gains relocs.
                    let gonx = |d: usize| gone_o(d) || d == si || rel.contains(&d);
                    let gont = |d: usize| gone_o(d) || d == p1;
                    let nogone = |_: usize| false;
                    let dl_t = defs.get(&t).map(|v| v.as_slice()).unwrap_or(&[]);
                    let dl_x = defs.get(&x).map(|v| v.as_slice()).unwrap_or(&[]);
                    // The copy-get must read exactly the producer.
                    let mut ok = obs(dl_t, &[], gi, &nogone) == [p1];
                    let mut why = if ok { 0 } else { 1 };
                    let mut tre: Vec<usize> = Vec::new();
                    let mut xrd: Vec<usize> = Vec::new();
                    if ok {
                        for &u in uses.get(&t).map(|v| v.as_slice()).unwrap_or(&[]) {
                            // Nop markers count as reads: they may be
                            // retargeted like a get (the self-copy stays a
                            // no-op on whichever local it points at).
                            if u == gi || !matches!(ins[u].0, WPeek::Get(_) | WPeek::Nop(_)) {
                                continue;
                            }
                            let s = obs(dl_t, &[], u, &nogone);
                            if s.contains(&p1) {
                                // reads p1's value -> retarget to x;
                                // valid only when p1 is its sole source
                                // and x carries exactly p1's value to u.
                                let sx = obs(dl_x, &[p1], u, &gonx);
                                if s != [p1] || sx != [p1] {
                                    ok = false;
                                    why = 2;
                                    break;
                                }
                                tre.push(u);
                            } else {
                                // keeps reading t: new t-defs must be
                                // exactly its old sources.
                                let st = obs(dl_t, rel, u, &gont);
                                if st != s {
                                    ok = false;
                                    why = 3;
                                    break;
                                }
                            }
                        }
                    }
                    if ok {
                        for &u in uses.get(&x).map(|v| v.as_slice()).unwrap_or(&[]) {
                            if !matches!(ins[u].0, WPeek::Get(_) | WPeek::Nop(_)) {
                                continue;
                            }
                            let so = obs(dl_x, &[], u, &nogone);
                            let sn = obs(dl_x, &[p1], u, &gonx);
                            let (mut a, mut c) = (so.clone(), sn.clone());
                            norm(&mut a, p1, si);
                            norm(&mut c, p1, si);
                            if a == c {
                                continue;
                            }
                            // Every old source was a window def being
                            // relocated to t: retarget the read iff t
                            // mirrors x's old reaching set exactly.
                            if !so.is_empty()
                                && so.iter().all(|d| rel.contains(d))
                                && obs(dl_t, rel, u, &gont) == so
                            {
                                xrd.push(u);
                                continue;
                            }
                            ok = false;
                            why = 4;
                            break;
                        }
                    }
                    // p1 must be ours alone (not another pair's get/set)
                    // and temp/dest unique among earlier live candidates;
                    // rewrite sets must be disjoint.
                    if ok {
                        let mut mine: FxHashSet<usize> =
                            [p1, gi, si].iter().copied().collect();
                        mine.extend(rel.iter().copied());
                        mine.extend(tre.iter().copied());
                        mine.extend(xrd.iter().copied());
                        if mine.iter().any(|d| committed.contains(d)) {
                            ok = false;
                            why = 5;
                        } else {
                            for (cj, &(_, qg, qs, qt, qx)) in cand.iter().enumerate() {
                                if cj >= ci || !live[cj] {
                                    continue;
                                }
                                if qg == p1
                                    || qs == p1
                                    || qx == x
                                    || qx == t
                                    || qt == x
                                    || qt == t
                                {
                                    ok = false;
                                    why = 6;
                                    break;
                                }
                            }
                            if ok {
                                committed.extend(mine);
                            }
                        }
                    }
                    if ok {
                        acts[ci] = Some((tre, xrd));
                    } else {
                        if verbose && pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::wasm","wpeep: drop t={t} x={x} p1={} why={why}", ins[p1].1)
                        };
                        live[ci] = false;
                        acts[ci] = None;
                        dropped = true;
                    }
                }
                if !dropped {
                    break;
                }
            }
            (live, acts)
        };
        // Audit mode: snapshot the round's input; the rewrite below
        // states what it assumed as `Claim`s, checked after it applies.
        let before = opts.audit.then(|| ins.clone());
        let mut claims: Vec<wpeep_audit::Claim> = Vec::new();
        let mut changed = false;
        for (ci, &(p1, gi, si, t, x)) in cand.iter().enumerate() {
            if !live[ci] {
                continue;
            }
            let Some((tre, xrd)) = acts[ci].clone() else {
                continue;
            };
            match ins[p1].0 {
                WPeek::Set(_) => ins[p1].0 = WPeek::Set(x),
                WPeek::Tee(_) => ins[p1].0 = WPeek::Tee(x),
                _ => continue,
            }
            ins[gi].0 = WPeek::Del;
            ins[si].0 = WPeek::Del;
            if before.is_some() {
                claims.push(wpeep_audit::Claim { p1, gi, si, t, x });
            }
            // A retargeted Nop stays a marker (still a no-op self-copy,
            // now on the other local); a real get becomes a real get.
            for &u in &tre {
                ins[u].0 = match ins[u].0 {
                    WPeek::Nop(_) => WPeek::Nop(x),
                    _ => WPeek::Get(x),
                };
            }
            for &d in &xdefs[ci] {
                ins[d].0 = match ins[d].0 {
                    WPeek::Set(_) => WPeek::Set(t),
                    WPeek::Tee(_) => WPeek::Tee(t),
                    k => k,
                };
            }
            for &u in &xrd {
                ins[u].0 = match ins[u].0 {
                    WPeek::Nop(_) => WPeek::Nop(t),
                    _ => WPeek::Get(t),
                };
            }
            changed = true;
        }
        if changed && let Some(old) = before {
            let v = wpeep_audit::audit(b, &old, &ins, &claims);
            if v.is_none() {
                // Over budget: the round stands as the pass decided.
                opts.skipped.set(opts.skipped.get() + 1);
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::wasm", "wpeep audit: round over budget ({} insts), not audited", old.len());
                }
            }
            if let Some(v) = v
                && !v.is_empty()
            {
                opts.violations.set(opts.violations.get() + v.len() as u32);
                let msg = format!(
                    "wpeep audit: copy-forward round rejected in {}\n  {}",
                    opts.tag,
                    v.join("\n  ")
                );
                if opts.audit_panic {
                    panic!("{msg}");
                }
                eprintln!("{msg}");
                // Guard: drop the whole round; earlier rounds stand.
                ins = old;
                break;
            }
        }
        if !changed {
            break;
        }
        ins.retain(|&(k, _, _, _)| !matches!(k, WPeek::Del));
        // Dead-write sweep: a `local.set t` whose value no read can
        // observe is just `drop`; a `local.tee t` likewise is a no-op
        // (it leaves the stack untouched). A read positioned after the
        // write observes it unless a dominating later def covers it; a
        // read inside a shared loop before it may see it through a
        // back-edge.
        let mut dread: Vec<usize> = Vec::new();
        for (i, &(k, _, _, _)) in ins.iter().enumerate() {
            let l = match k {
                WPeek::Set(l) | WPeek::Tee(l) => l,
                _ => continue,
            };
            let mut live = false;
            for (u, &(k2, _, _, _)) in ins.iter().enumerate() {
                if !matches!(k2, WPeek::Get(l2) if l2 == l) {
                    continue;
                }
                if u == i {
                    continue;
                }
                if u > i {
                    // covered by a dominating later write before u?
                    let covered = (i + 1..u).any(|d2| {
                        matches!(ins[d2].0, WPeek::Set(l2) | WPeek::Tee(l2) if l2 == l) && {
                            let (cd, cu) = (&ctx[ins[d2].3], &ctx[ins[u].3]);
                            cd.len() <= cu.len() && cu[..cd.len()] == cd[..]
                        }
                    });
                    if !covered {
                        live = true;
                        break;
                    }
                    continue;
                }
                // u < i: only via a shared-loop back-edge after i.
                let share = ctx[ins[i].3]
                    .iter()
                    .any(|e| dop[e.0 as usize] == 0x03 && ctx[ins[u].3].contains(e));
                if share {
                    live = true;
                    break;
                }
            }
            if !live {
                dread.push(i);
            }
        }
        for &i in &dread {
            ins[i].0 = match ins[i].0 {
                WPeek::Tee(_) => WPeek::Del,
                _ => WPeek::Drop,
            };
        }
    }
    wtrace!("copyfwd", ins);
    // Keep-on-stack: `P; local.set t; W; local.get t` where W never
    // touches the spilled value keeps it on the operand stack instead:
    // the set becomes a tee (the write still happens, so t's reads
    // need no further checking), the window pushes that survive to the
    // get hoist below the producer, and the get goes away. When the
    // get was the only reader of the write the set deletes outright.
    // Prelude gets are keyed on the target instruction's byte offset â€”
    // stable as `ins` entries are marked Del and compacted. Anchors are
    // protected: no later rewrite may delete the instruction a prelude
    // group emits before, or the gets would silently vanish.
    let mut prel: Vec<(usize, u32)> = Vec::new();
    let mut protected: FxHashSet<usize> = FxHashSet::default();
    for _ in 0..crate::env_get("PLIRON_WASM_KOS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
    {
        // (set idx, get idx, producer start, hoisted get idxs)
        let mut acts2: Vec<(usize, usize, usize, Vec<usize>)> = Vec::new();
        {
            let eff = |i: usize| -> Option<(u32, u32)> {
                // calls decode as Ctl but have a signature-known
                // effect.
                if matches!(b[ins[i].1], 0x10 | 0x11) {
                    let mut q = ins[i].1 + 1;
                    let idx = urd(b, &mut q)?;
                    return if b[ins[i].1] == 0x10 {
                        fsig.get(idx as usize).copied()
                    } else {
                        let (pp, qq) = *tsig.get(idx as usize)?;
                        Some((pp + 1, qq)) // +1: the table index operand
                    };
                }
                match ins[i].0 {
                    WPeek::Get(_) => Some((0, 1)),
                    WPeek::Set(_) => Some((1, 0)),
                    WPeek::Tee(_) => Some((1, 1)),
                    WPeek::Other => {
                        let op = b[ins[i].1];
                        Some(match op {
                            0x01 => (0, 0),
                            0x1a => (1, 0),
                            0x1b | 0x1c => (3, 1),
                            0x23 => (0, 1),
                            0x28..=0x35 => (1, 1),
                            0x41..=0x44 => (0, 1),
                            0x45 | 0x50 => (1, 1),
                            0x46..=0x4f | 0x51..=0x66 => (2, 1),
                            0x67..=0x69 | 0x79..=0x7b | 0x8b..=0x91 | 0x99..=0x9f => {
                                (1, 1)
                            }
                            0x6a..=0x78 | 0x7c..=0x8a | 0x92..=0x98 | 0xa0..=0xa6 => {
                                (2, 1)
                            }
                            0xa7..=0xc4 => (1, 1),
                            0xd1 | 0xd2 => (0, 1),
                            _ => return None,
                        })
                    }
                    _ => None,
                }
            };
            for s in 0..ins.len() {
                let t = match ins[s].0 {
                    WPeek::Set(t) if !protected.contains(&ins[s].1) => t,
                    _ => continue,
                };
                // next `local.get t` in the same context, with no other
                // write of t in between (u then reads s's value).
                let mut u = None;
                for v in s + 1..ins.len() {
                    match ins[v].0 {
                        WPeek::Get(l2) if l2 == t => {
                            u = Some(v);
                            break;
                        }
                        WPeek::Set(l2) | WPeek::Tee(l2) if l2 == t => break,
                        _ => {}
                    }
                }
                let Some(u) = u else { continue };
                if protected.contains(&ins[u].1) {
                    continue;
                }
                if ctx[ins[s].3] != ctx[ins[u].3] {
                    continue;
                }
                // Window stack simulation: slots above the kept value
                // and the instruction that produced each. May never
                // dip below the kept value.
                let mut slots: Vec<usize> = Vec::new();
                let mut wok = true;
                for v in s + 1..u {
                    let Some((pp, qq)) = eff(v) else { wok = false; break };
                    if slots.len() < pp as usize {
                        wok = false;
                        break;
                    }
                    for _ in 0..pp {
                        slots.pop();
                    }
                    for _ in 0..qq {
                        slots.push(v);
                    }
                }
                if !wok {
                    continue;
                }
                let hoists: Vec<usize> = slots;
                // Producer start for the kept value: minimal
                // self-contained suffix (net +1, no negative dip).
                let mut need2: i32 = 1;
                let mut ps = None;
                for v in (0..s).rev() {
                    let Some((pp, qq)) = eff(v) else { break };
                    need2 -= qq as i32;
                    need2 += pp as i32;
                    if need2 == 0 {
                        ps = Some(v);
                        break;
                    }
                    if need2 < 0 {
                        break;
                    }
                }
                let Some(ps) = ps else { continue };
                // Positional check: emit the hoists at `ps`, then simulate
                // [ps, u) including pending prelude pushes of earlier
                // rewrites (they're not in `ins`). The transform is only
                // sound when the stack at `u` is [.., h0..hn, kept]: any
                // other value still on the stack between the hoisted gets
                // and the kept value — a `local.tee`'s kept copy, a value
                // the producer leaked, or a prelude queued inside the span
                // — makes a later consumer pop the wrong operand.
                const KEPT: usize = usize::MAX;
                const PREL: usize = usize::MAX - 1;
                let mut slots: Vec<usize> = Vec::new();
                let mut ok = true;
                // Preludes queued for the same `ps` anchor emit first.
                for &(off, _) in &prel {
                    if off == ins[ps].1 {
                        slots.push(PREL);
                    }
                }
                slots.extend_from_slice(&hoists);
                for v in ps..u {
                    for &(off, _) in &prel {
                        if off == ins[v].1 {
                            slots.push(PREL);
                        }
                    }
                    if v == s {
                        if slots.is_empty() {
                            ok = false;
                            break;
                        }
                        slots.pop();
                        slots.push(KEPT);
                        continue;
                    }
                    if hoists.contains(&v) {
                        continue; // deleted at its old position
                    }
                    let Some((pp, qq)) = eff(v) else {
                        ok = false;
                        break;
                    };
                    if slots.len() < pp as usize {
                        ok = false;
                        break;
                    }
                    for _ in 0..pp {
                        slots.pop();
                    }
                    for _ in 0..qq {
                        slots.push(v);
                    }
                }
                let suf = slots.len().saturating_sub(hoists.len() + 1);
                if !ok
                    || slots.last() != Some(&KEPT)
                    || slots[suf..suf + hoists.len()] != hoists[..]
                {
                    continue;
                }
                // Each surviving push must hoist below the producer:
                // only `local.get lx` moves, and only when no def of
                // lx sits between the insertion point and its old
                // position.
                let mut gok = true;
                for &g in &hoists {
                    match ins[g].0 {
                        WPeek::Get(lx) => {
                            if protected.contains(&ins[g].1) {
                                gok = false;
                                break;
                            }
                            for v in ps..g {
                                if matches!(ins[v].0, WPeek::Set(l2) | WPeek::Tee(l2) if l2 == lx)
                                {
                                    gok = false;
                                    break;
                                }
                            }
                        }
                        _ => {
                            gok = false;
                            break;
                        }
                    }
                    if !gok {
                        break;
                    }
                }
                if !gok {
                    continue;
                }
                acts2.push((s, u, ps, hoists));
            }
        } // eff scope ends
        // Only candidates whose whole [producer-start, get] span is
        // strictly disjoint from every accepted span: the producer is
        // part of the span â€” an interleaving rewrite can delete an
        // instruction the other's producer or prelude anchor needs.
        let mut sel: Vec<(usize, usize, usize, Vec<usize>)> = Vec::new();
        'cand: for a in acts2 {
            for &(_, u2, ps2, _) in &sel {
                if !(a.1 < ps2 || a.2 > u2) {
                    continue 'cand;
                }
            }
            sel.push(a);
        }
        let applied = !sel.is_empty();
        for (s, u, ps, hoists) in sel {
            protected.insert(ins[ps].1);
            for &g in &hoists {
                if let WPeek::Get(lx) = ins[g].0 {
                    prel.push((ins[ps].1, lx));
                }
                ins[g].0 = WPeek::Del;
            }
            let t = match ins[s].0 {
                WPeek::Set(t) => t,
                _ => unreachable!(),
            };
            // If no read other than u can observe this write (a later
            // read, or an earlier one through a shared-loop
            // back-edge), drop the write entirely; else keep it as a
            // tee.
            let dead = !ins.iter().enumerate().any(|(v, &(k2, _, _, _))| {
                if !matches!(k2, WPeek::Get(l2) if l2 == t) || v == u {
                    return false;
                }
                if v > s {
                    return true;
                }
                ctx[ins[s].3]
                    .iter()
                    .any(|e| dop[e.0 as usize] == 0x03 && ctx[ins[v].3].contains(e))
            });
            ins[s].0 = if dead { WPeek::Del } else { WPeek::Tee(t) };
            ins[u].0 = WPeek::Del;
        }
        ins.retain(|&(k, _, _, _)| !matches!(k, WPeek::Del));
        if !applied {
            break;
        }
    }
    wtrace!("kos", ins);
    // Loop-back-edge `if` fold: `if c { get*/set*/tee*; br L } [else { R }]
    // end` — waffle lowers `brif` to a param'd loop edge this way, parking
    // the bound copies in scratch locals inside the `if` arm where the
    // copy-forward can't retarget them. LLVM keeps the loop's carried
    // locals updated unconditionally (`local.tee`) then `br_if`s. When the
    // arm is only local copies + `br` and the else arm reads none of the
    // copied locals (and holds no depth-referencing/structured op), hoist
    // the copies before `if`, emit `br_if` for the edge, drop the
    // if/else/end wrapper. `PLIRON_WASM_BRF=0` disables. `prel` hoisted
    // pushes emit by byte-position — a `prel` anchored inside the folded
    // region would land at the wrong operand-stack point, so each fold
    // checks the range it reorders rather than bailing on any prel in
    // the body.
    if crate::env_get("PLIRON_WASM_BRF") != Some("0") {
        let mut out: Vec<(WPeek, usize, usize, usize)> = Vec::with_capacity(ins.len());
        let mut i = 0;
        'iter: while i < ins.len() {
            let (k, s, _e, cx) = ins[i];
            if !(matches!(k, WPeek::Ctl) && b[s] == 0x04) {
                out.push(ins[i]);
                i += 1;
                continue;
            }
            'fold: {
                // `I` is the if's decode index — the first inner inst's ctx.top.
                let Some(&ix1) = ins.get(i + 1) else { break 'fold };
                let outer = ctx[cx].len();
                if ctx[ix1.3].len() != outer + 1 {
                    break 'fold;
                }
                let Some(&(iid, _)) = ctx[ix1.3].last() else { break 'fold };
                // arm1 = (iid,0) insts; `else`/`end` markers end it.
                let mut arm1: Vec<usize> = Vec::new();
                let mut arm2: Vec<(WPeek, usize, usize, usize)> = Vec::new();
                let mut writes: FxHashSet<u32> = FxHashSet::default();
                let mut br_i: Option<usize> = None;
                let mut else_s: Option<usize> = None;
                let mut bad = false;
                let mut j = i + 1;
                // Operand-stack depth the arm1 copies leave above the `if`'s
                // consumed condition — a `Set`/`Tee` with nothing pushed pops
                // the cond instead, and a nonzero final depth buries it.
                let mut bal: i32 = 0;
                // Non-depth, non-structured Ctl opcodes allowed in the else
                // arm (calls/return — no label operand to retarget).
                let safe_ctl = |op: u8| matches!(op, 0x00 | 0x01 | 0x0f | 0x10..=0x14);
                while j < ins.len() {
                    let (k2, s2, _e2, c2) = ins[j];
                    let Some(&(ci, arm)) = ctx[c2].last() else { break };
                    if ci != iid {
                        break;
                    }
                    if arm == 0 {
                        if matches!(k2, WPeek::Ctl) && b[s2] == 0x05 {
                            else_s = Some(s2);
                            j += 1; // `else` marker — arm2 follows
                            // else arm: nested structured control is
                            // allowed — `br`/`br_if` label operands that
                            // reach past their own nesting retarget -1
                            // for the `if` level being removed (operand
                            // == reldepth would target the `if` itself —
                            // unrepresentable → reject). `Get`/`Nop` read
                            // a hoisted dest → reject; `Set`/`Tee`/`Drop`
                            // only write → fine.
                            let mut rel = 0usize;
                            while j < ins.len() {
                                let (k2, s2, _e2, c2) = ins[j];
                                // `Br`/`BrIf` are synthesized by earlier
                                // folds — their `s` isn't a `br` byte.
                                match k2 {
                                    WPeek::Br(d) | WPeek::BrIf(d) => {
                                        let d = d as usize;
                                        if d == rel {
                                            bad = true;
                                            break;
                                        }
                                        if d < rel {
                                            arm2.push(ins[j]);
                                        } else {
                                            arm2.push((
                                                if matches!(k2, WPeek::Br(_)) {
                                                    WPeek::Br((d - 1) as u32)
                                                } else {
                                                    WPeek::BrIf((d - 1) as u32)
                                                },
                                                s2,
                                                s2,
                                                c2,
                                            ));
                                        }
                                    }
                                    WPeek::Get(t) | WPeek::Nop(t)
                                        if writes.contains(&t) =>
                                    {
                                        bad = true;
                                        break;
                                    }
                                    WPeek::Ctl => match b[s2] {
                                        0x0b if rel == 0 => {
                                            break; // `end` closing the `if`
                                        }
                                        0x0b => {
                                            rel -= 1;
                                            arm2.push(ins[j]);
                                        }
                                        0x05 if rel == 0 => {
                                            bad = true; // a second `else`
                                            break;
                                        }
                                        // nested `else` belongs to a nested `if`
                                        0x05 => arm2.push(ins[j]),
                                        0x02 | 0x03 | 0x04 => {
                                            rel += 1;
                                            arm2.push(ins[j]);
                                        }
                                        0x0c | 0x0d => {
                                            let mut dp = s2 + 1;
                                            let Some(d) = urd(b, &mut dp) else {
                                                bad = true;
                                                break;
                                            };
                                            let d = d as usize;
                                            if d == rel {
                                                bad = true;
                                                break;
                                            }
                                            if d < rel {
                                                arm2.push(ins[j]);
                                            } else {
                                                arm2.push((
                                                    if b[s2] == 0x0c {
                                                        WPeek::Br((d - 1) as u32)
                                                    } else {
                                                        WPeek::BrIf((d - 1) as u32)
                                                    },
                                                    s2,
                                                    s2,
                                                    c2,
                                                ));
                                            }
                                        }
                                        _ if !safe_ctl(b[s2]) => {
                                            bad = true;
                                            break;
                                        }
                                        _ => arm2.push(ins[j]),
                                    },
                                    _ => arm2.push(ins[j]),
                                }
                                j += 1;
                            }
                            break; // else arm done — `ins[j]` is the `end`
                        }
                        if matches!(k2, WPeek::Ctl) && b[s2] == 0x0b {
                            break; // `end` (no-else `if`)
                        }
                        arm1.push(j);
                        match k2 {
                            WPeek::Get(_) => bal += 1,
                            WPeek::Set(t) => {
                                if bal == 0 {
                                    bad = true; // `set` would pop the cond
                                }
                                bal -= 1;
                                writes.insert(t);
                            }
                            WPeek::Tee(t) => {
                                if bal == 0 {
                                    bad = true; // `tee` would pop the cond
                                }
                                writes.insert(t);
                            }
                            WPeek::Nop(t) => {
                                writes.insert(t);
                            }
                            WPeek::Ctl if b[s2] == 0x0c => {
                                if br_i.replace(j).is_some() {
                                    bad = true; // a second `br` in arm1
                                }
                            }
                            _ => bad = true,
                        }
                    } else {
                        break; // arm==1 instrs live inside the else scan
                    }
                    j += 1;
                }
                // The `if` region must end at a single `end` (ins[j]) —
                // that's the wrapper we drop.
                let valid = !bad
                    && bal == 0
                    && !arm1.is_empty()
                    && br_i.is_some()
                    && br_i == arm1.last().copied()
                    && j < ins.len()
                    && matches!(ins[j].0, WPeek::Ctl)
                    && b[ins[j].1] == 0x0b;
                if !valid {
                    break 'fold;
                }
                // A `prel` anchored on a deleted inst (the `if`, arm1's
                // insts, `else`, `end`) emits at the wrong operand-stack
                // point or loses its anchor — arm2 insts keep their byte
                // positions and stack depth, so anchors there stay valid.
                let del_hi = ins[br_i.unwrap()].1;
                if prel.iter().any(|&(pp, _)| {
                    (pp >= ins[i].1 && pp <= del_hi)
                        || Some(pp) == else_s
                        || pp == ins[j].1
                }) {
                    break 'fold;
                }
                // The hoisted `writes` hold the arm's (new) value on the
                // else/exit path too — reject if anything past `end` reads
                // one before rewriting it. A rewrite clears a local only
                // on the straight-line continuation at the `end`'s own
                // depth: a `set` inside a nested block or conditional arm
                // does not dominate later reads on other paths.
                let mut live = writes.clone();
                let end_ctx = ins[j].3;
                let mut dom = true;
                for k in (j + 1)..ins.len() {
                    if ins[k].3 != end_ctx
                        || matches!(ins[k].0, WPeek::Ctl | WPeek::Br(_) | WPeek::BrIf(_))
                    {
                        dom = false;
                    }
                    match ins[k].0 {
                        WPeek::Get(t) | WPeek::Nop(t) if live.contains(&t) => {
                            bad = true;
                            break;
                        }
                        WPeek::Set(t) | WPeek::Tee(t) if dom => {
                            live.remove(&t);
                            if live.is_empty() {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if bad {
                    break 'fold;
                }
                // `br`'s label depth counts the `if` level we remove.
                let bi = br_i.unwrap();
                let bs = ins[bi].1;
                let mut dp = bs + 1;
                let Some(d) = urd(b, &mut dp) else { break 'fold };
                if d == 0 {
                    break 'fold; // `br` targeted the `if`'s own end
                }
                // Emit: arm1 copies, `br_if`, else-arm insts. Drop
                // `if`, arm1's `br`, `else`, `end`.
                for &x in &arm1[..arm1.len() - 1] {
                    out.push(ins[x]);
                }
                out.push((WPeek::BrIf(d - 1), ins[i].1, ins[i].1, ins[i].3));
                out.extend(arm2.iter().copied());
                i = j + 1; // past `end`
                continue 'iter;
            }
            out.push(ins[i]);
            i += 1;
        }
        ins = out;
    }
    // Dead-else: `if c { A } else { br K } end` where the `end` is
    // followed by K enclosing `end`s — the else arm's branch lands at
    // the same continuation the `end` fallthrough reaches, so the whole
    // arm is dead; arm1's own trailing `br` dies on the same grounds.
    // Arm1's other insts are kept verbatim (the `if` level stays, so
    // its label depths are unchanged). `if c {}` reduces to `drop`: it
    // only consumes the condition. Only the void blocktype is safe.
    // `PLIRON_WASM_DELSE=0` off.
    if opts.delse {
        let mut out: Vec<(WPeek, usize, usize, usize)> = Vec::with_capacity(ins.len());
        let mut i = 0;
        'iter: while i < ins.len() {
            if !(matches!(ins[i].0, WPeek::Ctl) && b[ins[i].1] == 0x04)
                || b.get(ins[i].1 + 1) != Some(&0x40)
            {
                out.push(ins[i]);
                i += 1;
                continue;
            }
            'fold: {
                // Scan arms at rel 0: arm1 is anything, arm2 must be
                // exactly `br K`.
                let mut rel = 0usize;
                let mut in_arm2 = false;
                let mut arm1: Vec<usize> = Vec::new();
                let mut arm2: Vec<usize> = Vec::new();
                let mut j = i + 1;
                let mut bad = false;
                while j < ins.len() {
                    let (k2, s2, _e2, _c2) = ins[j];
                    // Only `Ctl` bytes drive boundary/depth tracking —
                    // synthesized `Br`/`BrIf` insts have `s` pointing at
                    // arbitrary bytes.
                    if matches!(k2, WPeek::Ctl) {
                        match b[s2] {
                            0x0b if rel == 0 => break, // `end` of the `if`
                            0x0b => rel -= 1,
                            0x05 if rel == 0 => {
                                if in_arm2 {
                                    bad = true;
                                    break;
                                }
                                in_arm2 = true;
                                j += 1;
                                continue;
                            }
                            0x02 | 0x03 | 0x04 => rel += 1,
                            _ => {}
                        }
                    }
                    if in_arm2 {
                        arm2.push(j);
                    } else {
                        arm1.push(j);
                    }
                    j += 1;
                }
                if bad || j >= ins.len() || arm2.len() != 1 {
                    break 'fold;
                }
                let a2only = arm2[0];
                let br_of = |x: usize| -> Option<usize> {
                    match ins[x].0 {
                        WPeek::Br(d) => Some(d as usize),
                        _ if b[ins[x].1] == 0x0c => {
                            let mut dp = ins[x].1 + 1;
                            urd(b, &mut dp).map(|d| d as usize)
                        }
                        _ => None,
                    }
                };
                let Some(k) = br_of(a2only) else {
                    break 'fold;
                };
                // Arm1's own trailing `br K'` is dead on the same
                // grounds — the fallthrough reaches its target through
                // K' `end`s — so drop it when those ends exist too.
                let mut a1len = arm1.len();
                let k1 = a1len.checked_sub(1).and_then(|x| br_of(arm1[x]));
                let mut kmax = k.max(k1.unwrap_or(0));
                // `br K` from inside an arm exits the `if` + K outer
                // labels — the fallthrough lands there only when the K
                // parent tails are empty (`end`s only).
                let ends_clear = |n: usize| {
                    (j + 1..j + 1 + n)
                        .all(|x| ins.get(x).is_some_and(|&(_, s, _, _)| b[s] == 0x0b))
                };
                if !ends_clear(kmax) {
                    if k1.is_some_and(|x| x > k) {
                        // Arm1's `br` reaches further than the else's —
                        // keep it, fold the else arm alone.
                        kmax = k;
                        if !ends_clear(kmax) {
                            break 'fold;
                        }
                    } else {
                        break 'fold;
                    }
                } else if k1.is_some() {
                    a1len -= 1; // drop arm1's trailing `br`
                }
                // `prel` anchors in the deleted span (arm1's `br`, the
                // `else` marker, arm2's `br`) lose their emit slot; it
                // sits between the kept insts and the `end`.
                let lo = arm1[..a1len]
                    .last()
                    .map(|&x| ins[x].1)
                    .unwrap_or(ins[i].1);
                if prel
                    .iter()
                    .any(|&(pp, _)| pp > lo && pp <= ins[a2only].1)
                {
                    break 'fold;
                }
                if a1len == 0 {
                    // `if c {} else {br K}` — only consumes the cond.
                    out.push((WPeek::Drop, ins[i].1, ins[i].1, ins[i].3));
                } else {
                    out.push(ins[i]);
                    for &x in &arm1[..a1len] {
                        out.push(ins[x]);
                    }
                    out.push(ins[j]);
                }
                i = j + 1;
                continue 'iter;
            }
            out.push(ins[i]);
            i += 1;
        }
        ins = out;
    }
    // Branch-if: `if c { br K } else { A } end` → `br_if (K-1); A` —
    // waffle's relooper always wraps a CondBr in `if`/`else`; when the
    // then-arm is a bare forward branch the whole structure is one
    // `br_if`. `br K` inside the arm counts the `if` itself, so the
    // flat `br_if` targets K-1. A's own `br`s lose one label level the
    // same way (`d > rel` retargets, `d == rel` targets the removed
    // `if` — unrepresentable). `PLIRON_WASM_BRI=0` off.
    if opts.bri {
        let safe_ctl = |op: u8| matches!(op, 0x00 | 0x01 | 0x0f | 0x10..=0x14);
        let mut out: Vec<(WPeek, usize, usize, usize)> = Vec::with_capacity(ins.len());
        let mut i = 0;
        'iter: while i < ins.len() {
            if !(matches!(ins[i].0, WPeek::Ctl) && b[ins[i].1] == 0x04)
                || b.get(ins[i].1 + 1) != Some(&0x40)
            {
                out.push(ins[i]);
                i += 1;
                continue;
            }
            'fold: {
                // arm1 must be exactly `br K` (K >= 1); then the else
                // arm runs to the `if`'s `end` at rel 0.
                let Some(&a1) = ins.get(i + 1) else { break 'fold };
                let k = match a1.0 {
                    WPeek::Br(d) => Some(d as usize),
                    WPeek::Ctl if b[a1.1] == 0x0c => {
                        let mut dp = a1.1 + 1;
                        urd(b, &mut dp).map(|d| d as usize)
                    }
                    _ => None,
                };
                let Some(k) = k else { break 'fold };
                if k == 0 {
                    break 'fold;
                }
                match ins.get(i + 2).map(|x| (x.0, b[x.1])) {
                    // `if c { br K } end` — implicit empty else: the
                    // whole construct is just `br_if`.
                    Some((WPeek::Ctl, 0x0b)) => {
                        if prel.iter().any(|&(pp, _)| {
                            pp == ins[i].1 || pp == a1.1 || pp == ins[i + 2].1
                        }) {
                            break 'fold;
                        }
                        out.push((
                            WPeek::BrIf((k - 1) as u32),
                            ins[i].1,
                            ins[i].1,
                            ins[i].3,
                        ));
                        i += 3;
                        continue 'iter;
                    }
                    Some((WPeek::Ctl, 0x05)) => {}
                    _ => break 'fold,
                }
                // Scan the else arm (i+3 .. j) retargeting `br`/`br_if`
                // label operands for the removed `if` level.
                let mut rel = 0usize;
                let mut arm: Vec<(WPeek, usize, usize, usize)> = Vec::new();
                let mut j = i + 3;
                let mut bad = false;
                while j < ins.len() {
                    let (k2, s2, _e2, c2) = ins[j];
                    // `Br`/`BrIf` are synthesized by earlier folds —
                    // their `s` isn't a `br` byte, so match the variant,
                    // not the opcode.
                    match k2 {
                        WPeek::Br(d) | WPeek::BrIf(d) => {
                            let d = d as usize;
                            if d == rel {
                                bad = true;
                                break;
                            }
                            if d < rel {
                                arm.push(ins[j]);
                            } else {
                                arm.push((
                                    if matches!(k2, WPeek::Br(_)) {
                                        WPeek::Br((d - 1) as u32)
                                    } else {
                                        WPeek::BrIf((d - 1) as u32)
                                    },
                                    s2,
                                    s2,
                                    c2,
                                ));
                            }
                        }
                        WPeek::Ctl => match b[s2] {
                            0x0b if rel == 0 => break, // `end` of the `if`
                            0x0b => {
                                rel -= 1;
                                arm.push(ins[j]);
                            }
                            0x05 if rel == 0 => {
                                bad = true;
                                break;
                            }
                            // nested `else` belongs to a nested `if`
                            0x05 => arm.push(ins[j]),
                            0x02 | 0x03 | 0x04 => {
                                rel += 1;
                                arm.push(ins[j]);
                            }
                            0x0c | 0x0d => {
                                let mut dp = s2 + 1;
                                let Some(d) = urd(b, &mut dp) else {
                                    bad = true;
                                    break;
                                };
                                let d = d as usize;
                                if d == rel {
                                    bad = true;
                                    break;
                                }
                                if d < rel {
                                    arm.push(ins[j]);
                                } else {
                                    arm.push((
                                        if b[s2] == 0x0c {
                                            WPeek::Br((d - 1) as u32)
                                        } else {
                                            WPeek::BrIf((d - 1) as u32)
                                        },
                                        s2,
                                        s2,
                                        c2,
                                    ));
                                }
                            }
                            // `br_table` (0x0e) carries label operands
                            // that would need retargeting — it isn't in
                            // `safe_ctl`, so it rejects below.
                            _ if !safe_ctl(b[s2]) => {
                                bad = true;
                                break;
                            }
                            _ => arm.push(ins[j]),
                        },
                        _ => arm.push(ins[j]),
                    }
                    j += 1;
                }
                if bad || j >= ins.len() {
                    break 'fold;
                }
                // `prel` anchors on the deleted insts (the `if`, arm1's
                // `br`, the `else`, the `end`) lose their emit slot; A's
                // insts keep byte positions and stack depth.
                if prel.iter().any(|&(pp, _)| {
                    pp == ins[i].1
                        || pp == a1.1
                        || pp == ins[i + 2].1
                        || pp == ins[j].1
                }) {
                    break 'fold;
                }
                out.push((WPeek::BrIf((k - 1) as u32), ins[i].1, ins[i].1, ins[i].3));
                out.extend(arm);
                i = j + 1;
                continue 'iter;
            }
            out.push(ins[i]);
            i += 1;
        }
        ins = out;
    }
    // Branch-tail: `br K` followed directly by the K+1 `end`s it would
    // jump through — the fallthrough lands on the same continuation, so
    // the branch is dead. A live fold requires an empty operand stack
    // at the `br` (the ends execute normally once it's gone), void
    // crossed constructs, and a non-`loop` target (a `br` to a `loop`
    // jumps to its header). Unreachable `br`s fold unconditionally.
    // Type-index blocktypes or unsized operands poison the body.
    // `PLIRON_WASM_BTR=0` off.
    if opts.btr {
        // Per-inst state: `None` = unreachable (a dead-region marker
        // is still open), `Some(None)` = live but stack depth unknown,
        // `Some(Some(x))` = live at operand depth x. `dead_at`
        // clearing past its boundary end must resume as *unknown*,
        // never as dead — a live `br` folded under dead rules skips
        // the loop-target check and can delete a back-edge.
        let mut sd: Vec<Option<Option<u32>>> = Vec::with_capacity(ins.len());
        // construct stack: (arm base, depth after this construct's end)
        let mut cst: Vec<(Option<u32>, Option<u32>, bool, usize)> = Vec::new();
        let mut end_op: Vec<u8> = vec![0; ins.len()]; // opener opcode per `end`
        let mut end_r: Vec<u32> = vec![u32::MAX; ins.len()]; // result arity
        let mut d: Option<u32> = Some(0);
        // Dead region: `dead_at` is the cst level whose `end` resumes
        // flow (the branch target's); `dead_pos` is the marker's ins
        // index — an `else` of a construct open at the marker resumes
        // flow into arm2 regardless of level.
        let mut dead_at: Option<usize> = None;
        let mut dead_pos: Option<usize> = None;
        // Forward branches land *inside* the span a marker makes dead
        // (a match's `br_table` picks an arm the previous arm already
        // branched over). Precompute every branch's landing point —
        // the `end` of a targeted block/if, the inst after a targeted
        // `loop` opener — and resume liveness when the scan passes it.
        let mut resume: rustc_hash::FxHashSet<usize> = Default::default();
        {
            let mut ost: Vec<usize> = Vec::new();
            let mut end_of = vec![usize::MAX; ins.len()];
            let mut resolved: Vec<usize> = Vec::new();
            for (i, &(k, s, ..)) in ins.iter().enumerate() {
                macro_rules! land {
                    ($dd:expr) => {
                        if ($dd as usize) < ost.len() {
                            resolved.push(ost[ost.len() - 1 - $dd as usize]);
                        }
                    };
                }
                match k {
                    WPeek::Br(dd) | WPeek::BrIf(dd) => land!(dd),
                    WPeek::Ctl if b[s] == 0x0c || b[s] == 0x0d => {
                        let mut dp = s + 1;
                        if let Some(dd) = urd(b, &mut dp) {
                            land!(dd);
                        }
                    }
                    WPeek::Ctl if b[s] == 0x0e => {
                        let mut dp = s + 1;
                        if let Some(n) = urd(b, &mut dp) {
                            for _ in 0..=n {
                                if let Some(dd) = urd(b, &mut dp) {
                                    land!(dd);
                                }
                            }
                        }
                    }
                    WPeek::Ctl if matches!(b[s], 0x02 | 0x03 | 0x04) => ost.push(i),
                    WPeek::Ctl if b[s] == 0x0b => {
                        if let Some(o) = ost.pop() {
                            end_of[o] = i;
                        }
                    }
                    _ => {}
                }
            }
            for o in resolved {
                if b[ins[o].1] == 0x03 {
                    resume.insert(o + 1);
                } else if end_of[o] != usize::MAX {
                    resume.insert(end_of[o]);
                }
            }
        }
        let mut ok = true;
        let mut i = 0;
        'model: while i < ins.len() {
            if resume.contains(&i) {
                dead_at = None;
                dead_pos = None;
            }
            let (k, s, _e, _c) = ins[i];
            sd.push(if dead_at.is_some() { None } else { Some(d) });
            match k {
                WPeek::Del | WPeek::Nop(_) => {}
                WPeek::Get(_) => d = d.and_then(|x| x.checked_add(1)),
                WPeek::Set(_) | WPeek::Drop | WPeek::BrIf(_) => {
                    d = d.and_then(|x| x.checked_sub(1))
                }
                WPeek::Tee(_) => {}
                // pop-1/push-1: depth unchanged, underflows to unknown
                WPeek::Eqz => d = d.and_then(|x| x.checked_sub(1)).map(|x| x + 1),
                WPeek::Br(dd) => {
                    d = None;
                    if dead_at.is_none() {
                        dead_at = Some(cst.len().saturating_sub(dd as usize));
                        dead_pos = Some(i);
                    }
                }
                WPeek::Other => {
                    let pp: (u32, u32) = match b[s] {
                        0x01 => (0, 0),
                        0x1a => (1, 0),
                        0x1b | 0x1c => (3, 1),
                        0x23 => (0, 1),
                        0x24 => (1, 0),
                        0x25 => (1, 1),
                        0x26 => (2, 0),
                        0x28..=0x35 => (1, 1),
                        0x36..=0x3e => (2, 0),
                        0x3f => (0, 1),
                        0x40 => (1, 1),
                        0x41..=0x44 => (0, 1),
                        0x45 | 0x50 => (1, 1),
                        0x46..=0x4f | 0x51..=0x66 => (2, 1),
                        0x67..=0x69 | 0x79..=0x7b | 0x8b..=0x91 | 0x99..=0x9f => (1, 1),
                        0x6a..=0x78 | 0x7c..=0x8a | 0x92..=0x98 | 0xa0..=0xa6 => (2, 1),
                        0xa7..=0xc4 => (1, 1),
                        0xd0 | 0xd2 => (0, 1),
                        0xd1 => (1, 1),
                        0xfc => match b.get(s + 1).copied().unwrap_or(0) {
                            0..=7 => (1, 1),
                            8 | 10 | 11 | 12 | 14 | 17 => (3, 0),
                            9 | 13 => (0, 0),
                            15 => (2, 1),
                            16 => (0, 1),
                            _ => {
                                ok = false;
                                break 'model;
                            }
                        },
                        _ => {
                            ok = false;
                            break 'model;
                        }
                    };
                    d = d.and_then(|x| x.checked_sub(pp.0))
                        .and_then(|x| x.checked_add(pp.1));
                }
                WPeek::Ctl => match b[s] {
                    op @ (0x02 | 0x03 | 0x04) => {
                        // blocktype: 0x40 void, a valtype → (0 in, 1
                        // out); anything else is a type index — the
                        // model can't size it.
                        let (p, r) = match b.get(s + 1).copied() {
                            Some(0x40) => (0u32, 0u32),
                            Some(0x6f | 0x70 | 0x78..=0x7f) => (0, 1),
                            _ => {
                                ok = false;
                                break 'model;
                            }
                        };
                        let pops = p + (op == 0x04) as u32;
                        d = d.and_then(|x| x.checked_sub(pops));
                        let base = d;
                        let post = base.and_then(|x| x.checked_add(r));
                        cst.push((base, post, op == 0x03, i));
                        // result arity rides along in `end_r`'s slot:
                        // the low 8 bits of the opener index are never
                        // touched here — stash `r` separately below.
                        end_r[i] = r;
                        d = base.and_then(|x| x.checked_add(p));
                    }
                    0x05 => {
                        // arm2 is reachable when this `if` was open at
                        // the dead marker — the marker is in arm1, and
                        // arm2 is the other path.
                        if let Some(p) = dead_pos
                            && matches!(cst.last(), Some(&(_, _, _, op_i)) if op_i < p)
                        {
                            dead_at = None;
                            dead_pos = None;
                        }
                        if dead_at.is_none() {
                            let Some(&(base, ..)) = cst.last() else {
                                ok = false;
                                break 'model;
                            };
                            d = base;
                        }
                    }
                    0x0b => {
                        if cst.is_empty() {
                            break 'model; // the function's own `end`
                        }
                        if dead_at == Some(cst.len()) {
                            dead_at = None;
                            dead_pos = None;
                        }
                        let (_, post, _, op_i) = cst.pop().unwrap();
                        end_op[i] = b[ins[op_i].1];
                        end_r[i] = end_r[op_i];
                        if dead_at.is_none() {
                            d = post;
                        }
                    }
                    0x0c | 0x0d => {
                        let mut dp = s + 1;
                        let Some(dd) = urd(b, &mut dp) else {
                            ok = false;
                            break 'model;
                        };
                        if b[s] == 0x0c {
                            d = None;
                            if dead_at.is_none() {
                                dead_at =
                                    Some(cst.len().saturating_sub(dd as usize));
                                dead_pos = Some(i);
                            }
                        } else {
                            d = d.and_then(|x| x.checked_sub(1));
                        }
                    }
                    0x0e | 0x00 | 0x0f => {
                        d = None;
                        if dead_at.is_none() {
                            dead_at = Some(cst.len());
                            dead_pos = Some(i);
                        }
                    }
                    0x10 => {
                        let mut dp = s + 1;
                        let Some(fi) = urd(b, &mut dp) else {
                            ok = false;
                            break 'model;
                        };
                        let Some(&(np, nr)) = fsig.get(fi as usize) else {
                            ok = false;
                            break 'model;
                        };
                        d = d.and_then(|x| x.checked_sub(np))
                            .and_then(|x| x.checked_add(nr));
                    }
                    0x11 => {
                        let mut dp = s + 1;
                        let Some(ti) = urd(b, &mut dp) else {
                            ok = false;
                            break 'model;
                        };
                        let Some(&(np, nr)) = tsig.get(ti as usize) else {
                            ok = false;
                            break 'model;
                        };
                        d = d.and_then(|x| x.checked_sub(np + 1))
                            .and_then(|x| x.checked_add(nr));
                    }
                    _ => {
                        ok = false;
                        break 'model;
                    }
                },
            }
            i += 1;
        }
        if ok {
            let mut dels: Vec<(usize, bool)> = Vec::new();
            for (i, &(k, s, ..)) in ins.iter().enumerate() {
                // A `br_if` under the same ends is conditional — its
                // fallthrough already lands there, so it folds to a
                // `drop` of the condition (needs depth exactly 1).
                let (kk, is_if) = match k {
                    WPeek::Br(d2) => (Some(d2 as usize), false),
                    WPeek::BrIf(d2) => (Some(d2 as usize), true),
                    WPeek::Ctl if b[s] == 0x0c => {
                        let mut dp = s + 1;
                        (urd(b, &mut dp).map(|x| x as usize), false)
                    }
                    WPeek::Ctl if b[s] == 0x0d => {
                        let mut dp = s + 1;
                        (urd(b, &mut dp).map(|x| x as usize), true)
                    }
                    _ => (None, false),
                };
                let Some(kk) = kk else { continue };
                // `sd` `None` = unreachable code: the stack is
                // polymorphic there, so the ends need no arity checks —
                // removing a dead branch changes nothing semantically.
                // `Some(None)` is *live* code whose operand depth the
                // model lost track of — the structural checks below
                // (void crossed ends, non-`loop` target) still apply:
                // a `br` to a `loop` jumps to its header, and deleting
                // it turns a back-edge into an exit.
                let dead = match sd.get(i) {
                    Some(&None) => true,
                    Some(&Some(None)) => false,
                    Some(&Some(Some(x))) => {
                        if x != is_if as u32 {
                            continue;
                        }
                        false
                    }
                    _ => continue,
                };
                let live = !dead;
                // The next kk+1 real insts must be `end`s of void
                // (result-arity-0) constructs; the last closes the
                // target, which must not be a `loop`.
                let mut c = 0usize;
                let mut j = i + 1;
                let mut okk = true;
                while c <= kk {
                    while matches!(
                        ins.get(j).map(|x| x.0),
                        Some(WPeek::Del | WPeek::Nop(_))
                    ) {
                        j += 1;
                    }
                    let Some(&(k2, s2, ..)) = ins.get(j) else {
                        okk = false;
                        break;
                    };
                    if !(matches!(k2, WPeek::Ctl) && b[s2] == 0x0b) {
                        okk = false;
                        break;
                    }
                    // Live code: every crossed construct must be void —
                    // the ends execute with the empty stack the `br`
                    // leaves behind. Dead code needs no arity check.
                    if live && end_r[j] != 0 {
                        okk = false;
                        break;
                    }
                    // A `br` to a `loop` jumps to its header — only a
                    // live branch's landing matters for that.
                    if live && c == kk && end_op[j] == 0x03 {
                        okk = false;
                        break;
                    }
                    c += 1;
                    j += 1;
                }
                if !okk {
                    continue;
                }
                if prel.iter().any(|&(pp, _)| pp == s) {
                    continue;
                }
                dels.push((i, is_if));
            }
            let mut dels = dels;
            if let Some(n) = crate::env_get("PLIRON_WASM_BTR_LIM")
                .and_then(|v| v.parse::<usize>().ok())
            {
                dels.truncate(n);
            }
            if let Some(x) = crate::env_get("PLIRON_WASM_BTR_SKIP")
                .and_then(|v| usize::from_str_radix(&v, 16).ok().or(v.parse().ok()))
            {
                dels.retain(|&(i, _)| ins[i].1 != x);
            }
            for &(i, is_if) in &dels {
                // A live `br_if` still has to pop its condition; a dead
                // one disappears with the rest of the dead code.
                ins[i].0 = if is_if && sd[i].is_some() {
                    WPeek::Drop
                } else {
                    WPeek::Del
                };
            }
        }
        wtrace!("btr", ins);
    }
    // Fresh local index for `tee`-captured values / LICM-hoisted
    // temporaries — shared by every local-minting pass below.
    let mut next_local = nparams + groups.iter().map(|&(c, _)| c).sum::<u32>();
    let mut minted = 0u32;
    const MAX_SFWD_LOCALS: u32 = 48;
    // Bounded loop-invariant hoisting: a pure `get A; get B; <binop>`
    // triple inside a `loop` whose locals are written nowhere in the
    // loop's span recomputes per iteration (chars_ascii's `ptr+off`
    // bound check, etc.) — hoist it once before the loop into a fresh
    // local and leave `get T` in place. Only non-trapping binary ops
    // are hoisted (no div/rem).
    {
        let restype = |op: u8| -> Option<u8> {
            match op {
                0x6a..=0x6c | 0x71..=0x78 => Some(0x7f), // i32 arith
                0x46..=0x4f | 0x51..=0x66 => Some(0x7f), // compares -> i32
                0x7c..=0x7e | 0x83..=0x8a => Some(0x7e), // i64 arith
                0x92..=0x98 => Some(0x7d),             // f32 arith
                0xa0..=0xa6 => Some(0x7c),             // f64 arith
                _ => None,
            }
        };
        let mut k = 0usize;
        let mut splices: Vec<(usize, Vec<(WPeek, usize, usize, usize)>)> = Vec::new();
        while k < ins.len() {
            if !(matches!(ins[k].0, WPeek::Ctl) && b[ins[k].1] == 0x03) {
                k += 1;
                continue;
            }
            // Match the loop's `end` by depth over Ctl openers.
            let mut d = 1i32;
            let mut e = k + 1;
            while e < ins.len() {
                if matches!(ins[e].0, WPeek::Ctl) {
                    match b[ins[e].1] {
                        0x02 | 0x03 | 0x04 => d += 1,
                        0x0b => {
                            d -= 1;
                            if d == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                e += 1;
            }
            if e >= ins.len() {
                break;
            }
            let mut written: FxHashSet<u32> = FxHashSet::default();
            for m in (k + 1)..e {
                if let WPeek::Set(l) | WPeek::Tee(l) | WPeek::Nop(l) = ins[m].0 {
                    written.insert(l);
                }
            }
            // Invariant-chain hoisting: a contiguous self-contained run
            // of pure ops + `get`s (all locals unwritten in the loop) +
            // consts producing exactly one value recomputes each
            // iteration — `hmget`'s tag broadcast
            // (`get h2;const25;shr;wrap;and;extend;constB;mul`) is the
            // shape. Hoist `run;set T` to the preamble, leave `get T`.
            let pure_eff = |op: u8| -> Option<(u32, u32)> {
                Some(match op {
                    0x41..=0x44 => (0, 1),
                    0x45 | 0x50 => (1, 1),
                    0x46..=0x4f | 0x51..=0x66 => (2, 1),
                    0x67..=0x69 | 0x79..=0x7b | 0x8b..=0x91 | 0x99..=0x9f => (1, 1),
                    0x6a..=0x6c | 0x71..=0x78 | 0x7c..=0x7e | 0x83..=0x8a
                    | 0x92..=0x98 | 0xa0..=0xa6 => (2, 1),
                    0xa7 | 0xac | 0xad | 0xb2..=0xbb | 0xc0..=0xc4 => (1, 1),
                    _ => return None,
                })
            };
            let chain_vt = |op: u8| -> Option<u8> {
                Some(match op {
                    // i32.const/eqz/cmps/i32-arith/wrap/i32-extends -> i32
                    0x41 | 0x45 | 0x50 | 0x46..=0x4f | 0x51..=0x66 | 0x67..=0x69
                    | 0x6a..=0x78 | 0xa7 | 0xb8 | 0xc0 | 0xc1 => 0x7f,
                    // i64.const/i64-unary/arith/extend-i32/i64-extends/reint -> i64
                    0x42 | 0x79..=0x8a | 0xac | 0xad | 0xb9 | 0xc2..=0xc4 => 0x7e,
                    // f32.const/unary/arith/convert-i32/demote/reint -> f32
                    0x43 | 0x8b..=0x98 | 0xb2 | 0xb3 | 0xb4 | 0xba => 0x7d,
                    // f64.const/unary/arith/convert-i32/promote/reint -> f64
                    0x44 | 0x99..=0xa6 | 0xb5 | 0xb6 | 0xb7 | 0xbb => 0x7c,
                    _ => return None,
                })
            };
            let mut seen: FxHashMap<(u32, u32, u8), u32> = FxHashMap::default();
            let mut hoist: Vec<(WPeek, usize, usize, usize)> = Vec::new();
            let mut rewrites: Vec<(usize, u32)> = Vec::new();
            let mut m = k + 1;
            while m + 2 < e {
                let (WPeek::Get(a), ..) = ins[m] else {
                    m += 1;
                    continue;
                };
                let (WPeek::Get(bb), ..) = ins[m + 1] else {
                    m += 1;
                    continue;
                };
                let (WPeek::Other, s3, ..) = ins[m + 2] else {
                    m += 1;
                    continue;
                };
                let op = b[s3];
                if written.contains(&a) || written.contains(&bb) {
                    m += 1;
                    continue;
                }
                let Some(vt) = restype(op) else {
                    m += 1;
                    continue;
                };
                if minted >= MAX_SFWD_LOCALS {
                    m += 1;
                    continue;
                }
                let t = match seen.get(&(a, bb, op)) {
                    Some(&t) => t,
                    None => {
                        let t = next_local;
                        next_local += 1;
                        minted += 1;
                        groups.push((1, vt));
                        hoist.push((WPeek::Get(a), ins[k].1, ins[k].1, ins[k].3));
                        hoist.push((WPeek::Get(bb), ins[k].1, ins[k].1, ins[k].3));
                        // `Other` instrs emit `b[s..e]` — keep the op's
                        // own byte range so the hoist emits the real op.
                        hoist.push((ins[m + 2].0, ins[m + 2].1, ins[m + 2].2, ins[k].3));
                        hoist.push((WPeek::Set(t), ins[k].1, ins[k].1, ins[k].3));
                        seen.insert((a, bb, op), t);
                        t
                    }
                };
                rewrites.push((m, t));
                m += 3;
            }
            for (m, t) in rewrites {
                ins[m].0 = WPeek::Get(t);
                ins[m + 1].0 = WPeek::Nop(0);
                ins[m + 2].0 = WPeek::Nop(0);
            }
            let chain_on = crate::env_get("PLIRON_WASM_LICM_CHAIN") != Some("0");
            let chain_dbg = crate::env_get("PLIRON_WASM_LICM_CHAIN") == Some("dbg");
            let mut m2 = k + 1;
            while m2 < e && chain_on {
                if minted >= MAX_SFWD_LOCALS {
                    break;
                }
                // Longest self-contained pure run starting at `m2` —
                // `depth` counts net pushed values; dipping to 0 means
                // the run consumed a below-run value (not self-contained).
                let mut depth = 0i64;
                let mut last = 0usize;
                let mut j = m2;
                while j < e {
                    // A `prel` KOS push anchored at `ins[j].1` materializes
                    // a `Get` at `ins[j]`'s index for `ins[j]`'s consumer —
                    // `Nop`ing the instr orphans the push (stack leak).
                    if prel.iter().any(|&(pp, _)| pp == ins[j].1) {
                        break;
                    }
                    let (pops, pushes) = match ins[j].0 {
                        WPeek::Get(l) => {
                            if written.contains(&l) {
                                break;
                            }
                            (0, 1)
                        }
                        WPeek::Other => match pure_eff(b[ins[j].1]) {
                            Some(pe) => pe,
                            None => break,
                        },
                        _ => break,
                    };
                    depth += pushes as i64 - pops as i64;
                    if depth <= 0 {
                        break;
                    }
                    if depth == 1 {
                        last = j;
                    }
                    j += 1;
                }
                if last >= m2 + 3 {
                    let Some(vt) = chain_vt(b[ins[last].1]) else {
                        m2 += 1;
                        continue;
                    };
                    if chain_dbg {
                        eprintln!(
                            "licm-chain k={k} {m2}..{last}: {:?}",
                            (m2..=last)
                                .map(|q| (ins[q].0, b.get(ins[q].1).copied()))
                                .collect::<Vec<_>>()
                        );
                    }
                    let t = next_local;
                    next_local += 1;
                    minted += 1;
                    groups.push((1, vt));
                    for q in m2..=last {
                        hoist.push((ins[q].0, ins[q].1, ins[q].2, ins[k].3));
                    }
                    hoist.push((WPeek::Set(t), ins[k].1, ins[k].1, ins[k].3));
                    ins[m2].0 = WPeek::Get(t);
                    for q in (m2 + 1)..=last {
                        ins[q].0 = WPeek::Nop(0);
                    }
                    m2 = last + 1;
                } else {
                    m2 += 1;
                }
            }
            if !hoist.is_empty() {
                splices.push((k, hoist));
            }
            k = e + 1;
        }
        // Splice hoisted preambles before their loops — ascending
        // position order with a cumulative insert offset.
        splices.sort_by_key(|(k, _)| *k);
        let mut acc = 0usize;
        for (k, vec) in splices {
            let n = vec.len();
            for (j, t) in vec.into_iter().enumerate() {
                ins.insert(k + acc + j, t);
            }
            acc += n;
        }
    }
    // Materialize `prel` keep-on-stack pushes into the instruction
    // stream so `sfwd`'s operand-stack walk sees a balanced body: each
    // `(pos, local)` pair emits a `local.get` before the instruction at
    // that byte position — represented here as a synthetic `Get`
    // spliced in at the anchor's index (its `.1` keeps the anchor's
    // byte offset, which is a producer op — never `0x0f` — so the
    // `drop_ret` scan below can't misread it). `prel` is then emptied
    // so the emit loop doesn't double-emit the same pushes.
    if !prel.is_empty() {
        let mut splices: Vec<(usize, WPeek, usize, usize, usize)> = Vec::new();
        for &(pos, lx) in &prel {
            if let Some(k) = ins.iter().position(|&(_, s, _, _)| s == pos) {
                splices.push((k, WPeek::Get(lx), pos, ins[k].2, ins[k].3));
            }
        }
        splices.sort_by_key(|&(k, ..)| k);
        for (off, (k, kind, s, e, c)) in splices.into_iter().enumerate() {
            ins.insert(k + off, (kind, s, e, c));
        }
        prel.clear();
    }
    wtrace!("brf", ins);
    // `walias` — operand-stack-aware locals cancellation, phase 1
    // (straight-line spans). Four rewrites:
    //
    //   (B) `set T … get T` where T's operand stays on the stack the
    //       whole span — deleting the pair leaves it for the `get`'s
    //       consumer to read. `pend[t]` records the operand's depth;
    //       a pop to that depth consumes it (in the deleted-set world
    //       it would be the consumer's operand), so `pend` dies at
    //       `len <= dv`. A second `get T` forces the pair back
    //       (tracked in `canceled`).
    //   `get T; drop` → `Del` both — the pushed copy is discarded.
    //   `set T`/`tee T` never `get` before their next write →
    //       `Drop`/`Del` (dead local write — `drop` keeps the same
    //       stack effect for `set`; `tee` is net-0 so it deletes).
    //   `set T(get U)`/`tee T(get U)` → `get T` rewrites to `get U`
    //       while neither local is rewritten (copymap).
    //
    // `vstack` abstracts the operand stack — each entry is the local
    // whose value it holds (`Named`, pushed by `get`/`tee`) or `OTHER`.
    // `pend` holds live `set T` candidates (ins idx, operand's stack
    // depth); `canceled` remembers deleted pairs so a second
    // `get T` restores them; `last_write`/`loop_gets` feed the
    // dead-write checks (a `get` inside a containing loop reads the
    // in-loop write via the backedge).
    // Locals written inside each `loop` body (header ins idx -> set):
    // carried facts (`copymap`/`last_write`/`fmap`) can't flow into a
    // `loop` for locals the body rewrites — a loop-top read may see a
    // later iteration's value, not the entry-edge one.
    let mut loop_writes: FxHashMap<usize, FxHashSet<u32>> = FxHashMap::default();
    {
        let mut stk: Vec<(u8, usize)> = Vec::new();
        for (i, &(k, ..)) in ins.iter().enumerate() {
            match k {
                WPeek::Ctl => match b[ins[i].1] {
                    op @ (0x02 | 0x03 | 0x04) => stk.push((op, i)),
                    0x0b => {
                        stk.pop();
                    }
                    _ => {}
                },
                WPeek::Set(t) | WPeek::Tee(t) | WPeek::Nop(t) => {
                    for &(op, hi) in &stk {
                        if op == 0x03 {
                            loop_writes.entry(hi).or_default().insert(t);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if crate::env_get("PLIRON_WASM_WALIAS") != Some("0") {
        // Mode bits: 1=set/get pair-cancel, 2=get;drop, 4=dead-write,
        // 8=copy-forward.
        let wmode: u32 = crate::env_get("PLIRON_WASM_WALIAS_MODE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(15);
        const OTHER: u32 = u32::MAX;
        // `None` = opaque (bail at decode anyway for the weird ops);
        // the walk treats it as a span boundary, never a miscount.
        let eff = |s: usize| -> Option<(u32, u32)> {
            let op = b[s];
            Some(match op {
                0x01 => (0, 0),
                0x1a => (1, 0),
                0x1b | 0x1c => (3, 1),               // select
                0x23 => (0, 1),                      // global.get
                0x24 => (1, 0),                      // global.set
                0x25 => (1, 1),                      // table.get
                0x26 => (2, 0),                      // table.set
                0x28..=0x35 => (1, 1),               // loads
                0x36..=0x3e => (2, 0),               // stores
                0x3f => (0, 1),                      // memory.size
                0x40 => (1, 1),                      // memory.grow
                0x41..=0x44 => (0, 1),               // consts
                0x45 | 0x50 => (1, 1),               // eqz
                0x46..=0x4f | 0x51..=0x66 => (2, 1), // compares
                0x67..=0x69 | 0x79..=0x7b | 0x8b..=0x91 | 0x99..=0x9f => (1, 1),
                0x6a..=0x78 | 0x7c..=0x8a | 0x92..=0x98 | 0xa0..=0xa6 => (2, 1),
                0xa7..=0xc4 => (1, 1), // conversions
                0xd0 | 0xd2 => (0, 1), // ref.null / ref.func
                0xd1 => (1, 1),        // ref.is_null
                0xfc => match b.get(s + 1).copied().unwrap_or(0) {
                    0..=7 => (1, 1),                  // trunc_sat*
                    8 | 10 | 11 | 12 | 14 | 17 => (3, 0), // mem.init/copy/fill, table.*
                    9 | 13 => (0, 0),                 // data.drop / elem.drop
                    15 => (2, 1),                     // table.grow
                    16 => (0, 1),                     // table.size
                    _ => return None,
                },
                _ => return None,
            })
        };
        // Loop spans (for the backedge-read rule) + per-loop locals read.
        let mut ctl_stk: Vec<usize> = Vec::new();
        let mut loop_spans: Vec<(usize, usize)> = Vec::new();
        for (i, &(k, s, ..)) in ins.iter().enumerate() {
            if !matches!(k, WPeek::Ctl) {
                continue;
            }
            match b[s] {
                0x02 | 0x03 | 0x04 => ctl_stk.push(i),
                0x0b => {
                    if let Some(k2) = ctl_stk.pop()
                        && b[ins[k2].1] == 0x03
                    {
                        loop_spans.push((k2, i));
                    }
                }
                _ => {}
            }
        }
        // For each local, the set of loop spans containing a `get` —
        // a `set`/`tee` inside any of those loops is backedge-read.
        let mut loop_read: FxHashMap<u32, FxHashSet<usize>> = FxHashMap::default();
        let mut enclosers: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
        for (li, &(k, e)) in loop_spans.iter().enumerate() {
            for i in (k + 1)..e {
                if let WPeek::Get(l) = ins[i].0 {
                    loop_read.entry(l).or_default().insert(li);
                }
                enclosers.entry(i).or_default().push(li);
            }
        }
        // `pi` is backedge-read iff some enclosing loop reads `t`.
        let loop_reads = |pi: usize, t: u32| -> bool {
            let Some(loops) = enclosers.get(&pi) else {
                return false;
            };
            let Some(read) = loop_read.get(&t) else {
                return false;
            };
            loops.iter().any(|li| read.contains(li))
        };
        if crate::env_set("PLIRON_WASM_SFWD_DBG") {
            for (i, &(k, s, ..)) in ins.iter().enumerate() {
                eprintln!("  walias-ins[{i}] {k:?} op={:02x}", b[s]);
            }
        }
        let mut vstack: Vec<u32> = Vec::new();
        let mut pend: FxHashMap<u32, (usize, usize)> = FxHashMap::default();
        let mut canceled: FxHashMap<u32, (usize, usize)> = FxHashMap::default();
        let mut copymap: FxHashMap<u32, u32> = FxHashMap::default();
        // local -> (ins idx, is_tee, was-read)
        let mut last_write: FxHashMap<u32, (usize, bool, bool)> = FxHashMap::default();
        // `copymap`/`last_write` merge across structured control: a
        // `set T(get U)` on every arm leaves `T := U` at the merge, so
        // `get T` below it can still forward to `get U` — `path` holds
        // one frame per `block`/`loop`/`if` scope. `saved` = pre-`if`
        // state (restored at `else`); `snaps` = `copymap`/`last_write`
        // snapshots at `br`/`br_if`/`br_table` edges targeting this
        // frame's `end`.
        struct Frame {
            op: u8,
            saved: Option<(FxHashMap<u32, u32>, FxHashMap<u32, (usize, bool, bool)>)>,
            snaps: Vec<(FxHashMap<u32, u32>, FxHashMap<u32, (usize, bool, bool)>)>,
        }
        let mut path: Vec<Frame> = Vec::new();
        // Whether straight-line code reaches the next `ins` — `br`/
        // `br_table`/`return`/`unreachable` kill the fallthrough edge
        // so an `end` merge must not count it.
        let mut reach = true;
        // `ins_span[i]` = straight-line span id: bumps at every control
        // boundary (block/loop/if/else/end/br*/ret/unreachable — not
        // calls). A `set T` may eager-delete a prior dead write only in
        // its own span: a write seen via `last_write` from an earlier
        // span may be live on another path's merge.
        let mut ins_span = vec![0u32; ins.len()];
        {
            let mut span = 0u32;
            for (i, &(k, ..)) in ins.iter().enumerate() {
                ins_span[i] = span;
                match k {
                    WPeek::Ctl | WPeek::Br(_) | WPeek::BrIf(_) => span += 1,
                    _ => {}
                }
            }
        }
        let mut dels: FxHashSet<usize> = FxHashSet::default();
        let mut drops: Vec<usize> = Vec::new();
        let mut rewrites: Vec<(usize, WPeek)> = Vec::new();
        for i in 0..ins.len() {
            match ins[i].0 {
                WPeek::Del => continue,
                WPeek::Nop(l) => {
                    // `Nop` is a deleted `get l; set l` self-copy —
                    // it reads *and* writes `l`, so a pending `set`/`get`
                    // cancel can't cross it (it would read stale `l`)
                    // and copy forwards on `l` go stale. Its `get l`
                    // half must resurrect a canceled pair like any
                    // `get` — the read must see the restored write.
                    if let Some(&(si, gi)) = canceled.get(&l) {
                        dels.remove(&si);
                        dels.remove(&gi);
                        canceled.remove(&l);
                        last_write.insert(l, (si, false, true));
                    }
                    pend.remove(&l);
                    copymap.retain(|&k, &mut v| k != l && v != l);
                    for e in vstack.iter_mut() {
                        if *e == l {
                            *e = OTHER;
                        }
                    }
                    if let Some(lw) = last_write.get_mut(&l) {
                        lw.2 = true;
                    }
                    continue;
                }
                WPeek::Eqz => {
                    // pop the cond, push a fresh non-local value
                    if vstack.pop().is_none() {
                        pend.clear();
                    }
                    vstack.push(OTHER);
                    continue;
                }
                WPeek::Ctl | WPeek::BrIf(_) | WPeek::Br(_) => {
                    // Merge `copymap`/`last_write` per ctl kind — a
                    // `set T(get U)` on every incoming arm leaves
                    // `T := U` at the merge, so `get T` below forwards.
                    // `pend`/`vstack`/`canceled`/`fmap` are stack/local
                    // facts that can't cross a merge.
                    // Synthetic `Br`/`BrIf` (the loop-backedge fold)
                    // carry the label — `b[s]` isn't their opcode.
                    let op = match ins[i].0 {
                        WPeek::Br(_) => 0x0c,
                        WPeek::BrIf(_) => 0x0d,
                        _ => b[ins[i].1],
                    };
                    let snap = |cm: &FxHashMap<u32, u32>,
                                lw: &FxHashMap<u32, (usize, bool, bool)>| {
                        (cm.clone(), lw.clone())
                    };
                    // `br`/`br_if`/`br_table` targets: label depth d ->
                    // `path[path.len()-1-d]`. A `br` to `loop` is a
                    // backedge — its snapshot doesn't merge at `end`.
                    let snap_to = |d: u32,
                                   path: &mut Vec<Frame>,
                                   cm: &FxHashMap<u32, u32>,
                                   lw: &FxHashMap<u32, (usize, bool, bool)>| {
                        let idx = path.len().wrapping_sub(1 + d as usize);
                        if crate::env_set("PLIRON_WASM_WALIAS_TRACE") {
                            eprintln!(
                                "    walias-ctl[{i}] snap_to d={d} idx={idx} cm={cm:?}"
                            );
                        }
                        if let Some(f) = path.get_mut(idx)
                            && f.op != 0x03
                        {
                            f.snaps.push(snap(cm, lw));
                        }
                    };
                    match op {
                        0x02 => {
                            path.push(Frame {
                                op,
                                saved: None,
                                snaps: Vec::new(),
                            });
                            reach = true;
                        }
                        0x03 => {
                            path.push(Frame {
                                op,
                                saved: None,
                                snaps: Vec::new(),
                            });
                            reach = true;
                            // Backedge: `copymap`/`last_write` entries
                            // for locals the body writes are stale
                            // from iteration 2 on — kill them at entry.
                            if let Some(ws) = loop_writes.get(&i) {
                                copymap
                                    .retain(|k, v| !ws.contains(k) && !ws.contains(v));
                                last_write.retain(|k, _| !ws.contains(k));
                            }
                        }
                        0x04 => {
                            path.push(Frame {
                                op,
                                saved: Some(snap(&copymap, &last_write)),
                                snaps: Vec::new(),
                            });
                            reach = true;
                        }
                        0x05 => {
                            if let Some(f) = path.last_mut() {
                                // Arm1 contributes its `end` state only
                                // when it falls through — a `br`/
                                // `return`/`unreachable` arm's facts
                                // come from dead code and must not
                                // merge (br-snapshots cover it).
                                if reach {
                                    f.snaps.push(snap(&copymap, &last_write));
                                }
                                if let Some((cm, lw)) = f.saved.take() {
                                    copymap = cm;
                                    last_write = lw;
                                }
                            }
                            reach = true;
                        }
                        0x0b => {
                            if let Some(mut f) = path.pop() {
                                let mut bags = std::mem::take(&mut f.snaps);
                                if reach {
                                    bags.push(snap(&copymap, &last_write));
                                }
                                // `if` without `else`: the false edge
                                // reaches `end` with pre-`if` state.
                                if let Some(a) = f.saved {
                                    bags.push(a);
                                }
                                copymap.clear();
                                last_write.clear();
                                let mut it = bags.into_iter();
                                if let Some((mut cm, mut lw)) = it.next() {
                                    for (c, l) in it {
                                        cm.retain(|k, v| c.get(k) == Some(v));
                                        for (k, &v2) in l.iter() {
                                            // Differing or path-only
                                            // writes: `t` is live but
                                            // the write ins isn't
                                            // pinned — a MAX sentinel
                                            // that can't be deleted.
                                            match lw.get_mut(k) {
                                                Some(e) if *e == v2 => {}
                                                Some(e) => {
                                                    *e = (usize::MAX, false, true)
                                                }
                                                None => {
                                                    lw.insert(
                                                        *k,
                                                        (usize::MAX, false, true),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    copymap = cm;
                                    last_write = lw;
                                    reach = true;
                                } else {
                                    // No incoming path — `end` unreachable.
                                    reach = false;
                                }
                                if crate::env_set("PLIRON_WASM_WALIAS_TRACE") {
                                    eprintln!(
                                        "    walias-ctl[{i}] end op={} cm={copymap:?}",
                                        f.op
                                    );
                                }
                            }
                        }
                        0x0c | 0x0d => {
                            // br/br_if — snapshot `copymap`/`last_write`
                            // to the target frame's `end` merge.
                            // `br`'s fallthrough is dead; `br_if` keeps.
                            // In dead code (`!reach`) the state is
                            // garbage — no snapshot.
                            if reach {
                                let d = match ins[i].0 {
                                    WPeek::Br(d) | WPeek::BrIf(d) => d,
                                    _ => {
                                        let mut p = ins[i].1 + 1;
                                        urd(b, &mut p).unwrap_or(0)
                                    }
                                };
                                snap_to(d, &mut path, &copymap, &last_write);
                            }
                            if op == 0x0c {
                                reach = false;
                            }
                        }
                        0x0e => {
                            // br_table — snapshot to every target.
                            if reach {
                                let mut p = ins[i].1 + 1;
                                let n = urd(b, &mut p).unwrap_or(0);
                                for _ in 0..=n {
                                    let d = urd(b, &mut p).unwrap_or(0);
                                    snap_to(d, &mut path, &copymap, &last_write);
                                }
                            }
                            reach = false;
                        }
                        0x00 | 0x0f => {
                            reach = false;
                        }
                        _ => {}
                    }
                    // `canceled` survives — a `get T` anywhere must
                    // restore its pair since the `Del`'d `set` may be
                    // T's only writer. `copymap`/`last_write` merge at
                    // `end`/`else` and survive INTO `block`/`loop`/
                    // `if`/`call`; only `br`/`br_table`/`return`/
                    // `unreachable` (dead fallthrough) clear them.
                    vstack.clear();
                    pend.clear();
                    if matches!(op, 0x0c | 0x0e | 0x00 | 0x0f) {
                        copymap.clear();
                        last_write.clear();
                    }
                    continue;
                }
                WPeek::Drop => {
                    if vstack.pop().is_none() {
                        // Pop below the tracked region — may consume a
                        // pending set's operand slot.
                        pend.clear();
                    }
                }
                WPeek::Set(t) | WPeek::Tee(t) => {
                    let is_tee = matches!(ins[i].0, WPeek::Tee(_));
                    // T reassigned: kill pending/copy facts and demote
                    // `Named(T)` stack entries (they hold OLD T).
                    // `canceled` survives — a later `get T` on ANY path
                    // may still need the pair restored; re-emitting it
                    // only adds back a write T already had.
                    pend.remove(&t);
                    copymap.retain(|&k, &mut v| k != t && v != t);
                    for e in vstack.iter_mut() {
                        if *e == t {
                            *e = OTHER;
                        }
                    }
                    // Flush the previous unread write — same span only:
                    // a `last_write` entry from an earlier span may be
                    // live on a path that merges without this `set`
                    // (e.g. the other arm of an `if`).
                    if let Some(&(pi, ptee, read)) = last_write.get(&t)
                        && !read
                        && ins_span.get(pi) == ins_span.get(i)
                    {
                        let dead = wmode & 4 != 0 && !loop_reads(pi, t);
                        if dead {
                            if ptee {
                                dels.insert(pi);
                            } else {
                                drops.push(pi);
                            }
                        }
                    }
                    let popped = vstack.pop();
                    if popped.is_none() {
                        // Pop below the tracked region — may consume a
                        // pending set's operand slot.
                        pend.clear();
                    }
                    if let Some(u) = popped
                        && u != OTHER
                    {
                        copymap.insert(t, u);
                    }
                    if is_tee {
                        vstack.push(t);
                    } else if popped.is_some() {
                        // Only a *tracked* operand can be canceled: when
                        // `vstack` is empty the value came from an
                        // unknown source (multi-value call results,
                        // block params), and deleting the `set` would
                        // leave it for the next consumer to mis-pop.
                        pend.insert(t, (i, vstack.len()));
                    }
                    last_write.insert(t, (i, is_tee, false));
                }
                WPeek::Get(t) => {
                    // A second `get T` resurrects a canceled pair —
                    // the restored `set` re-emits and rewrites T, so any
                    // pending/copy fact on T is stale.
                    if let Some(&(si, gi)) = canceled.get(&t) {
                        dels.remove(&si);
                        dels.remove(&gi);
                        canceled.remove(&t);
                        pend.remove(&t);
                        copymap.retain(|&k, &mut v| k != t && v != t);
                        for e in vstack.iter_mut() {
                            if *e == t {
                                *e = OTHER;
                            }
                        }
                        last_write.insert(t, (si, false, true));
                    }
                    // `get T; drop` — the pushed copy is discarded.
                    // (`0x1a` decodes as `Other`, not `WPeek::Drop`.)
                    let mut j = i + 1;
                    while matches!(ins.get(j).map(|x| x.0), Some(WPeek::Del | WPeek::Nop(_))) {
                        j += 1;
                    }
                    if wmode & 2 != 0
                        && (matches!(ins.get(j).map(|x| x.0), Some(WPeek::Drop))
                        || ins
                            .get(j)
                            .is_some_and(|x| matches!(x.0, WPeek::Other) && b[x.1] == 0x1a))
                    {
                        dels.insert(i);
                        dels.insert(j);
                        continue;
                    }
                    // (B) `set T` candidate: the operand slot `dv` sits
                    // untouched at the current stack top — cancel both
                    // so the consumer reads the value directly.
                    if wmode & 1 != 0
                        && let Some(&(si, dv)) = pend.get(&t)
                        && vstack.len() == dv
                    {
                        dels.insert(si);
                        dels.insert(i);
                        pend.remove(&t);
                        canceled.insert(t, (si, i));
                        last_write.insert(t, (si, false, true));
                        // Model the `get`'s push — in the emitted stream
                        // the consumer reads the kept operand at `dv`.
                        vstack.push(t);
                        continue;
                    }
                    pend.remove(&t);
                    // Copy-forward `get T` -> `get U`.
                    let v = match copymap.get(&t) {
                        Some(&u) if wmode & 8 != 0 => u,
                        _ => t,
                    };
                    rewrites.push((i, WPeek::Get(v)));
                    if let Some(lw) = last_write.get_mut(&v) {
                        lw.2 = true;
                    }
                    vstack.push(v);
                }
                WPeek::Other => {
                    let Some((pops, pushes)) = eff(ins[i].1) else {
                        vstack.clear();
                        pend.clear();
                        copymap.clear();
                        last_write.clear();
                        continue;
                    };
                    for _ in 0..pops {
                        if vstack.pop().is_none() {
                            // Pop below the tracked region — may
                            // consume a pending set's operand slot.
                            pend.clear();
                        }
                    }
                    for _ in 0..pushes {
                        vstack.push(OTHER);
                    }
                }
            }
            // A pop to `len == dv` consumes the pending set's operand
            // slot — in the deleted-set world the phantom lives there.
            let len = vstack.len();
            pend.retain(|_, &mut (si, dv)| si == i || len > dv);
            if crate::env_set("PLIRON_WASM_WALIAS_TRACE") {
                eprintln!(
                    "    walias-trace[{i}] {:?} len={} pend={:?} cx={canceled:?} cm={copymap:?}",
                    ins[i].0,
                    len,
                    pend.keys().collect::<Vec<_>>()
                );
            }
        }
        if crate::env_set("PLIRON_WASM_SFWD_DBG") && !dels.is_empty() {
            let ks: Vec<_> = dels.iter().map(|&d| format!("{}:{:?}", d, ins[d].0)).collect();
            eprintln!("  walias-delkinds: {ks:?}");
        }
        // End-of-body flush: unread writes are dead. `pi == usize::MAX`
        // is a merge sentinel (t live from multiple path-writes).
        for (&t, &(pi, ptee, read)) in last_write.iter() {
            if read || pi == usize::MAX {
                continue;
            }
            if wmode & 4 == 0 || loop_reads(pi, t) {
                continue;
            }
            if ptee {
                dels.insert(pi);
            } else {
                drops.push(pi);
            }
        }
        if crate::env_set("PLIRON_WASM_SFWD_DBG") {
            eprintln!(
                "walias: dels={:?} drops={:?} rewrites={:?}",
                dels.iter().collect::<Vec<_>>(),
                drops,
                rewrites
            );
        }
        let orig: Vec<WPeek> = ins.iter().map(|e| e.0).collect();
        // A `set` in both sets must stay `Del` — the canceled pair
        // needs the value flowing, not dropped.
        for i in drops {
            if !dels.contains(&i) {
                ins[i].0 = WPeek::Drop;
            }
        }
        for i in dels {
            ins[i].0 = WPeek::Del;
        }
        for (i, k) in rewrites {
            ins[i].0 = k;
        }
        // Provenance verifier: replay orig vs edited streams tracking
        // each stack slot's producer token; every pop in a non-deleted
        // instr must consume the same token in both worlds. `Del`'d
        // instrs skip; phantoms shift the edited stack freely.
        if crate::env_set("PLIRON_WASM_WALIAS_VERIFY") {
            let mut ost: Vec<u32> = Vec::new();
            let mut est: Vec<u32> = Vec::new();
            let mut olo: FxHashMap<u32, u32> = FxHashMap::default();
            let mut elo: FxHashMap<u32, u32> = FxHashMap::default();
            // `lo` merges across structured control like `copymap`/
            // `last_write` do in the pass — `t`'s token survives an
            // `end` iff every incoming path wrote it from the same
            // producer; else it drops to ambient. `snaps`/`saved` hold
            // (orig, edited) map pairs; `st` stacks still can't cross.
            struct VF {
                op: u8,
                saved: Option<(FxHashMap<u32, u32>, FxHashMap<u32, u32>)>,
                snaps: Vec<(FxHashMap<u32, u32>, FxHashMap<u32, u32>)>,
            }
            let mut vpath: Vec<VF> = Vec::new();
            let mut vreach = true;
            // `lo` merge: `t`'s token survives iff every bag agrees;
            // a disagreement yields a PHI token hashed from the
            // sorted constituent tokens — two locals holding the same
            // path-varying value (a sound `get t`→`get u` rewrite)
            // produce the same PHI token. `t` absent on a bag counts
            // as its ambient token.
            let vmerge = |bags: Vec<FxHashMap<u32, u32>>| -> FxHashMap<u32, u32> {
                let mut keys: FxHashSet<u32> = FxHashSet::default();
                for b in &bags {
                    keys.extend(b.keys());
                }
                let mut m = FxHashMap::default();
                'k: for &k in &keys {
                    let mut toks: Vec<u32> = Vec::with_capacity(bags.len());
                    for b in &bags {
                        toks.push(*b.get(&k).unwrap_or(&(0x8000_0000 + k)));
                    }
                    for &t in &toks[1..] {
                        if t != toks[0] {
                            toks.sort_unstable();
                            toks.dedup();
                            let mut h = 0xcbf2_9ce4_8422_2325u64;
                            for &t in &toks {
                                h ^= t as u64;
                                h = h.wrapping_mul(0x0000_0100_0000_01b3);
                            }
                            m.insert(k, 0xC000_0000 | (h as u32 & 0x3fff_ffff));
                            continue 'k;
                        }
                    }
                    m.insert(k, toks[0]);
                }
                m
            };
            let mut bad: Option<(usize, u32, u32, WPeek, WPeek)> = None;
            for i in 0..ins.len() {
                let ok = orig[i];
                let ek = ins[i].0;
                // Structured-control handling shared by both sims (the
                // ctl stream is identical — dels never touch `Ctl`).
                if matches!(ok, WPeek::Ctl | WPeek::Br(_) | WPeek::BrIf(_)) {
                    let op = match ok {
                        WPeek::Br(_) => 0x0c,
                        WPeek::BrIf(_) => 0x0d,
                        _ => b[ins[i].1],
                    };
                    let vsnap = |om: &FxHashMap<u32, u32>, em: &FxHashMap<u32, u32>| {
                        (om.clone(), em.clone())
                    };
                    let vsnap_to = |d: u32,
                                    vpath: &mut Vec<VF>,
                                    om: &FxHashMap<u32, u32>,
                                    em: &FxHashMap<u32, u32>| {
                        let idx = vpath.len().wrapping_sub(1 + d as usize);
                        if let Some(f) = vpath.get_mut(idx)
                            && f.op != 0x03
                        {
                            f.snaps.push(vsnap(om, em));
                        }
                    };
                    match op {
                        0x02 | 0x03 => {
                            vpath.push(VF {
                                op,
                                saved: None,
                                snaps: Vec::new(),
                            });
                            vreach = true;
                        }
                        0x04 => {
                            vpath.push(VF {
                                op,
                                saved: Some(vsnap(&olo, &elo)),
                                snaps: Vec::new(),
                            });
                            vreach = true;
                        }
                        0x05 => {
                            if let Some(f) = vpath.last_mut() {
                                if vreach {
                                    f.snaps.push(vsnap(&olo, &elo));
                                }
                                if let Some((om, em)) = f.saved.take() {
                                    olo = om;
                                    elo = em;
                                }
                            }
                            vreach = true;
                        }
                        0x0b => {
                            if let Some(mut f) = vpath.pop() {
                                let mut bags = std::mem::take(&mut f.snaps);
                                if vreach {
                                    bags.push(vsnap(&olo, &elo));
                                }
                                if let Some(a) = f.saved {
                                    bags.push(a);
                                }
                                vreach = !bags.is_empty();
                                let (obs, ebs): (Vec<_>, Vec<_>) =
                                    bags.into_iter().unzip();
                                olo = vmerge(obs);
                                elo = vmerge(ebs);
                            }
                        }
                        0x0c | 0x0d => {
                            if vreach {
                                let d = match ok {
                                    WPeek::Br(d) | WPeek::BrIf(d) => d,
                                    _ => {
                                        let mut p = ins[i].1 + 1;
                                        urd(b, &mut p).unwrap_or(0)
                                    }
                                };
                                vsnap_to(d, &mut vpath, &olo, &elo);
                            }
                            if op == 0x0c {
                                vreach = false;
                            }
                        }
                        0x0e => {
                            if vreach {
                                let mut p = ins[i].1 + 1;
                                let n = urd(b, &mut p).unwrap_or(0);
                                for _ in 0..=n {
                                    let d = urd(b, &mut p).unwrap_or(0);
                                    vsnap_to(d, &mut vpath, &olo, &elo);
                                }
                            }
                            vreach = false;
                        }
                        0x00 | 0x0f => {
                            vreach = false;
                        }
                        _ => {}
                    }
                    ost.clear();
                    est.clear();
                    continue;
                }
                let sim = |k: WPeek,
                           st: &mut Vec<u32>,
                           lo: &mut FxHashMap<u32, u32>,
                           i: usize|
                 -> Vec<u32> {
                    match k {
                        WPeek::Del => vec![],
                        WPeek::Nop(_) => vec![],
                        WPeek::Get(t) => {
                            st.push(*lo.get(&t).unwrap_or(&(0x8000_0000 + t)));
                            vec![]
                        }
                        WPeek::Set(t) => {
                            let v = st.pop().unwrap_or(u32::MAX);
                            lo.insert(t, v);
                            vec![v]
                        }
                        WPeek::Tee(t) => {
                            let v = st.pop().unwrap_or(u32::MAX);
                            lo.insert(t, v);
                            st.push(v);
                            vec![v]
                        }
                        WPeek::Drop => vec![st.pop().unwrap_or(u32::MAX)],
                        WPeek::Eqz => {
                            let v = st.pop().unwrap_or(u32::MAX);
                            st.push(i as u32);
                            vec![v]
                        }
                        WPeek::Ctl | WPeek::BrIf(_) | WPeek::Br(_) => {
                            st.clear();
                            lo.clear();
                            vec![]
                        }
                        WPeek::Other => match eff(ins[i].1) {
                            Some((p, u)) => {
                                let mut popped = Vec::new();
                                for _ in 0..p {
                                    popped.push(st.pop().unwrap_or(u32::MAX));
                                }
                                for _ in 0..u {
                                    st.push(i as u32);
                                }
                                popped
                            }
                            None => {
                                st.clear();
                                lo.clear();
                                vec![]
                            }
                        },
                    }
                };
                let opops = sim(ok, &mut ost, &mut olo, i);
                let epops = sim(ek, &mut est, &mut elo, i);
                // A `Del`'d `set`/`tee` still "pops" in the orig world —
                // the operand stays as the phantom, so compare the slot
                // it sat in.
                let epops = if epops.is_empty()
                    && matches!(ek, WPeek::Del)
                    && matches!(ok, WPeek::Set(_) | WPeek::Tee(_))
                    && !est.is_empty()
                {
                    vec![*est.last().unwrap()]
                } else {
                    epops
                };
                // `set`→`Drop` keeps one pop; dels'd instrs pop in orig
                // only. Compare only slots both sims actually popped.
                for j in 0..opops.len().min(epops.len()) {
                    if bad.is_none()
                        && opops[j] != epops[j]
                        && opops[j] != u32::MAX
                        && epops[j] != u32::MAX
                    {
                        bad = Some((i, opops[j], epops[j], ok, ek));
                    }
                }
            }
            if let Some((i, ta, tb, ok, ek)) = bad {
                eprintln!(
                    "  walias-VERIFY-FAIL@{i}: orig {ok:?} popped tok {ta}, edited {ek:?} popped tok {tb}"
                );
                eprintln!("    olo={olo:?} elo={elo:?} ost={ost:?} est={est:?}");
                for j in i.saturating_sub(8)..=i.min(ins.len() - 1) {
                    eprintln!("    ins[{j}] {:?}->{:?} op={:02x}", orig[j], ins[j].0, b[ins[j].1]);
                }
            }
        }
        // Emit-model balance check: replay the rewritten stream —
        // `Del` on a `set` keeps its value (push), on a `get` removes
        // a push; `Drop` pops. Any underflow means a dels bug.
        if crate::env_set("PLIRON_WASM_SFWD_DBG") {
            let mut d = 0i32;
            for (i, &(k, s, ..)) in ins.iter().enumerate() {
                let (p, u) = match k {
                    WPeek::Del | WPeek::Nop(_) => (0, 0),
                    WPeek::Get(_) => (0, 1),
                    WPeek::Set(_) => (1, 0),
                    WPeek::Tee(_) => (1, 1),
                    WPeek::Drop => (1, 0),
                    WPeek::Eqz => (1, 1),
                    WPeek::Ctl | WPeek::BrIf(_) | WPeek::Br(_) => (0, 0),
                    WPeek::Other => eff(s).unwrap_or((0, 0)),
                };
                d = d - p as i32 + u as i32;
                if d < 0 {
                    eprintln!("  walias-UNDERFLOW@{i} {k:?} op={:02x}", b[s]);
                }
            }
        }
    }
    wtrace!("walias", ins);
    // Store→load forwarding through a provably-stable base local: the
    // frame-resident aggregate splice leaves `get fp; get v; store +K`
    // … `get fp; load +K` — the load re-reads bytes the store just
    // wrote (the call-boundary spill/reload that costs ~10 memory ops
    // per loop iteration around inlined callees). Within one
    // straight-line span, a `store` whose operand stack resolves to
    // `get B`-produced base + a pushed value — found by walking
    // `need` operand slots back through `es` — followed by a `load`
    // whose single operand is `get B` at the same offset and width is
    // exactly `local.get L` for an `L` holding the stored bytes. When
    // the value producer is itself `local.get V`, `L=V` (the existing
    // zero-cost forward); otherwise a `local.tee L` spliced just before
    // the store captures any computed value (`f64` math in `nbody`'s
    // velocity churn, overwritten sources, tee-shaped producers) into a
    // fresh local — `L` is then written exactly once and immune to the
    // `set`/`tee` kill rule. The map entry dies on any
    // `Ctl`/`BrIf`/`Br`/call/`memory.*` boundary, on a write of the
    // base or forwarding local, and on any store whose base isn't a
    // provable `get B` at a disjoint offset. `PLIRON_WASM_SFWD=0`
    // disables.
    if crate::env_get("PLIRON_WASM_SFWD") != Some("0") {
        // (opcode, memarg offset) for a memory op.
        let memop = |i: usize| -> Option<(u8, u32)> {
            if !matches!(ins[i].0, WPeek::Other) {
                return None;
            }
            let s = ins[i].1;
            let op = b[s];
            if !(0x28..=0x3e).contains(&op) {
                return None;
            }
            let mut q = s + 1;
            urd(b, &mut q)?; // align
            let off = urd(b, &mut q)?;
            Some((op, off))
        };
        let swidth = |op: u8| -> u32 {
            match op {
                0x3a | 0x3b => 1,         // i32/i64 store8
                0x3c | 0x3d => 2,         // i32/i64 store16
                0x37 | 0x39 => 8,         // i64/f64 store
                _ => 4,                   // i32/f32 store, i64.store32
            }
        };
        let lwidth = |op: u8| -> u32 {
            match op {
                0x2c | 0x2d | 0x30 | 0x31 => 1, // *.load8_s/u
                0x2e | 0x2f | 0x32 | 0x33 => 2, // *.load16_s/u
                0x29 | 0x2b => 8,             // i64/f64 load
                _ => 4,                       // i32/f32 load, i64.load32
            }
        };
        let stype = |op: u8| -> u8 {
            match op {
                0x37 => 0x7e, // i64.store -> i64 local
                0x38 => 0x7d, // f32.store -> f32 local
                0x39 => 0x7c, // f64.store -> f64 local
                _ => 0x7f,    // i32.store -> i32 local
            }
        };
        // (base local, offset) -> (forwarding local, store opcode,
        // store ins, addr ins, exposed, flow). `exposed` marks a
        // possibly-aliasing unforwarded read of the range; an unexposed
        // entry is overwritten by the next same-slot store in the same
        // straight-line `flow` region, so its store is dead: `Del` the
        // addr `get` and turn the store into `drop` (its two pushes →
        // one pop: net-0). A store in a different region (an `if` arm,
        // after a `br`/`end`) does not dominate the old store — the old
        // store must keep writing on paths that skip the new one.
        // Forwarded loads read the captured local, not memory, so they
        // never expose.
        type FM = FxHashMap<(u32, u32), (u32, u8, usize, usize, bool, u32)>;
        let mut flow: u32 = 0;
        let mut fmap: FM = FxHashMap::default();
        // `fmap` merges across structured control like `copymap`/`last_write`:
        // an entry survives an `end` iff every incoming path holds the
        // same (fwd, sop, s_ins, a_ins) — `exposed` ORs in case one path
        // saw an unforwarded read. Loop-entry kills entries whose base
        // or forwarding local the body rewrites; `br` to `loop`
        // (backedge) never snapshots.
        struct FF {
            op: u8,
            saved: Option<FM>,
            snaps: Vec<FM>,
        }
        let mut fpath: Vec<FF> = Vec::new();
        let mut freach = true;
        // Rewrites are collected, not applied mid-scan: `dead` hides a
        // deleted `get B` from `prev_e`/`get_of` and `repl` respells a
        // forwarded load as the `get` it becomes, so later producer
        // lookups see the post-rewrite instruction stream.
        let dead = std::cell::RefCell::new(FxHashSet::default());
        let repl = std::cell::RefCell::new(FxHashMap::default());
        let dropped = std::cell::RefCell::new(FxHashSet::default()); // store -> `drop`
        let mut rewrites: Vec<(usize, usize, u32)> = Vec::new(); // (get ins, load ins, L)
        let mut store_dels: Vec<(usize, usize)> = Vec::new(); // (addr ins, store ins)
        let mut inserts: Vec<(usize, (WPeek, usize, usize, usize))> = Vec::new();
        let dbg = crate::env_set("PLIRON_WASM_SFWD_DBG");
        // `PLIRON_WASM_SFWD_FWLIM=N`: allow at most N forwarded loads in
        // this body — bisects a bad forward without disabling tracking.
        let fwlim = crate::env_get("PLIRON_WASM_SFWD_FWLIM").and_then(|v| v.parse::<usize>().ok());
        // `PLIRON_WASM_SFWD_FWSKIP=N`: skip only the Nth (1-based)
        // forwarded load — pairs with FWLIM to pin one rewrite.
        let fwskip =
            crate::env_get("PLIRON_WASM_SFWD_FWSKIP").and_then(|v| v.parse::<usize>().ok());
        let mut nfw = 0usize;
        let mut n_clear = 0usize;
        let mut n_walkfail = 0usize;
        let mut n_exposed_set = 0usize;
        let last_fail = std::cell::RefCell::new(String::new());
        let prev_e = |mut j: usize| -> Option<usize> {
            let dead = dead.borrow();
            loop {
                j = j.checked_sub(1)?;
                if !dead.contains(&j)
                    && !matches!(ins[j].0, WPeek::Del | WPeek::Nop(_))
                {
                    return Some(j);
                }
            }
        };
        let get_of = |j: usize| -> Option<u32> {
            if dead.borrow().contains(&j) {
                return None;
            }
            if let Some(&v) = repl.borrow().get(&j) {
                return Some(v);
            }
            match ins[j].0 {
                WPeek::Get(l) => Some(l),
                _ => None,
            }
        };
        // Stack effect honoring rewrites: a `repl`'d load now behaves
        // like the `get L` it emits, a dropped store pops one (the
        // addr-get's push was deleted alongside). None = opaque (calls,
        // control, memory, markers) — aborts the operand walk.
        let esf = |j: usize| -> Option<(u32, u32)> {
            if repl.borrow().contains_key(&j) {
                return Some((0, 1));
            }
            if dropped.borrow().contains(&j) {
                return Some((1, 0));
            }
            match ins[j].0 {
                WPeek::Get(_) => Some((0, 1)),
                WPeek::Set(_) => Some((1, 0)),
                WPeek::Tee(_) => Some((1, 1)),
                WPeek::Other => {
                    let op = b[ins[j].1];
                    Some(match op {
                        0x01 => (0, 0),                      // nop
                        0x1a => (1, 0),                      // drop
                        0x1b | 0x1c => (3, 1),               // select
                        0x23 => (0, 1),                      // global.get
                        0x28..=0x35 => (1, 1),               // loads
                        0x36..=0x3e => (2, 0),               // stores
                        0x41..=0x44 => (0, 1),               // consts
                        0x45 | 0x50 => (1, 1),               // eqz
                        0x46..=0x4f | 0x51..=0x66 => (2, 1), // compares
                        0x67..=0x69 | 0x79..=0x7b | 0x8b..=0x91 | 0x99..=0x9f => {
                            (1, 1)
                        }
                        0x6a..=0x78 | 0x7c..=0x8a | 0x92..=0x98 | 0xa0..=0xa6 => {
                            (2, 1)
                        }
                        0xa7..=0xc4 => (1, 1), // conversions
                        0xd1 | 0xd2 => (0, 1), // ref.null / ref.func
                        _ => {
                            if dbg {
                                *last_fail.borrow_mut() =
                                    format!("esf op{:02x}@{j}", op);
                            }
                            return None;
                        }
                    })
                }
                k => {
                    if dbg {
                        *last_fail.borrow_mut() = format!("esf {:?}@{j}", k);
                    }
                    None
                }
            }
        };
        // Resolve a memory op's operand stack: walking back, `rem` is
        // how many slots still need earlier producers — an instr's
        // pushes cover the top of the uncovered region, its pops add
        // deeper needs below it. The window's top slot (a store's
        // value) is covered by the instr that first pushes while
        // `rem == need0`; its bottom slot (the address) is covered
        // once an instr covers all `rem` remaining — that producer is
        // what must be `local.get B`. Unknown effects (calls, control,
        // markers) abort the walk.
        let operands = |i: usize, need0: u32| -> Option<(usize, Option<usize>)> {
            let mut j = i;
            let mut rem = need0;
            let mut val = None;
            let mut steps = 0;
            let mut trace = String::new();
            loop {
                let Some(nj) = prev_e(j) else {
                    if dbg { *last_fail.borrow_mut() = format!("prev_e-empty@{j} rem={rem} [{trace}]"); }
                    return None;
                };
                j = nj;
                steps += 1;
                if steps > 80 {
                    if dbg { *last_fail.borrow_mut() = format!("steps80@{j} rem={rem} [{trace}]"); }
                    return None;
                }
                let Some((pops, pushes)) = esf(j) else { return None; };
                if dbg && i < 200 {
                    trace = format!("{j}:-{pops}+{pushes} {trace}");
                }
                let covered = pushes.min(rem);
                if covered > 0 && rem == need0 && val.is_none() {
                    val = Some(j);
                }
                if covered == rem {
                    return Some((j, val));
                }
                rem = rem - covered + pops;
            }
        };
        for i in 0..ins.len() {
            match ins[i].0 {
                WPeek::Ctl | WPeek::BrIf(_) | WPeek::Br(_) => {
                    flow += 1;
                    let op = match ins[i].0 {
                        WPeek::Br(_) => 0x0c,
                        WPeek::BrIf(_) => 0x0d,
                        _ => b[ins[i].1],
                    };
                    let fsnap_to = |d: u32, fpath: &mut Vec<FF>, fm: &FM| {
                        let idx = fpath.len().wrapping_sub(1 + d as usize);
                        if let Some(f) = fpath.get_mut(idx)
                            && f.op != 0x03
                        {
                            f.snaps.push(fm.clone());
                        }
                    };
                    match op {
                        0x02 => fpath.push(FF {
                            op,
                            saved: None,
                            snaps: Vec::new(),
                        }),
                        0x03 => {
                            fpath.push(FF {
                                op,
                                saved: None,
                                snaps: Vec::new(),
                            });
                            // Backedge hazard: a load forwarded to a pre-loop
                            // entry must hold on every iteration, but the scan
                            // sees the body once — a store or call later in the
                            // body rewrites memory for the *next* iteration.
                            // Kill any entry the body can clobber: overlapping
                            // same-base stores, an unresolvable store address,
                            // or any call/`memory.*` clears everything.
                            if !fmap.is_empty() {
                                let mut kill_all = false;
                                let mut slots: Vec<(u32, u32, u32)> = Vec::new(); // (base, off, width)
                                let mut depth = 1usize;
                                let mut j = i + 1;
                                let mut scanned = 0usize;
                                while depth > 0 && j < ins.len() && scanned < 4000 && !kill_all {
                                    scanned += 1;
                                    match ins[j].0 {
                                        WPeek::Ctl => match b[ins[j].1] {
                                            0x02 | 0x03 | 0x04 => depth += 1,
                                            0x0b => depth -= 1,
                                            0x10 | 0x11 => kill_all = true,
                                            _ => {}
                                        },
                                        WPeek::Other if b[ins[j].1] >= 0xfb => {
                                            kill_all = true;
                                        }
                                        WPeek::Other if (0x36..=0x3e).contains(&b[ins[j].1]) => {
                                            let Some((sop, off)) = memop(j) else {
                                                kill_all = true;
                                                break;
                                            };
                                            match operands(j, 2).and_then(|(a, _)| get_of(a)) {
                                                Some(bx) => slots.push((bx, off, swidth(sop))),
                                                None => kill_all = true,
                                            }
                                        }
                                        _ => {}
                                    }
                                    j += 1;
                                }
                                if kill_all || depth > 0 {
                                    fmap.clear();
                                } else if !slots.is_empty() {
                                    fmap.retain(|&(b2, o2), e| {
                                        slots.iter().all(|&(b3, o3, w3)| {
                                            b3 != b2 || o3 + w3 <= o2 || o2 + swidth(e.1) <= o3
                                        })
                                    });
                                }
                            }
                            if let Some(ws) = loop_writes.get(&i) {
                                fmap.retain(|&(b2, _), &mut e| {
                                    !ws.contains(&b2) && !ws.contains(&e.0)
                                });
                            }
                        }
                        0x04 => fpath.push(FF {
                            op,
                            saved: Some(fmap.clone()),
                            snaps: Vec::new(),
                        }),
                        0x05 => {
                            if let Some(f) = fpath.last_mut() {
                                if freach {
                                    f.snaps.push(fmap.clone());
                                }
                                if let Some(s) = f.saved.take() {
                                    fmap = s;
                                }
                            }
                            freach = true;
                        }
                        0x0b => {
                            if let Some(mut f) = fpath.pop() {
                                let mut bags = std::mem::take(&mut f.snaps);
                                if freach {
                                    bags.push(std::mem::take(&mut fmap));
                                }
                                if let Some(s) = f.saved {
                                    bags.push(s);
                                }
                                freach = !bags.is_empty();
                                let mut it = bags.into_iter();
                                let mut fm = it.next().unwrap_or_default();
                                for c in it {
                                    fm.retain(|k, e| match c.get(k) {
                                        Some(e2)
                                            if e2.0 == e.0
                                                && e2.1 == e.1
                                                && e2.2 == e.2
                                                && e2.3 == e.3 =>
                                        {
                                            e.4 |= e2.4;
                                            true
                                        }
                                        _ => false,
                                    });
                                }
                                fmap = fm;
                            }
                        }
                        0x0c | 0x0d => {
                            if freach {
                                let d = match ins[i].0 {
                                    WPeek::Br(d) | WPeek::BrIf(d) => d,
                                    _ => {
                                        let mut p = ins[i].1 + 1;
                                        urd(b, &mut p).unwrap_or(0)
                                    }
                                };
                                fsnap_to(d, &mut fpath, &fmap);
                            }
                            if op == 0x0c {
                                freach = false;
                            }
                        }
                        0x0e => {
                            if freach {
                                let mut p = ins[i].1 + 1;
                                let n = urd(b, &mut p).unwrap_or(0);
                                for _ in 0..=n {
                                    let d = urd(b, &mut p).unwrap_or(0);
                                    fsnap_to(d, &mut fpath, &fmap);
                                }
                            }
                            freach = false;
                        }
                        0x00 | 0x0f => freach = false,
                        // calls and misc `Ctl`s kill slot facts (a callee
                        // may write any reachable memory).
                        _ => {
                            if dbg && !fmap.is_empty() {
                                eprintln!("  ctl@{i} cleared {} entries", fmap.len());
                            }
                            n_clear += 1;
                            fmap.clear();
                        }
                    }
                }
                WPeek::Set(l) | WPeek::Tee(l) => {
                    let pre: Vec<_> = if dbg {
                        fmap.keys().copied().collect()
                    } else {
                        Vec::new()
                    };
                    fmap.retain(|&(b2, _), &mut e| b2 != l && e.0 != l);
                    if dbg {
                        let gone: Vec<_> =
                            pre.into_iter().filter(|k| !fmap.contains_key(k)).collect();
                        if !gone.is_empty() {
                            eprintln!("  set@{i} l={l} killed {gone:?}");
                        }
                    }
                }
                // `Nop(l)`/`Drop`/`Del`/`Eqz` leave locals and memory alone;
                // `Get` is a plain read.
                WPeek::Nop(_) | WPeek::Drop | WPeek::Del | WPeek::Get(_) | WPeek::Eqz => {}
                WPeek::Other => match b[ins[i].1] {
                    // calls decode as Ctl; `memory.*`/`0xfb`-`0xfe` are
                    // opaque — atomics/SIMD/GC bail at decode anyway.
                    0xfc => {
                        n_clear += 1;
                        fmap.clear();
                    }
                    0x36..=0x3e => {
                        let Some((sop, off)) = memop(i) else {
                            continue;
                        };
                        let w = swidth(sop);
                        let Some((a_ins, v_ins)) = operands(i, 2) else {
                            n_walkfail += 1;
                            if dbg {
                                eprintln!("  walkfail store@{i} op={sop:#x} ({})", last_fail.borrow());
                                if i < 200 {
                                    for k in (i.saturating_sub(16))..=i {
                                        eprintln!(
                                            "    {k}: {:?} op={:02x} dead={}",
                                            ins[k].0, b[ins[k].1],
                                            dead.borrow().contains(&k)
                                        );
                                    }
                                }
                            }
                            fmap.clear();
                            continue;
                        };
                        let Some(bx) = get_of(a_ins) else {
                            // Computed address — could alias any slot.
                            n_clear += 1;
                            fmap.clear();
                            continue;
                        };
                        // An unexposed earlier store to the same full
                        // slot is dead — every byte it wrote is
                        // overwritten before any read could have seen
                        // it (forwarded reads went through the captured
                        // local, not memory).
                        if let Some(&(_, sop_old, s_old, a_old, exposed, oflow)) =
                            fmap.get(&(bx, off))
                            && !exposed
                            && oflow == flow
                            && w >= swidth(sop_old)
                        {
                            dead.borrow_mut().insert(a_old);
                            dropped.borrow_mut().insert(s_old);
                            store_dels.push((a_old, s_old));
                            if dbg {
                                eprintln!("  store@{i} KILL ({bx},{off})");
                            }
                        }
                        // Keep only same-base slots this store can't
                        // reach: a store through a *different* local
                        // could still alias — two locals may hold equal
                        // addresses.
                        let pre: Vec<_> = if dbg {
                            fmap.keys().copied().collect()
                        } else { Vec::new() };
                        fmap.retain(|&(b2, o2), &mut e| {
                            b2 == bx
                                && (o2 + swidth(e.1) <= off
                                    || off + w <= o2)
                        });
                        if dbg {
                            let gone: Vec<_> = pre.into_iter().filter(|k| !fmap.contains_key(k)).collect();
                            if !gone.is_empty() {
                                eprintln!("  store@{i} off={off} retain-killed {gone:?}");
                            }
                        }
                        if !(0x36..=0x39).contains(&sop) {
                            continue; // narrow stores forward nothing
                        }
                        // `get V` producers forward V directly; a
                        // computed value records `u32::MAX` and a
                        // `tee T` is only minted when a load actually
                        // forwards through this entry — killing the
                        // store later costs no capture either way.
                        let fwd = v_ins.and_then(get_of).unwrap_or(u32::MAX);
                        fmap.insert((bx, off), (fwd, sop, i, a_ins, false, flow));
                        if dbg {
                            eprintln!("  store@{i} insert ({bx},{off}) fwd={fwd}");
                        }
                    }
                    0x28..=0x35 => {
                        let Some((lop, off)) = memop(i) else {
                            for e in fmap.values_mut() {
                                e.4 = true;
                            }
                            continue;
                        };
                        let lw = lwidth(lop);
                        // An unresolvable address could read anything.
                        let Some((a_ins, bx)) = operands(i, 1)
                            .and_then(|(a, _)| get_of(a).map(|bx| (a, bx)))
                        else {
                            n_walkfail += 1;
                            if dbg { eprintln!("  walkfail load@{i} ({})", last_fail.borrow()); }
                            n_exposed_set += fmap.len();
                            for e in fmap.values_mut() {
                                e.4 = true;
                            }
                            continue;
                        };
                        let mut forwarded = false;
                        if (0x28..=0x2b).contains(&lop)
                            && let Some(e) = fmap.get_mut(&(bx, off))
                            && lop + 0x0e == e.1
                        {
                            nfw += 1;
                            let mut fwd = e.0;
                            let allowed = crate::env_get("PLIRON_WASM_SFWD_FW") != Some("0")
                                && fwlim.is_none_or(|l| nfw <= l)
                                && fwskip != Some(nfw);
                            if !allowed {
                                fwd = u32::MAX;
                            }
                            if fwd == u32::MAX && allowed {
                                // First forwarded use of a computed
                                // value — splice `tee T` before the
                                // store now (net-0 stack: pops the
                                // value, pushes it back, keeps it).
                                if minted < MAX_SFWD_LOCALS {
                                    let t = next_local;
                                    next_local += 1;
                                    minted += 1;
                                    groups.push((1, stype(e.1)));
                                    let s = e.2;
                                    inserts.push((s, (WPeek::Tee(t), ins[s].1, ins[s].1, ins[s].3)));
                                    e.0 = t;
                                    fwd = t;
                                }
                            }
                            if fwd != u32::MAX {
                                dead.borrow_mut().insert(a_ins);
                                repl.borrow_mut().insert(i, fwd);
                                rewrites.push((a_ins, i, fwd));
                                forwarded = true;
                            }
                        }
                        // Exposure: this load reads memory. Mark every
                        // entry whose range it may have touched — a
                        // different base can't be proven disjoint; same
                        // base needs a range check. The just-forwarded
                        // entry itself read the captured local instead.
                        for (&(b2, o2), e) in fmap.iter_mut() {
                            if forwarded && b2 == bx && o2 == off {
                                continue;
                            }
                            if b2 != bx
                                || (o2 < off + lw && off < o2 + swidth(e.1))
                            {
                                if !e.4 {
                                    n_exposed_set += 1;
                                }
                                e.4 = true;
                            }
                        }
                    }
                    _ => {}
                },
            }
        }
        if crate::env_set("PLIRON_WASM_SFWD_DBG") {
            eprintln!(
                "sfwd: len={} entries={} fwds={} kills={} mints={} exposed_live={} clear={} walkfail={} exposed={} last={}",
                ins.len(),
                fmap.len(),
                rewrites.len(),
                store_dels.len(),
                minted,
                fmap.values().filter(|e| e.4).count(),
                n_clear,
                n_walkfail,
                n_exposed_set,
                last_fail.borrow()
            );
        }
        // Rewrites first — they edit in place using the original
        // indices; `tee` splices then land at their recorded positions
        // (each insert shifts later indices by one).
        for (a, s) in store_dels {
            ins[a].0 = WPeek::Del; // dead store's addr `get`
            ins[s].0 = WPeek::Drop; // pops the dangling value
        }
        for (m, i, vx) in rewrites {
            ins[m].0 = WPeek::Del; // the `get B` addr
            ins[i].0 = WPeek::Get(vx);
        }
        inserts.sort_by_key(|(pos, _)| *pos);
        for (k, (pos, t)) in inserts.into_iter().enumerate() {
            ins.insert(pos + k, t);
        }
    }
    wtrace!("sfwd", ins);
    // Dead stack-value flush — `X; drop` where `X` purely produces the
    // dropped value: `get`/`const` delete with the drop, `tee` keeps
    // its write as `set`, and a pure non-trapping op rewrites to a
    // `drop` on each of its own operands, so the fixpoint walks a dead
    // chain back to its pushes (`get a; get b; i32.sub; drop` goes
    // away entirely). Skips any inst that is a `prel` anchor — the
    // hoisted `local.get` emits at that byte position and needs its
    // operand-stack slot. `PLIRON_WASM_DDROP=0` disables.
    if opts.ddrop {
        // pops for a pure popN-push1 op eligible for `op;drop -> drop^N`
        // (excludes loads/stores/calls/globals-with-memory, div/rem and
        // trapping float->int truncations).
        fn pure_pops(op: u8) -> Option<u32> {
            Some(match op {
                0x23 => 0,                        // global.get
                0x1b => 3,                        // select
                0x45 | 0x50 => 1,                 // i32/i64.eqz
                0x46..=0x4f | 0x51..=0x5a => 2,   // i32/i64 compares
                0x5b..=0x60 | 0x61..=0x66 => 2,   // f32/f64 compares
                0x67..=0x69 => 1,                 // i32 clz/ctz/popcnt
                0x6a..=0x6c => 2,                 // i32 add/sub/mul
                0x71..=0x78 => 2,                 // i32 and/or/xor/shifts
                0x79..=0x7b => 1,                 // i64 clz/ctz/popcnt
                0x7c..=0x7e => 2,                 // i64 add/sub/mul
                0x83..=0x8a => 2,                 // i64 and..rotr
                0x8b..=0x91 => 1,                 // f32 unary
                0x92..=0x98 => 2,                 // f32 binary (no traps)
                0x99..=0x9f => 1,                 // f64 unary
                0xa0..=0xa6 => 2,                 // f64 binary
                0xa7 => 1,                        // i32.wrap_i64
                0xac | 0xad => 1,                 // i64.extend_i32_{s,u}
                0xb2..=0xbb => 1,                 // f*.convert/demote/promote
                0xbc..=0xbf => 1,                 // reinterpret
                _ => return None,
            })
        }
        let anchored = |s: usize| prel.iter().any(|&(pp, _)| pp == s);
        let mut changed = true;
        while changed {
            changed = false;
            let mut out: Vec<(WPeek, usize, usize, usize)> = Vec::with_capacity(ins.len());
            for i in 0..ins.len() {
                let (k, s, e, c) = ins[i];
                let is_drop = matches!(k, WPeek::Drop)
                    || (matches!(k, WPeek::Other) && b[s] == 0x1a);
                if !is_drop || anchored(s) {
                    out.push((k, s, e, c));
                    continue;
                }
                // Last emitting inst in `out` — Del/Nop emit nothing.
                let mut pi = out.len();
                let prev = loop {
                    if pi == 0 {
                        break None;
                    }
                    pi -= 1;
                    if !matches!(out[pi].0, WPeek::Del | WPeek::Nop(_)) {
                        break Some(pi);
                    }
                };
                let Some(pi) = prev else {
                    out.push((k, s, e, c));
                    continue;
                };
                let (pk, ps, _pe, _pc) = out[pi];
                if anchored(ps) {
                    out.push((k, s, e, c));
                    continue;
                }
                let pop0_push1 = matches!(pk, WPeek::Get(_))
                    || (matches!(pk, WPeek::Other) && matches!(b[ps], 0x41..=0x44));
                if pop0_push1 {
                    out.remove(pi);
                    changed = true;
                    continue;
                }
                if let WPeek::Tee(t) = pk {
                    // `tee; drop` — the stack copy dies, the write stays.
                    out[pi].0 = WPeek::Set(t);
                    changed = true;
                    continue;
                }
                if let WPeek::Other = pk {
                    if let Some(np) = pure_pops(b[ps]) {
                        out[pi].0 = WPeek::Del;
                        for _ in 0..np {
                            out.push((WPeek::Drop, s, s, c));
                        }
                        changed = true;
                        continue;
                    }
                }
                out.push((k, s, e, c));
            }
            ins = out;
        }
    }
    // Mirror of `bri`: `if c { A } else { br K } end` → `i32.eqz; br_if
    // (K-1); A`. The else arm must be a lone forward `br` (K >= 1;
    // `br 0` is `delse`'s shape), the `if` must be void-typed, and the
    // kept arm scans under the same rules as `bri`'s — a `br`/`br_if`
    // to the `if`'s own end rejects, outer-crossing labels retarget
    // -1. Runs late: `walias`/`sfwd`
    // delete arm-local copies that often leave a lone `br` behind.
    // `PLIRON_WASM_BRI2=0` off.
    if opts.bri2 {
        let safe_ctl = |op: u8| matches!(op, 0x00 | 0x01 | 0x0f | 0x10..=0x14);
        // Each fold emits the kept arm without rescanning it, so a
        // nested `if{A}else{br}` inside one needs another round. 8
        // rounds is generous — each strictly removes an `if`.
        for _round in 0..8 {
            let mut out: Vec<(WPeek, usize, usize, usize)> = Vec::with_capacity(ins.len());
            let mut i = 0;
            let mut folded = false;
            'iter: while i < ins.len() {
                if !(matches!(ins[i].0, WPeek::Ctl) && b[ins[i].1] == 0x04)
                    || b.get(ins[i].1 + 1) != Some(&0x40)
                {
                    out.push(ins[i]);
                    i += 1;
                    continue;
                }
                'fold: {
                    // Scan arm1 (i+1 .. j): ends at the arm's `else` or
                    // `end` at rel 0 — an `end` means no else arm.
                    let mut rel = 0usize;
                    let mut arm: Vec<(WPeek, usize, usize, usize)> = Vec::new();
                    let mut j = i + 1;
                    let mut bad = false;
                    let mut has_else = false;
                    while j < ins.len() {
                        let (k2, s2, _e2, c2) = ins[j];
                        match k2 {
                            WPeek::Br(d) | WPeek::BrIf(d) => {
                                let d = d as usize;
                                if d == rel {
                                    bad = true;
                                    break;
                                }
                                if d < rel {
                                    arm.push(ins[j]);
                                } else {
                                    arm.push((
                                        if matches!(k2, WPeek::Br(_)) {
                                            WPeek::Br((d - 1) as u32)
                                        } else {
                                            WPeek::BrIf((d - 1) as u32)
                                        },
                                        s2,
                                        s2,
                                        c2,
                                    ));
                                }
                            }
                            WPeek::Ctl => match b[s2] {
                                0x0b if rel == 0 => break, // `end` of the `if`
                                0x0b => {
                                    rel -= 1;
                                    arm.push(ins[j]);
                                }
                                0x05 if rel == 0 => {
                                    has_else = true;
                                    break;
                                }
                                // nested `else` belongs to a nested `if`
                                0x05 => arm.push(ins[j]),
                                0x02 | 0x03 | 0x04 => {
                                    rel += 1;
                                    arm.push(ins[j]);
                                }
                                0x0c | 0x0d => {
                                    let mut dp = s2 + 1;
                                    let Some(d) = urd(b, &mut dp) else {
                                        bad = true;
                                        break;
                                    };
                                    let d = d as usize;
                                    if d == rel {
                                        bad = true;
                                        break;
                                    }
                                    if d < rel {
                                        arm.push(ins[j]);
                                    } else {
                                        arm.push((
                                            if b[s2] == 0x0c {
                                                WPeek::Br((d - 1) as u32)
                                            } else {
                                                WPeek::BrIf((d - 1) as u32)
                                            },
                                            s2,
                                            s2,
                                            c2,
                                        ));
                                    }
                                }
                                // `br_table` (0x0e) carries label operands
                                // that would need retargeting — it isn't in
                                // `safe_ctl`, so it rejects below.
                                _ if !safe_ctl(b[s2]) => {
                                    bad = true;
                                    break;
                                }
                                _ => arm.push(ins[j]),
                            },
                            _ => arm.push(ins[j]),
                        }
                        j += 1;
                    }
                    if bad || !has_else || j + 2 >= ins.len() {
                        break 'fold;
                    }
                    // The else arm must be exactly `br K` (K >= 1)
                    // followed by the `if`'s `end`, ignoring dead
                    // `Del`/`Nop` markers earlier passes left behind.
                    let mut a2i = j + 1;
                    while matches!(
                        ins.get(a2i).map(|x| x.0),
                        Some(WPeek::Del | WPeek::Nop(_))
                    ) {
                        a2i += 1;
                    }
                    let Some(&a2) = ins.get(a2i) else {
                        break 'fold;
                    };
                    let k = match a2.0 {
                        WPeek::Br(d) => Some(d as usize),
                        WPeek::Ctl if b[a2.1] == 0x0c => {
                            let mut dp = a2.1 + 1;
                            urd(b, &mut dp).map(|d| d as usize)
                        }
                        _ => None,
                    };
                    let Some(k) = k else {
                        break 'fold;
                    };
                    if k == 0 {
                        break 'fold;
                    }
                    let mut e2i = a2i + 1;
                    while matches!(
                        ins.get(e2i).map(|x| x.0),
                        Some(WPeek::Del | WPeek::Nop(_))
                    ) {
                        e2i += 1;
                    }
                    if !(matches!(ins.get(e2i).map(|x| x.0), Some(WPeek::Ctl))
                        && b[ins[e2i].1] == 0x0b)
                    {
                        break 'fold;
                    }
                    // `prel` anchors on the deleted insts (the `if`,
                    // the `else`, arm2's `br`, the `end`) lose their
                    // emit slot; arm1's insts keep positions and depth.
                    if prel.iter().any(|&(pp, _)| {
                        pp == ins[i].1
                            || pp == ins[j].1
                            || pp == a2.1
                            || pp == ins[e2i].1
                    }) {
                        break 'fold;
                    }
                    out.push((WPeek::Eqz, ins[i].1, ins[i].1, ins[i].3)); // i32.eqz
                    out.push((WPeek::BrIf((k - 1) as u32), ins[i].1, ins[i].1, ins[i].3));
                    out.extend(arm);
                    i = e2i + 1;
                    folded = true;
                    continue 'iter;
                }
                out.push(ins[i]);
                i += 1;
            }
            ins = out;
            if !folded {
                break;
            }
        }
    }
    // Residual if/else→br_if: stackify `PLIRON_WBRIF=1` rewrites If
    // nodes but grows CODE (~16%) by defeating wpeep's if-based folds.
    // A late bytecode ibrk was tried and corrupted bodies (2026-10-28).
    // Next: emit br_if shapes from stackify CondBr in a wpeep-friendly
    // form, or port bri/brf to block+br_if.
    // (Local renumbering — hot-first index compaction — was tried and
    // reverted: the `groups` regroup it forces costs more than the
    // 1-byte-LEB savings it buys; ~+18B net on the module.)
    let mut nb = Vec::with_capacity(p);
    leb(&mut nb, groups.len() as u32);
    for &(c, t) in &groups {
        leb(&mut nb, c);
        nb.push(t);
    }
    // A `return` followed only by `end` opcodes (i.e. the last
    // non-`end` instruction of the body) is dead â€” the function
    // falls through with the same stack either way.
    let drop_ret = ins
        .iter()
        .rposition(|&(_, s, _, _)| b[s] != 0x0b)
        .filter(|&i| b[ins[i].1] == 0x0f);
    let mut wi = 0;
    while wi < ins.len() {
        // Keep-on-stack hoisted pushes emit as `local.get` preludes.
        while let Some(ppos) = prel.iter().position(|&(pp, _)| pp == ins[wi].1) {
            let (_, pl) = prel.remove(ppos);
            nb.push(0x20);
            leb(&mut nb, pl);
        }
        if Some(wi) == drop_ret {
            wi += 1;
            continue;
        }
        let &(k, s, e, _) = &ins[wi];
        // `i32.load*; i64.extend_i32_{s,u}` pairs that only became
        // adjacent after scheduling fuse here into the single wide
        // load (`i64.load{8,16,32}_{s,u}`); memarg bytes carry over.
        if let (WPeek::Other, Some(&(WPeek::Other, s2, _, _))) = (k, ins.get(wi + 1)) {
            // (fused opcode, operand bytes kept from the first instr).
            // `i32.load*; i64.extend_i32_{s,u}` pairs that only became
            // adjacent after scheduling fuse here into the single wide
            // load (`i64.load{8,16,32}_{s,u}`); memarg bytes carry over.
            // `i32.const 0; i32.eq` -> `i32.eqz` (and the i64 pair): the
            // const operand must be the canonical encoding of 0.
            let mop: Option<(u8, &[u8])> = match (b[s], b[s2]) {
                (0x28, 0xac) => Some((0x34, &b[s + 1..e])),
                (0x28, 0xad) => Some((0x35, &b[s + 1..e])),
                (0x2c, 0xac) => Some((0x30, &b[s + 1..e])),
                (0x2d, 0xac) => Some((0x31, &b[s + 1..e])),
                (0x2d, 0xad) => Some((0x31, &b[s + 1..e])),
                (0x2e, 0xac) => Some((0x32, &b[s + 1..e])),
                (0x2f, 0xac) => Some((0x33, &b[s + 1..e])),
                (0x2f, 0xad) => Some((0x33, &b[s + 1..e])),
                (0x29, 0xc2) => Some((0x30, &b[s + 1..e])),
                (0x29, 0xc3) => Some((0x32, &b[s + 1..e])),
                (0x29, 0xc4) => Some((0x34, &b[s + 1..e])),
                // `*loadN_u; *extendN_s` fuses to the signed load (a
                // zero-extended narrow value sign-extended is the sign
                // load), and `*loadN_s; *extendN_s` is a no-op extend.
                // A full-width load narrows to the access the extend
                // needs (`i32.load; i32.extend8_s` reads only the low
                // byte either way).
                (0x28, 0xc0) => Some((0x2c, &b[s + 1..e])),
                (0x28, 0xc1) => Some((0x2e, &b[s + 1..e])),
                (0x2c, 0xc0) => Some((0x2c, &b[s + 1..e])),
                (0x2d, 0xc0) => Some((0x2c, &b[s + 1..e])),
                (0x2e, 0xc1) => Some((0x2e, &b[s + 1..e])),
                (0x2f, 0xc1) => Some((0x2e, &b[s + 1..e])),
                (0x30, 0xc2) => Some((0x30, &b[s + 1..e])),
                (0x31, 0xc2) => Some((0x30, &b[s + 1..e])),
                (0x32, 0xc3) => Some((0x32, &b[s + 1..e])),
                (0x33, 0xc3) => Some((0x32, &b[s + 1..e])),
                (0x34, 0xc4) => Some((0x34, &b[s + 1..e])),
                (0x35, 0xc4) => Some((0x34, &b[s + 1..e])),
                (0x41, 0x46) if b[s + 1..e] == [0x00] => Some((0x45, &b[e..e])),
                (0x42, 0x51) if b[s + 1..e] == [0x00] => Some((0x50, &b[e..e])),
                _ => None,
            };
            if let Some((mop, tail)) = mop {
                nb.push(mop);
                nb.extend_from_slice(tail);
                wi += 2;
                continue;
            }
        }
        // `set L; get L` is exactly `tee L` — decode fuses them, but
        // later passes keep creating adjacent pairs; catch them here.
        // A `prel` anchored on the `get` must not lose its emit slot.
        if let (WPeek::Set(n), Some(&(WPeek::Get(m), gs, _, _))) = (k, ins.get(wi + 1)) {
            if n == m && !prel.iter().any(|&(pp, _)| pp == gs) {
                nb.push(0x22);
                leb(&mut nb, n);
                wi += 2;
                continue;
            }
        }
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
            WPeek::BrIf(d) => {
                nb.push(0x0d);
                leb(&mut nb, d);
            }
            WPeek::Br(d) => {
                nb.push(0x0c);
                leb(&mut nb, d);
            }
            WPeek::Ctl | WPeek::Other => nb.extend_from_slice(&b[s..e]),
            WPeek::Del | WPeek::Nop(_) => {}
            WPeek::Drop => nb.push(0x1a),
            WPeek::Eqz => nb.push(0x45),
        }
        wi += 1;
    }
    Some(nb)
}

/// Module-level driver for `peep_body`: walks sections, rewrites each
/// code-section function body. Leaves the module untouched on any
/// malformed section layout.
fn wpeep(bytes: &mut Vec<u8>, name: &str) {
    if bytes.len() < 8 || &bytes[..8] != b"\0asm\x01\0\0\0" {
        return;
    }
    let mut out = bytes[..8].to_vec();
    // (pops, pushes) per type index and per function index, for
    // stack-effect tracking inside `peep_body`.
    let mut tsig: Vec<(u32, u32)> = Vec::new();
    let mut fsig: Vec<(u32, u32)> = Vec::new();
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
            match id {
                1 => {
                    // Type section: functypes only.
                    let mut q = s;
                    let mut nty = match urd(bytes, &mut q) {
                        Some(n) => n,
                        None => return,
                    };
                    while nty > 0 {
                        nty -= 1;
                        let Some(ft) = bytes.get(q).copied() else { return };
                        q += 1;
                        if ft != 0x60 {
                            break;
                        }
                        let Some(np) = urd(bytes, &mut q) else { return };
                        for _ in 0..np {
                            let Some(_) = bytes.get(q) else { return };
                            q += 1;
                        }
                        let Some(nr) = urd(bytes, &mut q) else { return };
                        for _ in 0..nr {
                            let Some(_) = bytes.get(q) else { return };
                            q += 1;
                        }
                        tsig.push((np, nr));
                    }
                }
                2 => {
                    // Import section: kind-0 imports are functions.
                    let mut q = s;
                    let mut ni = match urd(bytes, &mut q) {
                        Some(n) => n,
                        None => return,
                    };
                    while ni > 0 {
                        ni -= 1;
                        for _ in 0..2 {
                            let Some(nl) = urd(bytes, &mut q) else { return };
                            if nskip(bytes, &mut q, nl as usize).is_none() {
                                return;
                            }
                        }
                        let Some(kind) = bytes.get(q).copied() else { return };
                        q += 1;
                        match kind {
                            0x00 => {
                                let Some(ti) = urd(bytes, &mut q) else { return };
                                if let Some(&sg) = tsig.get(ti as usize) {
                                    fsig.push(sg);
                                } else {
                                    fsig.push((0, 0));
                                }
                            }
                            0x01 => {
                                // table: reftype + limits
                                if bytes.get(q).is_none() {
                                    return;
                                }
                                q += 1;
                                let Some(fl) = bytes.get(q).copied() else { return };
                                q += 1;
                                let Some(_) = urd(bytes, &mut q) else { return };
                                if fl & 1 != 0 {
                                    let Some(_) = urd(bytes, &mut q) else { return };
                                }
                            }
                            0x02 => {
                                // memory: limits
                                let Some(fl) = bytes.get(q).copied() else { return };
                                q += 1;
                                let Some(_) = urd(bytes, &mut q) else { return };
                                if fl & 1 != 0 {
                                    let Some(_) = urd(bytes, &mut q) else { return };
                                }
                            }
                            0x03 => {
                                // global: type + mut
                                if nskip(bytes, &mut q, 2).is_none() {
                                    return;
                                }
                            }
                            _ => return,
                        }
                    }
                }
                3 => {
                    // Function section: type index per declared func.
                    let mut q = s;
                    let mut nf = match urd(bytes, &mut q) {
                        Some(n) => n,
                        None => return,
                    };
                    while nf > 0 {
                        nf -= 1;
                        let Some(ti) = urd(bytes, &mut q) else { return };
                        if let Some(&sg) = tsig.get(ti as usize) {
                            fsig.push(sg);
                        } else {
                            fsig.push((0, 0));
                        }
                    }
                }
                _ => {}
            }
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
        let imp = fsig.len().saturating_sub(nf as usize);
        let mut spans = Vec::with_capacity(nf as usize);
        for _ in 0..nf {
            let Some(bsz) = urd(bytes, &mut q) else {
                return;
            };
            let be = q + bsz as usize;
            if be > e {
                return;
            }
            spans.push((q, be));
            q = be;
        }
        if q != e {
            return; // trailing bytes inside the code section
        }
        // `PLIRON_WASM_PEEP_LIMIT=N`: peephole only the first N bodies
        // this process emits — bisects a wpeep miscompile to one body.
        // Bodies are numbered in order (one block reservation per module)
        // and peepholed in parallel: each reads only its own bytes.
        // `PLIRON_WASM_PEEP_MOD=<substr>` limits peepholes to modules
        // whose name contains it; `PLIRON_WASM_PEEP_FI=N` then caps the
        // per-module body index — both deterministic under parallel CGU
        // finishing, which the global PEEPED counter is not.
        static PEEPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let pmod = crate::env_get("PLIRON_WASM_PEEP_MOD");
        if pmod
            .as_deref()
            .is_some_and(|m| !m.split(',').any(|p| name.contains(p)))
        {
            return;
        }
        let limit = crate::env_get("PLIRON_WASM_PEEP_FI")
            .or_else(|| crate::env_get("PLIRON_WASM_PEEP_LIMIT"))
            .and_then(|v| v.parse::<u64>().ok());
        let base = PEEPED.fetch_add(nf as u64, std::sync::atomic::Ordering::Relaxed);
        // PLIRON: `PLIRON_WPEEP_PAR=0` forces this map serial —
        // bisects races inside peep_body_opts.
        let serial = crate::env_get("PLIRON_WPEEP_PAR").as_deref() == Some("0");
        let one = |fi: usize| {
            let (q, be) = spans[fi];
            let k = if pmod.is_some() {
                fi as u64
            } else {
                base + fi as u64
            };
            if limit.is_none_or(|l| k < l) {
                if crate::env_set("PLIRON_WASM_PEEP_LOG") {
                    eprintln!("wpeep body {name}#{fi} (k={k}, {} bytes)", be - q);
                }
                let nparams = fsig.get(imp + fi).map(|&(np, _)| np).unwrap_or(0);
                if let Some(dir) = crate::env_get("PLIRON_WASM_BODYDUMP") {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(
                        format!("{dir}/body_{}_{name}_{fi}_pre.bin", std::process::id()),
                        &bytes[q..be],
                    );
                }
                let out = {
                    let mut opts = PeepOpts::from_env();
                    opts.tag = format!("{name}#{fi}");
                    opts.trace = crate::env_get("PLIRON_WASM_TRACE_BODY")
                        .and_then(|v| v.parse::<u64>().ok())
                        == Some(k);
                    peep_body_opts(&bytes[q..be], &fsig, &tsig, nparams, &opts)
                        .unwrap_or_else(|| bytes[q..be].to_vec())
                };
                if let Some(dir) = crate::env_get("PLIRON_WASM_BODYDUMP") {
                    let _ = std::fs::write(
                        format!("{dir}/body_{}_{name}_{fi}_post.bin", std::process::id()),
                        &out,
                    );
                }
                out
            } else {
                bytes[q..be].to_vec()
            }
        };
        let bodies: Vec<Vec<u8>> = if serial {
            (0..spans.len()).map(&one).collect()
        } else {
            par_map(spans.len(), one)
        };
        for nb in bodies {
            leb(&mut body_out, nb.len() as u32);
            body_out.extend_from_slice(&nb);
        }
        leb(&mut out, body_out.len() as u32);
        out.extend_from_slice(&body_out);
        p = e;
    }
    *bytes = out;
}

#[cfg(test)]
mod wpeep_tests {
    use super::*;

    /// Wrap an instruction stream in a body prefix: one local group of
    /// `nl` i32s, then the instructions, then the body `end`.
    fn body(nl: u32, ins: &[u8]) -> Vec<u8> {
        let mut b = vec![0x01, nl as u8, 0x7f];
        b.extend_from_slice(ins);
        b.push(0x0b);
        b
    }

    fn has_seq(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    // Regression: the `btr` dead-region model must resume liveness at a
    // forward branch's landing point — a `br_table` arm selects code
    // past a `br` that the linear scan marks dead (pulldown-cmark's
    // Parser::next match dispatch). Without the resume, a `br` to a
    // `loop` header inside that span folds unconditionally, turning a
    // back-edge into a fallthrough exit.
    //
    //   block T
    //     block W
    //       block X
    //         global.get 0; br_table [X.end, W.end]
    //       end
    //       br 1                    ; -> T.end: dead until T.end
    //     end                     ; <- arm1 lands here: live again
    //     loop C
    //       if { if { br 2 } }    ; -> C header (back-edge)
    //     end
    //   end
    #[test]
    fn dead_region_resumes_at_branch_landing() {
        let b = body(
            1,
            &[
                0x02, 0x40, // block T
                0x02, 0x40, //   block W
                0x02, 0x40, //     block X
                0x23, 0x00, //       global.get 0
                0x0e, 0x01, 0x00, 0x01, // br_table [0], default 1
                0x0b, //            end X
                0x0c, 0x01, //     br 1 -> T (dead marker)
                0x0b, //          end W   <- arm1 lands here
                0x03, 0x40, //   loop C
                0x23, 0x00, //     global.get 0
                0x04, 0x40, //     if
                0x23, 0x00, //       global.get 0
                0x04, 0x40, //       if
                0x0c, 0x02, //         br 2 -> C (back-edge)
                0x0b, //         end
                0x0b, //       end
                0x0b, //     end C
                0x0b, //   end T
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(
            has_seq(&out, &[0x0c, 0x02]) || has_seq(&out, &[0x0d, 0x01]),
            "back-edge folded to fallthrough: {out:02x?}"
        );
    }

    // Regression: a `local.get x; local.set x` no-op pair inside one arm
    // of an if is still a *use* of x â€” waffle emits it when a br's block
    // param is copied onto the local that already holds it. The
    // copy-forward fixpoint must not retarget the producer onto x across
    // it: that changes what x holds on the arm's exit path. (This is the
    // <&str>::lt miscompile: the then-arm carried the len difference in
    // l4 through a deleted self-copy; retargeting the memcmp producer to
    // l4 made the block yield memcmp's 0 instead of -1.)
    //
    //   block
    //     get 1; get 3; i32.sub; set 4    ; d0 = l4
    //     i32.const 0; tee 5              ; producer p1 -> t=5
    //     i32.eqz; if
    //       get 4; set 4; br 1            ; self-copy (deleted as a pair)
    //     else
    //       get 5; set 4; br 1            ; copy t -> x
    //     end
    //   end
    //   get 4; drop
    #[test]
    fn self_copy_blocks_producer_retarget() {
        let b = body(
            8,
            &[
                0x02, 0x40, // block
                0x20, 0x01, 0x20, 0x03, 0x6b, // get 1; get 3; i32.sub
                0x21, 0x04, // set 4
                0x41, 0x00, // i32.const 0
                0x22, 0x05, // tee 5
                0x45, // i32.eqz
                0x04, 0x40, // if
                0x20, 0x04, 0x21, 0x04, // get 4; set 4
                0x0c, 0x01, // br 1
                0x05, // else
                0x20, 0x05, 0x21, 0x04, // get 5; set 4
                0x0c, 0x01, // br 1
                0x0b, // end
                0x0b, // end
                0x20, 0x04, 0x1a, // get 4; drop
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // The producer must stay on local 5 and the else copy must
        // survive â€” retargeting it (tee 4 / deleting get5;set4) is the
        // miscompile.
        assert!(has_seq(&out, &[0x22, 0x05]), "producer lost: {out:02x?}");
        assert!(has_seq(&out, &[0x20, 0x05, 0x21, 0x04]), "copy gone: {out:02x?}");
        assert!(!has_seq(&out, &[0x22, 0x04]), "miscompile: {out:02x?}");
    }

    // Regression (wasm std `words`): the producer and the copy sit in
    // different arms' reach — one arm branches out before the copy, so on
    // that path x must keep its old value. Reaching-def sets are equal
    // before and after the retarget (both sources still reach the final
    // read), so only the straight-line requirement rejects it.
    //
    //   i32.const 99; set 2          ; x's old value (frame-slot address)
    //   block
    //     i32.const 7; set 7         ; producer p1 -> t=7
    //     get 0; if
    //       br 1                     ; exit: x must still be 99
    //     else
    //       get 7; set 2             ; copy t -> x
    //     end
    //   end
    //   get 2; drop
    fn branch_past_copy_body() -> Vec<u8> {
        body(
            8,
            &[
                0x41, 0x63, 0x21, 0x02, // i32.const 99; set 2
                0x02, 0x40, // block
                0x41, 0x07, 0x21, 0x07, // i32.const 7; set 7
                0x20, 0x00, // get 0
                0x04, 0x40, // if
                0x0c, 0x01, // br 1
                0x05, // else
                0x20, 0x07, 0x21, 0x02, // get 7; set 2
                0x0b, // end
                0x0b, // end
                0x20, 0x02, 0x1a, // get 2; drop
            ],
        )
    }

    fn opts(audit: bool, pathcheck: bool) -> PeepOpts {
        PeepOpts {
            audit,
            audit_panic: false,
            tag: String::new(),
            pathcheck,
            trace: false,
            violations: std::cell::Cell::new(0),
            skipped: std::cell::Cell::new(0),
            ddrop: false,
            delse: false,
            bri: false,
            bri2: false,
            btr: false,
        }
    }

    fn opts_btr() -> PeepOpts {
        let mut o = opts(false, false);
        o.btr = true;
        o
    }

    fn opts_dd() -> PeepOpts {
        let mut o = opts(false, false);
        o.ddrop = true;
        o
    }

    fn opts_delse() -> PeepOpts {
        let mut o = opts(false, false);
        o.delse = true;
        o
    }

    #[test]
    fn branch_past_copy_blocks_producer_retarget() {
        let b = branch_past_copy_body();
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(has_seq(&out, &[0x20, 0x07, 0x21, 0x02]), "copy gone: {out:02x?}");
        assert!(!has_seq(&out, &[0x41, 0x07, 0x21, 0x02]), "miscompile: {out:02x?}");
        assert!(!has_seq(&out, &[0x41, 0x07, 0x22, 0x02]), "miscompile: {out:02x?}");
    }

    // The audit catches the dab149b class on its own: with the
    // straight-line requirement off, the pass's set-based check accepts
    // the retarget (first test: it really does miscompile), and the
    // Datalog audit rejects the round (second: the read after the block
    // sees `i32.const 99` in the old program and the producer in the new
    // one on the `br` path) and reverts it.
    #[test]
    fn without_pathcheck_branch_past_copy_miscompiles() {
        let out = peep_body_opts(&branch_past_copy_body(), &[], &[], 0, &opts(false, false)).unwrap();
        // The unguarded retarget rewrites the producer onto x (the
        // else-arm copy is gone), and the dead-write flush drops the
        // retargeted `const 7; set 2` — `41 07 1a` is the evidence.
        assert!(
            has_seq(&out, &[0x41, 0x07, 0x1a]) && !has_seq(&out, &[0x20, 0x07, 0x21, 0x02]),
            "expected the unguarded retarget: {out:02x?}"
        );
    }

    #[test]
    fn audit_rejects_branch_past_copy() {
        let o = opts(true, false);
        let out = peep_body_opts(&branch_past_copy_body(), &[], &[], 0, &o).unwrap();
        assert!(o.violations.get() > 0, "audit missed it: {out:02x?}");
        assert!(has_seq(&out, &[0x20, 0x07, 0x21, 0x02]), "copy gone: {out:02x?}");
        assert!(!has_seq(&out, &[0x41, 0x07, 0x21, 0x02]), "miscompile survived: {out:02x?}");
    }

    // No false alarm on a sound forward: the rewrite still fires.
    #[test]
    fn audit_accepts_plain_copy() {
        let b = body(8, &[0x41, 0x07, 0x22, 0x05, 0x20, 0x05, 0x21, 0x04, 0x20, 0x04, 0x1a]);
        let o = opts(true, true);
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert_eq!(o.violations.get(), 0);
        // Copy eliminated — `tee4` folds then dies unread.
        assert!(!has_seq(&out, &[0x20, 0x05, 0x21, 0x04]), "copy left: {out:02x?}");
    }

    // Same shape without the interfering self-copy: forwarding must still
    // fire, retargeting the producer onto the copy destination.
    #[test]
    fn plain_copy_still_forwards() {
        let b = body(
            8,
            &[
                0x41, 0x07, // i32.const 7
                0x22, 0x05, // tee 5
                0x20, 0x05, 0x21, 0x04, // get 5; set 4
                0x20, 0x04, 0x1a, // get 4; drop
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // `set4;get4` folds to `tee4`, then the dead-write flush drops
        // the `tee` — 4 is never read. The copy pair must be gone.
        assert!(!has_seq(&out, &[0x20, 0x05, 0x21, 0x04]), "copy left: {out:02x?}");
        assert!(!has_seq(&out, &[0x22, 0x04]), "dead tee kept: {out:02x?}");
    }

    // Regression (wasm `fib`): waffle lowers the loop-backedge `brif` as
    // `if c { param-copies; br L } else { exit }`, parking the copies in
    // the `if` arm. When the arm is only `get`/`set`/`tee` + `br` and the
    // else arm doesn't read the copied locals, fold to hoisted copies +
    // `br_if` — LLVM's `tee`/`br_if` loop-back shape.
    //
    //   loop
    //     i32.const 1; set 2; i32.const 0; set 3      ; producers
    //     i32.const 5; i32.const 2; i32.ge_u          ; c
    //     if
    //       get 2; get 3; set 1; set 4; br 1          ; params -> loop
    //     else
    //       get 3; return                             ; reads only scratch
    //     end
    //   end
    #[test]
    fn backedge_if_folds_to_br_if() {
        let b = body(
            6,
            &[
                0x03, 0x40, // loop
                0x41, 0x01, 0x21, 0x02, // i32.const 1; local.set 2
                0x41, 0x00, 0x21, 0x03, // i32.const 0; local.set 3
                0x41, 0x05, 0x41, 0x02, 0x4e, // i32.const 5; 2; ge_u -> c
                0x04, 0x40, // if
                0x20, 0x02, 0x20, 0x03, 0x21, 0x01, 0x21, 0x04, // get2;get3;set1;set4
                0x0c, 0x01, // br 1 (-> loop)
                0x05, // else
                0x20, 0x03, // local.get 3
                0x0f, // return
                0x0b, // end (if)
                0x0b, // end (loop)
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(
            has_seq(&out, &[0x20, 0x02, 0x20, 0x03, 0x21, 0x01, 0x21, 0x04, 0x0d, 0x00]),
            "no hoisted br_if: {out:02x?}"
        );
        assert!(!has_seq(&out, &[0x0c, 0x01]), "br not folded: {out:02x?}");
    }

    // The else arm reading a copied local must NOT fold — hoisting it
    // would make the else see the new value.
    #[test]
    fn backedge_if_else_read_blocks_fold() {
        let b = body(
            6,
            &[
                0x03, 0x40, // loop
                0x41, 0x01, 0x21, 0x02, // i32.const 1; local.set 2
                0x41, 0x05, 0x41, 0x02, 0x4e, // c
                0x04, 0x40, // if
                0x20, 0x02, 0x21, 0x01, // get2; set1 (copies local1)
                0x0c, 0x01, // br 1
                0x05, // else
                0x20, 0x01, // local.get 1 — reads the copied local
                0x1a, // drop
                0x0b, // end (if)
                0x0b, // end (loop)
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(!has_seq(&out, &[0x0d]), "unsound br_if: {out:02x?}");
        assert!(has_seq(&out, &[0x0c, 0x01]), "br still there: {out:02x?}");
    }

    // Regression (wasm `fib` inner loop): `if c { get2;get3;set1;set4;
    // br L } else { get2;extend;get3;add;tee1;return }` — the else arm
    // writes a copy dest (allowed — writes aren't reads) and ends in
    // `return` (safe Ctl — no label operand).
    #[test]
    fn backedge_if_fib_shape_folds() {
        let b = body(
            6,
            &[
                0x03, 0x40, // loop
                0x20, 0x01, 0x20, 0x04, 0x41, 0x7f, 0x6a, // get1;get4;const-1;add
                0x10, 0x05, // call 5
                0x20, 0x04, 0x41, 0x7e, 0x6a, 0x21, 0x02, // get4;const-2;add;set2
                0x7c, 0x21, 0x03, // i64.add; set3
                0x20, 0x02, 0x41, 0x02, 0x4f, 0x22, 0x04, // get2;const2;ge_u;tee4
                0x04, 0x40, // if
                0x20, 0x02, 0x20, 0x03, 0x21, 0x01, 0x21, 0x04, // get2;get3;set1;set4
                0x0c, 0x01, // br 1
                0x05, // else
                0x20, 0x02, 0xad, 0x20, 0x03, 0x7c, 0x22, 0x01, 0x0f, // get2;ext;get3;add;tee1;return
                0x0b, // end (if)
                0x0b, // end (loop)
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(
            has_seq(&out, &[0x20, 0x02, 0x20, 0x03, 0x21, 0x01, 0x21, 0x04, 0x0d, 0x00]),
            "no br_if fold: {out:02x?}"
        );
        assert!(!has_seq(&out, &[0x0c, 0x01]), "br not folded: {out:02x?}");
    }

    // `if c {br L} else {loop{…;br M}}` — the hashbrown probe shape.
    // The else arm's nested `br` reaching past its own `loop` retargets
    // -1 for the removed `if` level (`br 2` -> `br 1`); a `br` staying
    // inside its nesting is untouched.
    #[test]
    fn backedge_if_else_structured_folds() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block @1
                0x41, 0x01, // i32.const 1 — the if's condition
                0x04, 0x40, // if
                0x0c, 0x01, //   br 1 (-> @1)
                0x05, //   else
                0x03, 0x40, //   loop @2
                0x41, 0x02, 0x1a, //     i32.const 2; drop
                0x0c, 0x02, //     br 2 (-> @1) -> retarget `br 1`
                0x0b, //   end (loop)
                0x0b, //   end (if)
                0x0b, // end (block)
            ],
        );
        // btr would fold the retargeted `br 1` (it exits past the
        // loop's `end` to @1 — a valid tail fold), so pin it off to
        // keep exercising bri's retargeting.
        let mut o = opts(false, false);
        o.bri = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(has_seq(&out, &[0x0d, 0x00, 0x03, 0x40]), "no fold: {out:02x?}");
        assert!(has_seq(&out, &[0x0c, 0x01]), "br not retargeted: {out:02x?}");
        assert!(!has_seq(&out, &[0x04, 0x40]), "if left: {out:02x?}");
    }

    // A `br` in the else arm at depth == rel would target the folded
    // `if`'s own end — unrepresentable after unwrapping, so brf must
    // not fold it. (delse handles this shape separately: `br 0` lands
    // at the `if`'s own `end`, so the else arm is dead — run this with
    // delse off to exercise brf's reject path.)
    #[test]
    fn backedge_if_else_br_to_if_blocks() {
        let b = body(
            4,
            &[
                0x41, 0x01, // i32.const 1
                0x04, 0x40, // if
                0x0c, 0x01, //   br 1
                0x05, //   else
                0x0c, 0x00, //   br 0 — targets the `if`'s own end
                0x0b, //   end (if)
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts(false, false)).unwrap();
        assert!(has_seq(&out, &[0x04, 0x40]), "folded anyway: {out:02x?}");
    }

    // Regression (wasm std `float_fmt`, `flt2dec::decoder::decode`): the
    // fold's stale-read scan past `end` cleared a written local on ANY
    // `set`/`tee` — including writes inside a *conditional* arm that do
    // not dominate later reads. Here `set 3` inside `if@C`'s taken arm
    // wrongly clears l3's stale-read flag, letting the fold hoist
    // `set 2`/`set 3` over an else path that reads l3's old value.
    //
    //   block @0
    //     block @1
    //       get 0; if @A
    //         get 1; if @B
    //           get 9; set 2; get 8; set 3; br 3   ; writes {2,3} -> @0
    //         else
    //           get 5; set 2; br 2                  ; -> @1
    //         end
    //       else
    //       end
    //     end
    //     get 1; if @C
    //       const 7; set 3; br 1                    ; conditional write
    //     else
    //       get 3; drop                             ; stale read of 3!
    //     end
    //   end
    #[test]
    fn backedge_if_conditional_rewrite_blocks_fold() {
        let b = body(
            10,
            &[
                0x02, 0x40, // block @0
                0x02, 0x40, // block @1
                0x20, 0x00, // get 0
                0x04, 0x40, // if @A
                0x20, 0x01, // get 1
                0x04, 0x40, // if @B
                0x20, 0x09, 0x21, 0x02, // get 9; set 2
                0x20, 0x08, 0x21, 0x03, // get 8; set 3
                0x0c, 0x03, // br 3 (-> @0)
                0x05, // else
                0x20, 0x05, 0x21, 0x02, // get 5; set 2
                0x0c, 0x02, // br 2 (-> @1)
                0x0b, // end @B
                0x05, // else @A
                0x0b, // end @A
                0x0b, // end @1
                0x20, 0x01, // get 1
                0x04, 0x40, // if @C
                0x41, 0x07, 0x21, 0x03, // const 7; set 3
                0x0c, 0x01, // br 1 (-> @0)
                0x05, // else
                0x20, 0x03, 0x1a, // get 3; drop — stale read
                0x0b, // end @C
                0x0b, // end @0
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // `br 3` must survive — folding it to `br_if 2` hoists the
        // writes onto the else path where `get 3` reads them stale.
        assert!(has_seq(&out, &[0x0c, 0x03]), "unsound fold: {out:02x?}");
        assert!(!has_seq(&out, &[0x0d, 0x02]), "unsound br_if: {out:02x?}");
    }

    // ddrop: `tee; drop` keeps the write as `set`; a dead pure chain
    // (`get;get;sub;drop`) deletes back to its pushes.
    #[test]
    fn ddrop_tee_drop_becomes_set_dead_chain_dies() {
        let b = body(
            10,
            &[
                0x20, 0x01, 0x20, 0x02, 0x6b, // get1;get2;i32.sub
                0x22, 0x05, // tee 5 — write kept, stack copy dropped
                0x1a, // drop
                0x20, 0x03, 0x20, 0x04, 0x6a, 0x1a, // get3;get4;sub;drop — all dead
                0x20, 0x05, 0x20, 0x06, 0x6a, // get5;get6;add — l5 stays live
                0x21, 0x07, // set 7
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_dd()).unwrap();
        assert!(has_seq(&out, &[0x6b, 0x21, 0x05]), "tee->set lost: {out:02x?}");
        assert!(!has_seq(&out, &[0x22]), "tee kept: {out:02x?}");
        assert!(!has_seq(&out, &[0x20, 0x03]), "dead chain kept: {out:02x?}");
    }

    // delse: `if { A; br K } else { br K }` with K empty parent tails —
    // both arms land at the same continuation, so the else arm and
    // arm1's `br` die.
    #[test]
    fn delse_br_arms_fold_to_bare_if() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block A
                0x02, 0x40, // block B
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x41, 0x07, 0x21, 0x01, // const 7; set 1
                0x0c, 0x01, // br 1 -> B's end
                0x05, // else
                0x0c, 0x01, // br 1 — same landing
                0x0b, // end (if)
                0x0b, // end (B)
                0x20, 0x01, 0x0f, // get 1; return — shared tail
                0x0b, // end (A)
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_delse()).unwrap();
        assert!(
            has_seq(&out, &[0x04, 0x40, 0x41, 0x07, 0x21, 0x01, 0x0b]),
            "bare if not emitted: {out:02x?}"
        );
        assert!(!has_seq(&out, &[0x05]), "else kept: {out:02x?}");
        assert!(!has_seq(&out, &[0x0c, 0x01]), "br kept: {out:02x?}");
    }

    // A nonempty parent tail means `br K` skips real work the
    // fallthrough would run — the else arm is not dead.
    #[test]
    fn delse_nonempty_parent_tail_keeps_else() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block A
                0x02, 0x40, // block B
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x41, 0x07, 0x21, 0x01, // const 7; set 1
                0x0c, 0x01, // br 1
                0x05, // else
                0x0c, 0x01, // br 1 — skips the set below
                0x0b, // end (if)
                0x41, 0x09, 0x21, 0x03, // const 9; set 3 — B tail
                0x0b, // end (B)
                0x20, 0x01, 0x21, 0x02, // get 1; set 2 — l1 stays live
                0x0b, // end (A)
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_delse()).unwrap();
        assert!(has_seq(&out, &[0x05]), "else wrongly dropped: {out:02x?}");
    }

    // bri: `if { br K } else { A }` → `br_if (K-1); A` — the
    // then-arm's bare forward branch is one flat `br_if`.
    #[test]
    fn bri_if_br_else_folds_to_br_if() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block P
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x0c, 0x01, // br 1 -> P's end
                0x05, // else
                0x10, 0x05, // call 5
                0x0b, // end (if)
                0x0b, // end (P)
            ],
        );
        let mut o = opts_delse();
        o.bri = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(has_seq(&out, &[0x0d, 0x00, 0x10, 0x05]), "no br_if+A: {out:02x?}");
        assert!(!has_seq(&out, &[0x04, 0x40]), "if kept: {out:02x?}");
    }

    // A's own `br` reaching past the removed `if` retargets -1.
    #[test]
    fn bri_else_arm_br_retargets() {
        let b = body(
            5,
            &[
                0x02, 0x40, // block P
                0x02, 0x40, // block Q
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x0c, 0x02, // br 2 -> P's end
                0x05, // else
                0x10, 0x05, // call 5
                0x0c, 0x02, // br 2 -> P's end
                0x0b, // end (if)
                0x0b, // end (Q)
                0x0b, // end (P)
            ],
        );
        let mut o = opts_delse();
        o.bri = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(
            has_seq(&out, &[0x0d, 0x01, 0x10, 0x05, 0x0c, 0x01]),
            "no br_if 1 + call + br 1: {out:02x?}"
        );
    }

    // `if { br K }` with no else is just `br_if (K-1)`.
    #[test]
    fn bri_bare_if_br_folds() {
        let b = body(
            3,
            &[
                0x02, 0x40, // block P
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x0c, 0x01, // br 1 -> P's end
                0x0b, // end (if)
                0x0b, // end (P)
            ],
        );
        let mut o = opts(false, false);
        o.bri = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(has_seq(&out, &[0x0d, 0x00]), "no br_if 0: {out:02x?}");
        assert!(!has_seq(&out, &[0x04, 0x40]), "if kept: {out:02x?}");
    }

    // A `br` inside the else arm that targets the removed `if`'s own
    // end (`d == rel`) can't be expressed flat — no fold.
    #[test]
    fn bri_else_arm_br_to_if_blocks() {
        let b = body(
            3,
            &[
                0x02, 0x40, // block P
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x0c, 0x01, // br 1 -> P's end
                0x05, // else
                0x41, 0x07, 0x21, 0x01, // const 7; set 1
                0x0c, 0x00, // br 0 -> if's own end
                0x0b, // end (if)
                0x20, 0x01, 0x0f, // get 1; return
                0x0b, // end (P)
            ],
        );
        let mut o = opts(false, false);
        o.bri = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(has_seq(&out, &[0x04, 0x40]), "folded anyway: {out:02x?}");
    }

    // bri2: `if c { A } else { br K }` → `i32.eqz; br_if (K-1); A`.
    #[test]
    fn bri2_if_else_br_folds_to_eqz_br_if() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block P
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x10, 0x05, // call 5
                0x05, // else
                0x0c, 0x01, // br 1 -> P's end
                0x0b, // end (if)
                0x0b, // end (P)
            ],
        );
        let mut o = opts(false, false);
        o.bri2 = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(
            has_seq(&out, &[0x45, 0x0d, 0x00, 0x10, 0x05]),
            "no eqz;br_if 0;call: {out:02x?}"
        );
        assert!(!has_seq(&out, &[0x04, 0x40]), "if kept: {out:02x?}");
    }

    // Arm A's own `br` reaching past the removed `if` retargets -1.
    #[test]
    fn bri2_kept_arm_br_retargets() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block P
                0x02, 0x40, // block Q
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x10, 0x05, // call 5
                0x0c, 0x02, // br 2 -> P's end
                0x05, // else
                0x0c, 0x02, // br 2 -> P's end
                0x0b, // end (if)
                0x0b, // end (Q)
                0x0b, // end (P)
            ],
        );
        let mut o = opts(false, false);
        o.bri2 = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(
            has_seq(&out, &[0x45, 0x0d, 0x01, 0x10, 0x05, 0x0c, 0x01]),
            "no eqz;br_if 1 + call + br 1: {out:02x?}"
        );
    }

    // A `br` inside arm A that targets the removed `if`'s own end
    // (`d == rel`) can't be expressed flat — no fold.
    #[test]
    fn bri2_kept_arm_br_to_if_blocks() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block P
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x10, 0x05, // call 5
                0x0c, 0x00, // br 0 -> if's own end
                0x05, // else
                0x0c, 0x01, // br 1 -> P's end
                0x0b, // end (if)
                0x0b, // end (P)
            ],
        );
        let mut o = opts(false, false);
        o.bri2 = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(has_seq(&out, &[0x04, 0x40]), "folded anyway: {out:02x?}");
    }

    // `else { br 0 }` targets the `if`'s own end — `delse`'s shape, not
    // a forward exit; no fold.
    #[test]
    fn bri2_else_br_0_kept() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block P
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x10, 0x05, // call 5
                0x05, // else
                0x0c, 0x00, // br 0 -> if's own end
                0x0b, // end (if)
                0x0b, // end (P)
            ],
        );
        let mut o = opts(false, false);
        o.bri2 = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(has_seq(&out, &[0x04, 0x40]), "folded anyway: {out:02x?}");
        assert!(has_seq(&out, &[0x0c, 0x00]), "br 0 lost: {out:02x?}");
    }

    // A result-typed `if` can't fold — `br_if` can't carry the result.
    #[test]
    fn bri2_result_if_kept() {
        let b = body(
            4,
            &[
                0x20, 0x00, // get 0 -> c
                0x04, 0x7f, // if (result i32)
                0x41, 0x01, // i32.const 1
                0x05, // else
                0x0c, 0x00, // br 0
                0x0b, // end (if)
                0x1a, // drop
            ],
        );
        let mut o = opts(false, false);
        o.bri2 = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(has_seq(&out, &[0x04, 0x7f]), "folded anyway: {out:02x?}");
    }

    // Nested candidates: the outer fold emits arm A without rescanning,
    // so an `if{..}else{br}` inside it needs a second round. The inner
    // else's `br 2` (to P) is retargeted to `br 1` by the outer fold,
    // then folds itself next round.
    #[test]
    fn bri2_nested_folds_over_rounds() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block P
                0x20, 0x00, // get 0 -> c0
                0x04, 0x40, // if0
                0x20, 0x01, // get 1 -> c1
                0x04, 0x40, // if1
                0x10, 0x05, // call 5
                0x05, // else
                0x0c, 0x02, // br 2 -> P's end
                0x0b, // end (if1)
                0x05, // else (if0)
                0x0c, 0x01, // br 1 -> P's end
                0x0b, // end (if0)
                0x0b, // end (P)
            ],
        );
        let mut o = opts(false, false);
        o.bri2 = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(!has_seq(&out, &[0x04, 0x40]), "if kept: {out:02x?}");
        assert!(
            has_seq(&out, &[0x45, 0x0d, 0x00, 0x20, 0x01, 0x45, 0x0d, 0x00, 0x10, 0x05]),
            "no nested eqz;br_if: {out:02x?}"
        );
    }

    // `br_table` inside arm A carries label operands the fold doesn't
    // retarget — no fold.
    #[test]
    fn bri2_kept_arm_br_table_blocks() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block P
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x41, 0x00, // i32.const 0
                0x0e, 0x01, 0x00, 0x01, // br_table 0 1
                0x05, // else
                0x0c, 0x01, // br 1 -> P's end
                0x0b, // end (if)
                0x0b, // end (P)
            ],
        );
        let mut o = opts(false, false);
        o.bri2 = true;
        let out = peep_body_opts(&b, &[], &[], 0, &o).unwrap();
        assert!(has_seq(&out, &[0x04, 0x40]), "folded anyway: {out:02x?}");
        assert!(has_seq(&out, &[0x0e]), "br_table lost: {out:02x?}");
    }

    // `if c {} else { br 0 }` only consumes the condition — `drop`.
    #[test]
    fn delse_empty_if_becomes_drop() {
        let b = body(
            4,
            &[
                0x02, 0x40, // block A
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x05, // else
                0x0c, 0x00, // br 0 -> if's own end
                0x0b, // end (if)
                0x0b, // end (A)
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_delse()).unwrap();
        // `get 0; drop` dies to the dead-local sweep — the `if`, `else`
        // and `br` must all be gone either way.
        assert!(!has_seq(&out, &[0x04, 0x40]), "if kept: {out:02x?}");
        assert!(!has_seq(&out, &[0x05]), "else kept: {out:02x?}");
        assert!(!has_seq(&out, &[0x0c]), "br kept: {out:02x?}");
    }

    // sfwd: `get fp; get v; store` … `get fp; load` re-reads bytes the
    // store just wrote — the frame spill/reload around an spliced
    // callee. Same base local, same offset, same width, no aliasing
    // write between → the load is `get v`.
    #[test]
    fn sfwd_store_load_pair_forwards() {
        let b = body(
            4,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x20, 0x00, 0x20, 0x02, 0x36, 0x02, 0x04, // get0;get2;store fp+4
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 -> get 1
                0x20, 0x00, 0x28, 0x02, 0x04, // get0;load fp+4 -> get 2
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // The two `get fp; load` pairs collapse to `get 1` / `get 2` at
        // the load sites (after both stores).
        assert!(
            has_seq(&out, &[0x36, 0x02, 0x04, 0x20, 0x01, 0x20, 0x02]),
            "no fwd: {out:02x?}"
        );
        assert!(!has_seq(&out, &[0x28]), "load left: {out:02x?}");
    }

    // A write of the stored local between the store and the load kills
    // the forward — `get v` at the load would see the new value.
    #[test]
    fn sfwd_source_write_blocks() {
        let b = body(
            4,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x41, 0x07, 0x21, 0x01, // i32.const 7; set 1
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 — stays
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // The dead-write flush drops `set 1` (1 is never read), so the
        // load forwards to `get 1` — the deleted write means `get 1`
        // still reads the stored value. Sound, and smaller.
        assert!(has_seq(&out, &[0x20, 0x01]), "dead set kept: {out:02x?}");
        assert!(!has_seq(&out, &[0x28]), "load left: {out:02x?}");
    }

    // A write of the base local kills it too — the address moves.
    #[test]
    fn sfwd_base_write_blocks() {
        let b = body(
            4,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x20, 0x02, 0x21, 0x00, // get 2; set 0
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 — stays
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // `set 0` kills the forward; the load stays a load (copy-forward
        // may re-spell `get2;set0` as `tee0`, dropping the dead `get0`).
        assert!(has_seq(&out, &[0x28]), "unsound fwd: {out:02x?}");
    }

    // A store through a *different* base local could alias — kills all
    // entries (can't prove the two base locals differ at run time).
    #[test]
    fn sfwd_other_base_blocks() {
        let b = body(
            5,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x20, 0x04, 0x20, 0x02, 0x36, 0x02, 0x00, // get4;get2;store q+0
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 — stays
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(has_seq(&out, &[0x20, 0x00, 0x28, 0x02, 0x00]), "unsound fwd: {out:02x?}");
    }

    // An overlapping same-base store kills just that slot; a disjoint
    // same-base store leaves it alone.
    #[test]
    fn sfwd_overlap_kills_disjoint_survives() {
        let b = body(
            5,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x20, 0x00, 0x20, 0x02, 0x36, 0x02, 0x02, // get0;get2;store fp+2 (overlap [0,4))
                0x20, 0x00, 0x20, 0x03, 0x36, 0x02, 0x08, // get0;get3;store fp+8 (disjoint)
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 — stays (killed by +2)
                0x20, 0x00, 0x28, 0x02, 0x08, // get0;load fp+8 -> get 3
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(has_seq(&out, &[0x20, 0x00, 0x28, 0x02, 0x00]), "unsound fwd: {out:02x?}");
        // The disjoint +8 load forwards to `get 3` right after the +0
        // load stays as `get0;load+0`.
        assert!(
            has_seq(&out, &[0x28, 0x02, 0x00, 0x20, 0x03]),
            "no fwd: {out:02x?}"
        );
    }

    // A control boundary between the store and the load kills the map —
    // the load could observe a different write on another path.
    #[test]
    fn sfwd_ctl_blocks() {
        let b = body(
            4,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x02, 0x40, // block
                0x0b, // end
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // The store's slot fact crosses the empty block's `end` — the
        // load forwards to the stored value's local (index 1).
        assert!(has_seq(&out, &[0x20, 0x01]), "no fwd: {out:02x?}");
    }

    // A *computed* stored value (`i32.add`, not a `local.get`) is
    // captured by a `local.tee` spliced before the store into a fresh
    // local — the load then reads that local.
    #[test]
    fn sfwd_computed_val_tee_captures() {
        let b = body(
            8,
            &[
                0x20, 0x00, // get 0 (fp)
                0x20, 0x01, 0x20, 0x02, 0x6a, // get1;get2;i32.add (computed)
                0x36, 0x02, 0x00, // store fp+0
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // `add; tee T; store` … `get T` where T = 8 (8 declared i32s).
        assert!(
            has_seq(&out, &[0x6a, 0x22, 0x08, 0x36, 0x02, 0x00, 0x20, 0x08]),
            "no tee-capture: {out:02x?}"
        );
        assert!(!has_seq(&out, &[0x28]), "load left: {out:02x?}");
    }

    // A `set` of a *computed* value's producer local doesn't kill the
    // `tee`-captured forward — the capture local holds the stored
    // bytes, immune to producer writes.
    #[test]
    fn sfwd_tee_survives_source_write() {
        let b = body(
            8,
            &[
                0x20, 0x00, // get 0 (fp)
                0x20, 0x01, 0x20, 0x02, 0x6a, // get1;get2;i32.add
                0x36, 0x02, 0x00, // store fp+0
                0x41, 0x07, 0x21, 0x01, // const 7; set 1
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 -> get T
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(has_seq(&out, &[0x36, 0x02, 0x00]), "store gone: {out:02x?}");
        // `set 1` is dead (1 unread) — flushed away entirely (ddrop);
        // the load still forwards to the tee-captured `get 8`.
        assert!(
            has_seq(&out, &[0x36, 0x02, 0x00, 0x20, 0x08]),
            "no forward past set1: {out:02x?}"
        );
    }

    // The stack-walk stops at a computed address (`i32.add` producer,
    // not `local.get`) — such a store can't claim a base local and
    // clears the map (it may alias anything).
    #[test]
    fn sfwd_computed_addr_no_record() {
        let b = body(
            8,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x20, 0x00, 0x41, 0x10, 0x6a, 0x20, 0x02, 0x36, 0x02, 0x00, // store (get0+16)+0
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 — stays
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(has_seq(&out, &[0x28]), "unsound fwd: {out:02x?}");
    }

    // Loop-invariant `get;get;binop` hoists before the loop into a
    // minted local; the in-loop triple becomes a single `get`.
    #[test]
    fn licm_get_get_add_hoists() {
        let b = body(
            8,
            &[
                0x03, 0x40, // loop
                0x20, 0x00, 0x20, 0x01, 0x6a, // get0;get1;i32.add  (invariant)
                0x20, 0x02, 0x20, 0x03, 0x6b, // get2;get3;i32.sub  (invariant)
                0x6a, // i32.add (stack operands)
                0x1a, // drop
                0x0b, // end
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // preamble: get0;get1;add;set8;get2;get3;sub;set9; then an empty
        // loop — the in-loop `get8;get9;add;drop` is fully dead (ddrop).
        assert!(
            has_seq(
                &out,
                &[
                    0x20, 0x00, 0x20, 0x01, 0x6a, 0x21, 0x08, // get0;get1;add;set8
                    0x20, 0x02, 0x20, 0x03, 0x6b, 0x21, 0x09, // get2;get3;sub;set9
                    0x03, 0x40, // loop
                    0x0b, // end — loop body emptied
                ]
            ),
            "licm hoist missing: {out:02x?}"
        );
    }

    // A `get;get;binop` triple where either local is written in-loop
    // stays put.
    #[test]
    fn licm_written_local_stays() {
        let b = body(
            8,
            &[
                0x03, 0x40, // loop
                0x20, 0x00, 0x20, 0x01, 0x6a, // get0;get1;i32.add
                0x21, 0x01, // set 1  (writes operand!)
                0x0b, // end
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // no `set 8` preamble may appear
        assert!(
            !has_seq(&out, &[0x6a, 0x21, 0x08]),
            "unsound hoist: {out:02x?}"
        );
    }

    // Dead-store elimination: a second same-slot store before any read
    // makes the first store dead — it emits as `drop` (the val push is
    // consumed) and its addr `get` is deleted.
    #[test]
    fn sfwd_overwritten_store_drops() {
        let b = body(
            8,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x20, 0x00, 0x20, 0x02, 0x36, 0x02, 0x00, // get0;get2;store fp+0
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 -> get 2
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // store1's addr get and its dropped val die entirely (ddrop).
        assert!(
            has_seq(&out, &[0x20, 0x00, 0x20, 0x02, 0x36, 0x02, 0x00, 0x20, 0x02])
                && !has_seq(&out, &[0x20, 0x01]),
            "store1 not dropped: {out:02x?}"
        );
    }

    // nbody's pair shape: `get B; get B; load; tee; <f64 math>; tee;
    // store` — the store's addr is the *first* get, the load consumed
    // the second. The second pair's val chain pushes 3 locals before
    // the two binary ops so the addr-get still bottoms the window.
    #[test]
    fn sfwd_nbody_pair_shape() {
        let b = body(
            8,
            &[
                0x20, 0x00, 0x20, 0x00, 0x2b, 0x03, 0x18, // get0;get0;f64.load+24
                0x22, 0x01, // tee 1 (v_i)
                0x20, 0x02, 0x20, 0x03, 0xa2, 0xa0, // get2;get3;f64.mul;f64.sub
                0x22, 0x04, // tee 4 (v_i_new)
                0x39, 0x03, 0x18, // f64.store +24
                // second pair re-stores the same slot
                0x20, 0x00, 0x20, 0x01, 0x20, 0x02, 0x20, 0x03, 0xa2, 0xa0, // get0;get1;get2;get3;fmul;fsub
                0x22, 0x05, // tee 5
                0x39, 0x03, 0x18, // f64.store +24
                0x20, 0x00, 0x2b, 0x03, 0x18, // get0;f64.load +24 -> get T2
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // first store -> `drop` (addr get0 deleted, no tee minted —
        // capture is deferred until a load actually forwards). The
        // dead-write flush also drops the now-unread `tee 4`, and
        // `f64.mul;f64.sub;drop` then dies completely (ddrop).
        assert!(
            !has_seq(&out, &[0x22, 0x04]) && !has_seq(&out, &[0x1a]),
            "store1 not dropped: {out:02x?}"
        );
        // second store survives; its computed val mints `tee 8` at the
        // store when the trailing load forwards to it. The dead-write
        // flush also drops the now-shadowed `tee 5` (5 is never read).
        assert!(
            has_seq(&out, &[0xa2, 0xa0, 0x22, 0x08, 0x39, 0x03, 0x18, 0x20, 0x08]),
            "store2/fwd wrong: {out:02x?}"
        );
    }

    // An unforwarded read of the slot between the two stores keeps the
    // first store alive — `i32.load8_u` at the same offset overlaps the
    // 4-byte range and can't be forwarded.
    #[test]
    fn sfwd_exposed_store_survives() {
        let b = body(
            8,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x20, 0x00, 0x2d, 0x02, 0x00, // get0;i32.load8_u fp+0 (reads!)
                0x1a, // drop
                0x20, 0x00, 0x20, 0x02, 0x36, 0x02, 0x00, // get0;get2;store fp+0
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 -> get 2
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(has_seq(&out, &[0x2d, 0x02, 0x00]), "load8_u gone: {out:02x?}");
        // both stores still present: two `i36 02 00` sequences
        let cnt = (0..out.len().saturating_sub(2))
            .filter(|&k| out[k] == 0x36 && out[k + 1] == 0x02 && out[k + 2] == 0x00)
            .count();
        assert_eq!(cnt, 2, "store count: {out:02x?}");
    }

    // A store inside an `if` arm does not dominate an earlier
    // unconditional store to the same slot — the else path still reads
    // what the first store wrote. The eager KILL dropped the first
    // store anyway (words(100): a loop-top field store deleted by a
    // conditional overwrite left stale memory for the else path).
    #[test]
    fn sfwd_arm_store_cannot_kill_dominator() {
        let b = body(
            8,
            &[
                0x20, 0x00, 0x20, 0x01, 0x36, 0x02, 0x00, // get0;get1;store fp+0
                0x20, 0x02, 0x45, // get2;i32.eqz
                0x04, 0x40, // if
                0x20, 0x00, 0x20, 0x03, 0x36, 0x02, 0x00, // get0;get3;store fp+0 (arm)
                0x0b, // end
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0
                0x1a, // drop
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        // The first store must survive: `get1;drop` would be its
        // deleted shape (addr get killed, store -> drop of the value).
        assert!(
            !has_seq(&out, &[0x20, 0x01, 0x1a]),
            "store1 dropped: {out:02x?}"
        );
        assert!(has_seq(&out, &[0x36, 0x02, 0x00]), "store gone: {out:02x?}");
    }

    // A pre-loop entry forwarded to a load inside the loop must hold on
    // every iteration — a store to the same slot later in the body
    // rewrites it for the next iteration. (words(100): a pre-loop field
    // init forwarded inside the drop loop while an in-loop store and a
    // call rewrote the slot on earlier iterations.)
    #[test]
    fn sfwd_loop_store_blocks_backedge_forward() {
        let b = body(
            8,
            &[
                0x20, 0x00, 0x41, 0x07, 0x36, 0x02, 0x00, // get0;const7;store fp+0
                0x03, 0x40, // loop
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 — must stay a load
                0x1a, // drop
                0x20, 0x00, 0x41, 0x09, 0x36, 0x02, 0x00, // get0;const9;store fp+0
                0x20, 0x01, 0x45, 0x0d, 0x00, // get1;i32.eqz;br_if 0 (backedge)
                0x0b, // end
            ],
        );
        let out = peep_body(&b, &[], &[], 0).unwrap();
        assert!(
            has_seq(&out, &[0x28, 0x02, 0x00]),
            "in-loop load forwarded: {out:02x?}"
        );
    }

    // Same hazard through a call: a callee may write any memory, so a
    // loop containing a call invalidates every pre-loop entry for loads
    // inside the loop — even before the call in scan order.
    #[test]
    fn sfwd_loop_call_blocks_backedge_forward() {
        let b = body(
            8,
            &[
                0x20, 0x00, 0x41, 0x07, 0x36, 0x02, 0x00, // get0;const7;store fp+0
                0x03, 0x40, // loop
                0x20, 0x00, 0x28, 0x02, 0x00, // get0;load fp+0 — must stay a load
                0x1a, // drop
                0x10, 0x00, // call 0
                0x20, 0x01, 0x45, 0x0d, 0x00, // get1;i32.eqz;br_if 0
                0x0b, // end
            ],
        );
        let out = peep_body(&b, &[(0, 0)], &[(0, 0)], 0).unwrap();
        assert!(
            has_seq(&out, &[0x28, 0x02, 0x00]),
            "in-loop load forwarded past call: {out:02x?}"
        );
    }

    #[test]
    fn walias_body_file() {
        let Ok(path) = std::env::var("WALIAS_BODY") else {
            return;
        };
        let mut paths: Vec<_> = if std::path::Path::new(&path).is_dir() {
            std::fs::read_dir(&path)
                .unwrap()
                .filter_map(|e| {
                    let p = e.unwrap().path();
                    p.file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .ends_with("_pre.bin")
                        .then_some(p)
                })
                .collect()
        } else {
            vec![std::path::PathBuf::from(&path)]
        };
        paths.sort_by_key(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .trim_start_matches("body_")
                .trim_end_matches("_pre.bin")
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        });
        for p in paths {
            let b = std::fs::read(&p).unwrap();
            eprintln!("walias-file {}", p.display());
            let Some(out) = peep_body(&b, &[], &[], 0) else {
                continue;
            };
            std::fs::write(format!("{}.out", p.display()), &out).unwrap();
            eprintln!("wrote {}.out ({} -> {} bytes)", p.display(), b.len(), out.len());
        }
    }

    // btr: `br K` directly before the K+1 `end`s it jumps through is
    // dead — the fallthrough lands on the same continuation.
    #[test]
    fn btr_removes_redundant_br() {
        let b = body(
            2,
            &[
                0x02, 0x40, // block A
                0x02, 0x40, // block B
                0x0c, 0x01, // br 1 -> A's end (B's end + A's end follow)
                0x0b, // end B
                0x0b, // end A
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_btr()).unwrap();
        assert!(!has_seq(&out, &[0x0c]), "br kept: {out:02x?}");
    }

    // `br 2` across three void blocks folds too.
    #[test]
    fn btr_removes_br_2() {
        let b = body(
            2,
            &[
                0x02, 0x40, 0x02, 0x40, 0x02, 0x40, // block;block;block
                0x0c, 0x02, // br 2
                0x0b, 0x0b, 0x0b, // end x3
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_btr()).unwrap();
        assert!(!has_seq(&out, &[0x0c]), "br kept: {out:02x?}");
    }

    // A `br` to a `loop` jumps to its header — not the continuation —
    // so the same shape must not fold.
    #[test]
    fn btr_loop_target_kept() {
        let b = body(
            2,
            &[
                0x03, 0x40, // loop
                0x0c, 0x00, // br 0 -> loop header
                0x0b, // end loop
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_btr()).unwrap();
        assert!(has_seq(&out, &[0x0c, 0x00]), "br 0 gone: {out:02x?}");
    }

    // Non-empty operand stack at the `br` — dropping it would leave the
    // value dangling at a void `end`.
    #[test]
    fn btr_nonempty_stack_kept() {
        let b = body(
            2,
            &[
                0x02, 0x40, // block
                0x41, 0x07, // i32.const 7
                0x0c, 0x00, // br 0 (stack depth 1)
                0x0b, // end
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_btr()).unwrap();
        assert!(has_seq(&out, &[0x0c, 0x00]), "br 0 gone: {out:02x?}");
    }

    // A live inst between the `br` and the `end`s — not a tail branch.
    #[test]
    fn btr_non_end_between_kept() {
        let b = body(
            2,
            &[
                0x02, 0x40, // block
                0x0c, 0x00, // br 0
                0x01, // nop (dead but real)
                0x0b, // end
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_btr()).unwrap();
        assert!(has_seq(&out, &[0x0c, 0x00]), "br 0 gone: {out:02x?}");
    }

    // Live-unknown state (the depth model lost track) must not be
    // treated as dead: a `br` in it still targets real labels — here a
    // `loop` back-edge that dead-rules would delete. Regression test
    // for the wasmstdtest `words(100)` trap.
    #[test]
    fn btr_unknown_depth_loop_br_kept() {
        let b = body(
            1,
            &[
                0x21, 0x00, // set 0 (empty stack -> depth unknown)
                0x03, 0x40, // loop
                0x20, 0x00, // get 0
                0x04, 0x40, // if
                0x0c, 0x02, // br 2 (arm1 exits the body)
                0x05, // else — resumes into unknown-depth code
                0x0c, 0x01, // br 1 -> loop header
                0x0b, // end (if)
                0x0b, // end (loop)
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_btr()).unwrap();
        assert!(has_seq(&out, &[0x0c, 0x00]), "back-edge br 0 deleted: {out:02x?}");
    }

    // `br` last in an `if` arm — the next inst is `else`, not `end`.
    #[test]
    fn btr_if_arm_br_kept() {
        let b = body(
            2,
            &[
                0x20, 0x00, // get 0 -> c
                0x04, 0x40, // if
                0x0c, 0x00, // br 0 -> if's end
                0x05, // else
                0x0b, // end (if)
            ],
        );
        let out = peep_body_opts(&b, &[], &[], 0, &opts_btr()).unwrap();
        assert!(has_seq(&out, &[0x0c, 0x00]), "br 0 gone: {out:02x?}");
    }

    // Regression: a def of the copy target `x` in a sibling if-arm past
    // the copy window reaches reads in the other arm through the
    // enclosing loop's back-edge (core cgu.08#42: `set x` in the
    // then-arm, renamed `get x` reads in the else-arm). `reaches` used
    // to treat any arm-diverged pair as unreachable, so the pass
    // renamed `get t` -> `get x` reads that could observe the later
    // `set x` — a miscompile the audit rejected every build. The
    // candidate must now be refused up front.
    #[test]
    fn copyfwd_arm_diverged_backedge_def_rejects() {
        // t = 1 (sole producer `tee 1`), x = 2; `get 1; set 2` copy
        // before a loop that rewrites x in its then-arm and reads t in
        // the else-arm.
        let b = body(
            2,
            &[
                0x41, 0x00, // i32.const 0
                0x22, 0x01, // tee 1   (producer p1)
                0x20, 0x01, // get 1   (copy-get)
                0x21, 0x02, // set 2   (copy-set)
                0x03, 0x40, // loop
                0x20, 0x00, //   get 0 (if cond)
                0x04, 0x40, //   if
                0x20, 0x02, //     get 2 (loop var read)
                0x41, 0x02, //     i32.const 2
                0x6a, //           i32.add
                0x21, 0x02, //     set 2 (x-def in then-arm, past the window)
                0x0c, 0x01, //     br 1  (back-edge to the loop)
                0x05, //         else
                0x20, 0x00, //     get 0 (store addr)
                0x20, 0x01, //     get 1 (pre-fix renamed -> read the loop write)
                0x36, 0x00, 0x00, // i32.store
                0x0b, //         end if
                0x0b, //       end loop
            ],
        );
        let o = opts(true, false);
        let r = peep_body_opts(&b, &[], &[], 1, &o).unwrap();
        assert_eq!(o.violations.get(), 0);
        // The else-arm `get 1` must survive: retargeting it to `get 2`
        // is unsound while the loop's `set 2` can re-enter via the
        // back-edge.
        assert!(has_seq(&r, &[0x20, 0x01, 0x36, 0x00, 0x00])); // get 1; i32.store
    }

    // `PLIRON_WASM_BDSCAN=<dir>`: run every PLIRON_WASM_BODYDUMP pre-body
    // under the audited copy-forward and print violations with the file
    // and the nparams that reproduces them.
    #[test]
    fn audit_scan_dumped_bodies() {
        let Ok(dir) = std::env::var("PLIRON_WASM_BDSCAN") else {
            return;
        };
        let mut o = opts(true, true);
        o.ddrop = true;
        o.delse = true;
        o.bri = true;
        o.bri2 = true;
        o.btr = true;
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_str().is_some_and(|s| s.ends_with("_pre.bin")))
            .collect();
        paths.sort();
        // `call` decode needs (pops, pushes) per target — unknown in a
        // bare body dump, so sweep a few plausible shapes too.
        let sigs: Vec<Vec<(u32, u32)>> = [
            (0u32, 1u32),
            (1, 1),
            (2, 1),
            (4, 1),
            (0, 0),
            (1, 0),
        ]
        .iter()
        .map(|&s| vec![s; 256])
        .collect();
        for p in paths {
            let b = std::fs::read(&p).unwrap();
            'outer: for (si, fsig) in sigs.iter().enumerate() {
                for np in 0..=20u32 {
                    o.violations.set(0);
                    peep_body_opts(&b, fsig, fsig, np, &o);
                    if o.violations.get() > 0 {
                        eprintln!("VIOL {} nparams={np} sig={si}", p.display());
                        break 'outer;
                    }
                }
            }
        }
    }

}

/// Rebuild `succs`/`preds`/`pos_in_succ_pred`/`pos_in_pred_succ` from
/// terminators. Needed after `inline_site` rewrites the CFG; waffle's
/// later passes (CFGInfo, stackify) rely on them.
fn fixup_edges(b: &mut FunctionBody) {
    let mut succ: Vec<Vec<WBlock>> = vec![Vec::new(); b.blocks.len()];
    for blk in b.blocks.iter() {
        b.blocks[blk]
            .terminator
            .visit_targets(|t| succ[blk.index()].push(t.block));
    }
    for blk in b.blocks.iter() {
        b.blocks[blk].succs.clear();
        b.blocks[blk].preds.clear();
        b.blocks[blk].pos_in_succ_pred.clear();
        b.blocks[blk].pos_in_pred_succ.clear();
    }
    for (from, tos) in succ.iter().enumerate() {
        for &to in tos {
            let fb = WBlock::new(from);
            let pi = b.blocks[to].preds.len();
            let si = b.blocks[fb].succs.len();
            b.blocks[fb].succs.push(to);
            b.blocks[fb].pos_in_succ_pred.push(pi);
            b.blocks[to].preds.push(fb);
            b.blocks[to].pos_in_pred_succ.push(si);
        }
    }
}

/// Does the pure-op tree feeding `v` mention any value in `set`?
/// Follows aliases; treats unresolvable/non-pure leaves as clean.
fn tree_touches(cb: &FunctionBody, v: WV, set: &FxHashSet<WV>, depth: u32) -> bool {
    if depth > 16 {
        return true;
    }
    let v = cb.resolve_alias(v);
    if set.contains(&v) {
        return true;
    }
    match cb.values[v] {
        ValueDef::Operator(op, aa, _) if op.is_pure() => cb.arg_pool[aa]
            .iter()
            .any(|&a| tree_touches(cb, a, set, depth + 1)),
        _ => false,
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

/// A folded callee frame: `(fp value, size, align)`. Each function frame is
/// emitted as `s = global.get sp; t = s - SZ; fp = t & -A;
/// global.set sp = fp` at the head plus `global.set sp = s|fp` restores
/// on exits. `inline_site` folds every well-formed chain into one
/// entry-level reservation in the caller so the spliced body doesn't
/// rerun the adjust/restore dance at the callsite â€” which for loop-body
/// callsites costs ~10 ops per iteration.
fn frame_chains(cb: &FunctionBody, sp: waffle::Global) -> Vec<(WV, WV, WV, u64, u64)> {
    // users[v] = values whose defs reference v.
    let mut users: FxHashMap<WV, Vec<WV>> = FxHashMap::default();
    for cv in cb.values.iter() {
        let mut push = |u| users.entry(u).or_default().push(cv);
        match &cb.values[cv] {
            ValueDef::Operator(_, aa, _) => {
                for i in 0..cb.arg_pool[*aa].len() {
                    push(cb.arg_pool[*aa][i]);
                }
            }
            ValueDef::PickOutput(v, ..) | ValueDef::Alias(v) => push(*v),
            _ => {}
        }
    }
    // Values used by terminators can't be folded away.
    let mut tused: FxHashSet<WV> = FxHashSet::default();
    for blk in cb.blocks.iter() {
        cb.blocks[blk]
            .terminator
            .visit_uses(|u| _ = tused.insert(cb.resolve_alias(u)));
    }
    let mut out = Vec::new();
    for cv in cb.values.iter() {
        let ValueDef::Operator(O::GlobalGet { global_index }, ..) = &cb.values[cv] else {
            continue;
        };
        if *global_index != sp || tused.contains(&cv) {
            continue;
        }
        // Allowed uses of the SP read: exactly one `i32.sub sp0, SZ`
        // (the frame alloc) plus `global.set sp` restores.
        let mut sub = None;
        let mut ok = true;
        for &u in users.get(&cv).into_iter().flatten() {
            match &cb.values[u] {
                ValueDef::Operator(O::I32Sub, aa, _)
                    if sub.is_none() && cb.arg_pool[*aa].first() == Some(&cv) =>
                {
                    sub = Some(u)
                }
                ValueDef::Operator(O::GlobalSet { global_index }, ..) if *global_index == sp => {}
                _ => ok = false,
            }
        }
        let Some(sub) = sub.filter(|s| ok && !tused.contains(s)) else {
            continue;
        };
        let ValueDef::Operator(_, saa, _) = &cb.values[sub] else {
            continue;
        };
        let Some(size) = wconst(cb, cb.arg_pool[*saa][1]) else {
            continue;
        };
        // The sub feeds exactly the align `and` â€” the frame pointer.
        let mut fu = users.get(&sub).into_iter().flatten();
        let (Some(&fp), None) = (fu.next(), fu.next()) else {
            continue;
        };
        let ValueDef::Operator(O::I32And, aa, _) = &cb.values[fp] else {
            continue;
        };
        if cb.arg_pool[*aa].first() != Some(&sub) {
            continue;
        }
        let Some(mask) = wconst(cb, cb.arg_pool[*aa][1]) else {
            continue;
        };
        let align = (-mask).max(1) as u64;
        if !align.is_power_of_two() || size <= 0 {
            continue;
        }
        out.push((cv, sub, fp, size as u64, align));
    }
    out
}

/// Splice `callee` into `body` in place of the call `call_v` at
/// `blk.insts[pos]`. The block is split at the call: `blk` keeps the
/// prefix and merges the callee entry block (whose params alias the
/// call args), callee `return`s branch to a fresh join block carrying
/// the results as blockparams, and the join branches to the tail.
/// SSA stays valid: everything `after` uses was defined in `blk`'s
/// prefix or reaches it through the join, and `blk` still dominates
/// everything it dominated before. Callers must run `fixup_edges`
/// and re-optimize afterwards.
fn inline_site(
    body: &mut FunctionBody,
    blk: WBlock,
    mut pos: usize,
    call_v: WV,
    callee: &FunctionBody,
    sp: waffle::Global,
) {
    // Fold the callee's stack frames into the caller's frame: frames are
    // emitted as `s = global.get sp; t = s - SZ;
    // fp = t & -A; global.set sp = fp` at the head plus `global.set sp`
    // restores on exits, and the splice would re-execute that whole
    // dance at the callsite â€” ~10 ops per loop iteration for hot
    // callsites. Instead the frame is laid out inside the caller's own
    // reservation (`fp + K_i`, like LLVM's merged inline allocas): the
    // caller's `SZ` is bumped by the extra space and its mask widened
    // if a folded frame needs more alignment. A frameless caller gets
    // a fresh prologue-shaped reservation. `fold_base`: callee fp ->
    // its remapped base; `fold_drop`: callee insts to skip when copying.
    let mut fold_base: FxHashMap<WV, WV> = FxHashMap::default();
    let mut fold_drop: FxHashSet<WV> = FxHashSet::default();
    let chains = frame_chains(callee, sp);
    if !chains.is_empty() {
        let entry = body.entry;
        // Find the caller's frame chain rooted at entry insts[0]: any
        // `global.get sp` there starts either the real prologue or an
        // earlier splice's reservation â€” growing either is safe, since
        // every slot is fp-relative. `add` ops are new-frame bases.
        // (sub, and, gset entry position) of the caller's frame chain.
        let mut acc: Option<(WV, WV, usize)> = None;
        if let Some(&first) = body.blocks[entry].insts.first()
            && let ValueDef::Operator(O::GlobalGet { global_index }, ..) = &body.values[first]
            && *global_index == sp
        {
            let mut sub = None;
            let mut gpos = None;
            for (i, &iv) in body.blocks[entry].insts.iter().enumerate() {
                match body.values[iv] {
                    ValueDef::Operator(O::I32Sub, aa, _)
                        if body.arg_pool[aa].first() == Some(&first)
                            && wconst(body, body.arg_pool[aa][1]).is_some() =>
                    {
                        sub = sub.or(Some(iv))
                    }
                    ValueDef::Operator(O::GlobalSet { global_index }, ..) if global_index == sp => {
                        gpos = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            // and = the sub's I32And-with-const consumer; the gset must
            // set SP to it, else this isn't a frame chain â€” don't touch.
            if let (Some(s), Some(gpos)) = (sub, gpos) {
                let and = body.blocks[entry].insts.iter().find(|&&iv| {
                    matches!(
                        &body.values[iv],
                        ValueDef::Operator(O::I32And, aa, _)
                            if body.arg_pool[*aa].first() == Some(&s)
                                && wconst(body, body.arg_pool[*aa][1]).is_some()
                    )
                });
                if let Some(&a) = and
                    && matches!(
                        body.values[body.blocks[entry].insts[gpos]],
                        ValueDef::Operator(O::GlobalSet { .. }, aa, _)
                            if body.arg_pool[aa].first() == Some(&a)
                    )
                {
                    acc = Some((s, a, gpos));
                }
            }
        }
        // When the callsite itself is in `entry`, emitted insts must
        // land at or before the call's index â€” anything later gets
        // swept into `after` by the split below and stops dominating
        // the spliced body (SSA breakage, misaligned frames).
        let call_idx = if blk == entry {
            body.blocks[entry].insts.iter().position(|&i| i == call_v)
        } else {
            None
        };
        let pos_of = |b: &FunctionBody, v: WV| {
            b.blocks[entry]
                .insts
                .iter()
                .position(|&i| i == v)
                .unwrap_or(0)
        };
        // The anchored layout needs room for the new bases: they go
        // right after the frame's `global.set` (or before the call if
        // that comes first), but always after `a`'s def.
        let anchor_ok = match acc {
            Some((_, a, gpos)) => (gpos + 1).min(call_idx.unwrap_or(usize::MAX)) > pos_of(body, a),
            None => false,
        };
        // Lay out the folded frames after whatever space the chain
        // already reserves (0 for a fresh one).
        let (mut k, mut maxa, anchor): (u64, u64, Option<(WV, WV, usize)>) = if anchor_ok {
            let (s, a, gpos) = acc.unwrap();
            let sa = match &body.values[s] {
                ValueDef::Operator(_, aa, _) => *aa,
                _ => unreachable!(),
            };
            let aa = match &body.values[a] {
                ValueDef::Operator(_, aa, _) => *aa,
                _ => unreachable!(),
            };
            (
                wconst(body, body.arg_pool[sa][1]).unwrap_or(0) as u64,
                -wconst(body, body.arg_pool[aa][1]).unwrap_or(-16) as u64,
                Some((s, a, gpos)),
            )
        } else {
            (0, 16, None)
        };
        let mut offs = Vec::with_capacity(chains.len());
        for &(_, _, _, sz, al) in &chains {
            k = crate::types::align_to(k, al);
            offs.push(k);
            k += sz;
            maxa = maxa.max(al);
        }
        // Each emitted op carries an anchor â€” the inst it is inserted
        // before (None = end of entry) â€” so multiple inserts can't
        // drift past one another or past the call.
        let mut chunk: Vec<(WV, Option<WV>)> = Vec::new();
        if let Some((s, a, gpos)) = anchor {
            // New frame bases land right after the frame's `global.set`;
            // capture the anchor before emitting anything (add_op
            // appends to this same list and would shift the index).
            let nxt = body.blocks[entry].insts.get(gpos + 1).copied();
            let sa = match &body.values[s] {
                ValueDef::Operator(_, aa, _) => *aa,
                _ => unreachable!(),
            };
            let kc = body.add_op(
                entry,
                O::I32Const {
                    value: crate::types::align_to(k, maxa) as u32,
                },
                &[],
                &[WT::I32],
            );
            chunk.push((kc, Some(s)));
            body.arg_pool[sa][1] = kc;
            let aa = match &body.values[a] {
                ValueDef::Operator(_, aa, _) => *aa,
                _ => unreachable!(),
            };
            if maxa > -wconst(body, body.arg_pool[aa][1]).unwrap_or(-16) as u64 {
                let nm = body.add_op(
                    entry,
                    O::I32Const {
                        value: !(maxa as u32 - 1),
                    },
                    &[],
                    &[WT::I32],
                );
                chunk.push((nm, Some(a)));
                body.arg_pool[aa][1] = nm;
            }
            for (i, &(_, _, cfp, _, _)) in chains.iter().enumerate() {
                let kc = body.add_op(
                    entry,
                    O::I32Const {
                        value: offs[i] as u32,
                    },
                    &[],
                    &[WT::I32],
                );
                let ad = body.add_op(entry, O::I32Add, &[a, kc], &[WT::I32]);
                chunk.push((kc, nxt));
                chunk.push((ad, nxt));
                fold_base.insert(cfp, ad);
            }
        } else {
            // Fresh reservation: `ibase = align(SP - k); SP = ibase`,
            // plus per-frame `ibase + K` bases. Insert just past the
            // first `global.set sp` in entry â€” every callsite sits
            // after that point (calls can't precede the prologue, and
            // earlier splices only leave SP ops before the callsites
            // they replaced); position 0 when no SP adjust exists.
            let mut ipos = 0;
            for (i, &iv) in body.blocks[entry].insts.iter().enumerate() {
                if matches!(
                    body.values[iv],
                    ValueDef::Operator(O::GlobalSet { global_index }, ..)
                        if global_index == sp
                ) {
                    ipos = i + 1;
                    break;
                }
            }
            let nxt = body.blocks[entry].insts.get(ipos).copied();
            let mut seq: Vec<WV> = Vec::new();
            let mut add = |b: &mut FunctionBody, op: O, args: &[WV], tys: &[WT]| {
                let v = b.add_op(entry, op, args, tys);
                seq.push(v);
                v
            };
            let gg = add(body, O::GlobalGet { global_index: sp }, &[], &[WT::I32]);
            let ec = add(body, O::I32Const { value: k as u32 }, &[], &[WT::I32]);
            let t2 = add(body, O::I32Sub, &[gg, ec], &[WT::I32]);
            let mc = add(
                body,
                O::I32Const {
                    value: !(maxa as u32 - 1),
                },
                &[],
                &[WT::I32],
            );
            let ibase = add(body, O::I32And, &[t2, mc], &[WT::I32]);
            add(body, O::GlobalSet { global_index: sp }, &[ibase], &[]);
            for (i, &(_, _, cfp, _, _)) in chains.iter().enumerate() {
                let base = if offs[i] == 0 {
                    ibase
                } else {
                    let kc = add(
                        body,
                        O::I32Const {
                            value: offs[i] as u32,
                        },
                        &[],
                        &[WT::I32],
                    );
                    add(body, O::I32Add, &[ibase, kc], &[WT::I32])
                };
                fold_base.insert(cfp, base);
            }
            chunk = seq.iter().map(|&v| (v, nxt)).collect();
            // The caller had no frame of its own, so its return paths
            // may not restore SP; every Return block must end with a
            // `global.set sp` back to the pre-adjust value. That's the
            // earliest SP read in final entry order: `gg` itself when
            // the reservation lands before the pre-existing read.
            let mut first_get = None;
            for (i, &iv) in body.blocks[entry].insts.iter().enumerate() {
                if let ValueDef::Operator(O::GlobalGet { global_index }, ..) = &body.values[iv]
                    && *global_index == sp
                    && !seq.contains(&iv)
                {
                    first_get = Some((iv, i));
                    break;
                }
            }
            let gg_pos = ipos.min(call_idx.unwrap_or(usize::MAX));
            let orig = match first_get {
                Some((g, p)) if gg_pos > p => g,
                _ => gg,
            };
            for rb in body.blocks.iter() {
                if !matches!(body.blocks[rb].terminator, Terminator::Return { .. }) {
                    continue;
                }
                let restored = body.blocks[rb].insts.last().is_some_and(|&iv| {
                    matches!(
                        body.values[iv],
                        ValueDef::Operator(O::GlobalSet { global_index }, ..)
                            if global_index == sp
                    )
                });
                if !restored {
                    body.add_op(rb, O::GlobalSet { global_index: sp }, &[orig], &[]);
                }
            }
        }
        // Move emitted ops (currently at the end of entry's insts) to
        // their anchors â€” inserted just before the anchored inst, and
        // never past the call when it shares this block.
        {
            let insts = &mut body.blocks[entry].insts;
            for &(v, _) in &chunk {
                if let Some(p) = insts.iter().position(|&i| i == v) {
                    insts.remove(p);
                }
            }
            for (v, anchor) in chunk {
                let mut p = match anchor {
                    Some(a) => insts.iter().position(|&i| i == a).unwrap_or(insts.len()),
                    None => insts.len(),
                };
                if blk == entry
                    && let Some(cp) = insts.iter().position(|&i| i == call_v)
                {
                    p = p.min(cp);
                }
                insts.insert(p, v);
            }
        }
        // Emitting shifted inst positions; re-find the call.
        if let Some(p) = body.blocks[blk].insts.iter().position(|&i| i == call_v) {
            pos = p;
        }
        // The callee's SP ops die with the fold: the get/sub chain
        // members (fp is remapped through fold_base), plus every
        // `global.set sp` whose argument computes through folded
        // values â€” the entry adjust and the restores. A set whose arg
        // derives only from unfolded chains stays verbatim.
        let folded: FxHashSet<WV> = chains
            .iter()
            .flat_map(|&(g, s, cfp, _, _)| [g, s, cfp])
            .collect();
        for cv in callee.values.iter() {
            let ValueDef::Operator(O::GlobalSet { global_index }, aa, _) = &callee.values[cv]
            else {
                continue;
            };
            if *global_index == sp && tree_touches(callee, callee.arg_pool[*aa][0], &folded, 0) {
                fold_drop.insert(cv);
            }
        }
        for &(g, s, cfp, _, _) in &chains {
            fold_drop.extend([g, s, cfp]);
        }
    }
    let (cargs, tys): (Vec<WV>, Vec<WT>) = match &body.values[call_v] {
        ValueDef::Operator(_, args, tys) => (
            body.arg_pool[*args].iter().copied().collect(),
            body.type_pool[*tys].iter().copied().collect(),
        ),
        _ => (Vec::new(), Vec::new()),
    };

    // Block map: callee entry folds into `blk`; everything else is a
    // fresh caller block. `after` holds the post-call tail; `join`
    // collects callee returns and forwards to `after`.
    let after = body.blocks.push(Default::default());
    let join = body.blocks.push(Default::default());
    let mut bmap: Vec<WBlock> = Vec::with_capacity(callee.blocks.len());
    for cb in callee.blocks.iter() {
        bmap.push(if cb == callee.entry {
            blk
        } else {
            body.blocks.push(Default::default())
        });
    }

    // Value map: callee entry blockparams alias the call args; every
    // other callee value gets a fresh caller value (defined below).
    let mut src: Vec<WV> = Vec::with_capacity(callee.values.len());
    for cv in callee.values.iter() {
        let eparam = callee.blocks[callee.entry]
            .params
            .iter()
            .position(|&(_, pv)| pv == cv);
        src.push(match eparam {
            Some(i) => cargs[i],
            None => body.values.push(ValueDef::None),
        });
    }

    let rt = |t: &BlockTarget, bmap: &[WBlock], src: &[WV]| BlockTarget {
        block: bmap[t.block.index()],
        args: t.args.iter().map(|a| src[a.index()]).collect(),
    };
    let remap_term = |t: &Terminator, bmap: &[WBlock], src: &[WV]| -> Terminator {
        match t {
            Terminator::Br { target } => Terminator::Br {
                target: rt(target, bmap, src),
            },
            Terminator::CondBr {
                cond,
                if_true,
                if_false,
            } => Terminator::CondBr {
                cond: src[cond.index()],
                if_true: rt(if_true, bmap, src),
                if_false: rt(if_false, bmap, src),
            },
            Terminator::Select {
                value,
                targets,
                default,
            } => Terminator::Select {
                value: src[value.index()],
                targets: targets.iter().map(|t| rt(t, bmap, src)).collect(),
                default: rt(default, bmap, src),
            },
            // Callee returns become branches to the join block.
            Terminator::Return { values } => Terminator::Br {
                target: BlockTarget {
                    block: join,
                    args: values.iter().map(|a| src[a.index()]).collect(),
                },
            },
            t => t.clone(),
        }
    };

    // Copy callee value defs (entry params were aliased to call args).
    for cv in callee.values.iter() {
        let nv = src[cv.index()];
        if callee.blocks[callee.entry]
            .params
            .iter()
            .any(|&(_, pv)| pv == cv)
        {
            continue;
        }
        // Folded frame pointers alias their reserved base; the rest of
        // the dropped SP machinery gets no def at all.
        if let Some(&base) = fold_base.get(&cv) {
            body.values[nv] = ValueDef::Alias(base);
            continue;
        }
        if fold_drop.contains(&cv) {
            continue;
        }
        let def = match &callee.values[cv] {
            ValueDef::BlockParam(b2, i, ty) => ValueDef::BlockParam(bmap[b2.index()], *i, *ty),
            ValueDef::Operator(op, args, tys) => {
                let na: Vec<WV> = callee.arg_pool[*args]
                    .iter()
                    .map(|a| src[a.index()])
                    .collect();
                let nt: Vec<WT> = callee.type_pool[*tys].iter().copied().collect();
                let nal = if na.is_empty() {
                    Default::default()
                } else {
                    body.arg_pool.from_iter(na.iter().copied())
                };
                let ntl = match nt.len() {
                    0 => Default::default(),
                    1 => body.single_type_list(nt[0]),
                    _ => body.type_pool.from_iter(nt.iter().copied()),
                };
                ValueDef::Operator(op.clone(), nal, ntl)
            }
            ValueDef::PickOutput(v, i, ty) => ValueDef::PickOutput(src[v.index()], *i, *ty),
            ValueDef::Alias(v) => ValueDef::Alias(src[v.index()]),
            d => d.clone(),
        };
        body.values[nv] = def;
        let ob = callee.value_blocks[cv];
        if ob.is_valid() {
            body.value_blocks[nv] = bmap[ob.index()];
        }
        body.source_locs[nv] = callee.source_locs[cv];
    }

    // Split `blk`: prefix stays, tail moves to `after`.
    let tail: Vec<WV> = body.blocks[blk].insts.split_off(pos + 1);
    body.blocks[blk].insts.pop(); // drop the call itself
    body.blocks[after].insts = tail;
    body.blocks[after].terminator =
        std::mem::replace(&mut body.blocks[blk].terminator, Terminator::None);
    for &v in &body.blocks[after].insts {
        body.value_blocks[v] = after;
    }

    // Join block: blockparams carry the callee's results.
    let mut jparams: Vec<WV> = Vec::with_capacity(tys.len());
    for (i, &ty) in tys.iter().enumerate() {
        let v = body.values.push(ValueDef::BlockParam(join, i as u32, ty));
        body.value_blocks[v] = join;
        body.blocks[join].params.push((ty, v));
        jparams.push(v);
    }
    body.blocks[join].terminator = Terminator::Br {
        target: BlockTarget {
            block: after,
            args: Vec::new(),
        },
    };

    // Copy callee blocks: entry insts append to `blk` (its terminator
    // replaces the placeholder set during the split); the rest land in
    // their mapped blocks.
    for cb in callee.blocks.iter() {
        let nb = bmap[cb.index()];
        let insts: Vec<WV> = callee.blocks[cb]
            .insts
            .iter()
            .filter(|v| !fold_drop.contains(v))
            .map(|v| src[v.index()])
            .collect();
        if cb == callee.entry {
            body.blocks[blk].insts.extend(insts);
        } else {
            body.blocks[nb].insts = insts;
            body.blocks[nb].params = callee.blocks[cb]
                .params
                .iter()
                .map(|&(t, v)| (t, src[v.index()]))
                .collect();
            body.blocks[nb].desc = callee.blocks[cb].desc.clone();
        }
        body.blocks[nb].terminator = remap_term(&callee.blocks[cb].terminator, &bmap, &src);
    }

    // Retire the call value: route its uses to the join blockparams.
    match jparams.len() {
        0 => {}
        1 => body.values[call_v] = ValueDef::Alias(jparams[0]),
        _ => {
            for v in body.values.iter() {
                let i = match body.values[v] {
                    ValueDef::PickOutput(s, i, _) if s == call_v => i as usize,
                    _ => continue,
                };
                if i < jparams.len() {
                    body.values[v] = ValueDef::Alias(jparams[i]);
                }
            }
        }
    }
}

/// `if (c) { br m(x) } else { br m(y) }` diamonds whose arms are pure
/// forwarders (no insts, no blockparams) to the same single-param merge
/// become `select`, deleting two blocks and the branch structure â€”
/// LLVM emits `select` for the same shapes. Soundness: x is used at
/// its arm block, so it dominates that block; `a` is a pred of the
/// arm, so x dominates `a` too, and arm blocks define nothing else
/// (no insts, no params â€” a param could still have uses in blocks the
/// arm dominates, which is why param'd arms are rejected). Arm blocks
/// with other preds stay live via those edges and are unaffected.
/// Range folding: `x <u C` / `x >=u C` where `x`'s unsigned maximum is
/// provably below `C` folds to a constant. The common case is a table
/// bounds check `byte <u 256` on an `i32.load8_u` result (LLVM gets the
/// same from u8 range info); folding the compare lets DCE drop the
/// never-taken panic arm and the loop carry.

/// Constant value of `v`, if it is an int-const op.
fn wconst(b: &FunctionBody, v: WV) -> Option<i64> {
    match b.values[b.resolve_alias(v)] {
        ValueDef::Operator(O::I32Const { value }, ..) => Some(value as i32 as i64),
        ValueDef::Operator(O::I64Const { value }, ..) => Some(value as i64),
        _ => None,
    }
}

fn wrange(b: &mut FunctionBody) -> bool {
    // Static unsigned upper bound for an i32 value.
    fn ubound(b: &FunctionBody, v: WV, depth: u32) -> Option<u32> {
        if depth > 8 {
            return None;
        }
        let v = b.resolve_alias(v);
        match b.values[v] {
            ValueDef::Operator(O::I32Const { value }, ..) => Some(value as u32),
            ValueDef::Operator(O::I32Load8U { .. }, ..) => Some(0xff),
            ValueDef::Operator(O::I32Load16U { .. }, ..) => Some(0xffff),
            ValueDef::Operator(O::I32And, aa, _) => {
                let &[x, y, ..] = &b.arg_pool[aa][..] else {
                    return None;
                };
                // x & K <= K for a constant mask K.
                match (wconst(b, x), wconst(b, y)) {
                    (Some(m), _) => {
                        Some((m as u32).min(ubound(b, y, depth + 1).unwrap_or(u32::MAX)))
                    }
                    (_, Some(m)) => {
                        Some((m as u32).min(ubound(b, x, depth + 1).unwrap_or(u32::MAX)))
                    }
                    _ => None,
                }
            }
            ValueDef::Operator(O::I32ShrU, aa, _) => {
                let &[x, y, ..] = &b.arg_pool[aa][..] else {
                    return None;
                };
                // wasm masks the shift count to 5 bits.
                let s = (wconst(b, y)? as u32) & 31;
                Some(ubound(b, x, depth + 1)? >> s)
            }
            ValueDef::Operator(O::I32RemU, aa, _) => {
                let &[_, y, ..] = &b.arg_pool[aa][..] else {
                    return None;
                };
                // x % K < K (K = 0 traps, never produces a value).
                let m = wconst(b, y)? as u32;
                (m != 0).then_some(m - 1)
            }
            ValueDef::Operator(O::Select, aa, _) => {
                let &[x, y, ..] = &b.arg_pool[aa][..] else {
                    return None;
                };
                Some(ubound(b, x, depth + 1)?.max(ubound(b, y, depth + 1)?))
            }
            _ => None,
        }
    }
    let mut patch: Vec<(WBlock, WV, i32)> = Vec::new();
    let mut shls: Vec<(WBlock, WV, WV, u32, bool)> = Vec::new();
    for blk in b.blocks.iter() {
        for &iv in &b.blocks[blk].insts {
            // `x * 2^k` -> `x << k` for a constant power of two. `other`
            // must be the operand that did *not* supply k: when both
            // operands are consts, picking it by `wconst(x).is_some()`
            // shifts the wrong one.
            if !crate::env_set("PLIRON_WASM_NOSHL")
                && let ValueDef::Operator(op @ (O::I32Mul | O::I64Mul), aa, _) = &b.values[iv]
            {
                if let &[x, y, ..] = &b.arg_pool[*aa][..] {
                    let pow2 = |v| -> Option<u32> {
                        let k = wconst(b, v).filter(|k| *k > 0)?;
                        (k as u64)
                            .is_power_of_two()
                            .then(|| (k as u64).trailing_zeros())
                    };
                    let wide = *op == O::I64Mul;
                    if let Some((sh, other)) =
                        pow2(x).map(|s| (s, y)).or_else(|| pow2(y).map(|s| (s, x)))
                        && sh < if wide { 64 } else { 32 }
                    {
                        shls.push((blk, iv, other, sh, wide));
                    }
                }
                continue;
            }
            let fold = match &b.values[iv] {
                ValueDef::Operator(
                    op @ (O::I32LtU | O::I32LeU | O::I32GtU | O::I32GeU | O::I32Eq | O::I32Ne),
                    aa,
                    _,
                ) => {
                    let &[x, y, ..] = &b.arg_pool[*aa][..] else {
                        continue;
                    };
                    let Some(c) = wconst(b, y) else { continue };
                    let c = c as u32;
                    match op {
                        O::I32LtU if c == 0 => Some(0),
                        O::I32GeU if c == 0 => Some(1),
                        O::I32LeU if c == u32::MAX => Some(1),
                        O::I32GtU if c == u32::MAX => Some(0),
                        _ => match ubound(b, x, 0) {
                            Some(u) if u < c => match op {
                                O::I32LtU | O::I32LeU | O::I32Ne => Some(1),
                                O::I32GtU | O::I32GeU | O::I32Eq => Some(0),
                                _ => None,
                            },
                            Some(u) if u <= c => match op {
                                O::I32LeU => Some(1),
                                O::I32GtU => Some(0),
                                _ => None,
                            },
                            _ => None,
                        },
                    }
                }
                _ => None,
            };
            if let Some(k) = fold {
                patch.push((blk, iv, k));
            }
        }
    }
    let mut did = !patch.is_empty() || !shls.is_empty();
    // New ops must land *before* the folded inst's position: a use of iv
    // in the same block can precede block end, and add_op appends.
    let add_at = |b: &mut FunctionBody, blk: WBlock, iv: WV| -> usize {
        b.blocks[blk]
            .insts
            .iter()
            .position(|&i| i == iv)
            .unwrap_or_else(|| b.blocks[blk].insts.len())
    };
    for (blk, iv, k) in patch {
        let pos = add_at(&mut *b, blk, iv);
        let nc = b.add_op(blk, O::I32Const { value: k as u32 }, &[], &[WT::I32]);
        let at = b.blocks[blk].insts.len() - 1;
        b.blocks[blk].insts.remove(at);
        b.blocks[blk].insts.insert(pos, nc);
        b.values[iv] = ValueDef::Alias(nc);
    }
    for (blk, iv, x, k, wide) in shls {
        let pos = add_at(&mut *b, blk, iv);
        let c = b.add_op(
            blk,
            if wide {
                O::I64Const { value: k as u64 }
            } else {
                O::I32Const { value: k }
            },
            &[],
            &[if wide { WT::I64 } else { WT::I32 }],
        );
        let shl = b.add_op(
            blk,
            if wide { O::I64Shl } else { O::I32Shl },
            &[x, c],
            &[if wide { WT::I64 } else { WT::I32 }],
        );
        for (j, nv) in [c, shl].into_iter().enumerate() {
            let at = b.blocks[blk].insts.len() - 1;
            b.blocks[blk].insts.remove(at);
            b.blocks[blk].insts.insert(pos + j, nv);
        }
        b.values[iv] = ValueDef::Alias(shl);
    }
    // Fold CondBr on a constant cond (ours from the patch above, or any
    // that optimize/GVN produced): waffle's own passes leave a literal
    // `i32.const k; if` behind.
    let mut folds: Vec<WBlock> = Vec::new();
    for blk in b.blocks.iter() {
        if let Terminator::CondBr { cond, .. } = &b.blocks[blk].terminator
            && wconst(b, *cond).is_some()
        {
            folds.push(blk);
        }
    }
    if !folds.is_empty() {
        // Skipping a target removes an edge; a param'd block that loses
        // its last pred crashes waffle's const-meet on unreachable
        // blocks. preds can be stale after optimize(), so recompute,
        // then track each dead target's remaining edges as we fold.
        b.recompute_edges();
        let mut left: FxHashMap<WBlock, usize> = FxHashMap::default();
        for blk in folds {
            let Terminator::CondBr {
                cond,
                if_true,
                if_false,
            } = &b.blocks[blk].terminator
            else {
                continue;
            };
            let Some(k) = wconst(b, *cond) else { continue };
            let (keep, dead) = if k != 0 {
                (if_true, if_false)
            } else {
                (if_false, if_true)
            };
            if dead.block != keep.block {
                let rem = left
                    .entry(dead.block)
                    .or_insert(b.blocks[dead.block].preds.len());
                if *rem <= 1 && !b.blocks[dead.block].params.is_empty() {
                    continue;
                }
                *rem = rem.saturating_sub(1);
            }
            b.blocks[blk].terminator = Terminator::Br {
                target: keep.clone(),
            };
            did = true;
        }
    }
    did
}

/// Fold `i32.add(addr, K)`/`i32.sub(addr, K)` address operands into
/// load/store memarg offsets: `*(p + K)` becomes `load offset=K+off`.
/// LLVM does the same in wasm ISel; it differs from the wrap-around
/// u32 add only when the address computation wraps past 4GiB, which a
/// defined program can't produce.
fn memarg_of(op: &O) -> Option<waffle::MemoryArg> {
    Some(*match op {
        O::I32Load { memory }
        | O::I64Load { memory }
        | O::F32Load { memory }
        | O::F64Load { memory }
        | O::I32Load8S { memory }
        | O::I32Load8U { memory }
        | O::I32Load16S { memory }
        | O::I32Load16U { memory }
        | O::I64Load8S { memory }
        | O::I64Load8U { memory }
        | O::I64Load16S { memory }
        | O::I64Load16U { memory }
        | O::I64Load32S { memory }
        | O::I64Load32U { memory }
        | O::I32Store { memory }
        | O::I64Store { memory }
        | O::F32Store { memory }
        | O::F64Store { memory }
        | O::I32Store8 { memory }
        | O::I32Store16 { memory }
        | O::I64Store8 { memory }
        | O::I64Store16 { memory }
        | O::I64Store32 { memory } => memory,
        _ => return None,
    })
}
fn memarg_mut(op: &mut O) -> Option<&mut waffle::MemoryArg> {
    Some(match op {
        O::I32Load { memory }
        | O::I64Load { memory }
        | O::F32Load { memory }
        | O::F64Load { memory }
        | O::I32Load8S { memory }
        | O::I32Load8U { memory }
        | O::I32Load16S { memory }
        | O::I32Load16U { memory }
        | O::I64Load8S { memory }
        | O::I64Load8U { memory }
        | O::I64Load16S { memory }
        | O::I64Load16U { memory }
        | O::I64Load32S { memory }
        | O::I64Load32U { memory }
        | O::I32Store { memory }
        | O::I64Store { memory }
        | O::F32Store { memory }
        | O::F64Store { memory }
        | O::I32Store8 { memory }
        | O::I32Store16 { memory }
        | O::I64Store8 { memory }
        | O::I64Store16 { memory }
        | O::I64Store32 { memory } => memory,
        _ => return None,
    })
}

fn waddr(b: &mut FunctionBody) -> bool {
    // Constant add/sub chains peel one layer per pass.
    let mut did = false;
    for _ in 0..8 {
        if !waddr_once(b) {
            break;
        }
        did = true;
    }
    did
}

fn waddr_once(b: &mut FunctionBody) -> bool {
    let mut did = false;
    for blk in b.blocks.iter() {
        for &iv in &b.blocks[blk].insts {
            let ValueDef::Operator(op, aa, _) = &b.values[iv] else {
                continue;
            };
            let Some(m) = memarg_of(op) else { continue };
            let aa = *aa;
            let Some(&addr0) = b.arg_pool[aa].first() else {
                continue;
            };
            let addr = b.resolve_alias(addr0);
            let delta = match &b.values[addr] {
                ValueDef::Operator(O::I32Add, aa2, _) => {
                    let &[x, y, ..] = &b.arg_pool[*aa2][..] else {
                        continue;
                    };
                    if wconst(b, x).is_some() {
                        (y, wconst(b, x).unwrap())
                    } else if wconst(b, y).is_some() {
                        (x, wconst(b, y).unwrap())
                    } else {
                        continue;
                    }
                }
                ValueDef::Operator(O::I32Sub, aa2, _) => {
                    let &[x, y, ..] = &b.arg_pool[*aa2][..] else {
                        continue;
                    };
                    match wconst(b, y) {
                        Some(k) => (x, -k),
                        None => continue,
                    }
                }
                _ => continue,
            };
            let noff = m.offset as i64 + delta.1;
            if !(0..=u32::MAX as i64).contains(&noff) {
                continue;
            }
            let ValueDef::Operator(op2, ..) = &mut b.values[iv] else {
                continue;
            };
            if let Some(mm) = memarg_mut(op2) {
                mm.offset = noff as u32;
            }
            b.arg_pool[aa][0] = delta.0;
            did = true;
        }
    }
    did
}

fn wsel(b: &mut FunctionBody) -> bool {
    let mut patch: Vec<(WBlock, WV, WV, WV, WT, WBlock)> = Vec::new();
    for a in b.blocks.iter() {
        let (cond, ta, fa) = match &b.blocks[a].terminator {
            Terminator::CondBr {
                cond,
                if_true,
                if_false,
            } => (*cond, if_true, if_false),
            _ => continue,
        };
        let arm = |t: &BlockTarget| -> Option<(WBlock, WV)> {
            let tb = t.block;
            // Arms must be pure forwarders: no insts (side effects) and
            // no blockparams â€” a param can have uses in blocks the arm
            // dominates, and bypassing the arm would drop its def.
            if tb == a || !b.blocks[tb].insts.is_empty() || !b.blocks[tb].params.is_empty() {
                return None;
            }
            match &b.blocks[tb].terminator {
                Terminator::Br { target } if target.args.len() == 1 => {
                    Some((target.block, target.args[0]))
                }
                _ => None,
            }
        };
        let Some((mt, x)) = arm(ta) else { continue };
        let Some((mf, y)) = arm(fa) else { continue };
        if mt != mf {
            continue;
        }
        // Single-arg edges mean the merge has exactly one param; its
        // type drives the (untyped) select, which needs a scalar.
        if b.blocks[mt].params.len() != 1 {
            continue;
        }
        let ty = b.blocks[mt].params[0].0;
        if !matches!(ty, WT::I32 | WT::I64 | WT::F32 | WT::F64) {
            continue;
        }
        patch.push((a, cond, x, y, ty, mt));
    }
    for (a, cond, x, y, ty, m) in &patch {
        // O::Select args are [if-true, if-false, cond].
        let sel = b.add_op(*a, O::Select, &[*x, *y, *cond], &[*ty]);
        b.blocks[*a].terminator = Terminator::Br {
            target: BlockTarget {
                block: *m,
                args: vec![sel],
            },
        };
    }
    !patch.is_empty()
}

/// Conservative waffle-IR inliner, run after all bodies are lowered
/// and before the emit remap renumbers `Call` targets. Two rules:
/// always inline a callee at most `TINY` insts, and inline a
/// single-callsite callee at most `SOLO` insts. Once a callee's last
/// call edge is gone the linker's mark-sweep GC drops its standalone
/// body, which is where the code-size win lands.
/// Does `b` have irreducible control flow (an RPO retreating edge whose
/// target doesn't dominate its source — waffle's `Reducifier` test)?
/// `succs` must be current (`fixup_edges`).
fn wirreducible(b: &FunctionBody) -> bool {
    let cfg = waffle::cfg::CFGInfo::new(b);
    for (rpo, &block) in cfg.rpo.entries() {
        for &succ in &b.blocks[block].succs {
            let Some(sr) = cfg.rpo_pos[succ] else { continue };
            if sr.index() <= rpo.index() && !cfg.dominates(succ, block) {
                return true;
            }
        }
    }
    false
}

/// Memoized `wirreducible` of `funcs[i]` (see `PLIRON_WASM_NOIRRED`).
fn irred_at(funcs: &[FDecl], cache: &mut [Option<bool>], i: usize) -> bool {
    *cache[i].get_or_insert_with(|| funcs[i].body.as_ref().is_some_and(wirreducible))
}

/// Strongly connected components of the direct call graph over `funcs`
/// (`call` operands only; body-less imports are singleton nodes). Returns
/// one component id per function index. Iterative Tarjan, linear in
/// bodies + edges.
fn call_sccs(funcs: &[FDecl]) -> Vec<u32> {
    let n = funcs.len();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, f) in funcs.iter().enumerate() {
        let mut seen: FxHashSet<usize> = FxHashSet::default();
        for v in f.body.iter().flat_map(|b| b.values.values()) {
            if let ValueDef::Operator(O::Call { function_index }, ..) = v {
                let t = function_index.index();
                if seen.insert(t) {
                    succ[i].push(t);
                }
            }
        }
    }
    const UNVISITED: u32 = u32::MAX;
    let mut index = vec![UNVISITED; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut comp = vec![UNVISITED; n];
    let mut stack: Vec<usize> = Vec::new();
    let (mut next, mut ncomp) = (0u32, 0u32);
    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        // (node, next successor position)
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut pos)) = work.last_mut() {
            if *pos < succ[v].len() {
                let w = succ[v][*pos];
                *pos += 1;
                if index[w] == UNVISITED {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                work.pop();
                if let Some(&(p, _)) = work.last() {
                    low[p] = low[p].min(low[v]);
                }
                if low[v] == index[v] {
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp[w] = ncomp;
                        if w == v {
                            break;
                        }
                    }
                    ncomp += 1;
                }
            }
        }
    }
    comp
}

fn winline(funcs: &mut [FDecl], verbose: bool, sp: waffle::Global) {
    // `PLIRON_WASM_TINY=N` / `PLIRON_WASM_SOLO=N` (defaults 12 / 100 via
    // `pliron_cfg::DEFAULT_VALUES` for SOLO): always-inline and
    // single-callsite caps. SOLO 300→100: regex-wasm −39 KiB, hashmap/
    // tokenize runtime neutral (2026-10-28-overnight.md).
    let tiny: usize = crate::env_get("PLIRON_WASM_TINY")
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    let solo: usize = crate::env_get("PLIRON_WASM_SOLO")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    // Mid-size (<=64) multi-callsite inlining measured ~1% runtime for
    // +2% size on wbig â€” wasm calls really are cheap and the splice's
    // optimizer doesn't recover the rest. Not a rule.
    // Loop-body callsites execute the call every iteration, so a
    // moderately-sized callee is worth duplicating there — a
    // frame-spilled callee (5-field searcher/RandomState round-tripped
    // through linear memory per call) costs ~10 memory ops/iter.
    // 400 covers the observed `next_match` (~345) and `hash_one` (~246)
    // loop callees; `caller_cap` bounds the per-caller growth.
    let hot_cap = crate::env_get("PLIRON_WASM_HOT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    // `#[inline]` (LLVM `inlinehint`) callees: LLVM's inliner gives the
    // attribute a large budget bonus â€” mirror that with a raised cap.
    // Off by default: on a real std workload (regex wbig) it added
    // ~30% code size for no measurable runtime gain â€” wasm calls are
    // cheap and our post-splice optimizer doesn't exploit the merge
    // the way LLVM's does. `PLIRON_WASM_HINT=n` re-enables.
    let hint_cap = crate::env_get("PLIRON_WASM_HINT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // Even `#[inline(always)]` gets a hard ceiling: a pathological
    // multi-K-inst callee spliced at every site makes the per-caller
    // optimize() rounds dominate compile time and triples module size.
    const ALWAYS_CAP: usize = 300;
    const ROUNDS: usize = 8;
    let body_size = |b: &FunctionBody| -> usize { b.blocks.values().map(|d| d.insts.len()).sum() };
    // Total spliced insts per caller across rounds: the per-round
    // `cur_size` check alone can't bound the cascade — post-optimize
    // shrinkage can drop it back under the cap every round, letting a
    // caller grow ~cap per round for ROUNDS rounds.
    let mut grown = vec![0usize; funcs.len()];
    let mut inlined = 0usize;
    // `PLIRON_WASM_HOTLEAF=N` (default off; `1` means 2): the loop rule
    // only for callees with at most N calls of their own — the IR
    // inliner's `PLIRON_INLINE_LOOPCALLS` leaf rule. A callee that
    // carries many calls just moves that call traffic into the loop, and
    // every splice exposes them as fresh loop callsites: regex-syntax's
    // `parser::parse` went from 135 to 1,930 insts (LLVM: 760 B) and
    // `Ast::drop` from 737 to 2,070 under the rule; the hashmap kernels'
    // `hash_one` (SipHash, a leaf) is what the rule exists for.
    let hot_leaf: Option<u32> = match crate::env_get("PLIRON_WASM_HOTLEAF") {
        None | Some("0") => None,
        Some("1") => Some(2),
        Some(v) => v.parse().ok(),
    };
    // `PLIRON_WASM_HOTCAP=N` (default `caller_cap`): insts a caller may
    // absorb through the loop rule alone. The hot callers above all ended
    // at `caller_cap` (2,048) — the shared cascade cap is the only thing
    // stopping the loop rule, so it fills it.
    let hot_cap_total: usize = crate::env_get("PLIRON_WASM_HOTCAP")
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let mut hot_grown = vec![0usize; funcs.len()];
    // `PLIRON_WASM_NOREC=1`: the loop rule skips callees in the caller's
    // call-graph SCC. In a recursive mesh (drop glue of a recursive
    // enum, a recursive-descent visitor) every member is a loop callsite
    // of another member, so each round splices bodies that the previous
    // round already grew; every member then fills `caller_cap`. In
    // regex-syntax's wasm object ~100 drop-glue bodies reached 17 KB each
    // (LLVM's: 0.4-1.3 KB) and `Extractor::extract` 114 KB (LLVM 6 KB).
    // LLVM's inliner inlines bottom-up once per site and never into a
    // recursive cycle. A callee outside the SCC (hashbrown's `hash_one`
    // in a probe loop) is unaffected.
    let norec = pliron_cfg::env_is("PLIRON_WASM_NOREC", "1");
    // `PLIRON_WASM_NOIRRED=1`: never splice a callee with irreducible
    // control flow, nor into a caller that has it. waffle's backend
    // makes such a CFG reducible by context-sensitive block duplication
    // ("EXPONENTIAL BLOWUP POTENTIAL", backend/reducify.rs); a few
    // CLIF-optimized bodies (regex-syntax's `Seq::cross_reverse`,
    // `escape_into`) come out of lowering irreducible and compile at
    // 20-40 B per waffle inst instead of ~8. Spliced into a caller's
    // loops, the duplication compounds: `Extractor::extract_repetition`
    // compiled to 160 KB from 2,174 insts (74 B/inst), and the eight
    // such bodies were 458 KB of regex-syntax's 1.19 MB object.
    let noirred = pliron_cfg::env_is("PLIRON_WASM_NOIRRED", "1");
    for round in 0..ROUNDS {
        // Per-round irreducibility cache: a body only changes as a
        // caller, and a caller never splices after its own check.
        let mut irred: Vec<Option<bool>> = vec![None; funcs.len()];
        // callsite count per callee; ref.func keeps the standalone
        // body alive, so inlining one only duplicates code.
        let mut calls = vec![0u32; funcs.len()];
        let mut refs = vec![0u32; funcs.len()];
        // Calls *inside* each body (`PLIRON_WASM_HOTLEAF`'s leaf test).
        let mut ncalls = vec![0u32; funcs.len()];
        for (fi, f) in funcs.iter().enumerate() {
            for v in f.body.iter().flat_map(|b| b.values.values()) {
                match v {
                    ValueDef::Operator(O::Call { function_index }, ..) => {
                        calls[function_index.index()] += 1;
                        ncalls[fi] += 1;
                    }
                    ValueDef::Operator(O::RefFunc { func_index }, ..) => {
                        refs[func_index.index()] += 1;
                    }
                    _ => {}
                }
            }
        }
        let scc = if norec { call_sccs(funcs) } else { Vec::new() };
        let mut did = false;
        for ci in 0..funcs.len() {
            if funcs[ci].body.is_none() {
                continue;
            }
            // Fold empty-arm select diamonds (also runs on callers
            // with no inlinable sites).
            {
                let b = funcs[ci].body.as_mut().unwrap();
                if wsel(b) {
                    fixup_edges(b);
                    check_targets(b, &funcs[ci].name);
                    b.optimize(&waffle::OptOptions::default());
                }
            }
            // Loop-member blocks of this caller, for the hot rule.
            // Built lazily: CFGInfo+wloops is O(body), and callers whose
            // sites all qualify under the cheap rules never need it.
            let mut inloop: Option<FxHashSet<WBlock>> = None;
            let mut is_loop = |funcs: &[FDecl], ci: usize, blk: WBlock| -> bool {
                let s = inloop.get_or_insert_with(|| {
                    let b = funcs[ci].body.as_ref().unwrap();
                    let cfg = waffle::cfg::CFGInfo::new(b);
                    let mut s: FxHashSet<WBlock> = FxHashSet::default();
                    for (_, body) in wloops(b, &cfg) {
                        s.extend(body.iter().copied());
                    }
                    s
                });
                s.contains(&blk)
            };
            // Cascade cap: each round's splices expose fresh callsites
            // inside the copies, so small rules can replicate a big
            // callee body tens of times (core's `quicksort` hit 344k
            // wat-lines / ~0.6 MB of one function). Past this budget a
            // caller stays a caller. `cur_size` tracks growth across
            // splices instead of recounting the body per site.
            let caller_cap = crate::env_get("PLIRON_WASM_INLINE_CAP")
                .and_then(|v| v.parse().ok())
                .unwrap_or(2048);
            let mut cur_size = body_size(funcs[ci].body.as_ref().unwrap());
            if cur_size > caller_cap || grown[ci] > caller_cap {
                continue;
            }
            if noirred && irred_at(funcs, &mut irred, ci) {
                continue;
            }
            // One scan per caller: collect all eligible call values.
            // Positions are resolved per-site at splice time (the tail
            // split keeps every other call's identity valid).
            // (call value, callee, admitted by the loop rule alone)
            let sites: Vec<(WV, usize, bool)> = {
                let b = funcs[ci].body.as_ref().unwrap();
                let mut sites = Vec::new();
                for blk in b.blocks.iter() {
                    for &iv in &b.blocks[blk].insts {
                        if let ValueDef::Operator(O::Call { function_index }, args, _) =
                            &b.values[iv]
                        {
                            let fi = function_index.index();
                            // no self-inlining; callee must exist
                            if fi == ci {
                                continue;
                            }
                            // `#[inline(never)]` is a hard no â€”
                            // LLVM refuses to inline it too.
                            if funcs[fi].no_inline {
                                continue;
                            }
                            if noirred && irred_at(funcs, &mut irred, fi) {
                                continue;
                            }
                            let Some(cb) = funcs[fi].body.as_ref() else {
                                continue;
                            };
                            // Entry with preds can't merge (its
                            // blockparams would need the back-edge args
                            // too); locals beyond params would need
                            // remapping â€” our bodies have none; arity
                            // must match (the args become the params).
                            if !cb.blocks[cb.entry].preds.is_empty()
                                || cb.locals.len() != cb.n_params
                                || cb.blocks[cb.entry].params.len() != b.arg_pool[*args].len()
                            {
                                continue;
                            }
                            let n = body_size(cb);
                            let k = calls[fi] as usize;
                            // GC-completion: inlining all K sites
                            // copies n*(K-1) insts but frees the
                            // callee body + call ops + func entry
                            // (~10B); roughly break-even inside this
                            // budget in inst units.
                            let gc_done = refs[fi] == 0 && k >= 2 && n * (k - 1) <= 5 + 2 * k;
                            // Hint sites only in round 0: unrestricted
                            // re-scanning cascades `#[inline]` chains
                            // inside fresh splice copies and blows up
                            // compile time; LLVM inlines bottom-up
                            // once per site instead.
                            let other = (funcs[fi].always && n <= ALWAYS_CAP)
                                || (round == 0 && funcs[fi].hint && n <= hint_cap)
                                || n <= tiny
                                || (k == 1 && refs[fi] == 0 && n <= solo)
                                || gc_done;
                            if other {
                                sites.push((iv, fi, false));
                            } else if n <= hot_cap
                                && (!norec || scc[fi] != scc[ci])
                                && hot_leaf.is_none_or(|m| ncalls[fi] <= m)
                                && is_loop(funcs, ci, blk)
                            {
                                sites.push((iv, fi, true));
                            }
                        }
                    }
                }
                sites
            };
            // PLIRON: `PLIRON_WASM_INLINE_LIMIT=N` performs at most N
            // splices total — bisects a miscompile to one splice.
            let inline_limit = crate::env_get("PLIRON_WASM_INLINE_LIMIT")
                .and_then(|v| v.parse::<usize>().ok());
            // PLIRON: `PLIRON_WASM_INLINE_SKIP=<substr>` never inlines
            // into callers whose name contains it — bisects by caller.
            let inline_skip = crate::env_get("PLIRON_WASM_INLINE_SKIP")
                .is_some_and(|s| s.split(',').any(|p| funcs[ci].name.contains(p)));
            if !sites.is_empty() && !inline_skip {
                did = true;
                let irred_log = crate::env_get("PLIRON_WASM_INLINE_LOG").is_some();
                let mut was_irred =
                    irred_log && wirreducible(funcs[ci].body.as_ref().unwrap());
                for &(iv, fi, via_hot) in &sites {
                    if cur_size > caller_cap
                        || grown[ci] > caller_cap
                        || inline_limit.is_some_and(|l| inlined >= l)
                    {
                        break;
                    }
                    if via_hot && hot_grown[ci] + body_size(funcs[fi].body.as_ref().unwrap()) > hot_cap_total {
                        continue;
                    }
                    let callee = funcs[fi].body.clone().unwrap();
                    check_targets(&callee, &funcs[fi].name);
                    let b = funcs[ci].body.as_mut().unwrap();
                    let blk = b.value_blocks[iv];
                    if !blk.is_valid() {
                        continue;
                    }
                    if let Some(pos) = b.blocks[blk].insts.iter().position(|&i| i == iv) {
                        // PLIRON: index each splice for INLINE_LIMIT bisects.
                        if crate::env_get("PLIRON_WASM_INLINE_LOG").is_some() {
                            eprintln!(
                                "inl {} {} <- {}",
                                inlined, funcs[ci].name, funcs[fi].name
                            );
                        }
                        inline_site(b, blk, pos, iv, &callee, sp);
                        check_targets(b, &funcs[fi].name);
                        let n = body_size(&callee);
                        cur_size += n;
                        grown[ci] += n;
                        if via_hot {
                            hot_grown[ci] += n;
                        }
                        inlined += 1;
                        if irred_log && !was_irred {
                            fixup_edges(b);
                            if wirreducible(b) {
                                was_irred = true;
                                eprintln!(
                                    "irred {} <- {} (callee irred {})",
                                    funcs[ci].name,
                                    funcs[fi].name,
                                    wirreducible(&callee)
                                );
                            }
                        }
                    }
                }
                let b = funcs[ci].body.as_mut().unwrap();
                fixup_edges(b);
                check_targets(b, &funcs[ci].name);
                b.optimize(&waffle::OptOptions::default());
                if irred_log && !was_irred && wirreducible(b) {
                    eprintln!("irred-opt {}", funcs[ci].name);
                }
                // The body changed: re-test it when it is a callee later.
                irred[ci] = None;
            }
        }
        if !did {
            break;
        }
    }
    if verbose {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::wasm","pliron-wasm: inlined {inlined} callsites")
        };
    }
}

/// `PLIRON_WASM_GC=1`: module-level dead-function elimination after
/// `winline` (LLVM's GlobalDCE for the object). Roots are every defined
/// function that is not `Local`, every function whose address escapes
/// (`address_taken`: `Sym` consts in global initializers — the
/// `pliron.link` data relocations — and `llvm.addressof` ops) and every
/// `GOT.func.<sym>` global this module imported. Edges are `call` and
/// `ref.func` operands of live bodies. Unmarked bodies are dropped
/// (`body = None` with no caller: never imported, never emitted) and
/// their names returned so `link_section` leaves them out too. Mirrors
/// the roots/edges of `pliron-wasm-ld`'s GC, so this only removes what
/// the final link would have removed anyway.
fn wgc(o: &mut Obj, taken: &FxHashSet<String>, st: &State, name: &str) -> FxHashSet<String> {
    let n = o.funcs.len();
    let mut live = vec![false; n];
    let mut wl: Vec<usize> = Vec::new();
    let mut defined = 0usize;
    for (i, f) in o.funcs.iter().enumerate() {
        if f.body.is_none() {
            continue;
        }
        defined += 1;
        let local = st
            .funcs
            .get(&f.name)
            .is_some_and(|fi| fi.linkage == Linkage::Local);
        if !local || taken.contains(&f.name) || o.got.contains_key(&(true, f.name.clone())) {
            live[i] = true;
            wl.push(i);
        }
    }
    while let Some(i) = wl.pop() {
        let Some(b) = o.funcs[i].body.as_ref() else {
            continue;
        };
        for v in b.values.values() {
            let t = match v {
                ValueDef::Operator(O::Call { function_index }, ..) => function_index.index(),
                ValueDef::Operator(O::RefFunc { func_index }, ..) => func_index.index(),
                _ => continue,
            };
            if !live[t] {
                live[t] = true;
                wl.push(t);
            }
        }
    }
    let mut dropped = FxHashSet::default();
    for (i, f) in o.funcs.iter_mut().enumerate() {
        if f.body.is_some() && !live[i] {
            f.body = None;
            dropped.insert(f.name.clone());
        }
    }
    if pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::stats", "wasmgc {name}: dropped {} of {defined} fns", dropped.len());
    }
    dropped
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
/// `#[used(linker)]`: keep the object alive through link-time GC even
/// when nothing references it.
const USED: u8 = 8;

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
/// `.init_array` entry (constructor pointers the linker calls from
/// `__wasm_call_ctors`), bit 3 `#[used(linker)]` (GC root).
fn link_funcs(ctx: &Context, st: &State) -> Vec<(String, u8)> {
    st.funcs
        .iter()
        .filter(|(n, f)| has_body(ctx, f.op) && !st.dead_fns.contains(*n))
        .map(|(n, f)| {
            let l = if f.linkage == Linkage::Import {
                Linkage::Export
            } else {
                f.linkage
            };
            (n.clone(), link_flags(l))
        })
        .collect()
}

/// The data half of `pliron.link` (count first), see `link_funcs`.
fn link_data(ctx: &Context, st: &State) -> Vec<u8> {
    let mut out = Vec::new();
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
        out.push(link_flags(l) | if ctor { INIT_ARRAY } else { 0 } | if g.used {
            USED
        } else {
            0
        });
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

/// When no stack slots were ever allocated, the save/align/restore dance
/// around `__stack_pointer` is pure overhead. Drop every `global.set` of it
/// (entry adjust, return/unwind/rethrow restores) plus the now-dead
/// fp/mask producer chain rooted at `roots` â€” the callee's own SP is never
/// touched, so nothing needs restoring. A stray consumer of the dead
/// values cancels the whole rewrite (defensive).
fn elide_sp_frame(b: &mut FunctionBody, sp: Global, roots: [WV; 4]) {
    let mut dead: FxHashSet<WV> = roots.into_iter().collect();
    let mut wl: Vec<WV> = dead.iter().copied().collect();
    while let Some(v) = wl.pop() {
        if let ValueDef::Operator(_, args, _) = &b.values[v] {
            for i in 0..b.arg_pool[*args].len() {
                let a = b.arg_pool[*args][i];
                if dead.insert(a) {
                    wl.push(a);
                }
            }
        }
    }
    let mut kill = dead.clone();
    for blk in b.blocks.iter() {
        for &i in &b.blocks[blk].insts {
            if let ValueDef::Operator(O::GlobalSet { global_index: g }, ..) = &b.values[i]
                && *g == sp
            {
                kill.insert(i);
            }
        }
    }
    let mut bad = false;
    for blk in b.blocks.iter() {
        for &i in &b.blocks[blk].insts {
            if kill.contains(&i) {
                continue;
            }
            b.values[i].visit_uses(&b.arg_pool, |u| bad |= dead.contains(&u));
        }
        b.blocks[blk]
            .terminator
            .visit_uses(|u| bad |= dead.contains(&u));
    }
    if !bad {
        for blk in b.blocks.iter() {
            b.blocks[blk].insts.retain(|i| !kill.contains(i));
        }
    }
}

// ---------- CLIF -> waffle emit ----
//
// `lower_to_wasm_clif` runs FnLower + the pliron-clif-opt suite (same as the
// native backend) via `lower::lower_clif_wasm`, then translates each
// function's optimized CLIF to waffle SSA here â€” the pipeline's
// CSE/loop/idominate work (and, with `simd` on, loopvec->v128) in front of
// the waffle backend.
mod clif_emit {
    use super::*;

    // FnLower runs with an explicit wasm32 `TargetFrontendConfig` (see
    // `lower_clif_wasm`): pointers are i32 throughout the bookkeeping CLIF, so
    // `func_addr`/`symbol_value`/`stack_addr`/`call` are internally consistent.
    // CX maps each CLIF value to one wasm value (two for i128/f128 leaves,
    // v128 for vectors), coercing where reps differ.

    use cranelift_codegen::ir::{
        self, BlockArg, ExternalName, GlobalValueData, Inst as CInst, Opcode, StackSlot as CSS,
    };

    /// wasm `v128` op for a CLIF vector lane type; unlisted lanes panic.
    macro_rules! vl {
    ($lane:expr; $($lt:ident => $op:ident),+ $(,)?) => {
        match $lane {
            $(clt::$lt => O::$op,)+
            l => panic!("unsupported vector lane {l}"),
        }
    };
}

    fn cwtys(t: ClType, out: &mut Vec<WT>) {
        match t {
            clt::I128 | clt::F128 => {
                out.push(WT::I64);
                out.push(WT::I64);
            }
            t if t.is_vector() => out.push(WT::V128),
            clt::F16 => out.push(WT::I32),
            t => out.push(wty(t)),
        }
    }

    /// CLIF signature -> wasm SignatureData (i128/f128 split into i64 pairs).
    fn cwsig(s: &ir::Signature) -> SignatureData {
        let flat = |ps: &[ir::AbiParam]| -> Vec<WT> {
            let mut v = Vec::new();
            for p in ps {
                cwtys(p.value_type, &mut v);
            }
            v
        };
        SignatureData {
            params: flat(&s.params),
            returns: flat(&s.returns),
        }
    }

    /// Post-pipeline CLIF -> waffle `FunctionBody`.
    struct CX<'o, 'a> {
        o: &'o mut Obj,
        /// FuncId::index() -> symbol (the bookkeeping module's declarations).
        fn_names: &'a [String],
        /// DataId::index() -> symbol.
        data_names: &'a [String],
        b: FunctionBody,
        cur: WBlock,
        /// CLIF value -> wasm values (2 for i128/f128 leaves).
        vals: FxHashMap<ir::Value, Vals>,
        wbs: FxHashMap<ir::Block, WBlock>,
        /// Frame layout: CLIF stack slot -> byte offset under `fp`.
        slots: FxHashMap<CSS, u32>,
        // Frame machinery: entry computes fp = (sp - frame) & mask,
        // patched once `frame` is final.
        sp0: WV,
        fp: WV,
        frame_c: WV,
        mask_c: WV,
        frame: u64,
        frame_align: u64,
        name: String,
    }

    impl<'o, 'a> CX<'o, 'a> {
        fn new(
            o: &'o mut Obj,
            sig: Signature,
            fn_names: &'a [String],
            data_names: &'a [String],
        ) -> Self {
            let b = FunctionBody::new(&o.m, sig);
            let mut cx = CX {
                o,
                fn_names,
                data_names,
                b,
                cur: WBlock::invalid(),
                vals: FxHashMap::default(),
                wbs: FxHashMap::default(),
                slots: FxHashMap::default(),
                sp0: WV::invalid(),
                fp: WV::invalid(),
                frame_c: WV::invalid(),
                mask_c: WV::invalid(),
                frame: 0,
                frame_align: 16,
                name: String::new(),
            };
            let cur = cx.b.entry;
            cx.cur = cur;
            let sp = cx.o.sp;
            cx.sp0 = cx.op(O::GlobalGet { global_index: sp }, &[], WT::I32);
            cx.frame_c = cx.i32c(0);
            let t = cx.op(O::I32Sub, &[cx.sp0, cx.frame_c], WT::I32);
            cx.mask_c = cx.i32c(!15);
            cx.fp = cx.op(O::I32And, &[t, cx.mask_c], WT::I32);
            cx.op0(O::GlobalSet { global_index: sp }, &[cx.fp]);
            cx
        }

        fn op(&mut self, o: O, args: &[WV], ty: WT) -> WV {
            self.b.add_op(self.cur, o, args, &[ty])
        }

        fn op0(&mut self, o: O, args: &[WV]) {
            self.b.add_op(self.cur, o, args, &[]);
        }

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

        fn ma(&self, off: u64, bytes: u64) -> MemoryArg {
            MemoryArg {
                align: bytes.trailing_zeros(),
                offset: off as u32,
                memory: self.o.mem,
            }
        }

        /// Coerce a wasm rep to `t`'s container. Only the pointer-width seams
        /// need this: a 64-bit CLIF carrier holding a 32-bit address truncates
        /// (wrap), a 32-bit leaf passed to a 64-bit-typed use zero-extends.
        fn cvt(&mut self, v: WV, t: WT) -> WV {
            let f = self.ty_of(v);
            if f == t {
                return v;
            }
            match (f, t) {
                (WT::I64, WT::I32) => self.op(O::I32WrapI64, &[v], WT::I32),
                (WT::I32, WT::I64) => self.op(O::I64ExtendI32U, &[v], WT::I64),
                // f64-carried pointers: the address bits ride the f64's
                // pattern — reinterpret, then wrap/extend (`load.i32`
                // on `pow`'s exponent-carried table pointer).
                (WT::F64, WT::I64) => self.op(O::I64ReinterpretF64, &[v], WT::I64),
                (WT::I64, WT::F64) => self.op(O::F64ReinterpretI64, &[v], WT::F64),
                (WT::F32, WT::I32) => self.op(O::I32ReinterpretF32, &[v], WT::I32),
                (WT::I32, WT::F32) => self.op(O::F32ReinterpretI32, &[v], WT::F32),
                (WT::F64, WT::I32) => {
                    let w = self.op(O::I64ReinterpretF64, &[v], WT::I64);
                    self.op(O::I32WrapI64, &[w], WT::I32)
                }
                (WT::I32, WT::F64) => {
                    let w = self.op(O::I64ExtendI32U, &[v], WT::I64);
                    self.op(O::F64ReinterpretI64, &[w], WT::F64)
                }
                (WT::F32, WT::I64) => {
                    let w = self.op(O::I32ReinterpretF32, &[v], WT::I32);
                    self.op(O::I64ExtendI32U, &[w], WT::I64)
                }
                (WT::I64, WT::F32) => {
                    let w = self.op(O::I32WrapI64, &[v], WT::I32);
                    self.op(O::F32ReinterpretI32, &[w], WT::F32)
                }
                _ => panic!("cvt {f:?} -> {t:?}"),
            }
        }

        /// The CLIF value's wasm values.
        fn get(&self, v: ir::Value) -> &Vals {
            self.vals
                .get(&v)
                .unwrap_or_else(|| panic!("undef clif value {v}"))
        }

        fn get1(&self, v: ir::Value) -> WV {
            let vs = self.get(v);
            if vs.len() != 1 {
                panic!("get1 {v} has {} vals (fn {})", vs.len(), self.name);
            }
            vs[0]
        }

        fn set(&mut self, v: ir::Value, wv: WV) {
            self.vals.insert(v, smallvec![wv]);
        }

        fn setv(&mut self, v: ir::Value, vs: Vals) {
            self.vals.insert(v, vs);
        }

        /// Result wasm types of a CLIF inst/declared type.
        fn flat(&self, t: ClType) -> Vec<WT> {
            let mut v = Vec::new();
            cwtys(t, &mut v);
            v
        }

        fn vty(&self, f: &ir::Function, v: ir::Value) -> ClType {
            f.dfg.value_type(v)
        }

        /// Mask bits above `w` out of a canonical (zero-extended) narrow int.
        fn norm(&mut self, v: WV, w: u32) -> WV {
            match w {
                32 | 64 => v,
                _ if self.normed(v, w) => v,
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

        /// `v`'s i32 value provably fits `w` bits, so `norm` is a no-op.
        /// Covers the shapes narrow CLIF arithmetic leaves behind: icmp
        /// results (0/1), `and`s against masks or of 0/1 compares, selects
        /// whose arms already fit, and narrow unsigned loads.
        fn normed(&self, v: WV, w: u32) -> bool {
            let mask = if w >= 32 { u32::MAX } else { (1u32 << w) - 1 };
            let ValueDef::Operator(op, args, _) = &self.b.values[v] else {
                return false;
            };
            let arg = |i: usize| self.b.arg_pool[*args][i];
            match op {
                // wasm compares produce i32 0/1.
                O::I32Eq
                | O::I32Ne
                | O::I32LtS
                | O::I32LtU
                | O::I32GtS
                | O::I32GtU
                | O::I32LeS
                | O::I32LeU
                | O::I32GeS
                | O::I32GeU
                | O::I32Eqz
                | O::I64Eq
                | O::I64Ne
                | O::I64LtS
                | O::I64LtU
                | O::I64GtS
                | O::I64GtU
                | O::I64LeS
                | O::I64LeU
                | O::I64GeS
                | O::I64GeU
                | O::I64Eqz
                | O::F32Eq
                | O::F32Ne
                | O::F32Lt
                | O::F32Le
                | O::F32Gt
                | O::F32Ge
                | O::F64Eq
                | O::F64Ne
                | O::F64Lt
                | O::F64Le
                | O::F64Gt
                | O::F64Ge => w >= 1,
                O::I32Const { value } => *value <= mask,
                O::I64Const { value } => *value <= mask as u64,
                // a & b <= min(a, b): one in-range operand is enough.
                O::I32And => self.normed(arg(0), w) || self.normed(arg(1), w),
                O::I32ShrU => match &self.b.values[arg(1)] {
                    ValueDef::Operator(O::I32Const { value: s }, ..) => {
                        (u32::MAX >> s.min(&31)) <= mask
                    }
                    _ => false,
                },
                O::Select | O::TypedSelect { .. } => {
                    self.normed(arg(0), w) && self.normed(arg(1), w)
                }
                O::I32Load8U { .. } => w >= 8,
                O::I32Load16U { .. } => w >= 16,
                _ => false,
            }
        }

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

        /// Reserve `size`/`align` bytes in the frame; returns the i32 offset.
        fn frame_slot(&mut self, size: u64, align: u64) -> u32 {
            let align = align.max(1);
            self.frame_align = self.frame_align.max(align);
            let off = crate::types::align_to(self.frame, align);
            self.frame = off + size;
            off as u32
        }

        /// Address `fp + off` as an i32 pointer value.
        fn addr(&mut self, off: i64) -> WV {
            if off == 0 {
                self.fp
            } else {
                let c = self.i32c(off as u32);
                self.op(O::I32Add, &[self.fp, c], WT::I32)
            }
        }

        /// `w` bit unsigned/signed helpers on canonical narrow reps.
        fn binop(&mut self, t: ClType, o32: O, o64: O, a: WV, b: WV) -> WV {
            match wty(t) {
                WT::I64 => self.op(o64, &[a, b], WT::I64),
                _ => self.op(o32, &[a, b], WT::I32),
            }
        }

        /// `binop` for *comparisons*: operand picks the i32/i64 op, but the
        /// result is always an i32 flag regardless of operand width.
        fn cbinop(&mut self, t: ClType, o32: O, o64: O, a: WV, b: WV) -> WV {
            match wty(t) {
                WT::I64 => self.op(o64, &[a, b], WT::I32),
                _ => self.op(o32, &[a, b], WT::I32),
            }
        }

        /// Add `inst`'s results -> `vals` binding (split i128 into lo/hi).
        fn bind(&mut self, f: &ir::Function, inst: CInst, vs: Vals) {
            let rs = f.dfg.inst_results(inst);
            match rs.len() {
                0 => assert!(vs.is_empty()),
                1 => {
                    let t = self.vty(f, rs[0]);
                    if self.flat(t).len() == 1 {
                        assert_eq!(vs.len(), 1);
                        self.set(rs[0], vs[0]);
                    } else {
                        assert_eq!(vs.len(), 2);
                        self.setv(rs[0], vs);
                    }
                }
                n => {
                    assert_eq!(vs.len(), n);
                    for (r, v) in rs.iter().zip(vs) {
                        self.set(*r, v);
                    }
                }
            }
        }

        /// `symbol_value`/`func_addr`-style symbol resolution to a wasm value.
        fn sym_got(&mut self, func: bool, name: &str, off: i64) -> WV {
            let g = self.o.got_at(func, name);
            let base = self.op(O::GlobalGet { global_index: g }, &[], WT::I32);
            // Pointer values are i32 (wasm CLIF pointer width).
            if off == 0 {
                base
            } else {
                let c = self.i32c(off as u32);
                self.op(O::I32Add, &[base, c], WT::I32)
            }
        }

        /// The callee's symbol name (for `fidx` lookup and ABI checks).
        fn func_name(&self, fr: ir::FuncRef, f: &ir::Function) -> String {
            let efd = &f.dfg.ext_funcs[fr];
            match &efd.name {
                ExternalName::User(u) => {
                    let (_, idx) = self.user_name(f, *u);
                    self.fn_names
                        .get(idx as usize)
                        .cloned()
                        .unwrap_or_else(|| panic!("bad funcref idx{idx}"))
                }
                ExternalName::LibCall(lc) => lc.to_string(),
                n => panic!("func name {n:?}"),
            }
        }

        fn func_idx(&mut self, fr: ir::FuncRef, f: &ir::Function) -> Func {
            let efd = &f.dfg.ext_funcs[fr];
            let name = self.func_name(fr, f);
            match self.o.fidx.get(&name) {
                Some(&i) => Func::new(i),
                None => {
                    // Not one of this module's functions (a libcall stub like
                    // `__udivti3`): import it under the call's signature.
                    let sd = cwsig(&f.dfg.signatures[efd.signature]);
                    Func::new(self.o.libfunc(&name, sd))
                }
            }
        }

        /// Waffle signature for a `call_indirect`'s `SigRef`.
        fn sig_of(&mut self, f: &ir::Function, sr: ir::SigRef) -> Signature {
            self.o.sig(cwsig(&f.dfg.signatures[sr]))
        }

        fn block_target(&self, f: &ir::Function, bc: &ir::BlockCall) -> BlockTarget {
            let block = *self
                .wbs
                .get(&bc.block(&f.dfg.value_lists))
                .unwrap_or_else(|| panic!("unmapped block"));
            let args: Vec<WV> = bc
                .args(&f.dfg.value_lists)
                .map(|a| match a {
                    BlockArg::Value(v) => v,
                    _ => panic!("try_call block args on wasm"),
                })
                .flat_map(|v| self.get(v).iter().copied())
                .collect();
            BlockTarget { block, args }
        }

        /// 32-bit memory base: truncate the CLIF carrier (i32 leaf or the
        /// i64-typed ptr ops) down to i32.
        fn base32(&mut self, v: ir::Value) -> WV {
            self.cvt(self.get1(v), WT::I32)
        }

        /// Branch/trap/select condition: `cvt` wraps an i64 carrier to its
        /// low 32 bits, which loses values nonzero only in the high half â€”
        /// `(x & 0x8000..) != 0` sign tests wrapped to 0 and flipped
        /// `x < 0` branches (wasm `if`/`br_if`/`select` read i32). Emit
        /// `v != 0` for i64 operands instead.
        fn cond(&mut self, v: WV) -> WV {
            match self.ty_of(v) {
                WT::I64 => {
                    let z = self.i64c(0);
                    self.op(O::I64Ne, &[v, z], WT::I32)
                }
                // `brif`/`if`/`select` directly on a float (libm's
                // `if x` tests) — emit `v != 0.0`; `cvt` has no
                // float-to-i32 path.
                WT::F32 => {
                    let z = self.op(O::F32Const { value: 0 }, &[], WT::F32);
                    self.op(O::F32Ne, &[v, z], WT::I32)
                }
                WT::F64 => {
                    let z = self.op(O::F64Const { value: 0 }, &[], WT::F64);
                    self.op(O::F64Ne, &[v, z], WT::I32)
                }
                _ => self.cvt(v, WT::I32),
            }
        }

        fn load(&mut self, t: ClType, p: WV, off: u64) -> WV {
            let memory = self.ma(off, t.bytes() as u64);
            let (o, ty) = match t {
                clt::I8 => (O::I32Load8U { memory }, WT::I32),
                clt::I16 => (O::I32Load16U { memory }, WT::I32),
                clt::I32 => (O::I32Load { memory }, WT::I32),
                clt::I64 => (O::I64Load { memory }, WT::I64),
                clt::F32 => (O::F32Load { memory }, WT::F32),
                clt::F64 => (O::F64Load { memory }, WT::F64),
                t if t.is_vector() && t.bits() == 128 => (O::V128Load { memory }, WT::V128),
                t if t.is_vector() && t.bits() == 64 => (O::V128Load64Zero { memory }, WT::V128),
                t if t.is_vector() && t.bits() == 32 => (O::V128Load32Zero { memory }, WT::V128),
                t => panic!("load {t}"),
            };
            self.op(o, &[p], ty)
        }

        fn store(&mut self, t: ClType, v: WV, p: WV, off: u64) {
            let memory = self.ma(off, t.bytes() as u64);
            // Sub-128 vectors have no narrow store; write lane 0 of the
            // i64x2/i32x4 view (upper lanes are don't-care).
            if t.is_vector() && t.bits() < 128 {
                let (ext, st, wt) = if t.bits() == 64 {
                    (
                        O::I64x2ExtractLane { lane: 0 },
                        O::I64Store { memory },
                        WT::I64,
                    )
                } else {
                    (
                        O::I32x4ExtractLane { lane: 0 },
                        O::I32Store { memory },
                        WT::I32,
                    )
                };
                let x = self.op(ext, &[v], wt);
                self.op0(st, &[p, x]);
                return;
            }
            let o = match t {
                clt::I8 => O::I32Store8 { memory },
                clt::I16 => O::I32Store16 { memory },
                clt::I32 => O::I32Store { memory },
                clt::I64 => O::I64Store { memory },
                clt::F32 => O::F32Store { memory },
                clt::F64 => O::F64Store { memory },
                t if t.is_vector() && t.bits() == 128 => O::V128Store { memory },
                t => panic!("store {t}"),
            };
            self.op0(o, &[p, v]);
        }

        /// i128 `a` as (lo, hi).
        fn pair(&self, v: ir::Value) -> (WV, WV) {
            let vs = self.get(v);
            assert_eq!(vs.len(), 2);
            (vs[0], vs[1])
        }

        /// A signed-compare normalization: xor sign bit for unsigned-domain
        /// comparison of i128 hi words.
        fn icmp128(&mut self, cond: ir::condcodes::IntCC, a: ir::Value, b: ir::Value) -> WV {
            use ir::condcodes::IntCC as CC;
            let (al, ah) = self.pair(a);
            let (bl, bh) = self.pair(b);
            match cond {
                CC::Equal => {
                    let l = self.op(O::I64Eq, &[al, bl], WT::I32);
                    let h = self.op(O::I64Eq, &[ah, bh], WT::I32);
                    self.op(O::I32And, &[l, h], WT::I32)
                }
                CC::NotEqual => {
                    let l = self.op(O::I64Ne, &[al, bl], WT::I32);
                    let h = self.op(O::I64Ne, &[ah, bh], WT::I32);
                    self.op(O::I32Or, &[l, h], WT::I32)
                }
                _ => {
                    // (ah, al) < (bh, bl): hi ordered compare, then lo when equal.
                    let sgn = |cx: &mut Self, x: WV| -> WV {
                        let m = cx.i64c(1 << 63);
                        cx.op(O::I64Xor, &[x, m], WT::I64)
                    };
                    let signed = matches!(
                        cond,
                        CC::SignedLessThan
                            | CC::SignedGreaterThanOrEqual
                            | CC::SignedLessThanOrEqual
                            | CC::SignedGreaterThan
                    );
                    let (ah2, bh2) = if signed {
                        (sgn(self, ah), sgn(self, bh))
                    } else {
                        (ah, bh)
                    };
                    let (hlt, hgt) = (O::I64LtU, O::I64GtU);
                    let hge = O::I64GeU;
                    let (llt, lle, lgt, lge) = (O::I64LtU, O::I64LeU, O::I64GtU, O::I64GeU);
                    let heq = self.op(O::I64Eq, &[ah2, bh2], WT::I32);
                    match cond {
                        CC::UnsignedLessThan | CC::SignedLessThan => {
                            let h = self.op(hlt, &[ah2, bh2], WT::I32);
                            let l = self.op(llt, &[al, bl], WT::I32);
                            let e = self.op(O::I32And, &[heq, l], WT::I32);
                            self.op(O::I32Or, &[h, e], WT::I32)
                        }
                        CC::UnsignedLessThanOrEqual | CC::SignedLessThanOrEqual => {
                            let h = self.op(hlt, &[ah2, bh2], WT::I32);
                            let l = self.op(lle, &[al, bl], WT::I32);
                            let e = self.op(O::I32And, &[heq, l], WT::I32);
                            self.op(O::I32Or, &[h, e], WT::I32)
                        }
                        CC::UnsignedGreaterThan | CC::SignedGreaterThan => {
                            let h = self.op(hgt, &[ah2, bh2], WT::I32);
                            let l = self.op(lgt, &[al, bl], WT::I32);
                            let e = self.op(O::I32And, &[heq, l], WT::I32);
                            self.op(O::I32Or, &[h, e], WT::I32)
                        }
                        _ => {
                            let h = self.op(hge, &[ah2, bh2], WT::I32);
                            let l = self.op(lge, &[al, bl], WT::I32);
                            let e = self.op(O::I32And, &[heq, l], WT::I32);
                            self.op(O::I32Or, &[h, e], WT::I32)
                        }
                    }
                }
            }
        }
    }

    impl<'o, 'a> CX<'o, 'a> {
        /// Entry block waffle params -> CLIF function params (flattened).
        fn map_entry(&mut self, f: &ir::Function) {
            let entry = f.layout.entry_block().expect("no entry");
            let wb = self.b.entry;
            self.wbs.insert(entry, wb);
            let mut wi = 0usize;
            for &p in f.dfg.block_params(entry) {
                let n = self.flat(self.vty(f, p)).len();
                let vs: Vals = (0..n).map(|i| self.b.blocks[wb].params[wi + i].1).collect();
                wi += n;
                self.vals.insert(p, vs);
            }
            debug_assert_eq!(wi, self.b.blocks[wb].params.len());
        }

        /// Frame slot address as i64 CLIF carrier.
        fn slot_addr(&mut self, f: &ir::Function, ss: CSS, off: i64) -> WV {
            let o = if let Some(&o) = self.slots.get(&ss) {
                o
            } else {
                let sd = &f.sized_stack_slots[ss];
                let o = self.frame_slot(sd.size as u64, 1u64 << sd.align_shift);
                self.slots.insert(ss, o);
                o
            };
            self.addr(o as i64 + off)
        }

        /// A `code` trap: emit `unreachable` in a side block and continue.
        fn cond_trap(&mut self, cond: WV) {
            let trapb = self.b.add_block();
            let cont = self.b.add_block();
            let cur = self.cur;
            self.b.set_terminator(
                cur,
                Terminator::CondBr {
                    cond,
                    if_true: BlockTarget {
                        block: trapb,
                        args: vec![],
                    },
                    if_false: BlockTarget {
                        block: cont,
                        args: vec![],
                    },
                },
            );
            self.b.set_terminator(trapb, Terminator::Unreachable);
            self.cur = cont;
        }

        /// Integer binary/imm op dispatch. `t` is the CLIF controlling type.
        fn ibin(&mut self, op: Opcode, t: ClType, a: WV, b: WV) -> WV {
            use Opcode as P;
            let w = t.bits();
            match op {
                P::Iadd => {
                    let r = self.binop(t, O::I32Add, O::I64Add, a, b);
                    if w < 32 { self.norm(r, w) } else { r }
                }
                P::Isub => {
                    let r = self.binop(t, O::I32Sub, O::I64Sub, a, b);
                    if w < 32 { self.norm(r, w) } else { r }
                }
                P::Imul => {
                    let r = self.binop(t, O::I32Mul, O::I64Mul, a, b);
                    if w < 32 { self.norm(r, w) } else { r }
                }
                P::Udiv => self.binop(t, O::I32DivU, O::I64DivU, a, b),
                P::Sdiv => {
                    if w < 32 {
                        let (a, b) = (self.sext(a, w), self.sext(b, w));
                        let r = self.op(O::I32DivS, &[a, b], WT::I32);
                        self.norm(r, w)
                    } else {
                        self.binop(t, O::I32DivS, O::I64DivS, a, b)
                    }
                }
                P::Urem => self.binop(t, O::I32RemU, O::I64RemU, a, b),
                P::Srem => {
                    if w < 32 {
                        let (a, b) = (self.sext(a, w), self.sext(b, w));
                        let r = self.op(O::I32RemS, &[a, b], WT::I32);
                        self.norm(r, w)
                    } else {
                        self.binop(t, O::I32RemS, O::I64RemS, a, b)
                    }
                }
                P::Band => self.binop(t, O::I32And, O::I64And, a, b),
                P::Bor => self.binop(t, O::I32Or, O::I64Or, a, b),
                P::Bxor => self.binop(t, O::I32Xor, O::I64Xor, a, b),
                P::Ishl => {
                    // CLIF masks the amount by the value's bit width; wasm masks
                    // mod the container width. Narrow types must mask themselves.
                    // The amount may carry a wider CLIF type than the value â€”
                    // wasm shift operands must match the value's container.
                    let b = self.cvt(b, wty(t));
                    let b = if w < 32 { self.norm(b, w.ilog2()) } else { b };
                    let r = self.binop(t, O::I32Shl, O::I64Shl, a, b);
                    if w < 32 { self.norm(r, w) } else { r }
                }
                P::Ushr => {
                    let a = if w < 32 { self.norm(a, w) } else { a };
                    let b = self.cvt(b, wty(t));
                    let b = if w < 32 { self.norm(b, w.ilog2()) } else { b };
                    self.binop(t, O::I32ShrU, O::I64ShrU, a, b)
                }
                P::Sshr => {
                    let a = if w < 32 { self.sext(a, w) } else { a };
                    let b = self.cvt(b, wty(t));
                    let b = if w < 32 { self.norm(b, w.ilog2()) } else { b };
                    let r = self.binop(t, O::I32ShrS, O::I64ShrS, a, b);
                    if w < 32 { self.norm(r, w) } else { r }
                }
                P::Rotl => self.rot(t, a, b, true),
                P::Rotr => self.rot(t, a, b, false),
                P::Umin => self.minmax(t, a, b, O::I32LtU, O::I64LtU),
                P::Umax => self.minmax(t, a, b, O::I32GtU, O::I64GtU),
                P::Smin => self.minmax_s(t, a, b, true),
                P::Smax => self.minmax_s(t, a, b, false),
                P::Umulhi => self.umulhi(t, a, b, false),
                P::Smulhi => self.umulhi(t, a, b, true),
                _ => panic!("ibin {op}"),
            }
        }

        fn bnot(&mut self, t: ClType, v: WV) -> WV {
            let w = t.bits();
            match wty(t) {
                WT::I64 => {
                    let m = self.i64c(u64::MAX);
                    self.op(O::I64Xor, &[v, m], WT::I64)
                }
                _ => {
                    let m = self.i32c(if w < 32 {
                        ((1u64 << w) - 1) as u32
                    } else {
                        u32::MAX
                    });
                    self.op(O::I32Xor, &[v, m], WT::I32)
                }
            }
        }

        fn rot(&mut self, t: ClType, a: WV, b: WV, left: bool) -> WV {
            let w = t.bits();
            let b = self.cvt(b, wty(t));
            match wty(t) {
                WT::I64 => {
                    let c63 = self.i64c(63);
                    let amt = self.op(O::I64And, &[b, c63], WT::I64);
                    self.op(
                        if left { O::I64Rotl } else { O::I64Rotr },
                        &[a, amt],
                        WT::I64,
                    )
                }
                _ => {
                    let (o1, o2) = if left {
                        (O::I32Rotl, O::I32Rotr)
                    } else {
                        (O::I32Rotr, O::I32Rotl)
                    };
                    let _ = o2;
                    if w == 32 {
                        return self.op(o1, &[a, b], WT::I32);
                    }
                    // Narrow rotate: canonical form, amount mod w.
                    let a = self.norm(a, w);
                    let m = w.ilog2();
                    let amt = self.norm(b, m);
                    let l = self.op(
                        if left { O::I32Shl } else { O::I32ShrU },
                        &[a, amt],
                        WT::I32,
                    );
                    let wm = self.i32c(w);
                    let r32 = self.op(O::I32Sub, &[wm, amt], WT::I32);
                    let r = self.op(
                        if left { O::I32ShrU } else { O::I32Shl },
                        &[a, r32],
                        WT::I32,
                    );
                    let v = self.op(O::I32Or, &[l, r], WT::I32);
                    self.norm(v, w)
                }
            }
        }

        fn minmax(&mut self, t: ClType, a: WV, b: WV, c32: O, c64: O) -> WV {
            let c = self.cbinop(t, c32, c64, a, b);
            self.sel(c, a, b)
        }

        fn minmax_s(&mut self, t: ClType, a: WV, b: WV, min: bool) -> WV {
            let w = t.bits();
            let (a2, b2) = if w < 32 {
                (self.sext(a, w), self.sext(b, w))
            } else {
                (a, b)
            };
            let c = self.cbinop(
                t,
                if min { O::I32LtS } else { O::I32GtS },
                if min { O::I64LtS } else { O::I64GtS },
                a2,
                b2,
            );
            self.sel(c, a, b)
        }

        fn sel(&mut self, c: WV, x: WV, y: WV) -> WV {
            let t = self.ty_of(x);
            match t {
                WT::V128 | WT::FuncRef => self.op(O::TypedSelect { ty: t }, &[x, y, c], t),
                _ => self.op(O::Select, &[x, y, c], t),
            }
        }

        /// `a * b` high half; unsigned `umulhi` or signed `smulhi`.
        fn umulhi(&mut self, t: ClType, a: WV, b: WV, signed: bool) -> WV {
            let w = t.bits();
            match wty(t) {
                WT::I32 if w <= 32 => {
                    // i64 mul of (sign|zero)-extended halves, take bits [w, 2w).
                    let (ea, eb) = if signed {
                        let (a, b) = (self.sext(a, w), self.sext(b, w));
                        (
                            self.op(O::I64ExtendI32S, &[a], WT::I64),
                            self.op(O::I64ExtendI32S, &[b], WT::I64),
                        )
                    } else {
                        (
                            self.op(O::I64ExtendI32U, &[a], WT::I64),
                            self.op(O::I64ExtendI32U, &[b], WT::I64),
                        )
                    };
                    let p = self.op(O::I64Mul, &[ea, eb], WT::I64);
                    let wc = self.i64c(w as u64);
                    let sh = self.op(O::I64ShrU, &[p, wc], WT::I64);
                    let r = self.op(O::I32WrapI64, &[sh], WT::I32);
                    if w < 32 { self.norm(r, w) } else { r }
                }
                WT::I64 => {
                    // Standard mulh expansion on 32-bit halves.
                    let m32 = self.i64c(0xffff_ffff);
                    let c32 = self.i64c(32);
                    let lo = |cx: &mut Self, x: WV| cx.op(O::I64And, &[x, m32], WT::I64);
                    let hi = |cx: &mut Self, x: WV| cx.op(O::I64ShrU, &[x, c32], WT::I64);
                    let (al, ah) = (lo(self, a), hi(self, a));
                    let (bl, bh) = (lo(self, b), hi(self, b));
                    let p_ll = self.op(O::I64Mul, &[al, bl], WT::I64);
                    let p_lh = self.op(O::I64Mul, &[al, bh], WT::I64);
                    let p_hl = self.op(O::I64Mul, &[ah, bl], WT::I64);
                    let p_hh = self.op(O::I64Mul, &[ah, bh], WT::I64);
                    // hi = p_hh + hi(p_lh) + hi(p_hl) + carry(lo(p_lh)+lo(p_hl)+hi(p_ll))
                    let (lh_lo, hl_lo) = (lo(self, p_lh), lo(self, p_hl));
                    let lsum = self.op(O::I64Add, &[lh_lo, hl_lo], WT::I64);
                    let ll_hi = hi(self, p_ll);
                    let lsum = self.op(O::I64Add, &[lsum, ll_hi], WT::I64);
                    let carry = self.op(O::I64ShrU, &[lsum, c32], WT::I64);
                    let (lh_hi, hl_hi) = (hi(self, p_lh), hi(self, p_hl));
                    let hsum = self.op(O::I64Add, &[lh_hi, hl_hi], WT::I64);
                    let res = self.op(O::I64Add, &[hsum, carry], WT::I64);
                    let res = self.op(O::I64Add, &[p_hh, res], WT::I64);
                    if !signed {
                        return res;
                    }
                    // signed: subtract a when b<0, b when a<0 (hi fixup).
                    let c63 = self.i64c(63);
                    let sa = self.op(O::I64ShrS, &[a, c63], WT::I64);
                    let sb = self.op(O::I64ShrS, &[b, c63], WT::I64);
                    let fix1 = self.op(O::I64And, &[sa, b], WT::I64);
                    let fix2 = self.op(O::I64And, &[sb, a], WT::I64);
                    let fix = self.op(O::I64Add, &[fix1, fix2], WT::I64);
                    self.op(O::I64Sub, &[res, fix], WT::I64)
                }
                _ => panic!("umulhi {t}"),
            }
        }

        /// `iadd_cout`/`isub_bout` style carry: returns (sum, carry-out).
        fn addc(&mut self, t: ClType, a: WV, b: WV, sub: bool) -> (WV, WV) {
            let sum = self.binop(
                t,
                if sub { O::I32Sub } else { O::I32Add },
                if sub { O::I64Sub } else { O::I64Add },
                a,
                b,
            );
            let sum = if t.bits() < 32 {
                self.norm(sum, t.bits())
            } else {
                sum
            };
            let c = self.cbinop(
                t,
                if sub { O::I32GtU } else { O::I32LtU },
                if sub { O::I64GtU } else { O::I64LtU },
                sum,
                a,
            );
            (sum, c)
        }

        fn translate(mut self, f: &mut ir::Function) -> FunctionBody {
            // The pipeline leaves `Value` aliases (`v24 -> v0`); resolve them
            // so `get` never sees one.
            f.dfg.resolve_all_aliases();
            // Map all blocks first so forward branches resolve. Entry maps to
            // the FunctionBody's entry block (its params already match the sig).
            self.map_entry(f);
            for b in f.layout.blocks() {
                if self.wbs.contains_key(&b) {
                    continue;
                }
                self.wbs.insert(b, self.b.add_block());
            }
            // Block params for non-entry blocks (flattened like the sig).
            for b in f.layout.blocks() {
                if Some(b) == f.layout.entry_block() {
                    continue;
                }
                let wb = self.wbs[&b];
                for &p in f.dfg.block_params(b) {
                    for t in self.flat(self.vty(f, p)) {
                        let v = self.b.add_blockparam(wb, t);
                        self.vals.entry(p).or_insert_with(Vals::new).push(v);
                    }
                }
            }
            // Layout order can put a dominated (e.g. cold) block before its
            // def block â€” walk in RPO so defs precede uses.
            let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(f);
            let mut seen = FxHashSet::default();
            let mut post = Vec::new();
            let mut stack = vec![(f.layout.entry_block().unwrap(), false)];
            while let Some((b, done)) = stack.pop() {
                if done {
                    post.push(b);
                    continue;
                }
                if !seen.insert(b) {
                    continue;
                }
                stack.push((b, true));
                for s in cfg.succ_iter(b) {
                    if !seen.contains(&s) {
                        stack.push((s, false));
                    }
                }
            }
            post.reverse();
            // Unreachable blocks aren't in RPO; emit them in layout order.
            for b in f.layout.blocks() {
                if !seen.contains(&b) {
                    post.push(b);
                }
            }
            for b in post {
                self.cur = self.wbs[&b];
                for i in f.layout.block_insts(b) {
                    self.inst(f, i);
                }
            }
            self.finish(f)
        }

        fn finish(mut self, _f: &ir::Function) -> FunctionBody {
            // Patch frame placeholders (see CX::new).
            let frame = crate::types::align_to(self.frame, self.frame_align) as u32;
            let mask = !(self.frame_align as u32 - 1);
            let patch = vec![(self.frame_c, frame), (self.mask_c, mask)];
            for (v, c) in patch {
                if let ValueDef::Operator(op, ..) = &mut self.b.values[v] {
                    *op = O::I32Const { value: c };
                }
            }
            if self.frame == 0 {
                elide_sp_frame(
                    &mut self.b,
                    self.o.sp,
                    [self.sp0, self.fp, self.frame_c, self.mask_c],
                );
            }
            wrange(&mut self.b);
            waddr(&mut self.b);
            check_targets(&self.b, &self.name);
            self.b.optimize(&waffle::OptOptions::default());
            if crate::env_get("PLIRON_WASM_DUMP")
                .map(|f| f.is_empty() || self.name.contains(&f))
                .unwrap_or(false)
            {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::dump",
                        "==== wasm body {} ====\n{}",
                        self.name,
                        self.b.display("  ", None)
                    )
                };
            }
            self.b
        }
    }

    impl<'o, 'a> CX<'o, 'a> {
        /// One CLIF instruction. `cur` is the waffle block.
        fn inst(&mut self, f: &ir::Function, i: CInst) {
            use Opcode as P;
            let data = f.dfg.insts[i];
            let opc = data.opcode();
            match data {
                ir::InstructionData::UnaryImm {
                    opcode: P::Iconst,
                    imm,
                } => {
                    let t = f.dfg.value_type(f.dfg.inst_results(i)[0]);
                    let v = match wty(t) {
                        WT::I32 => self.i32c(imm.bits() as u32),
                        WT::I64 => self.i64c(imm.bits() as u64),
                        _ => panic!("iconst {t}"),
                    };
                    let v = if t.bits() < 32 {
                        self.norm(v, t.bits())
                    } else {
                        v
                    };
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::UnaryIeee32 { imm, .. } => {
                    let v = self.op(O::F32Const { value: imm.bits() }, &[], WT::F32);
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::UnaryIeee64 { imm, .. } => {
                    let v = self.op(O::F64Const { value: imm.bits() }, &[], WT::F64);
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::Binary { opcode, args } => {
                    let t = self.vty(f, args[0]);
                    if opcode == P::Iconcat {
                        let (lo, hi) = (self.get1(args[0]), self.get1(args[1]));
                        self.setv(f.dfg.inst_results(i)[0], smallvec![lo, hi]);
                        return;
                    }
                    if t == clt::I128 || t == clt::F128 {
                        self.i128_bin(opcode, f, i, args[0], args[1]);
                        return;
                    }
                    if t.is_vector() {
                        self.vbin(opcode, t, f, i, args);
                        return;
                    }
                    match opcode {
                        P::Fadd | P::Fsub | P::Fmul | P::Fdiv | P::Fmin | P::Fmax => {
                            self.fbin(opcode, t, f, i, args);
                        }
                        P::Fcopysign => {
                            let a = self.get1(args[0]);
                            let b = self.get1(args[1]);
                            let (o, ty) = if wty(t) == WT::F64 {
                                (O::F64Copysign, WT::F64)
                            } else {
                                (O::F32Copysign, WT::F32)
                            };
                            let v = self.op(o, &[a, b], ty);
                            self.set(f.dfg.inst_results(i)[0], v);
                        }
                        P::UaddOverflow
                        | P::SaddOverflow
                        | P::UsubOverflow
                        | P::SsubOverflow
                        | P::UmulOverflow
                        | P::SmulOverflow => {
                            let vs =
                                self.overflow(opcode, t, self.get1(args[0]), self.get1(args[1]));
                            self.bind(f, i, vs);
                        }
                        P::UaddSat | P::SaddSat | P::UsubSat | P::SsubSat => {
                            let v = self.sat_bin(opcode, t, self.get1(args[0]), self.get1(args[1]));
                            self.bind(f, i, smallvec![v]);
                        }
                        _ => {
                            let a = self.get1(args[0]);
                            let b = self.get1(args[1]);
                            let v = self.ibin(opcode, t, a, b);
                            self.bind(f, i, smallvec![v]);
                        }
                    }
                }
                ir::InstructionData::BinaryImm8 {
                    opcode: P::Extractlane,
                    arg,
                    imm,
                } => {
                    let t = self.vty(f, arg);
                    let v = self.extract_lane(t, arg, imm.into());
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::IntCompare { cond, args, .. } => {
                    let t = self.vty(f, args[0]);
                    if t.is_vector() {
                        let v = self.vicmp(cond, t, self.get1(args[0]), self.get1(args[1]));
                        self.set(f.dfg.inst_results(i)[0], v);
                        return;
                    }
                    let v = if t == clt::I128 {
                        self.icmp128(cond, args[0], args[1])
                    } else {
                        self.icmp(cond, t, self.get1(args[0]), self.get1(args[1]))
                    };
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::FloatCompare { cond, args, .. } => {
                    let t = self.vty(f, args[0]);
                    let v = if t.is_vector() {
                        self.vfcmp(cond, t, self.get1(args[0]), self.get1(args[1]))
                    } else {
                        self.fcmp(cond, t, self.get1(args[0]), self.get1(args[1]))
                    };
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::Ternary { opcode, args } => match opcode {
                    P::Bitselect => {
                        // (x & c) | (y & ~c)
                        let t = self.vty(f, args[0]);
                        let (c, x, y) =
                            (self.get1(args[0]), self.get1(args[1]), self.get1(args[2]));
                        let v = if t.is_vector() {
                            // wasm bitselect: (x & c) | (y & ~c); operand order
                            // is (x, y, c).
                            self.op(O::V128Bitselect, &[x, y, c], WT::V128)
                        } else {
                            let l = self.binop(t, O::I32And, O::I64And, x, c);
                            let nc = self.bnot(t, c);
                            let r = self.binop(t, O::I32And, O::I64And, y, nc);
                            self.binop(t, O::I32Or, O::I64Or, l, r)
                        };
                        self.bind(f, i, smallvec![v]);
                    }
                    P::Select | P::SelectSpectreGuard => {
                        // select c, x, y
                        let rt = self.vty(f, f.dfg.inst_results(i)[0]);
                        if rt.is_vector() {
                            // select c, x, y on vectors: c holds per-lane
                            // booleans (0/1) - turn into an all-ones mask and
                            // bitselect. CLIF select has no bits-flipping
                            // semantics; lane is truthy iff != 0.
                            let ct = self.vty(f, args[0]);
                            let c = self.get1(args[0]);
                            let zero = self.vsplat_zero();
                            let m = self.vicmp(ir::condcodes::IntCC::NotEqual, ct, c, zero);
                            let x = self.get1(args[1]);
                            let y = self.get1(args[2]);
                            let v = self.op(O::V128Bitselect, &[x, y, m], WT::V128);
                            self.set(f.dfg.inst_results(i)[0], v);
                            return;
                        }
                        let flat = self.flat(rt);
                        let c = self.cond(self.get1(args[0]));
                        if flat.len() == 1 {
                            let x = self.cvt(self.get1(args[1]), flat[0]);
                            let y = self.cvt(self.get1(args[2]), flat[0]);
                            let v = self.sel(c, x, y);
                            self.set(f.dfg.inst_results(i)[0], v);
                        } else {
                            let (x, y) = (self.get(args[1]).clone(), self.get(args[2]).clone());
                            let vs: Vals = x
                                .iter()
                                .zip(y.iter())
                                .map(|(&xv, &yv)| self.sel(c, xv, yv))
                                .collect();
                            self.setv(f.dfg.inst_results(i)[0], vs);
                        }
                    }
                    P::Fma => {
                        let t = self.vty(f, args[0]);
                        let (a, b, c) =
                            (self.get1(args[0]), self.get1(args[1]), self.get1(args[2]));
                        let name = if wty(t) == WT::F64 { "fma" } else { "fmaf" };
                        let vs = self.libcall1(name, &[t], &[t], &[a, b, c]);
                        self.set(f.dfg.inst_results(i)[0], vs[0]);
                    }
                    P::SaddOverflowCin
                    | P::UaddOverflowCin
                    | P::SsubOverflowBin
                    | P::UsubOverflowBin => {
                        let t = self.vty(f, args[0]);
                        let vs = self.overflow_in(
                            opcode,
                            t,
                            self.get1(args[0]),
                            self.get1(args[1]),
                            self.get1(args[2]),
                        );
                        self.bind(f, i, vs);
                    }
                    _ => panic!("ternary {opcode}"),
                },
                ir::InstructionData::Unary { opcode, arg } => {
                    let t = self.vty(f, arg);
                    let rt = self.vty(f, f.dfg.inst_results(i)[0]);
                    if t.is_vector() || rt.is_vector() {
                        self.vun(opcode, t, rt, f, i, arg);
                        return;
                    }
                    if (t == clt::I128 || t == clt::F128)
                        && matches!(
                            opcode,
                            P::Bnot
                                | P::Ineg
                                | P::Iabs
                                | P::Clz
                                | P::Ctz
                                | P::CtzNonzero
                                | P::Popcnt
                                | P::Bswap
                                | P::Bitrev
                                | P::Cls
                                | P::Bitcast
                        )
                    {
                        self.i128_un(opcode, t, f, i, arg);
                        return;
                    }
                    match opcode {
                        P::Bnot => {
                            let v = self.bnot(t, self.get1(arg));
                            self.bind(f, i, smallvec![v]);
                        }
                        P::Ineg => {
                            let z = self.leaf_const(t, 0);
                            let v = self.binop(t, O::I32Sub, O::I64Sub, z, self.get1(arg));
                            let v = if t.bits() < 32 {
                                self.norm(v, t.bits())
                            } else {
                                v
                            };
                            self.bind(f, i, smallvec![v]);
                        }
                        P::Iabs => {
                            let a = self.get1(arg);
                            let m = self.leaf_const(t, (t.bits() - 1) as u64);
                            let s = self.binop(t, O::I32ShrS, O::I64ShrS, a, m);
                            let x = self.binop(t, O::I32Xor, O::I64Xor, a, s);
                            let v = self.binop(t, O::I32Sub, O::I64Sub, x, s);
                            let v = if t.bits() < 32 {
                                self.norm(v, t.bits())
                            } else {
                                v
                            };
                            self.bind(f, i, smallvec![v]);
                        }
                        P::Uextend => {
                            let v = self.extend(f, arg, rt, false);
                            self.bind(f, i, v);
                        }
                        P::Sextend => {
                            let v = self.extend(f, arg, rt, true);
                            self.bind(f, i, v);
                        }
                        P::Ireduce => {
                            // i128 -> i64 keep low half; i64 -> i32 wrap; i32 ->
                            // narrow keeps canonical (already masked) rep.
                            let vs: Vals = self.get(arg).clone();
                            let rt_ws = self.flat(rt);
                            let out = if rt_ws.len() == 1 {
                                let v = self.cvt(vs[0], rt_ws[0]);
                                smallvec![if rt.bits() < 32 {
                                    self.norm(v, rt.bits())
                                } else {
                                    v
                                }]
                            } else {
                                Vals::from_vec(vs.iter().take(2).copied().collect())
                            };
                            self.bind(f, i, out);
                        }
                        P::Isplit => {
                            // (lo, hi) split of an i128 already held as a pair.
                            let vs = self.get(arg).clone();
                            let rs = f.dfg.inst_results(i);
                            self.set(rs[0], vs[0]);
                            self.set(rs[1], vs[1]);
                        }
                        P::BlackBox => {
                            // Identity marker.
                            let vs = self.get(arg).clone();
                            let rs = f.dfg.inst_results(i);
                            for (r, &v) in rs.iter().zip(vs.iter()) {
                                self.set(*r, v);
                            }
                        }
                        P::Bitcast => {
                            let a = self.get1(arg);
                            let v = self.reinterp(a, t, rt);
                            self.bind(f, i, smallvec![v]);
                        }
                        P::Fpromote | P::Fdemote => {
                            let a = self.get1(arg);
                            let o = match (wty(t), wty(rt)) {
                                (WT::F32, WT::F64) => O::F64PromoteF32,
                                (WT::F64, WT::F32) => O::F32DemoteF64,
                                _ => panic!("{opcode} {t}->{rt}"),
                            };
                            let v = self.op(o, &[a], wty(rt));
                            self.set(f.dfg.inst_results(i)[0], v);
                        }
                        P::FcvtToUint | P::FcvtToSint | P::FcvtToUintSat | P::FcvtToSintSat => {
                            let v = self.fcvt_to_int(opcode, t, rt, self.get1(arg));
                            self.bind(f, i, v);
                        }
                        P::FcvtFromUint | P::FcvtFromSint => {
                            let v = self.fcvt_from_int(opcode, t, rt, arg);
                            self.bind(f, i, v);
                        }
                        P::Sqrt
                        | P::Ceil
                        | P::Floor
                        | P::Trunc
                        | P::Nearest
                        | P::Fabs
                        | P::Fneg => {
                            let a = self.get1(arg);
                            let wt = wty(t);
                            let o = match (opcode, wt) {
                                (P::Sqrt, WT::F32) => O::F32Sqrt,
                                (P::Sqrt, WT::F64) => O::F64Sqrt,
                                (P::Ceil, WT::F32) => O::F32Ceil,
                                (P::Ceil, WT::F64) => O::F64Ceil,
                                (P::Floor, WT::F32) => O::F32Floor,
                                (P::Floor, WT::F64) => O::F64Floor,
                                (P::Trunc, WT::F32) => O::F32Trunc,
                                (P::Trunc, WT::F64) => O::F64Trunc,
                                (P::Nearest, WT::F32) => O::F32Nearest,
                                (P::Nearest, WT::F64) => O::F64Nearest,
                                (P::Fabs, WT::F32) => O::F32Abs,
                                (P::Fabs, WT::F64) => O::F64Abs,
                                (P::Fneg, WT::F32) => O::F32Neg,
                                (P::Fneg, WT::F64) => O::F64Neg,
                                _ => panic!("{opcode} {t}"),
                            };
                            let v = self.op(o, &[a], wt);
                            self.set(f.dfg.inst_results(i)[0], v);
                        }
                        P::Clz
                        | P::Ctz
                        | P::CtzNonzero
                        | P::Popcnt
                        | P::Bswap
                        | P::Bitrev
                        | P::Cls => {
                            let a = self.get1(arg);
                            let v = self.bitop(opcode, t, a);
                            self.bind(f, i, smallvec![v]);
                        }
                        P::Bmask => {
                            // scalar: nonzero -> all 1s, zero -> 0.
                            let a = self.get1(arg);
                            let aw = wty(t);
                            let z = if aw == WT::I64 {
                                self.i64c(0)
                            } else {
                                self.i32c(0)
                            };
                            let ne = self.op(
                                if aw == WT::I64 { O::I64Ne } else { O::I32Ne },
                                &[a, z],
                                WT::I32,
                            );
                            let z32 = self.i32c(0);
                            let m = self.op(O::I32Sub, &[z32, ne], WT::I32);
                            let v = if wty(rt) == WT::I64 {
                                self.op(O::I64ExtendI32S, &[m], WT::I64)
                            } else {
                                m
                            };
                            self.bind(f, i, smallvec![v]);
                        }
                        P::Splat
                        | P::VhighBits
                        | P::Swizzle
                        | P::ScalarToVector
                        | P::IaddPairwise
                        | P::SwidenLow
                        | P::SwidenHigh
                        | P::UwidenLow
                        | P::UwidenHigh
                        | P::Snarrow
                        | P::Unarrow
                        | P::Uunarrow
                        | P::Fvdemote
                        | P::FvpromoteLow
                        | P::AvgRound
                        | P::UaddSat
                        | P::SaddSat
                        | P::UsubSat
                        | P::SsubSat => {
                            panic!("vector/saturating op {opcode} on wasm clif path")
                        }
                        _ => panic!("unary {opcode}"),
                    }
                }
                // bitcast carries MemFlags (LoadNoOffset format) in this
                // cranelift, not Unary.
                ir::InstructionData::LoadNoOffset {
                    opcode: P::Bitcast,
                    arg,
                    ..
                } => {
                    let t = self.vty(f, arg);
                    let rt = self.vty(f, f.dfg.inst_results(i)[0]);
                    let (ft, fr) = (self.flat(t).len(), self.flat(rt).len());
                    if ft == 2 && fr == 2 {
                        // i128<->f128: same two-i64 bit pattern.
                        let vs = self.get(arg).clone();
                        self.setv(f.dfg.inst_results(i)[0], vs);
                        return;
                    }
                    if ft == 2 && rt.is_vector() && rt.bits() == 128 {
                        // i128/f128 -> v128: pack (lo, hi) into i64x2 lanes.
                        let (al, ah) = self.pair(arg);
                        let z = self.op(O::V128Const { value: 0 }, &[], WT::V128);
                        let v = self.op(O::I64x2ReplaceLane { lane: 0 }, &[z, al], WT::V128);
                        let v = self.op(O::I64x2ReplaceLane { lane: 1 }, &[v, ah], WT::V128);
                        self.set(f.dfg.inst_results(i)[0], v);
                        return;
                    }
                    if t.is_vector() && t.bits() == 128 && fr == 2 {
                        // v128 -> i128/f128: extract the i64x2 lanes.
                        let x = self.get1(arg);
                        let lo = self.op(O::I64x2ExtractLane { lane: 0 }, &[x], WT::I64);
                        let hi = self.op(O::I64x2ExtractLane { lane: 1 }, &[x], WT::I64);
                        self.setv(f.dfg.inst_results(i)[0], smallvec![lo, hi]);
                        return;
                    }
                    let v = self.reinterp(self.get1(arg), t, rt);
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::Load { opcode, arg, .. }
                | ir::InstructionData::LoadNoOffset { opcode, arg, .. }
                    if matches!(
                        opcode,
                        P::Load
                            | P::AtomicLoad
                            | P::Uload8
                            | P::Sload8
                            | P::Uload16
                            | P::Sload16
                            | P::Uload32
                            | P::Sload32
                            | P::Uload8x8
                            | P::Sload8x8
                            | P::Uload16x4
                            | P::Sload16x4
                            | P::Uload32x2
                            | P::Sload32x2
                    ) =>
                {
                    let rt = self.vty(f, f.dfg.inst_results(i)[0]);
                    let off = match data {
                        ir::InstructionData::Load { offset, .. } => i64::from(offset),
                        _ => 0,
                    };
                    let p = self.base32(arg);
                    let vs = self.do_load(opcode, rt, p, off as u64);
                    self.bind(f, i, vs);
                }
                ir::InstructionData::Store { opcode, args, .. }
                | ir::InstructionData::StoreNoOffset { opcode, args, .. }
                    if matches!(
                        opcode,
                        P::Store | P::AtomicStore | P::Istore8 | P::Istore16 | P::Istore32
                    ) =>
                {
                    let st = self.vty(f, args[0]);
                    let off = match data {
                        ir::InstructionData::Store { offset, .. } => i64::from(offset),
                        _ => 0,
                    };
                    let p = self.base32(args[1]);
                    self.do_store(f, opcode, st, args[0], p, off as u64);
                }
                ir::InstructionData::StackAddr {
                    stack_slot, offset, ..
                } => {
                    let v = self.slot_addr(f, stack_slot, i64::from(offset));
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::DynamicStackAddr { .. } => panic!("dynamic stack slot"),
                ir::InstructionData::FuncAddr { func_ref, .. } => {
                    let name = self.fr_name(f, func_ref);
                    let v = self.sym_got(true, &name, 0);
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::UnaryGlobalValue {
                    opcode,
                    global_value,
                } => match opcode {
                    P::SymbolValue => {
                        let (func, name, off) = self.gv_sym(f, global_value);
                        let v = self.sym_got(func, &name, off);
                        self.set(f.dfg.inst_results(i)[0], v);
                    }
                    P::TlsValue => panic!("tls on wasm"),
                    _ => panic!("gv op {opcode}"),
                },
                ir::InstructionData::Call {
                    opcode, func_ref, ..
                } => {
                    let args = f.dfg.inst_args(i).to_vec();
                    // Memfuncs arrive as libcalls from the cranelift frontend;
                    // wasm bulk-memory ops cover them natively.
                    if let &ExternalName::LibCall(lc) = &f.dfg.ext_funcs[func_ref].name {
                        use ir::LibCall as L;
                        if matches!(lc, L::Memcpy | L::Memmove | L::Memset | L::Memcmp) {
                            let mem = self.o.mem;
                            let d = self.cvt(self.get1(args[0]), WT::I32);
                            let s = self.cvt(self.get1(args[1]), WT::I32);
                            let n = self.cvt(self.get1(args[2]), WT::I32);
                            match lc {
                                L::Memcpy | L::Memmove => self.op0(
                                    O::MemoryCopy {
                                        dst_mem: mem,
                                        src_mem: mem,
                                    },
                                    &[d, s, n],
                                ),
                                L::Memset => self.op0(O::MemoryFill { mem }, &[d, s, n]),
                                _ => {
                                    // memcmp(d, s, n): bail to a helper below.
                                    panic!("memcmp libcall on wasm clif path")
                                }
                            }
                            return;
                        }
                    }
                    let sig = f.dfg.signatures[f.dfg.ext_funcs[func_ref].signature].clone();
                    // core::arch::wasm32 memory intrinsics are `llvm.*`
                    // link_name functions that surface as `__pliron_llvm_*`
                    // imports; wasm covers them natively.
                    if matches!(&f.dfg.ext_funcs[func_ref].name, ExternalName::User(_)) {
                        let name = self.func_name(func_ref, f);
                        if name.starts_with("__pliron_llvm_wasm_memory_grow") {
                            let wargs = self.call_args(&sig, &args);
                            let mem = self.o.mem;
                            let v = self.op(O::MemoryGrow { mem }, &[wargs[1]], WT::I32);
                            self.bind_call(f, i, smallvec![v]);
                            return;
                        }
                        if name.starts_with("__pliron_llvm_wasm_memory_size") {
                            let mem = self.o.mem;
                            let v = self.op(O::MemorySize { mem }, &[], WT::I32);
                            self.bind_call(f, i, smallvec![v]);
                            return;
                        }
                        if let Some((o, n, rt)) = vw_intrinsic(&name) {
                            let wargs = self.call_args(&sig, &args);
                            let rs = f.dfg.inst_results(i);
                            // `simd128` only enabled per-function
                            // (`#[target_feature]`, the stdarch dispatch
                            // pattern) leaves `NATIVE_SIMD` off, so vectors
                            // scalarize to per-lane signature params and
                            // results. Pack each operand's lanes into one
                            // v128 and extract the result lanes back out.
                            if wargs.len() == n && rs.len() <= 1 {
                                let v = self.op(o, &wargs[..n], rt);
                                self.bind_call(f, i, smallvec![v]);
                                return;
                            }
                            if n > 0 && wargs.len() % n == 0 {
                                let lanes = wargs.len() / n;
                                let at = sig.params[0].value_type;
                                let mut wv: Vec<WV> = Vec::with_capacity(n);
                                for c in wargs.chunks(lanes) {
                                    wv.push(self.pack_lanes(at, c));
                                }
                                let v = self.op(o, &wv, rt);
                                let vs: Vals = if rs.len() <= 1 {
                                    smallvec![v]
                                } else {
                                    let lt = self.vty(f, rs[0]);
                                    (0..rs.len())
                                        .map(|k| self.extract_lane_v(lt, v, k as u8))
                                        .collect()
                                };
                                self.bind_call(f, i, vs);
                                return;
                            }
                            // Lane counts that do not divide evenly fall
                            // through to a normal call to the trap stub.
                        }
                    }
                    // FnLower::libcall fabricates `(...)->i128/f128` call sigs
                    // for compiler-rt helpers (`__divti3`, `fmodf128`, ...), but
                    // their real wasm C ABI passes a 128-bit result through an
                    // sret pointer arg. Mangled (`_R`/`_Z`) callees are Rust-ABI
                    // and keep the flattened pair; a defined callee's own sig
                    // decides.
                    if opcode == P::Call
                        && sig.returns.len() == 1
                        && matches!(sig.returns[0].value_type, clt::I128 | clt::F128)
                    {
                        let name = self.func_name(func_ref, f);
                        let sret = match self.o.fidx.get(&name) {
                            Some(&i) => {
                                let sd = &self.o.m.signatures[self.o.funcs[i].sig];
                                sd.returns.is_empty() && sd.params.first() == Some(&WT::I32)
                            }
                            None => {
                                !(name.starts_with("_R")
                                    || name.starts_with("_ZN")
                                    || name.starts_with("_Z"))
                            }
                        };
                        if sret {
                            let ra = self.frame_slot(16, 16) as i64;
                            let ret = self.addr(ra);
                            let wargs = self.call_args(&sig, &args);
                            let mut ps = vec![WT::I32];
                            ps.extend(wargs.iter().map(|&x| self.ty_of(x)));
                            let fi = self.o.libfunc(
                                &name,
                                SignatureData {
                                    params: ps,
                                    returns: vec![],
                                },
                            );
                            let mut a: Vec<WV> = vec![ret];
                            a.extend(wargs);
                            self.opn(
                                O::Call {
                                    function_index: Func::new(fi),
                                },
                                &a,
                                &[],
                            );
                            let vs =
                                smallvec![self.load(clt::I64, ret, 0), self.load(clt::I64, ret, 8)];
                            self.bind_call(f, i, vs);
                            return;
                        }
                    }
                    let fi = self.func_idx(func_ref, f);
                    let wargs = self.call_args(&sig, &args);
                    let rtys = cwsig(&sig).returns;
                    let vs = self.opn(O::Call { function_index: fi }, &wargs, &rtys);
                    if opcode == P::ReturnCall {
                        self.ret(&vs);
                    } else {
                        self.bind_call(f, i, vs);
                    }
                }
                ir::InstructionData::CallIndirect {
                    opcode, sig_ref, ..
                } => {
                    let args = f.dfg.inst_args(i).to_vec();
                    let sig = f.dfg.signatures[sig_ref].clone();
                    // call_indirect sig, callee, args... -- callee is first.
                    let (callee, cargs) = args.split_first().unwrap();
                    let callee = self.cvt(self.get1(*callee), WT::I32);
                    let mut wargs = self.call_args(&sig, cargs);
                    wargs.push(callee);
                    let rtys = cwsig(&sig).returns;
                    let sig_index = self.sig_of(f, sig_ref);
                    let table_index = self.o.table;
                    let vs = self.opn(
                        O::CallIndirect {
                            sig_index,
                            table_index,
                        },
                        &wargs,
                        &rtys,
                    );
                    if opcode == P::ReturnCallIndirect {
                        self.ret(&vs);
                    } else {
                        self.bind_call(f, i, vs);
                    }
                }
                ir::InstructionData::TryCall { .. }
                | ir::InstructionData::TryCallIndirect { .. } => {
                    panic!("try_call on wasm (emulated EH not wired)")
                }
                ir::InstructionData::Jump { destination, .. } => {
                    let t = self.block_target(f, &destination);
                    self.b
                        .set_terminator(self.cur, Terminator::Br { target: t });
                }
                ir::InstructionData::Brif { arg, blocks, .. } => {
                    let c = self.cond(self.get1(arg));
                    let if_true = self.block_target(f, &blocks[0]);
                    let if_false = self.block_target(f, &blocks[1]);
                    self.b.set_terminator(
                        self.cur,
                        Terminator::CondBr {
                            cond: c,
                            if_true,
                            if_false,
                        },
                    );
                }
                ir::InstructionData::BranchTable { arg, table, .. } => {
                    let c = self.cvt(self.get1(arg), WT::I32);
                    let all = f
                        .dfg
                        .jump_tables
                        .get(table)
                        .unwrap()
                        .all_branches()
                        .to_vec();
                    let default = self.block_target(f, &all[0]);
                    let targets: Vec<BlockTarget> =
                        all[1..].iter().map(|bc| self.block_target(f, bc)).collect();
                    self.b.set_terminator(
                        self.cur,
                        Terminator::Select {
                            value: c,
                            targets,
                            default,
                        },
                    );
                }
                ir::InstructionData::MultiAry {
                    opcode: P::Return, ..
                } => {
                    let args = f.dfg.inst_args(i).to_vec();
                    let vs = self.ret_vals(f, &args);
                    self.ret(&vs);
                }
                ir::InstructionData::Trap { .. } => {
                    self.b.set_terminator(self.cur, Terminator::Unreachable);
                }
                ir::InstructionData::CondTrap { arg, .. } => {
                    let c = self.cond(self.get1(arg));
                    // trapnz traps when nonzero; trapz when zero.
                    let c = if opc == P::Trapz {
                        let z = self.i32c(0);
                        self.op(O::I32Eq, &[c, z], WT::I32)
                    } else {
                        c
                    };
                    self.cond_trap(c);
                }
                ir::InstructionData::IntAddTrap { args, .. } => {
                    // uadd_overflow_trap / iadd_trap: emit the add, trap on carry.
                    let t = self.vty(f, args[0]);
                    let (sum, cout) = self.addc(t, self.get1(args[0]), self.get1(args[1]), false);
                    self.bind(f, i, smallvec![sum]);
                    self.cond_trap(cout);
                }
                ir::InstructionData::AtomicRmw { op, args, .. } => {
                    let t = self.vty(f, args[1]);
                    let p = self.base32(args[0]);
                    let x = self.get1(args[1]);
                    let old = self.load(t, p, 0);
                    let nv = self.atomic_op(op, t, old, x);
                    self.store(t, nv, p, 0);
                    self.bind(f, i, smallvec![old]);
                }
                ir::InstructionData::AtomicCas { args, .. } => {
                    let t = self.vty(f, args[1]);
                    let p = self.base32(args[0]);
                    let old = self.load(t, p, 0);
                    let ok = self.icmp(ir::condcodes::IntCC::Equal, t, old, self.get1(args[1]));
                    let nv = self.sel(ok, self.get1(args[2]), old);
                    self.store(t, nv, p, 0);
                    self.bind(f, i, smallvec![old]);
                }
                ir::InstructionData::NullAry { opcode } => match opcode {
                    P::GetStackPointer => {
                        let g = self.op(
                            O::GlobalGet {
                                global_index: self.o.sp,
                            },
                            &[],
                            WT::I32,
                        );
                        let rt = self.vty(f, f.dfg.inst_results(i)[0]);
                        let v = self.cvt(g, wty(rt));
                        self.set(f.dfg.inst_results(i)[0], v);
                    }
                    P::GetFramePointer => {
                        let rt = self.vty(f, f.dfg.inst_results(i)[0]);
                        let v = self.cvt(self.fp, wty(rt));
                        self.set(f.dfg.inst_results(i)[0], v);
                    }
                    P::GetReturnAddress => panic!("return address on wasm"),
                    // No-ops in a single-threaded wasm world.
                    P::Nop | P::Fence | P::Debugtrap | P::SequencePoint => {}
                    _ => panic!("nullary {opcode}"),
                },
                ir::InstructionData::TernaryImm8 {
                    opcode: P::Insertlane,
                    args,
                    imm,
                } => {
                    let t = self.vty(f, args[0]);
                    let v = self.insert_lane(t, args[0], args[1], imm.into());
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::Shuffle {
                    opcode: P::Shuffle,
                    args,
                    imm,
                } => {
                    let (a, b) = (self.get1(args[0]), self.get1(args[1]));
                    let bytes = f.dfg.immediates[imm].as_slice();
                    let lanes: [u8; 16] = bytes
                        .try_into()
                        .unwrap_or_else(|_| panic!("shuffle mask {} bytes", bytes.len()));
                    let v = self.op(O::I8x16Shuffle { lanes }, &[a, b], WT::V128);
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::UnaryConst {
                    opcode: P::Vconst,
                    constant_handle,
                } => {
                    let bytes = f.dfg.constants.get(constant_handle).as_slice();
                    // Sub-128-bit vectors: upper lanes are don't-care, zero-pad.
                    if bytes.len() > 16 {
                        panic!("vconst {} bytes", bytes.len());
                    }
                    let mut padded = [0u8; 16];
                    padded[..bytes.len()].copy_from_slice(bytes);
                    let value = u128::from_le_bytes(padded);
                    let v = self.op(O::V128Const { value }, &[], WT::V128);
                    self.set(f.dfg.inst_results(i)[0], v);
                }
                ir::InstructionData::ExceptionHandlerAddress { .. } => panic!("eh on wasm"),
                _ => panic!("untranslated clif opcode {opc}"),
            }
        }
    }

    impl<'o, 'a> CX<'o, 'a> {
        /// Constant of CLIF type `t` holding `bits`.
        fn leaf_const(&mut self, t: ClType, bits: u64) -> WV {
            match wty(t) {
                WT::I64 => {
                    let m = if t.bits() >= 64 {
                        u64::MAX
                    } else {
                        (1u64 << t.bits()) - 1
                    };
                    self.i64c(bits & m)
                }
                _ => {
                    let m = if t.bits() >= 32 {
                        u64::MAX
                    } else {
                        (1u64 << t.bits()) - 1
                    };
                    self.i32c((bits & m) as u32)
                }
            }
        }

        /// Resolve an `ExternalName::User` ref to (namespace, index).
        fn user_name(&self, f: &ir::Function, u: ir::UserExternalNameRef) -> (u32, u32) {
            let n = &f.params.user_named_funcs()[u];
            (n.namespace, n.index)
        }

        fn fr_name(&self, f: &ir::Function, fr: ir::FuncRef) -> String {
            match &f.dfg.ext_funcs[fr].name {
                ExternalName::User(u) => {
                    let (ns, idx) = self.user_name(f, *u);
                    self.fn_names
                        .get(idx as usize)
                        .cloned()
                        .unwrap_or_else(|| panic!("bad func ref ns{ns} idx{idx}"))
                }
                ExternalName::LibCall(lc) => format!("{lc:?}"),
                n => panic!("non-user func ref {n:?}"),
            }
        }

        /// GlobalValue -> (is_func, symbol, byte offset). Follows IAddImm chains.
        fn gv_sym(&self, f: &ir::Function, gv: ir::GlobalValue) -> (bool, String, i64) {
            match &f.global_values[gv] {
                GlobalValueData::Symbol { name, offset, .. } => match name {
                    ExternalName::User(u) => {
                        let (ns, idx) = self.user_name(f, *u);
                        let names = if ns == 0 {
                            self.fn_names
                        } else {
                            self.data_names
                        };
                        (
                            ns == 0,
                            names
                                .get(idx as usize)
                                .cloned()
                                .unwrap_or_else(|| panic!("bad gv ns{ns} idx{idx}")),
                            offset.bits(),
                        )
                    }
                    n => panic!("non-user symbol gv {n:?}"),
                },
                GlobalValueData::IAddImm { base, offset, .. } => {
                    let (func, name, off) = self.gv_sym(f, *base);
                    (func, name, off + offset.bits())
                }
                g => panic!("global value {g:?}"),
            }
        }

        /// icmp `a cond b` -> i32. `t` is the operand type.
        fn icmp(&mut self, cond: ir::condcodes::IntCC, t: ClType, a: WV, b: WV) -> WV {
            use ir::condcodes::IntCC as CC;
            if t == clt::F32 || t == clt::F64 {
                let (eq, ne, lt, le, gt, ge) = if t == clt::F64 {
                    (O::F64Eq, O::F64Ne, O::F64Lt, O::F64Le, O::F64Gt, O::F64Ge)
                } else {
                    (O::F32Eq, O::F32Ne, O::F32Lt, O::F32Le, O::F32Gt, O::F32Ge)
                };
                // only ever reached via shenanigans; fcmp handles float conds.
                let v = self.op(eq, &[a, b], WT::I32);
                let _ = (ne, lt, le, gt, ge);
                return v;
            }
            let w = t.bits();
            let (a, b) = if w < 32 {
                match cond {
                    CC::SignedLessThan
                    | CC::SignedGreaterThanOrEqual
                    | CC::SignedLessThanOrEqual
                    | CC::SignedGreaterThan => (self.sext(a, w), self.sext(b, w)),
                    _ => (self.norm(a, w), self.norm(b, w)),
                }
            } else {
                (a, b)
            };
            let i64 = wty(t) == WT::I64;
            let ops = |s: bool| -> [O; 8] {
                if i64 {
                    if s {
                        [
                            O::I64Eq,
                            O::I64Ne,
                            O::I64LtS,
                            O::I64GeS,
                            O::I64LeS,
                            O::I64GtS,
                            O::I64LtS,
                            O::I64GtS,
                        ]
                    } else {
                        [
                            O::I64Eq,
                            O::I64Ne,
                            O::I64LtU,
                            O::I64GeU,
                            O::I64LeU,
                            O::I64GtU,
                            O::I64LtU,
                            O::I64GtU,
                        ]
                    }
                } else if s {
                    [
                        O::I32Eq,
                        O::I32Ne,
                        O::I32LtS,
                        O::I32GeS,
                        O::I32LeS,
                        O::I32GtS,
                        O::I32LtS,
                        O::I32GtS,
                    ]
                } else {
                    [
                        O::I32Eq,
                        O::I32Ne,
                        O::I32LtU,
                        O::I32GeU,
                        O::I32LeU,
                        O::I32GtU,
                        O::I32LtU,
                        O::I32GtU,
                    ]
                }
            };
            let o = ops(matches!(
                cond,
                CC::SignedLessThan
                    | CC::SignedGreaterThanOrEqual
                    | CC::SignedLessThanOrEqual
                    | CC::SignedGreaterThan
            ));
            let (x, y, op) = match cond {
                CC::Equal => (a, b, o[0]),
                CC::NotEqual => (a, b, o[1]),
                CC::SignedLessThan | CC::UnsignedLessThan => (a, b, o[2]),
                CC::SignedGreaterThanOrEqual | CC::UnsignedGreaterThanOrEqual => (a, b, o[3]),
                CC::SignedLessThanOrEqual | CC::UnsignedLessThanOrEqual => (a, b, o[4]),
                CC::SignedGreaterThan | CC::UnsignedGreaterThan => (a, b, o[5]),
            };
            let _ = (o[6], o[7]);
            self.op(op, &[x, y], WT::I32)
        }

        fn fcmp(&mut self, cond: ir::condcodes::FloatCC, t: ClType, a: WV, b: WV) -> WV {
            use ir::condcodes::FloatCC as CC;
            let f64 = wty(t) == WT::F64;
            let (eq, ne, lt, le, gt, ge) = if f64 {
                (O::F64Eq, O::F64Ne, O::F64Lt, O::F64Le, O::F64Gt, O::F64Ge)
            } else {
                (O::F32Eq, O::F32Ne, O::F32Lt, O::F32Le, O::F32Gt, O::F32Ge)
            };
            let ord = |cx: &mut Self| -> WV {
                // !(a!=a) & !(b!=b)
                let l = cx.op(ne, &[a, a], WT::I32);
                let r = cx.op(ne, &[b, b], WT::I32);
                let u = cx.op(O::I32Or, &[l, r], WT::I32);
                let one = cx.i32c(1);
                cx.op(O::I32Xor, &[u, one], WT::I32)
            };
            let uno = |cx: &mut Self| -> WV {
                let l = cx.op(ne, &[a, a], WT::I32);
                let r = cx.op(ne, &[b, b], WT::I32);
                cx.op(O::I32Or, &[l, r], WT::I32)
            };
            match cond {
                CC::Ordered => ord(self),
                CC::Unordered => uno(self),
                CC::Equal => self.op(eq, &[a, b], WT::I32),
                CC::NotEqual => self.op(ne, &[a, b], WT::I32),
                CC::LessThan => self.op(lt, &[a, b], WT::I32),
                CC::LessThanOrEqual => self.op(le, &[a, b], WT::I32),
                CC::GreaterThan => self.op(gt, &[a, b], WT::I32),
                CC::GreaterThanOrEqual => self.op(ge, &[a, b], WT::I32),
                CC::OrderedNotEqual => {
                    let x = self.op(ne, &[a, b], WT::I32);
                    let o = ord(self);
                    self.op(O::I32And, &[x, o], WT::I32)
                }
                CC::UnorderedOrEqual => {
                    let x = self.op(eq, &[a, b], WT::I32);
                    let u = uno(self);
                    self.op(O::I32Or, &[x, u], WT::I32)
                }
                CC::UnorderedOrLessThan => {
                    let x = self.op(lt, &[a, b], WT::I32);
                    let u = uno(self);
                    self.op(O::I32Or, &[x, u], WT::I32)
                }
                CC::UnorderedOrLessThanOrEqual => {
                    let x = self.op(le, &[a, b], WT::I32);
                    let u = uno(self);
                    self.op(O::I32Or, &[x, u], WT::I32)
                }
                CC::UnorderedOrGreaterThan => {
                    let x = self.op(gt, &[a, b], WT::I32);
                    let u = uno(self);
                    self.op(O::I32Or, &[x, u], WT::I32)
                }
                CC::UnorderedOrGreaterThanOrEqual => {
                    let x = self.op(ge, &[a, b], WT::I32);
                    let u = uno(self);
                    self.op(O::I32Or, &[x, u], WT::I32)
                }
            }
        }

        /// Float binary: fmin/fmax preserve NaN via libcall; others map 1:1.
        fn fbin(
            &mut self,
            op: Opcode,
            t: ClType,
            f: &ir::Function,
            i: CInst,
            args: [ir::Value; 2],
        ) {
            use Opcode as P;
            let wt = wty(t);
            let a = self.get1(args[0]);
            let b = self.get1(args[1]);
            let o = match (op, wt) {
                (P::Fadd, WT::F32) => O::F32Add,
                (P::Fadd, WT::F64) => O::F64Add,
                (P::Fsub, WT::F32) => O::F32Sub,
                (P::Fsub, WT::F64) => O::F64Sub,
                (P::Fmul, WT::F32) => O::F32Mul,
                (P::Fmul, WT::F64) => O::F64Mul,
                (P::Fdiv, WT::F32) => O::F32Div,
                (P::Fdiv, WT::F64) => O::F64Div,
                (P::Fmin, WT::F32) => O::F32Min,
                (P::Fmin, WT::F64) => O::F64Min,
                (P::Fmax, WT::F32) => O::F32Max,
                (P::Fmax, WT::F64) => O::F64Max,
                _ => panic!("fbin {op} {t}"),
            };
            let v = self.op(o, &[a, b], wt);
            self.set(f.dfg.inst_results(i)[0], v);
        }

        /// Load CLIF type `t` at `p + off`, handling ext loads and i128.
        fn do_load(&mut self, op: Opcode, t: ClType, p: WV, off: u64) -> Vals {
            use Opcode as P;
            if t == clt::I128 || t == clt::F128 {
                let lo = self.load(clt::I64, p, off);
                let hi = self.load(clt::I64, p, off + 8);
                return smallvec![lo, hi];
            }
            let mem = self.o.mem;
            let bytes = |b: u64| MemoryArg {
                align: b.trailing_zeros(),
                offset: off as u32,
                memory: mem,
            };
            let (o, ty) = match op {
                P::Load | P::AtomicLoad => {
                    return smallvec![self.load(t, p, off)];
                }
                // Vector widening loads.
                P::Uload8x8 => (O::V128Load8x8U { memory: bytes(8) }, WT::V128),
                P::Sload8x8 => (O::V128Load8x8S { memory: bytes(8) }, WT::V128),
                P::Uload16x4 => (O::V128Load16x4U { memory: bytes(8) }, WT::V128),
                P::Sload16x4 => (O::V128Load16x4S { memory: bytes(8) }, WT::V128),
                P::Uload32x2 => (O::V128Load32x2U { memory: bytes(8) }, WT::V128),
                P::Sload32x2 => (O::V128Load32x2S { memory: bytes(8) }, WT::V128),
                _ => {
                    let i64 = wty(t) == WT::I64;
                    match op {
                        P::Uload8 => (O::I32Load8U { memory: bytes(1) }, WT::I32),
                        P::Sload8 => (O::I32Load8S { memory: bytes(1) }, WT::I32),
                        P::Uload16 => (O::I32Load16U { memory: bytes(2) }, WT::I32),
                        P::Sload16 => (O::I32Load16S { memory: bytes(2) }, WT::I32),
                        P::Uload32 if i64 => {
                            return smallvec![self.op(
                                O::I64Load32U { memory: bytes(4) },
                                &[p],
                                WT::I64,
                            )];
                        }
                        P::Sload32 => (O::I64Load32S { memory: bytes(4) }, WT::I64),
                        _ => panic!("load op {op}"),
                    }
                }
            };
            let i64 = !t.is_vector() && wty(t) == WT::I64;
            let mut v = self.op(o, &[p], ty);
            // Canonicalize narrow unsigned loads to the same rep as zext.
            if matches!(op, P::Uload8 | P::Uload16) && i64 {
                v = self.op(O::I64ExtendI32U, &[v], WT::I64);
            }
            smallvec![v]
        }

        fn do_store(
            &mut self,
            f: &ir::Function,
            op: Opcode,
            t: ClType,
            v: ir::Value,
            p: WV,
            off: u64,
        ) {
            use Opcode as P;
            if t == clt::I128 || t == clt::F128 {
                let (lo, hi) = self.pair(v);
                self.store(clt::I64, lo, p, off);
                self.store(clt::I64, hi, p, off + 8);
                return;
            }
            let mem = self.o.mem;
            let bytes = |b: u64| MemoryArg {
                align: b.trailing_zeros(),
                offset: off as u32,
                memory: mem,
            };
            // `storeN`/`istoreN (ireduce x)` is x's low N bytes outright —
            // store the reduce's source instead (`i64.storeN` writes the
            // low N bytes of an i64), skipping the wrap.
            let want = match op {
                P::Istore8 => 8,
                P::Istore16 => 16,
                P::Istore32 => 32,
                P::Store if t.is_int() && !t.is_vector() => t.bits(),
                _ => 0,
            };
            let mut vv = f.dfg.resolve_aliases(v);
            if want > 0 {
                for _ in 0..4 {
                    let Some(d) = f.dfg.value_def(vv).inst() else {
                        break;
                    };
                    let ir::InstructionData::Unary {
                        opcode: Opcode::Ireduce,
                        arg,
                    } = f.dfg.insts[d]
                    else {
                        break;
                    };
                    if f.dfg.value_type(vv).bits() < want {
                        break;
                    }
                    let a = f.dfg.resolve_aliases(arg);
                    if self.get(a).len() != 1 {
                        break;
                    }
                    vv = a;
                }
            }
            // Narrow store of an unwrapped value: pick the store op by
            // the source's width.
            let narrow_store = |s: &mut Self, v: WV, bits: u32| {
                let (v, i64) = match s.ty_of(v) {
                    WT::I32 => (v, false),
                    WT::I64 => (v, true),
                    _ => (s.cvt(v, WT::I64), true),
                };
                let o = match (bits, i64) {
                    (8, false) => O::I32Store8 { memory: bytes(1) },
                    (8, true) => O::I64Store8 { memory: bytes(1) },
                    (16, false) => O::I32Store16 { memory: bytes(2) },
                    (16, true) => O::I64Store16 { memory: bytes(2) },
                    (_, false) => O::I32Store { memory: bytes(4) },
                    (_, true) => O::I64Store32 { memory: bytes(4) },
                };
                s.op0(o, &[p, v]);
            };
            match op {
                P::Store | P::AtomicStore => {
                    if op == P::Store && t.is_int() && !t.is_vector() && t.bits() <= 32 {
                        narrow_store(self, self.get1(vv), t.bits())
                    } else {
                        self.store(t, self.get1(v), p, off)
                    }
                }
                P::Istore8 | P::Istore16 | P::Istore32 => {
                    narrow_store(self, self.get1(vv), want)
                }
                _ => panic!("store op {op}"),
            }
        }

        /// uextend/sextend `arg` -> `rt` (i64->i128 and narrow steps too).
        fn extend(&mut self, f: &ir::Function, arg: ir::Value, rt: ClType, signed: bool) -> Vals {
            let st = self.vty(f, arg);
            if rt == clt::I128 || rt == clt::F128 {
                let raw = self.get1(arg);
                let a = if wty(st) == WT::I64 {
                    raw
                } else if signed {
                    // Sign-extend the narrow container to i32, then i32->i64
                    // signed â€” `cvt`'s I64ExtendI32U would leave -1 as
                    // 0xFFFFFFFF (e.g. a signed i8 enum discriminant cast).
                    let s = self.sext(raw, st.bits());
                    self.op(O::I64ExtendI32S, &[s], WT::I64)
                } else {
                    // Zero-extend: canonicalize the narrow container first so
                    // stray high bits can't leak into the low word.
                    let z = self.norm(raw, st.bits());
                    self.op(O::I64ExtendI32U, &[z], WT::I64)
                };
                let hi = if signed {
                    let s = self.i64c(63);
                    self.op(O::I64ShrS, &[a, s], WT::I64)
                } else {
                    self.i64c(0)
                };
                return smallvec![a, hi];
            }
            let a = self.get1(arg);
            let v = match (wty(st), wty(rt)) {
                (WT::I32, WT::I64) => {
                    if signed {
                        let a = if st.bits() < 32 {
                            self.sext(a, st.bits())
                        } else {
                            a
                        };
                        self.op(O::I64ExtendI32S, &[a], WT::I64)
                    } else {
                        self.op(O::I64ExtendI32U, &[a], WT::I64)
                    }
                }
                (WT::I32, WT::I32) => {
                    let a = self.norm(a, st.bits());
                    if signed { self.sext(a, st.bits()) } else { a }
                }
                (WT::I64, WT::I64) => a,
                _ => panic!("extend {st}->{rt}"),
            };
            smallvec![v]
        }

        /// CLIF bitcast: reinterpret scalar reps.
        fn reinterp(&mut self, v: WV, st: ClType, rt: ClType) -> WV {
            // v128<->v128 bitcasts are a free reinterpret. Scalar<->vector
            // bitcasts only arise for sub-128 vectors (e.g. i64 -> i32x2):
            // lane 0 of the same-width int view; other lanes are don't-care.
            if st.is_vector() || rt.is_vector() {
                if st.is_vector() && rt.is_vector() {
                    return v;
                }
                let bits = st.bits().max(rt.bits());
                let (ex, rp, it, wt) = match bits {
                    64 => (
                        O::I64x2ExtractLane { lane: 0 },
                        O::I64x2ReplaceLane { lane: 0 },
                        clt::I64,
                        WT::I64,
                    ),
                    32 => (
                        O::I32x4ExtractLane { lane: 0 },
                        O::I32x4ReplaceLane { lane: 0 },
                        clt::I32,
                        WT::I32,
                    ),
                    _ => panic!("bitcast {st}->{rt}"),
                };
                if st.is_vector() {
                    let x = self.op(ex, &[v], wt);
                    return if wty(rt) == wt {
                        x
                    } else {
                        self.reinterp(x, it, rt)
                    };
                }
                let x = if wty(st) == wt {
                    v
                } else {
                    self.reinterp(v, st, it)
                };
                let z = self.op(O::V128Const { value: 0 }, &[], WT::V128);
                return self.op(rp, &[z, x], WT::V128);
            }
            match (wty(st), wty(rt)) {
                (a, b) if a == b => v,
                (WT::I32, WT::F32) => self.op(O::F32ReinterpretI32, &[v], WT::F32),
                (WT::F32, WT::I32) => self.op(O::I32ReinterpretF32, &[v], WT::I32),
                (WT::I64, WT::F64) => self.op(O::F64ReinterpretI64, &[v], WT::F64),
                (WT::F64, WT::I64) => self.op(O::I64ReinterpretF64, &[v], WT::I64),
                (WT::I64, WT::I32) => self.op(O::I32WrapI64, &[v], WT::I32),
                (WT::I32, WT::I64) => self.op(O::I64ExtendI32U, &[v], WT::I64),
                _ => panic!("bitcast {st}->{rt}"),
            }
        }

        /// fcvt_to_int(+sat); unsaturated wasm trap ops carry the CLIF flags.
        fn fcvt_to_int(&mut self, op: Opcode, ft: ClType, rt: ClType, a: WV) -> Vals {
            use Opcode as P;
            let wf = wty(ft);
            if rt == clt::I128 {
                // Via libcall: compiler-rt `__fix*` helpers, keyed on ft.
                let name = match (op, ft) {
                    (P::FcvtToSint | P::FcvtToSintSat, clt::F64) => "__fixdfti",
                    (P::FcvtToUint | P::FcvtToUintSat, clt::F64) => "__fixunsdfti",
                    (P::FcvtToSint | P::FcvtToSintSat, clt::F32) => "__fixsfti",
                    _ => "__fixunssfti",
                };
                let _ = wf;
                return self.libcall1(name, &[clt::I128], &[ft], &[a]);
            }
            let o = match (op, wf, wty(rt)) {
                (P::FcvtToSint, WT::F32, WT::I32) => O::I32TruncF32S,
                (P::FcvtToSint, WT::F32, WT::I64) => O::I64TruncF32S,
                (P::FcvtToSint, WT::F64, WT::I32) => O::I32TruncF64S,
                (P::FcvtToSint, WT::F64, WT::I64) => O::I64TruncF64S,
                (P::FcvtToUint, WT::F32, WT::I32) => O::I32TruncF32U,
                (P::FcvtToUint, WT::F32, WT::I64) => O::I64TruncF32U,
                (P::FcvtToUint, WT::F64, WT::I32) => O::I32TruncF64U,
                (P::FcvtToUint, WT::F64, WT::I64) => O::I64TruncF64U,
                (P::FcvtToSintSat, WT::F32, WT::I32) => O::I32TruncSatF32S,
                (P::FcvtToSintSat, WT::F32, WT::I64) => O::I64TruncSatF32S,
                (P::FcvtToSintSat, WT::F64, WT::I32) => O::I32TruncSatF64S,
                (P::FcvtToSintSat, WT::F64, WT::I64) => O::I64TruncSatF64S,
                (P::FcvtToUintSat, WT::F32, WT::I32) => O::I32TruncSatF32U,
                (P::FcvtToUintSat, WT::F32, WT::I64) => O::I64TruncSatF32U,
                (P::FcvtToUintSat, WT::F64, WT::I32) => O::I32TruncSatF64U,
                (P::FcvtToUintSat, WT::F64, WT::I64) => O::I64TruncSatF64U,
                _ => panic!("fcvt {op} {ft}->{rt}"),
            };
            let mut v = self.op(o, &[a], wty(rt));
            if rt.bits() < 32 {
                v = self.norm(v, rt.bits());
            }
            smallvec![v]
        }

        fn fcvt_from_int(&mut self, op: Opcode, st: ClType, rt: ClType, arg: ir::Value) -> Vals {
            use Opcode as P;
            if st == clt::I128 {
                let name = match (op, rt) {
                    (P::FcvtFromSint, clt::F64) => "__floattidf",
                    (P::FcvtFromUint, clt::F64) => "__floatuntidf",
                    (P::FcvtFromSint, clt::F32) => "__floattisf",
                    _ => "__floatuntisf",
                };
                let (lo, hi) = self.pair(arg);
                return self.libcall1(name, &[rt], &[clt::I128], &[lo, hi]);
            }
            let a = self.get1(arg);
            let signed = op == P::FcvtFromSint;
            let a = if signed {
                self.sext(a, st.bits())
            } else {
                self.norm(a, st.bits())
            };
            let wi = wty(st);
            let wr = wty(rt);
            let o = match (signed, wi, wr) {
                (true, WT::I32, WT::F32) => O::F32ConvertI32S,
                (true, WT::I32, WT::F64) => O::F64ConvertI32S,
                (true, WT::I64, WT::F32) => O::F32ConvertI64S,
                (true, WT::I64, WT::F64) => O::F64ConvertI64S,
                (false, WT::I32, WT::F32) => O::F32ConvertI32U,
                (false, WT::I32, WT::F64) => O::F64ConvertI32U,
                (false, WT::I64, WT::F32) => O::F32ConvertI64U,
                (false, WT::I64, WT::F64) => O::F64ConvertI64U,
                _ => panic!("fcvt {st}->{rt}"),
            };
            smallvec![self.op(o, &[a], wr)]
        }

        /// Import `name` under a signature built from the operand reps and call
        /// it. Used for compiler-rt helpers (`__multi3`, `__udivti3`, ...).
        /// i128/f128 results use the sret ABI (matching the helpers' real
        /// signatures): a 16-byte frame slot as the first arg, loaded back.
        fn libcall1(&mut self, name: &str, rets: &[ClType], _args: &[ClType], xs: &[WV]) -> Vals {
            let mut rty = Vec::new();
            for t in rets {
                cwtys(*t, &mut rty);
            }
            if rty == [WT::I64, WT::I64] {
                let off = self.frame_slot(16, 16);
                let c = self.i32c(off);
                let ret = self.op(O::I32Add, &[self.fp, c], WT::I32);
                let mut ps = vec![WT::I32];
                ps.extend(xs.iter().map(|&x| self.ty_of(x)));
                let i = self.o.libfunc(
                    name,
                    SignatureData {
                        params: ps,
                        returns: vec![],
                    },
                );
                let mut a: Vec<WV> = vec![ret];
                a.extend_from_slice(xs);
                self.opn(
                    O::Call {
                        function_index: Func::new(i),
                    },
                    &a,
                    &[],
                );
                return smallvec![self.load(clt::I64, ret, 0), self.load(clt::I64, ret, 8)];
            }
            let aty: Vec<WT> = xs.iter().map(|&x| self.ty_of(x)).collect();
            let rtys = rty.clone();
            let f = Func::new(self.o.libfunc(
                name,
                SignatureData {
                    params: aty,
                    returns: rty,
                },
            ));
            self.opn(O::Call { function_index: f }, xs, &rtys)
        }

        /// clz/ctz/popcnt/bswap/cls/bitrev on canonical narrow ints.
        fn bitop(&mut self, op: Opcode, t: ClType, a: WV) -> WV {
            use Opcode as P;
            let w = t.bits();
            let i64 = wty(t) == WT::I64;
            match op {
                P::Popcnt => {
                    let r = if i64 {
                        self.op(O::I64Popcnt, &[a], WT::I64)
                    } else {
                        self.op(O::I32Popcnt, &[a], WT::I32)
                    };
                    if w < 32 { self.norm(r, w) } else { r }
                }
                P::Clz => {
                    if i64 {
                        self.op(O::I64Clz, &[a], WT::I64)
                    } else if w == 32 {
                        self.op(O::I32Clz, &[a], WT::I32)
                    } else {
                        // clz(canonical) - (32 - w)
                        let c = self.op(O::I32Clz, &[a], WT::I32);
                        let off = self.i32c(32 - w);
                        self.op(O::I32Sub, &[c, off], WT::I32)
                    }
                }
                // `ctz_nonzero` has the same lowering â€” the caller's nonzero
                // guarantee just lets us skip the zero case a plain ctz
                // already produces.
                P::Ctz | P::CtzNonzero => {
                    if i64 {
                        self.op(O::I64Ctz, &[a], WT::I64)
                    } else if w == 32 {
                        self.op(O::I32Ctz, &[a], WT::I32)
                    } else {
                        // ctz(a | (1<<w))  -- sets a marker bit above w
                        let hi = self.i32c(1 << w);
                        let x = self.op(O::I32Or, &[a, hi], WT::I32);
                        self.op(O::I32Ctz, &[x], WT::I32)
                    }
                }
                P::Bswap => {
                    let r = if i64 {
                        self.bswap64(a)
                    } else {
                        self.bswap32(a)
                    };
                    // narrow: byte-reverse in the container then pull the bytes
                    // down into place.
                    if w < 32 {
                        let s = self.i32c(32 - w);
                        self.op(O::I32ShrU, &[r, s], WT::I32)
                    } else {
                        r
                    }
                }
                P::Cls => {
                    // cls x = clz((x << 1) ^ x) - 1 on the sign-extended rep.
                    let a = if w < 64 { self.sext(a, w) } else { a };
                    let one = self.leaf_const(t, 1);
                    let sh = self.binop(t, O::I32Shl, O::I64Shl, a, one);
                    let x = self.binop(t, O::I32Xor, O::I64Xor, sh, a);
                    let c = if i64 {
                        self.op(O::I64Clz, &[x], WT::I64)
                    } else {
                        self.op(O::I32Clz, &[x], WT::I32)
                    };
                    self.binop(t, O::I32Sub, O::I64Sub, c, one)
                }
                P::Bitrev => {
                    // Swap adjacent bits, then pairs, then nibbles; a final
                    // byte swap finishes the reversal.
                    let wt = wty(t);
                    let (shl, shr, and, or) = if i64 {
                        (O::I64Shl, O::I64ShrU, O::I64And, O::I64Or)
                    } else {
                        (O::I32Shl, O::I32ShrU, O::I32And, O::I32Or)
                    };
                    let mut x = a;
                    for (k, m) in [
                        (1u64, 0x5555_5555_5555_5555u64),
                        (2, 0x3333_3333_3333_3333),
                        (4, 0x0f0f_0f0f_0f0f_0f0f),
                    ] {
                        let m = if i64 { m } else { m & 0xffff_ffff };
                        let c = self.leaf_const(t, k);
                        let mm = self.leaf_const(t, m);
                        let l = self.op(and, &[x, mm], wt);
                        let l = self.op(shl, &[l, c], wt);
                        let r = self.op(shr, &[x, c], wt);
                        let r = self.op(and, &[r, mm], wt);
                        x = self.op(or, &[l, r], wt);
                    }
                    if w == 8 {
                        self.norm(x, 8)
                    } else {
                        self.bitop(P::Bswap, t, x)
                    }
                }
                _ => panic!("bitop {op}"),
            }
        }

        /// Byte-reverse a full i32.
        fn bswap32(&mut self, x: WV) -> WV {
            let m1 = self.i32c(0x00ff_00ff);
            let m2 = self.i32c(0xff00_ff00);
            let c8 = self.i32c(8);
            let lo = self.op(O::I32And, &[x, m1], WT::I32);
            let lo = self.op(O::I32Shl, &[lo, c8], WT::I32);
            let hi = self.op(O::I32And, &[x, m2], WT::I32);
            let hi = self.op(O::I32ShrU, &[hi, c8], WT::I32);
            let y = self.op(O::I32Or, &[lo, hi], WT::I32);
            let c16 = self.i32c(16);
            self.op(O::I32Rotl, &[y, c16], WT::I32)
        }

        /// Byte-reverse a full i64 via two i32 reversals and a half swap.
        fn bswap64(&mut self, x: WV) -> WV {
            let c32 = self.i64c(32);
            let lo = self.op(O::I32WrapI64, &[x], WT::I32);
            let hi = self.op(O::I64ShrU, &[x, c32], WT::I64);
            let hi = self.op(O::I32WrapI64, &[hi], WT::I32);
            let (lo, hi) = (self.bswap32(hi), self.bswap32(lo));
            let lo64 = self.op(O::I64ExtendI32U, &[lo], WT::I64);
            let hi64 = self.op(O::I64ExtendI32U, &[hi], WT::I64);
            let hi64 = self.op(O::I64Shl, &[hi64, c32], WT::I64);
            self.op(O::I64Or, &[hi64, lo64], WT::I64)
        }

        /// (value, flag) for the `*_overflow` binary ops.
        fn overflow(&mut self, op: Opcode, t: ClType, a: WV, b: WV) -> Vals {
            use Opcode as P;
            let w = t.bits();
            let flag = |cx: &mut Self, x: WV, y: WV, o32: O, o64: O| cx.cbinop(t, o32, o64, x, y);
            match op {
                P::UaddOverflow => {
                    let s = self.ibin(P::Iadd, t, a, b);
                    let c = flag(self, s, a, O::I32LtU, O::I64LtU);
                    smallvec![s, c]
                }
                P::UsubOverflow => {
                    let d = self.ibin(P::Isub, t, a, b);
                    let c = flag(self, a, b, O::I32LtU, O::I64LtU);
                    smallvec![d, c]
                }
                P::SaddOverflow | P::SsubOverflow => {
                    let s = self.ibin(
                        if op == P::SaddOverflow {
                            P::Iadd
                        } else {
                            P::Isub
                        },
                        t,
                        a,
                        b,
                    );
                    // of = sign bit of (a^s) & ((a^b)^~1 * sign) -- i.e.
                    // sadd: (a^s)&(b^s); ssub: (a^b)&(a^s)
                    let x1 = self.binop(t, O::I32Xor, O::I64Xor, a, s);
                    let x2 = if op == P::SaddOverflow {
                        self.binop(t, O::I32Xor, O::I64Xor, b, s)
                    } else {
                        self.binop(t, O::I32Xor, O::I64Xor, a, b)
                    };
                    let m = self.binop(t, O::I32And, O::I64And, x1, x2);
                    let m = if w < 32 { self.sext(m, w) } else { m };
                    let z = self.leaf_const(t, 0);
                    let c = self.cbinop(t, O::I32LtS, O::I64LtS, m, z);
                    smallvec![s, c]
                }
                P::UmulOverflow | P::SmulOverflow => {
                    let signed = op == P::SmulOverflow;
                    let prod = self.ibin(P::Imul, t, a, b);
                    let hi = self.umulhi(t, a, b, signed);
                    let c = if signed {
                        // hi should equal the sign-extension of prod's top bit.
                        let sh = self.leaf_const(t, w as u64 - 1);
                        let e = self.binop(t, O::I32ShrS, O::I64ShrS, prod, sh);
                        self.cbinop(t, O::I32Ne, O::I64Ne, hi, e)
                    } else {
                        let z = self.leaf_const(t, 0);
                        self.cbinop(t, O::I32Ne, O::I64Ne, hi, z)
                    };
                    smallvec![prod, c]
                }
                _ => panic!("overflow {op}"),
            }
        }

        /// (value, flag) for `*_overflow_cin`/`_bin` -- the carry/borrow input
        /// is a clif i8 in {0,1} folded into the first operand before the main op.
        fn overflow_in(&mut self, op: Opcode, t: ClType, a: WV, b: WV, cin: WV) -> Vals {
            use Opcode as P;
            let cin = self.cvt(cin, wty(t));
            // a (+|-) cin first; cin in {0,1} can only carry when a is at its
            // boundary, which the main op's flag still catches for the pair.
            let a2 = if matches!(op, P::UaddOverflowCin | P::SaddOverflowCin) {
                self.binop(t, O::I32Add, O::I64Add, a, cin)
            } else {
                self.binop(t, O::I32Sub, O::I64Sub, a, cin)
            };
            let a2 = if t.bits() < 32 {
                self.norm(a2, t.bits())
            } else {
                a2
            };
            self.overflow(
                match op {
                    P::UaddOverflowCin => P::UaddOverflow,
                    P::SaddOverflowCin => P::SaddOverflow,
                    P::UsubOverflowBin => P::UsubOverflow,
                    _ => P::SsubOverflow,
                },
                t,
                a2,
                b,
            )
        }

        /// Single-threaded atomic rmw: load, op, store. Returns the new value.
        fn atomic_op(&mut self, op: ir::AtomicRmwOp, t: ClType, old: WV, x: WV) -> WV {
            use ir::AtomicRmwOp as A;
            match op {
                A::Add => self.binop(t, O::I32Add, O::I64Add, old, x),
                A::Sub => self.binop(t, O::I32Sub, O::I64Sub, old, x),
                A::And => self.binop(t, O::I32And, O::I64And, old, x),
                A::Or => self.binop(t, O::I32Or, O::I64Or, old, x),
                A::Xor => self.binop(t, O::I32Xor, O::I64Xor, old, x),
                A::Nand => {
                    let a = self.binop(t, O::I32And, O::I64And, old, x);
                    self.bnot(t, a)
                }
                A::Xchg => x,
                A::Umax => self.minmax(t, old, x, O::I32GtU, O::I64GtU),
                A::Umin => self.minmax(t, old, x, O::I32LtU, O::I64LtU),
                A::Smax => self.minmax_s(t, old, x, false),
                A::Smin => self.minmax_s(t, old, x, true),
            }
        }

        /// Scalar saturating add/sub on canonical reps.
        fn sat_bin(&mut self, op: Opcode, t: ClType, a: WV, b: WV) -> WV {
            use Opcode as P;
            let w = t.bits();
            let signed = matches!(op, P::SaddSat | P::SsubSat);
            if w < 64 {
                // Widen to i64, clamp, wrap back to the canonical rep.
                let a64 = match (signed, w == 32) {
                    (true, true) => self.op(O::I64ExtendI32S, &[a], WT::I64),
                    (false, true) => self.op(O::I64ExtendI32U, &[a], WT::I64),
                    (true, false) => {
                        let x = self.sext(a, w);
                        self.op(O::I64ExtendI32S, &[x], WT::I64)
                    }
                    (false, false) => self.op(O::I64ExtendI32U, &[a], WT::I64),
                };
                let b64 = match (signed, w == 32) {
                    (true, true) => self.op(O::I64ExtendI32S, &[b], WT::I64),
                    (false, true) => self.op(O::I64ExtendI32U, &[b], WT::I64),
                    (true, false) => {
                        let x = self.sext(b, w);
                        self.op(O::I64ExtendI32S, &[x], WT::I64)
                    }
                    (false, false) => self.op(O::I64ExtendI32U, &[b], WT::I64),
                };
                let r = if matches!(op, P::UaddSat | P::SaddSat) {
                    self.op(O::I64Add, &[a64, b64], WT::I64)
                } else {
                    self.op(O::I64Sub, &[a64, b64], WT::I64)
                };
                let r = if signed {
                    let mx = self.i64c(((1i64 << (w - 1)) - 1) as u64);
                    let mn = self.i64c((-(1i64 << (w - 1))) as u64);
                    let gt = self.op(O::I64GtS, &[r, mx], WT::I32);
                    let r = self.sel(gt, mx, r);
                    let lt = self.op(O::I64LtS, &[r, mn], WT::I32);
                    self.sel(lt, mn, r)
                } else {
                    let mx = self.i64c((1u64 << w) - 1);
                    let z = self.i64c(0);
                    if matches!(op, P::UaddSat) {
                        let gt = self.op(O::I64GtU, &[r, mx], WT::I32);
                        self.sel(gt, mx, r)
                    } else {
                        let lt = self.op(O::I64LtS, &[r, z], WT::I32);
                        self.sel(lt, z, r)
                    }
                };
                let v = self.op(O::I32WrapI64, &[r], WT::I32);
                return if w < 32 { self.norm(v, w) } else { v };
            }
            let sub = matches!(op, P::UsubSat | P::SsubSat);
            let r = if sub {
                self.op(O::I64Sub, &[a, b], WT::I64)
            } else {
                self.op(O::I64Add, &[a, b], WT::I64)
            };
            if signed {
                // add: (a^r)&(b^r) < 0; sub: (a^b)&(a^r) < 0.
                let x = self.op(O::I64Xor, &[a, r], WT::I64);
                let y = if sub {
                    self.op(O::I64Xor, &[a, b], WT::I64)
                } else {
                    self.op(O::I64Xor, &[b, r], WT::I64)
                };
                let m = self.op(O::I64And, &[x, y], WT::I64);
                let z = self.i64c(0);
                let ovf = self.op(O::I64LtS, &[m, z], WT::I32);
                // a < 0 ? MIN : MAX
                let neg = self.op(O::I64LtS, &[a, z], WT::I32);
                let mx = self.i64c(i64::MAX as u64);
                let mn = self.i64c(i64::MIN as u64);
                let sat = self.sel(neg, mn, mx);
                self.sel(ovf, sat, r)
            } else {
                let ovf = if sub {
                    self.op(O::I64LtU, &[a, b], WT::I32)
                } else {
                    self.op(O::I64LtU, &[r, a], WT::I32)
                };
                let sat = self.i64c(if sub { 0 } else { u64::MAX });
                self.sel(ovf, sat, r)
            }
        }

        /// i128/f128 unary ops on (lo,hi) pairs.
        fn i128_un(&mut self, op: Opcode, t: ClType, f: &ir::Function, i: CInst, arg: ir::Value) {
            use Opcode as P;
            let (al, ah) = self.pair(arg);
            let r0 = f.dfg.inst_results(i)[0];
            match op {
                // i128<->f128 reinterpret: identical pair.
                P::Bitcast => self.setv(r0, smallvec![al, ah]),
                P::Bnot => {
                    let m = self.i64c(u64::MAX);
                    let lo = self.op(O::I64Xor, &[al, m], WT::I64);
                    let m = self.i64c(u64::MAX);
                    let hi = self.op(O::I64Xor, &[ah, m], WT::I64);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Ineg => {
                    let z = self.i64c(0);
                    let lo = self.op(O::I64Sub, &[z, al], WT::I64);
                    let nz = self.op(O::I64Ne, &[al, z], WT::I32);
                    let b64 = self.op(O::I64ExtendI32U, &[nz], WT::I64);
                    let hi = self.op(O::I64Sub, &[z, ah], WT::I64);
                    let hi = self.op(O::I64Sub, &[hi, b64], WT::I64);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Iabs => {
                    // negated pair, selected by the sign of `ah`.
                    let z = self.i64c(0);
                    let neg = self.op(O::I64LtS, &[ah, z], WT::I32);
                    let lo = self.op(O::I64Sub, &[z, al], WT::I64);
                    let nz = self.op(O::I64Ne, &[al, z], WT::I32);
                    let b64 = self.op(O::I64ExtendI32U, &[nz], WT::I64);
                    let hi = self.op(O::I64Sub, &[z, ah], WT::I64);
                    let hi = self.op(O::I64Sub, &[hi, b64], WT::I64);
                    let lo = self.sel(neg, lo, al);
                    let hi = self.sel(neg, hi, ah);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Clz => {
                    let clh = self.op(O::I64Clz, &[ah], WT::I64);
                    let cll = self.op(O::I64Clz, &[al], WT::I64);
                    let p64 = self.i64c(64);
                    let lop = self.op(O::I64Add, &[cll, p64], WT::I64);
                    let z = self.i64c(0);
                    let h0 = self.op(O::I64Eq, &[ah, z], WT::I32);
                    let cnt = self.sel(h0, lop, clh);
                    let z = self.i64c(0);
                    self.setv(r0, smallvec![cnt, z]);
                }
                P::Ctz | P::CtzNonzero => {
                    let ctl = self.op(O::I64Ctz, &[al], WT::I64);
                    let cth = self.op(O::I64Ctz, &[ah], WT::I64);
                    let p64 = self.i64c(64);
                    let hip = self.op(O::I64Add, &[cth, p64], WT::I64);
                    let z = self.i64c(0);
                    let l0 = self.op(O::I64Eq, &[al, z], WT::I32);
                    let cnt = self.sel(l0, hip, ctl);
                    let z = self.i64c(0);
                    self.setv(r0, smallvec![cnt, z]);
                }
                P::Popcnt => {
                    let l = self.op(O::I64Popcnt, &[al], WT::I64);
                    let h = self.op(O::I64Popcnt, &[ah], WT::I64);
                    let cnt = self.op(O::I64Add, &[l, h], WT::I64);
                    let z = self.i64c(0);
                    self.setv(r0, smallvec![cnt, z]);
                }
                P::Bswap => {
                    let lo = self.bswap64(ah);
                    let hi = self.bswap64(al);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Bitrev => {
                    let lo = self.bitop(P::Bitrev, clt::I64, ah);
                    let hi = self.bitop(P::Bitrev, clt::I64, al);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Cls => {
                    // clz(x ^ sext(sign)) - 1.
                    let s63 = self.i64c(63);
                    let s = self.op(O::I64ShrS, &[ah, s63], WT::I64);
                    let yl = self.op(O::I64Xor, &[al, s], WT::I64);
                    let yh = self.op(O::I64Xor, &[ah, s], WT::I64);
                    let clh = self.op(O::I64Clz, &[yh], WT::I64);
                    let cll = self.op(O::I64Clz, &[yl], WT::I64);
                    let p64 = self.i64c(64);
                    let lop = self.op(O::I64Add, &[cll, p64], WT::I64);
                    let z = self.i64c(0);
                    let h0 = self.op(O::I64Eq, &[yh, z], WT::I32);
                    let cnt = self.sel(h0, lop, clh);
                    let one = self.i64c(1);
                    let cnt = self.op(O::I64Sub, &[cnt, one], WT::I64);
                    let z = self.i64c(0);
                    self.setv(r0, smallvec![cnt, z]);
                }
                _ => panic!("i128 unary {op} on {t}"),
            }
        }

        /// 128-bit shift on a (lo,hi) pair; `amt` is an i64 count (masked
        /// internally). `shl` selects left, `arith` the right-shift flavour.
        /// Same construction as the old dialect path's `shift128`.
        fn sh128(&mut self, shl: bool, arith: bool, lo: WV, hi: WV, amt: WV) -> (WV, WV) {
            let c63 = self.i64c(63);
            let c64 = self.i64c(64);
            let c1 = self.i64c(1);
            let k = self.op(O::I64And, &[amt, c63], WT::I64);
            let inv = self.op(O::I64Sub, &[c63, k], WT::I64);
            let big = self.op(O::I64And, &[amt, c64], WT::I64);
            let big = self.op(O::I64Eqz, &[big], WT::I32);
            let big = self.op(O::I32Eqz, &[big], WT::I32);
            if shl {
                let lo_sh = self.op(O::I64Shl, &[lo, k], WT::I64);
                let h = self.op(O::I64Shl, &[hi, k], WT::I64);
                let c = self.op(O::I64ShrU, &[lo, c1], WT::I64);
                let c = self.op(O::I64ShrU, &[c, inv], WT::I64);
                let hi_sh = self.op(O::I64Or, &[h, c], WT::I64);
                let z = self.i64c(0);
                (self.sel(big, z, lo_sh), self.sel(big, lo_sh, hi_sh))
            } else {
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
                (self.sel(big, hi_sh, lo_sh), self.sel(big, fill, hi_sh))
            }
        }

        /// (hi,lo)-ordered 128-bit unsigned less-than.
        fn ltu128(&mut self, ah: WV, al: WV, bh: WV, bl: WV) -> WV {
            let h = self.op(O::I64LtU, &[ah, bh], WT::I32);
            let e = self.op(O::I64Eq, &[ah, bh], WT::I32);
            let l = self.op(O::I64LtU, &[al, bl], WT::I32);
            let el = self.op(O::I32And, &[e, l], WT::I32);
            self.op(O::I32Or, &[h, el], WT::I32)
        }

        /// All-zero v128 constant.
        fn vsplat_zero(&mut self) -> WV {
            self.op(O::V128Const { value: 0 }, &[], WT::V128)
        }

        /// Vector binary ops: lane-wise wasm ops selected by the CLIF lane type.
        fn vbin(
            &mut self,
            op: Opcode,
            t: ClType,
            f: &ir::Function,
            i: CInst,
            args: [ir::Value; 2],
        ) {
            use Opcode as P;
            let lane = t.lane_type();
            let (a, b) = (self.get1(args[0]), self.get1(args[1]));
            // i64x2 has no min/max lanes: expand via vicmp (vicmp already
            // sign-biases unsigned i64 compares) + bitselect.
            if lane == clt::I64 && matches!(op, P::Smin | P::Smax | P::Umin | P::Umax) {
                let cc = match op {
                    P::Smin => ir::condcodes::IntCC::SignedLessThan,
                    P::Smax => ir::condcodes::IntCC::SignedGreaterThan,
                    P::Umin => ir::condcodes::IntCC::UnsignedLessThan,
                    _ => ir::condcodes::IntCC::UnsignedGreaterThan,
                };
                let m = self.vicmp(cc, t, a, b);
                let v = self.op(O::V128Bitselect, &[a, b, m], WT::V128);
                self.set(f.dfg.inst_results(i)[0], v);
                return;
            }
            // i64x2 saturating add/sub: no lanes either.
            if lane == clt::I64 && matches!(op, P::UaddSat | P::UsubSat | P::SaddSat | P::SsubSat) {
                let add = matches!(op, P::UaddSat | P::SaddSat);
                let s = self.op(
                    if add { O::I64x2Add } else { O::I64x2Sub },
                    &[a, b],
                    WT::V128,
                );
                let v = if matches!(op, P::UaddSat | P::UsubSat) {
                    // uadd: sum <u a -> all 1s. usub: a <u b -> 0.
                    let (x, y) = if add { (s, a) } else { (a, b) };
                    let m = self.vicmp(ir::condcodes::IntCC::UnsignedLessThan, t, x, y);
                    let k = self.op(
                        O::V128Const {
                            value: if add { u128::MAX } else { 0 },
                        },
                        &[],
                        WT::V128,
                    );
                    self.op(O::V128Bitselect, &[k, s, m], WT::V128)
                } else {
                    // ovf bit = sign of (a^s)&(a^?b): add uses b^s, sub a^b.
                    let z = self.op(O::V128Const { value: 0 }, &[], WT::V128);
                    let x1 = self.op(O::V128Xor, &[a, s], WT::V128);
                    let (xa, xb) = if add { (b, s) } else { (a, b) };
                    let x2 = self.op(O::V128Xor, &[xa, xb], WT::V128);
                    let m = self.op(O::V128And, &[x1, x2], WT::V128);
                    let ovf = self.vicmp(ir::condcodes::IntCC::SignedLessThan, t, m, z);
                    let neg = self.vicmp(ir::condcodes::IntCC::SignedLessThan, t, a, z);
                    let mn = self.op(
                        O::V128Const {
                            value: 0x8000_0000_0000_0000_8000_0000_0000_0000,
                        },
                        &[],
                        WT::V128,
                    );
                    let mx = self.op(
                        O::V128Const {
                            value: 0x7fff_ffff_ffff_ffff_7fff_ffff_ffff_ffff,
                        },
                        &[],
                        WT::V128,
                    );
                    let sat = self.op(O::V128Bitselect, &[mn, mx, neg], WT::V128);
                    self.op(O::V128Bitselect, &[sat, s, ovf], WT::V128)
                };
                self.set(f.dfg.inst_results(i)[0], v);
                return;
            }
            // i8x16 has no mul: extend both halves to i16, mul, and shuffle
            // the products' low bytes back together (wraparound = low byte).
            if lane == clt::I8 && op == P::Imul {
                let ae = self.op(O::I16x8ExtendLowI8x16U, &[a], WT::V128);
                let be = self.op(O::I16x8ExtendLowI8x16U, &[b], WT::V128);
                let ao = self.op(O::I16x8ExtendHighI8x16U, &[a], WT::V128);
                let bo = self.op(O::I16x8ExtendHighI8x16U, &[b], WT::V128);
                let pe = self.op(O::I16x8Mul, &[ae, be], WT::V128);
                let po = self.op(O::I16x8Mul, &[ao, bo], WT::V128);
                let mut lanes = [0u8; 16];
                for i in 0..8u8 {
                    lanes[i as usize] = 2 * i;
                    lanes[8 + i as usize] = 16 + 2 * i;
                }
                let v = self.op(O::I8x16Shuffle { lanes }, &[pe, po], WT::V128);
                self.set(f.dfg.inst_results(i)[0], v);
                return;
            }
            if op == P::IaddPairwise {
                // phadd-style: result lanes are x's adjacent pairs in the low
                // half, y's in the high half â€” shuffle evens and odds of each
                // input together, then lane-wise add.
                let lb = (lane.bits() / 8) as usize;
                let pairs = 16 / lb / 2;
                let (mut ev, mut od) = ([0u8; 16], [0u8; 16]);
                for s in 0..2 {
                    for p in 0..pairs {
                        for k in 0..lb {
                            ev[(s * pairs + p) * lb + k] = (s * 16 + 2 * p * lb + k) as u8;
                            od[(s * pairs + p) * lb + k] = (s * 16 + (2 * p + 1) * lb + k) as u8;
                        }
                    }
                }
                let e = self.op(O::I8x16Shuffle { lanes: ev }, &[a, b], WT::V128);
                let d = self.op(O::I8x16Shuffle { lanes: od }, &[a, b], WT::V128);
                let add = vl!(lane; I8 => I8x16Add, I16 => I16x8Add,
                          I32 => I32x4Add, I64 => I64x2Add);
                let v = self.op(add, &[e, d], WT::V128);
                self.set(f.dfg.inst_results(i)[0], v);
                return;
            }
            if matches!(op, P::Ishl | P::Ushr | P::Sshr) {
                let amt = self.cvt(b, WT::I32);
                let o = match op {
                    P::Ishl => {
                        vl!(lane; I8 => I8x16Shl, I16 => I16x8Shl, I32 => I32x4Shl, I64 => I64x2Shl)
                    }
                    P::Ushr => {
                        vl!(lane; I8 => I8x16ShrU, I16 => I16x8ShrU, I32 => I32x4ShrU, I64 => I64x2ShrU)
                    }
                    _ => {
                        vl!(lane; I8 => I8x16ShrS, I16 => I16x8ShrS, I32 => I32x4ShrS, I64 => I64x2ShrS)
                    }
                };
                let v = self.op(o, &[a, amt], WT::V128);
                self.set(f.dfg.inst_results(i)[0], v);
                return;
            }
            let o = match op {
                P::Iadd => {
                    vl!(lane; I8 => I8x16Add, I16 => I16x8Add, I32 => I32x4Add, I64 => I64x2Add)
                }
                P::Fadd => vl!(lane; F32 => F32x4Add, F64 => F64x2Add),
                P::Isub => {
                    vl!(lane; I8 => I8x16Sub, I16 => I16x8Sub, I32 => I32x4Sub, I64 => I64x2Sub)
                }
                P::Fsub => vl!(lane; F32 => F32x4Sub, F64 => F64x2Sub),
                P::Imul => vl!(lane; I16 => I16x8Mul, I32 => I32x4Mul, I64 => I64x2Mul),
                P::Fmul => vl!(lane; F32 => F32x4Mul, F64 => F64x2Mul),
                P::Fdiv => vl!(lane; F32 => F32x4Div, F64 => F64x2Div),
                P::Fmin => vl!(lane; F32 => F32x4Min, F64 => F64x2Min),
                P::Fmax => vl!(lane; F32 => F32x4Max, F64 => F64x2Max),
                P::Band => O::V128And,
                P::Bor => O::V128Or,
                P::Bxor => O::V128Xor,
                P::Swizzle => O::I8x16Swizzle,
                P::AvgRound => vl!(lane; I8 => I8x16AvgrU, I16 => I16x8AvgrU),
                P::UaddSat => vl!(lane; I8 => I8x16AddSatU, I16 => I16x8AddSatU),
                P::SaddSat => vl!(lane; I8 => I8x16AddSatS, I16 => I16x8AddSatS),
                P::UsubSat => vl!(lane; I8 => I8x16SubSatU, I16 => I16x8SubSatU),
                P::SsubSat => vl!(lane; I8 => I8x16SubSatS, I16 => I16x8SubSatS),
                P::Smin => vl!(lane; I8 => I8x16MinS, I16 => I16x8MinS, I32 => I32x4MinS),
                P::Smax => vl!(lane; I8 => I8x16MaxS, I16 => I16x8MaxS, I32 => I32x4MaxS),
                P::Umin => vl!(lane; I8 => I8x16MinU, I16 => I16x8MinU, I32 => I32x4MinU),
                P::Umax => vl!(lane; I8 => I8x16MaxU, I16 => I16x8MaxU, I32 => I32x4MaxU),
                _ => panic!("vec bin {op} {t}"),
            };
            let v = self.op(o, &[a, b], WT::V128);
            self.set(f.dfg.inst_results(i)[0], v);
        }

        /// Vector unary ops: splat, lane-wise arith, widen/narrow, converts.
        fn vun(
            &mut self,
            op: Opcode,
            t: ClType,
            rt: ClType,
            f: &ir::Function,
            i: CInst,
            arg: ir::Value,
        ) {
            use Opcode as P;
            let a = self.get1(arg);
            let r0 = f.dfg.inst_results(i)[0];
            let v = match op {
                // scalar_to_vector's other lanes are unspecified - splat is a
                // legal (and cheaper than lane-insert) implementation.
                P::Splat | P::ScalarToVector => {
                    let lane = rt.lane_type();
                    let x = self.cvt(a, wty(lane));
                    let o = vl!(lane; I8 => I8x16Splat, I16 => I16x8Splat,
                            I32 => I32x4Splat, I64 => I64x2Splat,
                            F32 => F32x4Splat, F64 => F64x2Splat);
                    self.op(o, &[x], WT::V128)
                }
                // v128-to-v128 bitcast is a reinterpret - same wasm value.
                P::Bitcast if t.is_vector() && rt.is_vector() => a,
                P::Bnot => self.op(O::V128Not, &[a], WT::V128),
                P::Ineg => {
                    let lane = t.lane_type();
                    let o = vl!(lane; I8 => I8x16Neg, I16 => I16x8Neg,
                            I32 => I32x4Neg, I64 => I64x2Neg);
                    self.op(o, &[a], WT::V128)
                }
                P::Iabs => {
                    let lane = t.lane_type();
                    let o = vl!(lane; I8 => I8x16Abs, I16 => I16x8Abs,
                            I32 => I32x4Abs, I64 => I64x2Abs);
                    self.op(o, &[a], WT::V128)
                }
                P::Fabs => {
                    let o = vl!(t.lane_type(); F32 => F32x4Abs, F64 => F64x2Abs);
                    self.op(o, &[a], WT::V128)
                }
                P::Fneg => {
                    let o = vl!(t.lane_type(); F32 => F32x4Neg, F64 => F64x2Neg);
                    self.op(o, &[a], WT::V128)
                }
                P::Sqrt | P::Ceil | P::Floor | P::Trunc | P::Nearest => {
                    let o = match op {
                        P::Sqrt => vl!(t.lane_type(); F32 => F32x4Sqrt, F64 => F64x2Sqrt),
                        P::Ceil => vl!(t.lane_type(); F32 => F32x4Ceil, F64 => F64x2Ceil),
                        P::Floor => vl!(t.lane_type(); F32 => F32x4Floor, F64 => F64x2Floor),
                        P::Trunc => vl!(t.lane_type(); F32 => F32x4Trunc, F64 => F64x2Trunc),
                        _ => vl!(t.lane_type(); F32 => F32x4Nearest, F64 => F64x2Nearest),
                    };
                    self.op(o, &[a], WT::V128)
                }
                P::Popcnt => {
                    let o = vl!(t.lane_type(); I8 => I8x16Popcnt);
                    self.op(o, &[a], WT::V128)
                }
                P::VhighBits => {
                    let o = vl!(t.lane_type(); I8 => I8x16Bitmask, I16 => I16x8Bitmask,
                            I32 => I32x4Bitmask, I64 => I64x2Bitmask);
                    self.op(o, &[a], WT::I32)
                }
                P::SwidenLow | P::SwidenHigh | P::UwidenLow | P::UwidenHigh => {
                    let lane = t.lane_type();
                    let o = match op {
                        P::SwidenLow => vl!(lane; I8 => I16x8ExtendLowI8x16S,
                                        I16 => I32x4ExtendLowI16x8S,
                                        I32 => I64x2ExtendLowI32x4S),
                        P::SwidenHigh => vl!(lane; I8 => I16x8ExtendHighI8x16S,
                                         I16 => I32x4ExtendHighI16x8S,
                                         I32 => I64x2ExtendHighI32x4S),
                        P::UwidenLow => vl!(lane; I8 => I16x8ExtendLowI8x16U,
                                        I16 => I32x4ExtendLowI16x8U,
                                        I32 => I64x2ExtendLowI32x4U),
                        _ => vl!(lane; I8 => I16x8ExtendHighI8x16U,
                             I16 => I32x4ExtendHighI16x8U,
                             I32 => I64x2ExtendHighI32x4U),
                    };
                    self.op(o, &[a], WT::V128)
                }
                P::Snarrow | P::Unarrow | P::Uunarrow => {
                    let lane = rt.lane_type();
                    let o = if op == P::Snarrow {
                        vl!(lane; I8 => I8x16NarrowI16x8S, I16 => I16x8NarrowI32x4S)
                    } else {
                        vl!(lane; I8 => I8x16NarrowI16x8U, I16 => I16x8NarrowI32x4U)
                    };
                    self.op(o, &[a], WT::V128)
                }
                P::IaddPairwise => {
                    let o = vl!(rt.lane_type(); I16 => I16x8ExtAddPairwiseI8x16S,
                            I32 => I32x4ExtAddPairwiseI16x8S);
                    self.op(o, &[a], WT::V128)
                }
                P::FcvtToSintSat | P::FcvtToUintSat => {
                    let o = match (op, t.lane_type(), rt.lane_type()) {
                        (P::FcvtToSintSat, clt::F32, clt::I32) => O::I32x4TruncSatF32x4S,
                        (P::FcvtToUintSat, clt::F32, clt::I32) => O::I32x4TruncSatF32x4U,
                        (P::FcvtToSintSat, clt::F64, clt::I32) => O::I32x4TruncSatF64x2SZero,
                        (P::FcvtToUintSat, clt::F64, clt::I32) => O::I32x4TruncSatF64x2UZero,
                        _ => panic!("vec fcvt {op} {t}->{rt}"),
                    };
                    self.op(o, &[a], WT::V128)
                }
                P::FcvtFromSint | P::FcvtFromUint => {
                    let o = match (op, t.lane_type(), rt.lane_type()) {
                        (P::FcvtFromSint, clt::I32, clt::F32) => O::F32x4ConvertI32x4S,
                        (P::FcvtFromUint, clt::I32, clt::F32) => O::F32x4ConvertI32x4U,
                        (P::FcvtFromSint, clt::I32, clt::F64) => O::F64x2ConvertLowI32x4S,
                        (P::FcvtFromUint, clt::I32, clt::F64) => O::F64x2ConvertLowI32x4U,
                        _ => panic!("vec fcvt {op} {t}->{rt}"),
                    };
                    self.op(o, &[a], WT::V128)
                }
                P::Fvdemote => self.op(O::F32x4DemoteF64x2Zero, &[a], WT::V128),
                P::FvpromoteLow => self.op(O::F64x2PromoteLowF32x4, &[a], WT::V128),
                P::VallTrue => {
                    let o = vl!(t.lane_type(); I8 => I8x16AllTrue,
                            I16 => I16x8AllTrue, I32 => I32x4AllTrue,
                            I64 => I64x2AllTrue);
                    self.op(o, &[a], WT::I32)
                }
                P::VanyTrue => self.op(O::V128AnyTrue, &[a], WT::I32),
                _ => panic!("vec unary {op} {t}->{rt}"),
            };
            self.set(r0, v);
        }

        /// Vector lane-wise integer compare: `cond` on vector type `t`,
        /// producing a v128 mask. i64x2 has no unsigned wasm compares.
        fn vicmp(&mut self, cond: ir::condcodes::IntCC, t: ClType, a: WV, b: WV) -> WV {
            use ir::condcodes::IntCC as CC;
            let ops: [Option<O>; 10] = match t.lane_type() {
                clt::I8 => [
                    O::I8x16Eq,
                    O::I8x16Ne,
                    O::I8x16LtS,
                    O::I8x16LtU,
                    O::I8x16GtS,
                    O::I8x16GtU,
                    O::I8x16LeS,
                    O::I8x16LeU,
                    O::I8x16GeS,
                    O::I8x16GeU,
                ]
                .map(Some),
                clt::I16 => [
                    O::I16x8Eq,
                    O::I16x8Ne,
                    O::I16x8LtS,
                    O::I16x8LtU,
                    O::I16x8GtS,
                    O::I16x8GtU,
                    O::I16x8LeS,
                    O::I16x8LeU,
                    O::I16x8GeS,
                    O::I16x8GeU,
                ]
                .map(Some),
                clt::I32 => [
                    O::I32x4Eq,
                    O::I32x4Ne,
                    O::I32x4LtS,
                    O::I32x4LtU,
                    O::I32x4GtS,
                    O::I32x4GtU,
                    O::I32x4LeS,
                    O::I32x4LeU,
                    O::I32x4GeS,
                    O::I32x4GeU,
                ]
                .map(Some),
                clt::I64 => [
                    Some(O::I64x2Eq),
                    Some(O::I64x2Ne),
                    Some(O::I64x2LtS),
                    None,
                    Some(O::I64x2GtS),
                    None,
                    Some(O::I64x2LeS),
                    None,
                    Some(O::I64x2GeS),
                    None,
                ],
                l => panic!("vicmp lane {l}"),
            };
            let k = match cond {
                CC::Equal => 0,
                CC::NotEqual => 1,
                CC::SignedLessThan => 2,
                CC::UnsignedLessThan => 3,
                CC::SignedGreaterThan => 4,
                CC::UnsignedGreaterThan => 5,
                CC::SignedLessThanOrEqual => 6,
                CC::UnsignedLessThanOrEqual => 7,
                CC::SignedGreaterThanOrEqual => 8,
                CC::UnsignedGreaterThanOrEqual => 9,
            };
            let mut a = a;
            let mut b = b;
            let mut k = k;
            if t.lane_type() == clt::I64 && matches!(k, 3 | 5 | 7 | 9) {
                // i64x2 has signed-only compares: flip both operands into
                // signed order by xoring the sign bit in every lane.
                let smin = self.op(
                    O::V128Const {
                        value: 0x8000_0000_0000_0000_8000_0000_0000_0000,
                    },
                    &[],
                    WT::V128,
                );
                a = self.op(O::V128Xor, &[a, smin], WT::V128);
                b = self.op(O::V128Xor, &[b, smin], WT::V128);
                k -= 1;
            }
            let o = ops[k].unwrap_or_else(|| panic!("vec icmp {cond} on {t}"));
            self.op(o, &[a, b], WT::V128)
        }

        /// Vector lane-wise float compare; NaN handling mirrors `fcmp` with
        /// v128 logic ops.
        fn vfcmp(&mut self, cond: ir::condcodes::FloatCC, t: ClType, a: WV, b: WV) -> WV {
            use ir::condcodes::FloatCC as CC;
            let (eq, ne, lt, le, gt, ge) = match t.lane_type() {
                clt::F32 => (
                    O::F32x4Eq,
                    O::F32x4Ne,
                    O::F32x4Lt,
                    O::F32x4Le,
                    O::F32x4Gt,
                    O::F32x4Ge,
                ),
                clt::F64 => (
                    O::F64x2Eq,
                    O::F64x2Ne,
                    O::F64x2Lt,
                    O::F64x2Le,
                    O::F64x2Gt,
                    O::F64x2Ge,
                ),
                l => panic!("vfcmp lane {l}"),
            };
            let uno = |cx: &mut Self| -> WV {
                let l = cx.op(ne, &[a, a], WT::V128);
                let r = cx.op(ne, &[b, b], WT::V128);
                cx.op(O::V128Or, &[l, r], WT::V128)
            };
            match cond {
                CC::Ordered => {
                    let u = uno(self);
                    self.op(O::V128Not, &[u], WT::V128)
                }
                CC::Unordered => uno(self),
                CC::Equal => self.op(eq, &[a, b], WT::V128),
                CC::NotEqual => self.op(ne, &[a, b], WT::V128),
                CC::LessThan => self.op(lt, &[a, b], WT::V128),
                CC::LessThanOrEqual => self.op(le, &[a, b], WT::V128),
                CC::GreaterThan => self.op(gt, &[a, b], WT::V128),
                CC::GreaterThanOrEqual => self.op(ge, &[a, b], WT::V128),
                CC::OrderedNotEqual => {
                    let x = self.op(ne, &[a, b], WT::V128);
                    let u = uno(self);
                    let o = self.op(O::V128Not, &[u], WT::V128);
                    self.op(O::V128And, &[x, o], WT::V128)
                }
                CC::UnorderedOrEqual
                | CC::UnorderedOrLessThan
                | CC::UnorderedOrLessThanOrEqual
                | CC::UnorderedOrGreaterThan
                | CC::UnorderedOrGreaterThanOrEqual => {
                    let o = match cond {
                        CC::UnorderedOrEqual => eq,
                        CC::UnorderedOrLessThan => lt,
                        CC::UnorderedOrLessThanOrEqual => le,
                        CC::UnorderedOrGreaterThan => gt,
                        _ => ge,
                    };
                    let x = self.op(o, &[a, b], WT::V128);
                    let u = uno(self);
                    self.op(O::V128Or, &[x, u], WT::V128)
                }
            }
        }

        /// `extractlane`: wasm lane extract; int lanes produce a canonical
        /// (zero-extended) narrow rep.
        fn extract_lane(&mut self, t: ClType, arg: ir::Value, lane: u8) -> WV {
            let a = self.get1(arg);
            match t.lane_type() {
                clt::I8 => self.op(O::I8x16ExtractLaneU { lane }, &[a], WT::I32),
                clt::I16 => self.op(O::I16x8ExtractLaneU { lane }, &[a], WT::I32),
                clt::I32 => self.op(O::I32x4ExtractLane { lane }, &[a], WT::I32),
                clt::I64 => self.op(O::I64x2ExtractLane { lane }, &[a], WT::I64),
                clt::F32 => self.op(O::F32x4ExtractLane { lane }, &[a], WT::F32),
                clt::F64 => self.op(O::F64x2ExtractLane { lane }, &[a], WT::F64),
                l => panic!("extractlane {l}"),
            }
        }

        /// `insertlane x, y, idx`: wasm `replace_lane` truncates/wraps narrow
        /// and wide scalars itself.
        fn insert_lane(&mut self, t: ClType, x: ir::Value, y: ir::Value, lane: u8) -> WV {
            let a = self.get1(x);
            let v = self.get1(y);
            let (v, o) = match t.lane_type() {
                clt::I8 => (self.cvt(v, WT::I32), O::I8x16ReplaceLane { lane }),
                clt::I16 => (self.cvt(v, WT::I32), O::I16x8ReplaceLane { lane }),
                clt::I32 => (self.cvt(v, WT::I32), O::I32x4ReplaceLane { lane }),
                clt::I64 => (self.cvt(v, WT::I64), O::I64x2ReplaceLane { lane }),
                clt::F32 => (self.cvt(v, WT::F32), O::F32x4ReplaceLane { lane }),
                clt::F64 => (self.cvt(v, WT::F64), O::F64x2ReplaceLane { lane }),
                l => panic!("insertlane {l}"),
            };
            self.op(o, &[a, v], WT::V128)
        }

        /// Build a v128 from scalarized per-lane wasm values (`splat` the
        /// first lane, then `replace_lane` the rest).
        fn pack_lanes(&mut self, lane: ClType, vs: &[WV]) -> WV {
            let splat = vl!(lane; I8 => I8x16Splat, I16 => I16x8Splat,
                    I32 => I32x4Splat, I64 => I64x2Splat,
                    F32 => F32x4Splat, F64 => F64x2Splat);
            let first = self.cvt(vs[0], wty(lane));
            let mut v = self.op(splat, &[first], WT::V128);
            for (k, &x) in vs.iter().enumerate().skip(1) {
                let (x, o) = match lane {
                    clt::I8 => (self.cvt(x, WT::I32), O::I8x16ReplaceLane { lane: k as u8 }),
                    clt::I16 => (self.cvt(x, WT::I32), O::I16x8ReplaceLane { lane: k as u8 }),
                    clt::I32 => (self.cvt(x, WT::I32), O::I32x4ReplaceLane { lane: k as u8 }),
                    clt::I64 => (self.cvt(x, WT::I64), O::I64x2ReplaceLane { lane: k as u8 }),
                    clt::F32 => (self.cvt(x, WT::F32), O::F32x4ReplaceLane { lane: k as u8 }),
                    clt::F64 => (self.cvt(x, WT::F64), O::F64x2ReplaceLane { lane: k as u8 }),
                    l => panic!("pack_lanes {l}"),
                };
                v = self.op(o, &[v, x], WT::V128);
            }
            v
        }

        /// `extractlane` on a wasm value already in hand (the `ir::Value`
        /// variant above resolves `arg` first).
        fn extract_lane_v(&mut self, lane: ClType, a: WV, k: u8) -> WV {
            match lane {
                clt::I8 => self.op(O::I8x16ExtractLaneU { lane: k }, &[a], WT::I32),
                clt::I16 => self.op(O::I16x8ExtractLaneU { lane: k }, &[a], WT::I32),
                clt::I32 => self.op(O::I32x4ExtractLane { lane: k }, &[a], WT::I32),
                clt::I64 => self.op(O::I64x2ExtractLane { lane: k }, &[a], WT::I64),
                clt::F32 => self.op(O::F32x4ExtractLane { lane: k }, &[a], WT::F32),
                clt::F64 => self.op(O::F64x2ExtractLane { lane: k }, &[a], WT::F64),
                l => panic!("extract_lane_v {l}"),
            }
        }

        /// i128 binary ops on (lo,hi) pairs.
        fn i128_bin(&mut self, op: Opcode, f: &ir::Function, i: CInst, a: ir::Value, b: ir::Value) {
            use Opcode as P;
            let (al, ah) = self.pair(a);
            // Shift ops may carry a narrow (non-i128) amount operand.
            let bvs = self.get(b).clone();
            let bl = bvs[0];
            let bh = if bvs.len() > 1 { bvs[1] } else { bl };
            let r0 = f.dfg.inst_results(i)[0];
            match op {
                P::Iadd => {
                    let lo = self.op(O::I64Add, &[al, bl], WT::I64);
                    let hi = self.op(O::I64Add, &[ah, bh], WT::I64);
                    let c = self.op(O::I64LtU, &[lo, al], WT::I32);
                    let c64 = self.op(O::I64ExtendI32U, &[c], WT::I64);
                    let hi = self.op(O::I64Add, &[hi, c64], WT::I64);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Isub => {
                    let lo = self.op(O::I64Sub, &[al, bl], WT::I64);
                    let bor = self.op(O::I64GtU, &[lo, al], WT::I32);
                    let bor = self.op(O::I64ExtendI32U, &[bor], WT::I64);
                    let hi = self.op(O::I64Sub, &[ah, bh], WT::I64);
                    let hi = self.op(O::I64Sub, &[hi, bor], WT::I64);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Imul | P::Udiv | P::Sdiv | P::Urem | P::Srem => {
                    let name = match op {
                        P::Imul => "__multi3",
                        P::Udiv => "__udivti3",
                        P::Sdiv => "__divti3",
                        P::Urem => "__umodti3",
                        _ => "__modti3",
                    };
                    let vs = self.libcall1(
                        name,
                        &[clt::I128],
                        &[clt::I128, clt::I128],
                        &[al, ah, bl, bh],
                    );
                    self.setv(r0, vs);
                }
                P::Band => {
                    let lo = self.op(O::I64And, &[al, bl], WT::I64);
                    let hi = self.op(O::I64And, &[ah, bh], WT::I64);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Bor => {
                    let lo = self.op(O::I64Or, &[al, bl], WT::I64);
                    let hi = self.op(O::I64Or, &[ah, bh], WT::I64);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Bxor => {
                    let lo = self.op(O::I64Xor, &[al, bl], WT::I64);
                    let hi = self.op(O::I64Xor, &[ah, bh], WT::I64);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Ishl | P::Ushr | P::Sshr => {
                    // Lowered inline: the compiler-rt shift helpers
                    // themselves contain i128 shifts, so libcalling
                    // would recurse.
                    let amt = self.cvt(bl, WT::I64);
                    let (lo, hi) = self.sh128(op == P::Ishl, op == P::Sshr, al, ah, amt);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Rotl | P::Rotr => {
                    // rot(x,k) = shift(x,k) | shift(x,128-k), counts mod 128.
                    let m = self.i64c(127);
                    let amt64 = self.cvt(bl, WT::I64);
                    let amt = self.op(O::I64And, &[amt64, m], WT::I64);
                    let c128 = self.i64c(128);
                    let neg0 = self.op(O::I64Sub, &[c128, amt], WT::I64);
                    let m2 = self.i64c(127);
                    let neg = self.op(O::I64And, &[neg0, m2], WT::I64);
                    let rotl = op == P::Rotl;
                    let (x0, x1) = self.sh128(rotl, false, al, ah, amt);
                    let (y0, y1) = self.sh128(!rotl, false, al, ah, neg);
                    let lo = self.op(O::I64Or, &[x0, y0], WT::I64);
                    let hi = self.op(O::I64Or, &[x1, y1], WT::I64);
                    self.setv(r0, smallvec![lo, hi]);
                }
                P::Umulhi | P::Smulhi => panic!("i128 mulhi (no __umulh128)"),
                P::UaddOverflow | P::SaddOverflow | P::UsubOverflow | P::SsubOverflow => {
                    // (result, flag): high-word sign test.
                    // Flag is a canonical i8.
                    let r1 = f.dfg.inst_results(i)[1];
                    let is_add = matches!(op, P::UaddOverflow | P::SaddOverflow);
                    let signed = matches!(op, P::SaddOverflow | P::SsubOverflow);
                    let (lo, hi) = if is_add {
                        let lo = self.op(O::I64Add, &[al, bl], WT::I64);
                        let c = self.op(O::I64LtU, &[lo, al], WT::I32);
                        let c64 = self.op(O::I64ExtendI32U, &[c], WT::I64);
                        let hi = self.op(O::I64Add, &[ah, bh], WT::I64);
                        let hi = self.op(O::I64Add, &[hi, c64], WT::I64);
                        (lo, hi)
                    } else {
                        let lo = self.op(O::I64Sub, &[al, bl], WT::I64);
                        let bor = self.op(O::I64GtU, &[lo, al], WT::I32);
                        let bor = self.op(O::I64ExtendI32U, &[bor], WT::I64);
                        let hi = self.op(O::I64Sub, &[ah, bh], WT::I64);
                        let hi = self.op(O::I64Sub, &[hi, bor], WT::I64);
                        (lo, hi)
                    };
                    let flag = if signed {
                        // add: (a^r)&(b^r) < 0; sub: (a^b)&(a^r) < 0.
                        let x = self.op(O::I64Xor, &[ah, hi], WT::I64);
                        let y = if is_add {
                            self.op(O::I64Xor, &[bh, hi], WT::I64)
                        } else {
                            self.op(O::I64Xor, &[ah, bh], WT::I64)
                        };
                        let m = self.op(O::I64And, &[x, y], WT::I64);
                        let z = self.i64c(0);
                        self.op(O::I64LtS, &[m, z], WT::I32)
                    } else if is_add {
                        self.ltu128(hi, lo, ah, al)
                    } else {
                        self.ltu128(ah, al, bh, bl)
                    };
                    self.setv(r0, smallvec![lo, hi]);
                    self.set(r1, flag);
                }
                P::UmulOverflow | P::SmulOverflow => {
                    // __rust_{i,u}128_mulo(a, b, &mut flag) -> i128: sret slot,
                    // four i64 args, flag out-param slot.
                    let r1 = f.dfg.inst_results(i)[1];
                    let name = if op == P::SmulOverflow {
                        "__rust_i128_mulo"
                    } else {
                        "__rust_u128_mulo"
                    };
                    let so = self.frame_slot(16, 16);
                    let fo = self.frame_slot(4, 4);
                    let sc = self.i32c(so);
                    let sa = self.op(O::I32Add, &[self.fp, sc], WT::I32);
                    let fc = self.i32c(fo);
                    let fa = self.op(O::I32Add, &[self.fp, fc], WT::I32);
                    let idx = self.o.libfunc(
                        name,
                        SignatureData {
                            params: vec![WT::I32, WT::I64, WT::I64, WT::I64, WT::I64, WT::I32],
                            returns: vec![],
                        },
                    );
                    self.opn(
                        O::Call {
                            function_index: Func::new(idx),
                        },
                        &[sa, al, ah, bl, bh, fa],
                        &[],
                    );
                    let lo = self.load(clt::I64, sa, 0);
                    let hi = self.load(clt::I64, sa, 8);
                    let flag = self.load(clt::I32, fa, 0);
                    self.setv(r0, smallvec![lo, hi]);
                    self.set(r1, flag);
                }
                _ => panic!("i128 op {op}"),
            }
        }

        /// Flattened, coercion-normalised wasm args for a call with `sig`.
        fn call_args(&mut self, sig: &ir::Signature, args: &[ir::Value]) -> Vec<WV> {
            let mut out = Vec::new();
            for (p, v) in sig.params.iter().zip(args.iter()) {
                let ws: Vals = self.get(*v).clone();
                let mut want = Vec::new();
                cwtys(p.value_type, &mut want);
                out.extend(ws.iter().zip(want.iter()).map(|(&x, &t)| self.cvt(x, t)));
            }
            out
        }

        /// Multi-result `call` binding: results map 1:1 to wasm return values.
        fn bind_call(&mut self, f: &ir::Function, i: CInst, vs: Vals) {
            self.bind(f, i, vs);
        }

        /// Values for `return`/`return_call` in wasm return order.
        fn ret_vals(&mut self, f: &ir::Function, args: &[ir::Value]) -> Vals {
            let _ = f;
            let mut out = Vals::new();
            for a in args {
                out.extend(self.get(*a).iter().copied());
            }
            out
        }

        /// Terminate the current block with a return of `vs`, then end the
        /// function frame (stack pointer restore happens here).
        fn ret(&mut self, vs: &[WV]) {
            let cur = self.cur;
            // Restore SP to the entry value (function epilogue shape):
            // `inline_site`'s frame folding only drops restores whose value
            // derives from the callee's folded frame chain â€” a `GlobalGet sp +
            // frame` form would survive the splice and ratchet SP upward on
            // every inlined call.
            let (sp, sp0) = (self.o.sp, self.sp0);
            self.op0(O::GlobalSet { global_index: sp }, &[sp0]);
            self.b.set_terminator(
                cur,
                Terminator::Return {
                    values: vs.to_vec(),
                },
            );
        }
    }

    /// The wasm path: lower_clif_wasm gives each defined function's
    /// optimized CLIF + FuncId/DataId symbol tables; CX translates them to
    /// waffle; `emit_obj` finishes the module.
    ///
    /// Everything read from the pliron `Context` (the signatures, the link
    /// and custom sections) is taken before lowering, so under
    /// `PLIRON_LOWERFREE` `lower_clif_wasm` can drop an owned `Context` as
    /// soon as phase (A) is done, and each function's CLIF is dropped as
    /// soon as it is translated.
    pub(crate) fn lower_to_wasm_clif(
        mut ctx: crate::lower::CtxSlot<'_>,
        st: &State,
        host: &dyn crate::lower::Host,
        name: &str,
        features: &[String],
        unwind: bool,
    ) -> Vec<u8> {
        let simd = features.iter().any(|f| f == "simd128");
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

        // Last reads of the pliron `Context` before lowering: the
        // signatures and the module tail's sections.
        let tail = {
            let c = ctx.get();
            for (n, f) in &st.funcs {
                if st.dead_fns.contains(n) {
                    continue;
                }
                // Declare the signature from the pliron leaf-flattening; the
                // CLIF sig is the same leaf set in the same order.
                let sig = o.sig(wsig(c, f.ty));
                o.funcs.push(FDecl {
                    name: n.clone(),
                    sig,
                    body: None,
                    always: f.always_inline,
                    hint: f.inline_hint,
                    no_inline: f.no_inline,
                });
                o.fidx.insert(n.clone(), o.funcs.len() - 1);
            }
            ModTail::new(c, st)
        };
        let t0 = std::time::Instant::now();
        let wc = crate::lower::lower_clif_wasm(&mut ctx, st, host, name, unwind, simd);
        pliron_cfg::trace("*wasm-clif*", t0.elapsed().as_secs_f64() * 1e3, name);
        let t0 = std::time::Instant::now();
        let crate::lower::WasmClif {
            funcs,
            fn_names,
            data_names,
        } = wc;
        let verbose = pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::wasm", tracing::Level::DEBUG);
        let mut stubs = 0;
        let mut defined = 0;
        // By value: a function's CLIF is dropped once translated.
        for (n, mut cf) in funcs {
            defined += 1;
            let i = o.fidx[&n];
            let sig = o.funcs[i].sig;
            if verbose {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::wasm","pliron-wasm: cx {n}")
                };
            }
            let body = match guarded(|| {
                let mut cx = CX::new(&mut o, sig, &fn_names, &data_names);
                cx.name = n.clone();
                cx.translate(&mut cf)
            }) {
                Ok(b) => b,
                Err(e) => {
                    stubs += 1;
                    if let Some(f) = crate::env_get("PLIRON_WASM_STUBLOG") {
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
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::wasm","pliron-wasm: stub {n}: {e}")
                        };
                    }
                    stub(&o.m, sig)
                }
            };
            o.funcs[i].body = Some(body);
        }
        pliron_cfg::trace("*wasm-translate*", t0.elapsed().as_secs_f64() * 1e3, name);
        drop((fn_names, data_names));
        pliron_cfg::mark(&format!("wasm-translate {name}"));
        let t0 = std::time::Instant::now();
        let obj = emit_obj(o, tail, st, name, features, stubs, defined, verbose);
        pliron_cfg::trace("*wasm-emit*", t0.elapsed().as_secs_f64() * 1e3, name);
        obj
    }
}

pub(crate) use clif_emit::lower_to_wasm_clif;
