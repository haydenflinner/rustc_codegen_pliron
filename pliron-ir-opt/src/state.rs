//! Per-CGU lowering state shared by the IR passes and the Cranelift
//! lowering: side tables keyed by pliron `Value`/`Operation`, plus the
//! rustc-independent helpers that read them.

use cranelift_codegen::isa::CallConv;
use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;
use pliron_llvm::attributes::AtomicRmwKindAttr;
use pliron_llvm::ops::FuncOp;
pub use pliron::cols::{ColMap, ColSet};

/// PLIRON: string-keyed companion to [ColMap] — intern `String` names to
/// dense u32 once (`ids`), values in a parallel Vec. `iter`/`keys`
/// follow intern (insertion) order, matching `IndexMap` semantics that
/// `funcs`/`globals` emission order relies on.
#[derive(Clone, Debug)]
pub struct SymMap<V> {
    ids: FxHashMap<String, u32>,
    names: Vec<String>,
    vals: Vec<Option<V>>,
}

impl<V> Default for SymMap<V> {
    fn default() -> Self {
        Self {
            ids: FxHashMap::default(),
            names: Vec::new(),
            vals: Vec::new(),
        }
    }
}

impl<V> SymMap<V> {
    #[inline]
    pub fn get(&self, k: &str) -> Option<&V> {
        self.ids.get(k).and_then(|&i| self.vals[i as usize].as_ref())
    }
    #[inline]
    pub fn get_mut(&mut self, k: &str) -> Option<&mut V> {
        match self.ids.get(k) {
            Some(&i) => self.vals[i as usize].as_mut(),
            None => None,
        }
    }
    /// IndexMap::get_key_value — the canonical stored key.
    pub fn get_key_value(&self, k: &str) -> Option<(&String, &V)> {
        self.ids
            .get(k)
            .and_then(|&i| self.vals[i as usize].as_ref().map(|v| (&self.names[i as usize], v)))
    }
    pub fn insert(&mut self, k: String, v: V) -> Option<V> {
        match self.ids.get(k.as_str()) {
            Some(&i) => self.vals[i as usize].replace(v),
            None => {
                let i = self.vals.len() as u32;
                self.ids.insert(k.clone(), i);
                self.names.push(k);
                self.vals.push(Some(v));
                None
            }
        }
    }
    #[inline]
    pub fn contains_key(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.vals.iter().flatten().count()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Insertion-ordered (`IndexMap` parity).
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.names
            .iter()
            .enumerate()
            .filter_map(|(i, n)| self.vals[i].as_ref().map(|v| (n, v)))
    }
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.iter().map(|(k, _)| k)
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }
}

impl<V> core::ops::Index<&str> for SymMap<V> {
    type Output = V;
    #[inline]
    fn index(&self, k: &str) -> &V {
        self.get(k).expect("SymMap index of absent key")
    }
}
impl<V> core::ops::Index<&String> for SymMap<V> {
    type Output = V;
    #[inline]
    fn index(&self, k: &String) -> &V {
        &self[k.as_str()]
    }
}
impl<V> core::ops::Index<String> for SymMap<V> {
    type Output = V;
    #[inline]
    fn index(&self, k: String) -> &V {
        &self[k.as_str()]
    }
}

