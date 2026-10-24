//! Source locations for DWARF (`-g`): the per-CGU tables behind
//! `Location::Dbg(id)` on pliron ops.
//!
//! The Builder interns one [`Loc`] per distinct (file, line, column,
//! scope, inlined-at) that rustc hands it and stamps each op it emits
//! with the id. Lowering copies the id into the CLIF srcloc, so after
//! compilation every machine-code range maps back to a `Loc`, which
//! `pliron-clif-lower`'s DWARF writer turns into line rows and inlined
//! subroutine trees. Id 0 means "no location" in every table.

use pliron::context::Context;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::ops::{GetElementPtrOp, StoreOp};
use rustc_hash::FxHashMap;

use crate::state::State;

/// A source position in a scope. `inlined_at` is the call site (a [`Loc`]
/// id in the caller's scope) when `scope` was inlined, else 0.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Loc {
    pub file: u32,
    pub line: u32,
    pub col: u32,
    /// The function this position belongs to: a [`DbgInfo::subs`] index.
    pub scope: u32,
    pub inlined_at: u32,
}

/// A function as debug info names it (`DW_TAG_subprogram`).
#[derive(Clone, Debug, Default)]
pub struct Sub {
    /// Path with generic args, e.g. `alloc::vec::Vec<u8>::push`.
    pub name: String,
    /// The mangled symbol, for debuggers that key frames on it.
    pub linkage: String,
    pub file: u32,
    pub line: u32,
}

/// A type as DWARF describes it, built on the rustc side from the layout.
/// Type ids index [`DbgInfo::types`]; 0 means `void`.
#[derive(Clone, Debug)]
pub enum Type {
    /// `DW_TAG_base_type`; `enc` is a `DW_ATE_*` value.
    Base { name: String, size: u32, enc: u8 },
    Ptr { name: String, pointee: u32 },
    /// Structs, tuples, closures, unions (all fields at offset 0), and
    /// the fat-pointer pairs.
    Struct { name: String, size: u32, align: u32, fields: Vec<Field>, union: bool },
    Array { name: String, elem: u32, len: u64, size: u32 },
    /// Fieldless enum: `(name, value)` per variant, over a `size`-byte tag.
    CEnum { name: String, size: u32, signed: bool, variants: Vec<(String, i128)> },
    /// Anything not described yet: just a name and a size.
    Opaque { name: String, size: u32 },
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub ty: u32,
    pub offset: u32,
}

/// A source variable (`DW_TAG_variable` / `DW_TAG_formal_parameter`).
#[derive(Clone, Debug)]
pub struct Var {
    pub name: String,
    pub ty: u32,
    pub scope: u32,
    /// 1-based argument index; 0 for a local.
    pub arg: u32,
    pub file: u32,
    pub line: u32,
}

/// "Variable `var` lives `offset` bytes into an alloca" (rustc's
/// `dbg_var_addr`), as seen at `loc`. `derefs`: then follow a pointer,
/// adding each offset (by-reference captures and arguments).
#[derive(Clone, Debug)]
pub struct Decl {
    pub var: u32,
    pub loc: u32,
    pub offset: u32,
    pub derefs: Vec<u32>,
    /// The alloca's size when declared: an erased alloca's `Value` can be
    /// reused, so lowering only trusts a decl whose alloca still matches.
    pub size: u64,
}

/// Bytes `[offset, offset + size)` of variable `var`, in the instance
/// whose declaration location is `loc` (so an inlined copy is a different
/// piece). A value carrying the piece's id in `Context::dbg_notes` holds
/// those bytes from its definition on; lowering turns each id into a
/// Cranelift value label, and the DWARF writer the label's machine-code
/// ranges into the variable's location list. One id per piece, not per
/// assignment: Cranelift ends a label's range where the next value with
/// the same label is defined, which is what a reassignment means.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Piece {
    pub var: u32,
    pub loc: u32,
    pub offset: u32,
    pub size: u32,
}

#[derive(Default)]
pub struct Pieces {
    pub list: Vec<Piece>,
    ids: FxHashMap<Piece, u32>,
    /// Pieces lowering gave a value label: some value holding them is
    /// computed at run time (as opposed to only constants).
    pub labelled: rustc_hash::FxHashSet<u32>,
}

impl Pieces {
    pub fn id(&mut self, p: Piece) -> u32 {
        *self.ids.entry(p).or_insert_with(|| {
            self.list.push(p);
            self.list.len() as u32 - 1
        })
    }
}