impl<'a, V> IntoIterator for &'a SymMap<V> {
    type Item = (&'a String, &'a V);
    type IntoIter = Box<dyn Iterator<Item = Self::Item> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<V> FromIterator<(String, V)> for SymMap<V> {
    fn from_iter<I: IntoIterator<Item = (String, V)>>(it: I) -> Self {
        let mut m = Self::default();
        m.extend(it);
        m
    }
}

impl FromIterator<String> for SymSet {
    fn from_iter<I: IntoIterator<Item = String>>(it: I) -> Self {
        let mut s = Self::default();
        s.extend(it);
        s
    }
}

impl<V> Extend<(String, V)> for SymMap<V> {
    fn extend<I: IntoIterator<Item = (String, V)>>(&mut self, it: I) {
        for (k, v) in it {
            self.insert(k, v);
        }
    }
}

/// PLIRON: interned-string set — [SymMap] with `()` values.
#[derive(Clone, Debug, Default)]
pub struct SymSet(SymMap<()>);

impl SymSet {
    #[inline]
    pub fn contains(&self, k: &str) -> bool {
        self.0.contains_key(k)
    }
    pub fn insert(&mut self, k: String) -> bool {
        self.0.insert(k, ()).is_none()
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Extend<String> for SymSet {
    fn extend<I: IntoIterator<Item = String>>(&mut self, it: I) {
        for k in it {
            self.insert(k);
        }
    }
}
use rustc_hash::FxHashMap;


#[derive(Clone, Debug, PartialEq)]
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
    /// Plain `#[inline]` (LLVM `inlinehint`): bigger inlining budget.
    pub inline_hint: bool,
    /// `#[cold]`: blocks calling it are laid out out of line.
    pub cold: bool,
    /// Declared or inferred unable to unwind (see nounwind.rs).
    pub nounwind: bool,
    /// Backend type of the value returned through the sret pointer.
    pub sret_ty: Option<TypeHandle>,
    /// Params pointing at memory rustc marks `noalias readonly` (frozen `&T`):
    /// (param index, dereferenceable bytes), valid when the function has
    /// `nparams` params.
    pub frozen: Vec<(usize, u64)>,
    /// Params rustc marks `noalias`, by the same backend index as `frozen`:
    /// (param index, pointee size when rustc knows it, not `readonly`).
    pub noalias: Vec<(usize, u64, bool)>,
    /// Params rustc marks `nonnull`, by the same backend index as `frozen`.
    pub nonnull: Vec<usize>,
    pub nparams: usize,
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
    /// Whether the callee is a foreign (extern) function; only meaningful for
    /// C-variadic signatures, where foreign callees get real inline C-ABI
    /// arguments and Rust callees get a packed buffer pointer.
    pub foreign: bool,
    /// Explicit calling convention for the call (e.g. `extern "win64"` calls
    /// on non-Windows hosts). None = the default for the callee kind.
    pub cc: Option<CallConv>,
    /// `become f(...)`: emit a `return_call`/`return_call_indirect` (real TCO)
    /// instead of call+ret. Only honored when the caller's own convention is
    /// `CallConv::Tail` (Cranelift requires it).
    pub tail: bool,
    /// Per-operand `captures(none)` flags (indexed like the call's operand
    /// list), deduced by rustc from the callee's MIR. A non-captured pointer
    /// can't outlive the call, so slotmerge treats it as a bounded use
    /// rather than a function-end escape.
    pub nocap: Option<Box<[bool]>>,
    /// Per-operand `(size, align)` for pointer args with a sized pointee —
    /// `&T`/`&mut T` (Direct) cover `size_of::<T>()` bytes at `T`'s align,
    /// `Indirect` params cover the arg's own layout. Raw pointers get none:
    /// `slice.as_mut_ptr()` legally covers the whole buffer. SROA pins
    /// exactly this range of the underlying alloca when the arg can't be
    /// proven copyable, letting the rest of the object promote (LLVM's
    /// partition-pinning equivalent).
    pub arg_extent: Option<Box<[(u64, u64)]>>,
    /// Per-operand rustc `noalias` flags (indexed like `nocap`): the operand
    /// is a `&mut`/`&`-frozen/Box borrow rustc declares unaliased for the
    /// call. The borrow's scope ends when the call returns, so a retained
    /// copy can never be legally used to touch the pointee afterwards —
    /// SROA treats such pins as non-escaping and syncs around them.
    pub arg_noalias: Option<Box<[bool]>>,
    /// `black_box` barrier: the callee is a never-defined `__pliron_bb*`
    /// symbol; lowering emits the CLIF `black_box` op instead of a call.
    pub bb: bool,
    /// Resolved `llvm.call.callee` symbol at CallInfo creation. The attr
    /// is constructor-only (set in `CallOp::new`), so the memoized copy
    /// can't go stale; `direct_callee` reads this instead of the attr
    /// dict, and ops without a `calls` entry still fall back to it.
    pub callee: Option<String>,
}

#[derive(Default)]
pub struct State {
    pub consts: ColMap<Value, ConstVal>,
    pub funcs: SymMap<FuncInfo>,
    pub globals: SymMap<GlobalInfo>,
    pub ident_to_sym: FxHashMap<String, String>,
    pub sym_to_ident: FxHashMap<String, Identifier>,
    pub allocas: ColMap<Value, (u64, u64)>,
    /// Allocas lowered as Cranelift variables (see sroa.rs), with their value type.
    pub promoted: ColMap<Value, TypeHandle>,
    /// Fresh copies SROA makes for a diverging call; never split again.
    pub escape_copies: ColSet<Value>,
    pub calls: ColMap<Ptr<Operation>, CallInfo>,
    pub intrinsics: ColMap<Ptr<Operation>, String>,
    pub rmw: ColMap<Ptr<Operation>, AtomicRmwKindAttr>,
    pub strs: SymMap<Value>,
    pub counter: usize,
    pub cgu: String,
    pub asm: String,
    /// EII symbol aliases: (alias, aliasee, weak); emitted as extra object symbols.
    pub aliases: Vec<(String, String, bool)>,
    /// call op → (landing block, is catch_unwind catch-all)
    pub invokes: ColMap<Ptr<Operation>, (Ptr<BasicBlock>, bool)>,
    pub last_call: Option<Ptr<Operation>>,
    pub llvm_stubs: SymSet,
    /// Volatile loads/stores/mem intrinsics: memory passes must leave them alone.
    pub volatile: ColSet<Ptr<Operation>>,
    /// Loads whose (pointer) result rustc marked `!nonnull`.
    pub nonnull: ColSet<Ptr<Operation>>,
    /// Entry params of local functions that point at frozen memory, with
    /// their dereferenceable size (see `FuncInfo::frozen`).
    pub frozen: ColMap<Value, u64>,
    /// Entry params of local functions that rustc marks `noalias`:
    /// (dereferenceable bytes when known, writable when not `readonly`).
    pub noalias: ColMap<Value, (u64, bool)>,
    /// Functions that write no memory (`true`) or none before a normal return (nowrite.rs).
    pub nowrite: SymMap<bool>,
    /// Functions that provably come back (return/unwind/tail-call/UB) and
    /// behave deterministically — no volatile, atomic, asm, trap, indirect
    /// or unknown-callee interaction (willreturn.rs).
    pub willreturn: SymSet,
    /// Per-callee body-level capture analysis: param indices provably not
    /// retained past the call (nocapture.rs).
    pub nocapture: SymMap<rustc_hash::FxHashSet<u32>>,
    /// Bodies inferred to return a non-null pointer (nnret.rs); lowering
    /// seeds their call results into `FnLower::nonnull` for jumpthread.
    pub nnret: SymSet,
    /// Loads rustc marked `!range [0, 2)` (`bool`s): already 0 or 1.
    pub bool01: ColSet<Ptr<Operation>>,
    /// Args inside inlined clones that bind a `noalias readonly` callee
    /// param (`inline.rs`): arg → (leaf within the arg, dereferenceable
    /// bytes, region id). The pointee is frozen only for the clone's
    /// duration — the region id is the clone's entry block, so hoists can
    /// stay inside the scope (`frozen_loads`) rather than the function-wide
    /// `readonly can_move` flags `FnLower::frozen` gets.
    pub region_frozen: ColMap<Value, Vec<(u32, u64, Ptr<BasicBlock>)>>,
    /// Caller-side operands bound to `region_frozen` leaves: `*v` is
    /// frozen for the callee call's duration — exactly the clone's span —
    /// so the record is `(dereferenceable bytes, clone entry)`. Kept
    /// separate from `region_frozen`: those records seed `FnLower::noalias`
    /// on clone args, while operands are ordinary caller values that
    /// outlive the clone and must not claim function-wide `noalias`.
    pub region_frozen_op: ColMap<Value, Vec<(u64, Ptr<BasicBlock>)>>,
    /// Load ops inside inlined clones whose address provably derives from a
    /// `region_frozen` arg at a constant offset inside the pointee: op →
    /// region id (the clone's entry block). At lowering these seed
    /// `FnLower::scoped` — licm may hoist them without a clobber scan as
    /// long as the preheader stays inside the region: `noalias readonly`
    /// promises no write to the pointee for the call's duration.
    pub frozen_loads: ColMap<Ptr<Operation>, Ptr<BasicBlock>>,
    /// Loads rustc marked `!invariant.load` (`set_invariant_load`): every
    /// load from that address yields the same value, so the location can
    /// forward across arbitrary stores — `*pivot` in the partition loop
    /// stays live while the element swaps happen.
    pub invariant: ColSet<Ptr<Operation>>,
    /// Pliron blocks belonging to an inlined clone → the clone's entry
    /// block (the region id used by `frozen_loads`/`region_frozen`).
    pub region_blocks: ColMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
    /// Clone entry block → the enclosing clone's entry block (the region
    /// containing the callsite), or itself when the call sits in original
    /// code. Regions nest: a load tagged R may hoist anywhere in R, so a
    /// block whose own region is a descendant of R is in-region for it.
    pub region_parent: ColMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
    /// Args inside inlined clones that bind any `noalias` callee param
    /// (`&mut [T]` included — `frozen` is the `readonly` subset): arg →
    /// leaf within the arg. Each becomes a DISTINCT memory root at
    /// lowering (`FnLower::noalias`) — `partition`'s `v` and `pivot` must
    /// not merge even though `v.as_ptr() == pivot + 1`: `noalias` declares
    /// their pointees disjoint, and keeping the roots separate is what lets
    /// licm hoist the pivot load past the element swaps.
    pub region_noalias: ColMap<Value, Vec<u32>>,
    /// Entry params rustc marks `nonnull` (`&T`/`&str` data ptrs, `Box`,
    /// `NonNull`): a bitmask over the arg's leaf indices (see
    /// `FuncInfo::nonnull`).
    pub nnargs: ColMap<Value, u64>,
    /// `inbounds` GEPs: null only if their base is.
    pub inbounds: ColSet<Ptr<Operation>>,
    /// Local fns with no remaining references after inlining; not lowered.
    pub dead_fns: SymSet,
    /// Lower non-volatile loads/stores as `notrap` (set by finish_module at -O).
    pub notrap: bool,
    /// Run `jumpthread` on each lowered Cranelift function (-O, `PLIRON_JUMPTHREAD`).
    pub jumpthread: bool,

    /// Run `loadfwd` on each lowered Cranelift function (`-O`, `PLIRON_LOADFWD`).
    pub loadfwd: bool,
    /// Drop stores into unread, non-escaping stack slots (`PLIRON_SLOT_DSE`).
    pub slot_dse: bool,
    /// Dead store elimination on each lowered Cranelift function
    /// (`-O`, `PLIRON_DSE`).
    pub dse: bool,

    /// Run `clifpeep` on each lowered Cranelift function (`-O`, `PLIRON_PEEP`).
    pub peep: bool,

    /// Run `tailmerge` on each lowered Cranelift function (`-O`, `PLIRON_TAILMERGE`).
    pub tailmerge: bool,
    /// Run `unreach` on each lowered Cranelift function (`-O`, `PLIRON_UNREACH`).
    pub unreach: bool,
    /// Run `taildup` on each lowered Cranelift function (`PLIRON_TAILDUP=0` disables).
    pub taildup: bool,
    /// LICM on each lowered Cranelift function (`-O`, `PLIRON_LICM`).
    pub licm: bool,
    /// IV strength reduction (`-O`, `PLIRON_INDUCT`).
    pub indvars: bool,
    /// Loop -> memset/memcpy idiom (`-O`, `PLIRON_IDIOM`).
    pub loopidiom: bool,
    /// Elementwise loop vectorizer (`-O`, `PLIRON_VEC`; SIMD targets only).
    pub loopvec: bool,
    /// Store-group SLP vectorizer (`-O`, `PLIRON_SLP`; SIMD targets only).
    pub slp: bool,
    /// Loop versioning to hoist provable bounds checks (`-O`, `PLIRON_BCHECK`).
    pub bcheck: bool,
    /// Accumulator tail-recursion elimination (`-O`, `PLIRON_TAILREC`).
    pub tailrec: bool,
    /// cond_br op → expected condition value (`likely`/`unlikely`).
    pub expect: ColMap<Ptr<Operation>, bool>,
    /// `#[link(wasm_import_module = ..)]` functions: symbol -> (module, name).
    pub wasm_imports: SymMap<(String, String)>,
    /// Loaded PGO profile (`PLIRON_PGO_USE`): FuncOp → function record.
    /// Only functions whose CFG fingerprint matched are present.
    pub prof: ColMap<Ptr<Operation>, crate::pgo::ProfFn>,
    /// Block → entry count (loaded profile). The inliner propagates it
    /// to cloned blocks (scaled by callsite/callee-entry ratio), so
    /// inlined code carries the callee's counts into the caller.
    pub prof_bcount: ColMap<Ptr<BasicBlock>, u64>,
    /// Terminator → count per successor slot (from edge counters and
    /// single-pred block counts); drives profile-weighted block layout
    /// and `expect` seeding.
    pub prof_edges: ColMap<Ptr<Operation>, Vec<u64>>,
}

pub fn mask(bits: u128, w: u32) -> u128 {
    if w >= 128 {
        bits
    } else {
        bits & ((1u128 << w) - 1)
    }
}

/// if the leaf counts disagree.
pub(crate) fn leaf_arg(ctx: &Context, e: Ptr<BasicBlock>, leaf: usize) -> Option<(Value, usize)> {
    let mut off = 0usize;
    for a in e.deref(ctx).arguments() {
        let n = crate::types::leaves(ctx, a.get_type(ctx)).len();
        if leaf >= off && leaf < off + n {
            return Some((a, leaf - off));
        }
        off += n;
    }
    None
}

pub fn noalias_values(ctx: &Context, st: &State) -> Vec<(Value, u64, bool)> {
    let mut out = Vec::new();
    for f in st.funcs.values() {
        if f.noalias.is_empty() {
            continue;
        }
        let Some(e) = Operation::get_op::<FuncOp>(f.op, ctx).and_then(|o| o.get_entry_block(ctx))
        else {
            continue;
        };
        out.extend(
            f.noalias
                .iter()
                .filter_map(|&(i, d, w)| leaf_arg(ctx, e, i).map(|(a, _)| (a, d, w))),
        );
    }
    out
}

/// Entry-block params of bodies in this module that point at frozen memory.
pub fn frozen_values(ctx: &Context, st: &State) -> Vec<(Value, u64)> {
    let mut out = Vec::new();
    for f in st.funcs.values() {
        if f.frozen.is_empty() {
            continue;
        }
        let Some(e) = Operation::get_op::<FuncOp>(f.op, ctx).and_then(|o| o.get_entry_block(ctx))
        else {
            continue;
        };
        out.extend(
            f.frozen
                .iter()
                .filter_map(|&(i, s)| leaf_arg(ctx, e, i).map(|(a, _)| (a, s))),
        );
    }
    out
}

/// Entry-block params of bodies in this module that rustc marks `nonnull`,
/// as (arg, bitmask of nonnull leaf offsets within the arg).
pub fn nonnull_values(ctx: &Context, st: &State) -> Vec<(Value, u64)> {
    let mut out = Vec::new();
    for f in st.funcs.values() {
        if f.nonnull.is_empty() {
            continue;
        }
        let Some(e) = Operation::get_op::<FuncOp>(f.op, ctx).and_then(|o| o.get_entry_block(ctx))
        else {
            continue;
        };
        for &i in &f.nonnull {
            let Some((a, off)) = leaf_arg(ctx, e, i) else {
                continue;
            };
            let e = out.iter_mut().find(|(x, _)| *x == a);
            match e {
                Some((_, m)) => *m |= 1 << off,
                None => out.push((a, 1 << off)),
            }
        }
    }
    out
}