#[derive(Default)]
pub struct DbgInfo {
    /// `DW_AT_name` / `DW_AT_comp_dir` / `DW_AT_producer` of the CU.
    pub cu_name: String,
    pub comp_dir: String,
    pub producer: String,
    /// Source paths as debug info spells them (after path remapping).
    pub files: Vec<String>,
    file_ids: FxHashMap<String, u32>,
    pub subs: Vec<Sub>,
    pub locs: Vec<Loc>,
    loc_ids: FxHashMap<Loc, u32>,
    /// Defined function symbol → its own subprogram.
    pub fn_sub: FxHashMap<String, u32>,
    pub types: Vec<Type>,
    pub vars: Vec<Var>,
    pub decls: Vec<Decl>,
    /// Alloca result → its decl group: the [`DbgInfo::groups`] index
    /// lowering tags the alloca's stack slot with.
    pub alloca_decls: FxHashMap<Value, u32>,
    /// The decls naming one alloca.
    pub groups: Vec<Vec<u32>>,
    /// `-O0` with full debuginfo: keep variable slots in memory (slotprom
    /// skips them) so every local has a location, as LLVM's allocas do.
    pub keep_slots: bool,
    /// Variable pieces held in SSA values (see [`Piece`]). Behind a lock
    /// because lowering (which only borrows `State`) interns the pieces of
    /// stores into promoted allocas.
    pub pieces: std::sync::Mutex<Pieces>,
    /// Track variables through SSA values (pieces); `PLIRON_DBGVALS=0`
    /// turns it off (the dropped spills' variables then read as
    /// unavailable).
    pub values: bool,
}

impl DbgInfo {
    pub fn new(cu_name: String, comp_dir: String, producer: String) -> Self {
        DbgInfo {
            cu_name,
            comp_dir,
            producer,
            files: vec![String::new()],
            subs: vec![Sub::default()],
            locs: vec![Loc::default()],
            types: vec![Type::Opaque { name: "void".into(), size: 0 }],
            ..Default::default()
        }
    }

    pub fn file(&mut self, path: &str) -> u32 {
        if let Some(&i) = self.file_ids.get(path) {
            return i;
        }
        let i = self.files.len() as u32;
        self.files.push(path.to_string());
        self.file_ids.insert(path.to_string(), i);
        i
    }

    pub fn sub(&mut self, s: Sub) -> u32 {
        self.subs.push(s);
        self.subs.len() as u32 - 1
    }

    pub fn loc(&mut self, l: Loc) -> u32 {
        if let Some(&i) = self.loc_ids.get(&l) {
            return i;
        }
        let i = self.locs.len() as u32;
        self.locs.push(l);
        self.loc_ids.insert(l, i);
        i
    }

    /// The byte size of type `t`.
    pub fn type_size(&self, t: u32) -> u32 {
        match &self.types[t as usize] {
            Type::Base { size, .. }
            | Type::Struct { size, .. }
            | Type::Array { size, .. }
            | Type::CEnum { size, .. }
            | Type::Opaque { size, .. } => *size,
            Type::Ptr { .. } => 8,
        }
    }

    /// The pieces of the variables `group` (an alloca's decls) puts at
    /// `[at, at + size)` of the alloca: one per decl the range falls in.
    pub fn group_pieces(&self, group: u32, at: u64, size: u64, out: &mut Vec<Piece>) {
        for &di in &self.groups[group as usize] {
            let d = &self.decls[di as usize];
            if !d.derefs.is_empty() || at < d.offset as u64 {
                continue;
            }
            let off = at - d.offset as u64;
            let vsize = self.type_size(self.vars[d.var as usize].ty) as u64;
            if off + size <= vsize && size > 0 {
                out.push(Piece { var: d.var, loc: d.loc, offset: off as u32, size: size as u32 });
            }
        }
    }

    /// Record `decl` as naming `alloca`.
    pub fn declare(&mut self, alloca: Value, decl: Decl) {
        self.decls.push(decl);
        let id = self.decls.len() as u32 - 1;
        let g = *self.alloca_decls.entry(alloca).or_insert_with(|| {
            self.groups.push(Vec::new());
            self.groups.len() as u32 - 1
        });
        self.groups[g as usize].push(id);
    }

    /// `l` as seen after inlining its function at call site `site`: the
    /// end of its inlined-at chain (the frame that was the callee's root)
    /// now continues at `site`. `memo` caches per call site.
    pub fn inline_at(&mut self, l: u32, site: u32, memo: &mut FxHashMap<u32, u32>) -> u32 {
        if l == 0 || site == 0 {
            return l;
        }
        if let Some(&r) = memo.get(&l) {
            return r;
        }
        let mut loc = self.locs[l as usize];
        loc.inlined_at = if loc.inlined_at == 0 {
            site
        } else {
            self.inline_at(loc.inlined_at, site, memo)
        };
        let r = self.loc(loc);
        memo.insert(l, r);
        r
    }
}

/// Before optimizing: erase each declared alloca nothing reads — only
/// stores into it, directly or through a GEP. At full debuginfo rustc
/// spills SSA locals into fresh allocas just to give them a location
/// (`spill_operand_to_stack`); left in, they skew inlining costs, slot
/// counts and load forwarding, so `-g` would change the code (it must
/// not; test.sh `debuginfo`). Instead each stored value carries its
/// variable pieces as `Context::dbg_notes` — not uses, so no pass sees
/// them — and the variables get location lists from wherever the values
/// end up. (A declared argument slot that is never read goes too — no
/// build reads it either.)
pub fn drop_spills(ctx: &mut Context, st: &mut State) {
    // Out of `st` for the walk: `gep_offset` reads the rest of it.
    let Some(mut d) = st.dbg.take() else {
        return;
    };
    ctx.dbg_notes.get_mut().clear();
    let mut pieces = Vec::new();
    let is_store_into = |ctx: &Context, u: &pliron::value::Use<Value>| {
        Operation::get_op::<StoreOp>(u.user_op(), ctx).is_some() && u.find_index(ctx) == 1
    };
    let allocas: Vec<Value> = d.alloca_decls.keys().copied().collect();
    let mut dropped = Vec::new();
    for a in allocas {
        let Some(def) = a.defining_op() else {
            continue;
        };
        let mut stores = Vec::new();
        let mut geps = Vec::new();
        let ok = a.uses(ctx).iter().all(|u| {
            let op = u.user_op();
            if is_store_into(ctx, u) {
                stores.push(op);
                true
            } else if Operation::get_op::<GetElementPtrOp>(op, ctx).is_some() && u.find_index(ctx) == 0 {
                let r = op.deref(ctx).get_result(ctx, 0);
                let us = r.uses(ctx);
                let ok = us.iter().all(|u| is_store_into(ctx, u));
                stores.extend(us.iter().map(|u| u.user_op()));
                geps.push(op);
                ok
            } else {
                false
            }
        });
        if !ok || stores.is_empty() {
            continue;
        }
        // The stored values now carry the variables (as LLVM turns a
        // promoted `dbg.declare` into `dbg.value`s of the stores).
        let g = d.alloca_decls[&a];
        for &op in stores.iter().filter(|_| d.values) {
            let o = op.deref(ctx);
            let p = o.get_operand(ctx, 1);
            let at = if p == a {
                Some(0)
            } else {
                p.defining_op().and_then(|g| crate::sroa::gep_offset(ctx, st, g))
            };
            let Some(at) = at.filter(|&x| x >= 0) else {
                continue;
            };
            let v = o.get_operand(ctx, 0);
            let size = crate::types::size_align(ctx, v.get_type(ctx)).0;
            pieces.clear();
            d.group_pieces(g, at as u64, size, &mut pieces);
            if pieces.is_empty() {
                continue;
            }
            let tab = d.pieces.get_mut().unwrap();
            let mut notes = ctx.dbg_notes.borrow_mut();
            let e = notes.entry(v).or_default();
            for &pc in &pieces {
                e.push(tab.id(pc));
                if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::dbg", tracing::Level::DEBUG) {
                    tracing::debug!(target: "pliron::dbg", "spill: {} [{}+{}] = v{}", d.vars[pc.var as usize].name, pc.offset, pc.size, v.uid());
                }
            }
        }
        // Each store and GEP uses the pointer once, so there are no repeats.
        for op in stores.into_iter().chain(geps) {
            Operation::erase(op, ctx);
        }
        Operation::erase(def, ctx);
        dropped.push(a);
    }
    for a in dropped {
        d.alloca_decls.remove(&a);
        st.allocas.remove(a);
    }
    st.dbg = Some(d);
}
