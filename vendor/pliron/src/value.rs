// SPDX-License-Identifier: Apache-2.0
// Copyright (c) The pliron contributors

//! SSA [Value]s: Use-Def and Def-Use Graph.
//! At the core of the IR infrastructure are SSA def-use and use-def chains.
//! Use-def and def-use chains are composed of four key structures:
//!   - [Value] describes a value definition, either a block argument, or an operation result.
//!   - [`Ptr<BasicBlock>`] describes a block definition.
//!     Its uses are in the predecessor branch operations.
//!   - [Use] describes the use of a definition.
//!     This may describe either a [Value] use (as operand in an [Operation])
//!     or a [BasicBlock] use (as successor of an [Operation]).

use crate::{
    arg_error,
    basic_block::BasicBlock,
    builtin::given_names,
    common_traits::{Named, Verify},
    context::{Context, Ptr},
    identifier::Identifier,
    linked_list::{ContainsLinkedList, LinkedList},
    location::{Located, Location},
    operation::{DefUseVerifyErr, Operand, Operation},
    printable::Printable,
    result::Result,
    r#type::{TypeHandle, Typed},
    verify_err, verify_error,
};
use alloc::{format, vec::Vec};
use core::{
    cell::{Ref, RefMut},
    hash::Hash,
    marker::PhantomData,
};

/// def-use chains are implemented for [Value]s and `Ptr<BasicBlock>`.
pub(crate) trait DefUseParticipant: Copy + Hash + Eq {}
impl DefUseParticipant for Value {}
impl DefUseParticipant for Ptr<BasicBlock> {}

/// PLIRON: sentinel for empty/terminated use-list links.
pub(crate) const USE_NIL: u32 = u32::MAX;

/// PLIRON: one node in a definition's intrusive use list. Nodes live in
/// the Context-wide [`UseLinks`] arena and are referenced by index;
/// `use_uid` encodes `(generation << 32) | slot_index` — the generation
/// bumps on every slot reuse so a stale `Use` can never alias the new
/// occupant.
#[derive(Debug)]
pub(crate) struct UseSlot {
    /// The operation owning the operand/successor that is this use.
    pub(crate) user_op: Ptr<Operation>,
    /// Identity of this use, also stored in the operand. High 32 bits
    /// are the slot generation, low 32 bits the slot index.
    pub(crate) use_uid: u64,
    /// Previous use slot of the same definition, or [`USE_NIL`].
    pub(crate) prev: u32,
    /// Next use slot of the same definition, or [`USE_NIL`].
    pub(crate) next: u32,
}

/// PLIRON: Context-wide arena of [`UseSlot`]s plus a free list. All
/// def-use links (value operands *and* block successors) share it, so a
/// `use_uid` is unique across both namespaces.
pub(crate) struct UseLinks {
    /// All use slots ever allocated; freed slots form a free list via `next`.
    pub(crate) arena: Vec<UseSlot>,
    /// Head of the free list, or [`USE_NIL`].
    pub(crate) free: u32,
}

impl UseLinks {
    pub(crate) fn new() -> Self {
        Self {
            arena: Vec::new(),
            free: USE_NIL,
        }
    }

    /// Allocate a slot for `user_op`, returning its index.
    /// Reused slots get a bumped generation in `use_uid`.
    fn alloc(&mut self, user_op: Ptr<Operation>) -> u32 {
        if self.free != USE_NIL {
            let idx = self.free;
            let slot = &mut self.arena[idx as usize];
            self.free = slot.next;
            let generation = (slot.use_uid >> 32) + 1;
            slot.use_uid = (generation << 32) | idx as u64;
            slot.user_op = user_op;
            slot.prev = USE_NIL;
            slot.next = USE_NIL;
            idx
        } else {
            let idx = self.arena.len() as u32;
            self.arena.push(UseSlot {
                user_op,
                use_uid: (1u64 << 32) | idx as u64,
                prev: USE_NIL,
                next: USE_NIL,
            });
            idx
        }
    }

    /// Return a slot to the free list.
    fn free_slot(&mut self, idx: u32) {
        let slot = &mut self.arena[idx as usize];
        slot.next = self.free;
        slot.prev = USE_NIL;
        self.free = idx;
    }
}

/// PLIRON: iterator over a definition's uses, in use-list order
/// (insertion order; removal splices the tail node into the freed
/// position, matching the old `SmallSet`/`indexmap` `swap_remove`
/// order bit-for-bit). Holds a shared borrow of [`Context::use_links`].
pub(crate) struct UseIter<'a, T: DefUseParticipant> {
    links: Ref<'a, UseLinks>,
    cur: u32,
    _d: PhantomData<T>,
}

impl<'a, T: DefUseParticipant> Iterator for UseIter<'a, T> {
    type Item = Use<T>;

    fn next(&mut self) -> Option<Use<T>> {
        if self.cur == USE_NIL {
            return None;
        }
        let slot = &self.links.arena[self.cur as usize];
        let r#use = Use {
            user_op: slot.user_op,
            use_uid: slot.use_uid,
            _dummy: PhantomData,
        };
        self.cur = slot.next;
        Some(r#use)
    }
}

/// A def node contains a list of its uses.
/// PLIRON: intrusive doubly-linked list over the Context's [`UseLinks`]
/// arena — `head`/`tail`/`count` only (12 bytes vs the old `SmallSet`'s
/// inline-slot-plus-optional-boxed-IndexSet), with no hashing on
/// insert/remove.
#[derive(Debug)]
pub(crate) struct DefNode<T: DefUseParticipant> {
    /// First use slot in [`UseLinks`], or [`USE_NIL`].
    head: u32,
    /// Last use slot in [`UseLinks`], or [`USE_NIL`].
    tail: u32,
    /// Number of uses.
    count: u32,
    _d: PhantomData<T>,
}

impl<T: DefUseParticipant> DefNode<T> {
    /// Create a new definition.
    pub(crate) fn new() -> DefNode<T> {
        DefNode {
            head: USE_NIL,
            tail: USE_NIL,
            count: 0,
            _d: PhantomData,
        }
    }

    /// Does the definition have a use?
    pub(crate) fn is_used(&self) -> bool {
        self.head != USE_NIL
    }

    /// How many uses does this definition have?
    pub(crate) fn num_uses(&self) -> usize {
        self.count as usize
    }

    /// Does this definition have a [Use] of `use` ? O(#uses) walk.
    pub(crate) fn has_use_of(&self, ctx: &Context, r#use: &Use<T>) -> bool {
        let links = ctx.use_links.borrow();
        let mut cur = self.head;
        while cur != USE_NIL {
            let slot = &links.arena[cur as usize];
            if slot.user_op == r#use.user_op && slot.use_uid == r#use.use_uid {
                return true;
            }
            cur = slot.next;
        }
        false
    }

    /// Get an iterator over all [Use]es of this definition.
    pub(crate) fn uses<'a>(&self, ctx: &'a Context) -> UseIter<'a, T> {
        UseIter {
            links: ctx.use_links.borrow(),
            cur: self.head,
            _d: PhantomData,
        }
    }

    /// This definition has a new use by `user_op`. Track it and return
    /// the corresponding [UseNode] and the use's uid, to be stored in
    /// the operand.
    /// PLIRON: takes the `use_links` arena mutably so callers creating
    /// many operands hold one borrow instead of one per operand.
    pub(crate) fn add_use(
        &mut self,
        links: &mut UseLinks,
        self_descr: T,
        user_op: Ptr<Operation>,
    ) -> (UseNode<T>, u64) {
        let idx = links.alloc(user_op);
        let uid = links.arena[idx as usize].use_uid;
        links.arena[idx as usize].prev = self.tail;
        if self.tail != USE_NIL {
            links.arena[self.tail as usize].next = idx;
        } else {
            self.head = idx;
        }
        self.tail = idx;
        self.count += 1;
        (UseNode { def: self_descr }, uid)
    }

    /// Remove `use` from the underlying definition.
    /// The tail node takes the removed node's chain position, so
    /// iteration order matches `indexmap`'s `swap_remove` semantics.
    /// PLIRON: takes the `use_links` arena mutably — see `add_use`.
    pub(crate) fn remove_use(&mut self, links: &mut UseLinks, r#use: &Use<T>) {
        let x = (r#use.use_uid & 0xffff_ffff) as u32;
        debug_assert!(
            links.arena[x as usize].use_uid == r#use.use_uid
                && links.arena[x as usize].user_op == r#use.user_op,
            "Def: attempt to remove a use that doesn't exist"
        );
        let t = self.tail;
        if x == t {
            // Remove the tail.
            let tp = links.arena[t as usize].prev;
            self.tail = tp;
            if tp == USE_NIL {
                self.head = USE_NIL;
            } else {
                links.arena[tp as usize].next = USE_NIL;
            }
        } else if links.arena[x as usize].next == t {
            // x directly precedes the tail: splicing x out leaves the
            // tail in x's former position — the swap_remove result.
            let xp = links.arena[x as usize].prev;
            if xp == USE_NIL {
                self.head = t;
            } else {
                links.arena[xp as usize].next = t;
            }
            links.arena[t as usize].prev = xp;
        } else {
            // Move the tail node into x's chain position.
            let (xp, xn) = (links.arena[x as usize].prev, links.arena[x as usize].next);
            let tp = links.arena[t as usize].prev;
            links.arena[tp as usize].next = USE_NIL;
            self.tail = tp;
            links.arena[t as usize].prev = xp;
            links.arena[t as usize].next = xn;
            if xp == USE_NIL {
                self.head = t;
            } else {
                links.arena[xp as usize].next = t;
            }
            links.arena[xn as usize].prev = t;
        }
        self.count -= 1;
        links.free_slot(x);
    }

    /// Replace the given use of the underlying definition `this` with `other`.
    pub(crate) fn replace_use_with(ctx: &Context, this: &T, r#use: &Use<T>, other: &T)
    where
        T: DefTrait + UseTrait,
    {
        if core::ptr::eq(&*this.get_defnode_ref(ctx), &*other.get_defnode_ref(ctx)) {
            return;
        }

        // Add r#use as a use of `other` and repoint the operand (its
        // [UseNode] and its use uid — the new list entry has a new uid).
        let mut links = ctx.use_links.borrow_mut();
        let (new_use_node, new_uid) =
            other.get_defnode_mut(ctx).add_use(&mut links, *other, r#use.user_op);
        {
            let mut operand = T::get_operand_mut(r#use, ctx);
            operand.r#use = new_use_node;
            operand.use_uid = new_uid;
        }

        // `this` will no longer have r#use as a use.
        this.get_defnode_mut(ctx).remove_use(&mut links, r#use);
    }
}

#[derive(Debug, thiserror::Error)]
enum UseError {
    #[error("Use does not correspond to any operand of its user operation")]
    OperandNotInUserOp,
    #[error("Use does not correspond to any successor of its user operation")]
    SuccessorNotInUserOp,
}

/// Interface for [UseNode] wrappers.
pub(crate) trait UseTrait: DefUseParticipant {
    /// Find the index of the operand/successor in the user operation that corresponds to this use.
    /// Returns [Err] if the use is not in its user operation's operands/successors.
    fn try_find_index(r#use: &Use<Self>, ctx: &Context) -> Result<usize>;
    /// Find the index of the operand/successor in the user operation that corresponds to this use.
    /// Panics if the use is not in its user operation's operands/successors.
    fn find(r#use: &Use<Self>, ctx: &Context) -> usize {
        Self::try_find_index(r#use, ctx)
            .expect("Use is not in its user operation's operands/successors")
    }
    /// Get a reference to the [UseNode] described by this use.
    fn get_usenode_ref<'a>(r#use: &Use<Self>, ctx: &'a Context) -> Ref<'a, UseNode<Self>>;
    /// Get a mutable reference to the [Operand] described by this use.
    /// PLIRON: `replace_use_with` must rewrite both the operand's
    /// [UseNode] and its `use_uid`, so it needs the whole [Operand].
    fn get_operand_mut<'a>(
        r#use: &Use<Self>,
        ctx: &'a Context,
    ) -> RefMut<'a, Operand<Self>>;
}

/// Interface for [DefNode] wrappers.
pub(crate) trait DefTrait: DefUseParticipant {
    /// Get a reference to the underlying [DefNode].
    fn get_defnode_ref<'a>(&self, ctx: &'a Context) -> Ref<'a, DefNode<Self>>;
    /// Get a mutable reference to the underlying [DefNode].
    fn get_defnode_mut<'a>(&self, ctx: &'a Context) -> RefMut<'a, DefNode<Self>>;
}

/// The defining entity of a [Value]:
/// Either an [Operation] (as its result) or a [BasicBlock] (as its argument).
/// Use [Value::find_index] to find the result / argument index in the defining entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefiningEntity {
    Op(Ptr<Operation>),
    Block(Ptr<BasicBlock>),
}

/// Describes a value definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Value {
    pub(crate) val_uid: u64,
    pub(crate) defining_entity: DefiningEntity,
}

#[derive(Debug, thiserror::Error)]
pub enum ValueError {
    #[error("Invalid Value: Defining operation has no result corresponding to this value")]
    NotInOpResult,
    #[error("Invalid Value: Defining block has no argument corresponding to this value")]
    NotInBlockArgument,
}

impl Value {
    /// Get the defining entity
    /// PLIRON: dense per-Context value ordinal — `get_new_value_uid`
    /// hands out a monotonically increasing uid, so it indexes the
    /// columnar side tables (`pliron_ir_opt::state::ColMap`/`ColSet`).
    /// Never reused: a deleted value's uid stays vacated.
    #[inline]
    pub fn uid(&self) -> u64 {
        self.val_uid
    }

    pub fn defining_entity(&self) -> DefiningEntity {
        self.defining_entity
    }

    /// If the defining entity of this value is [Operation], return it. Otherwise, return [None].
    pub fn defining_op(&self) -> Option<Ptr<Operation>> {
        match self.defining_entity {
            DefiningEntity::Op(op) => Some(op),
            DefiningEntity::Block(_) => None,
        }
    }

    /// If the defining entity of this value is [BasicBlock], return it. Otherwise, return [None].
    pub fn defining_block(&self) -> Option<Ptr<BasicBlock>> {
        match self.defining_entity {
            DefiningEntity::Op(_) => None,
            DefiningEntity::Block(block) => Some(block),
        }
    }

    /// Find the (result / argument) index of this value in its defining entity.
    /// Returns [Err] if this value is not currently defined by its defining entity.
    pub fn try_find_index(&self, ctx: &Context) -> Result<usize> {
        match self.defining_entity {
            DefiningEntity::Op(op) => {
                let op = op.deref(ctx);
                op.results_slice(ctx)
                    .iter()
                    .position(|res| res.val_uid == self.val_uid)
                    .ok_or_else(|| arg_error!(op.loc(), ValueError::NotInOpResult))
            }
            DefiningEntity::Block(block) => {
                let block = block.deref(ctx);
                block
                    .args_slice(ctx)
                    .iter()
                    .position(|arg| arg.val_uid == self.val_uid)
                    .ok_or_else(|| arg_error!(block.loc(), ValueError::NotInBlockArgument))
            }
        }
    }

    /// Find the (result / argument) index of this value in its defining entity.
    /// Panics if this value is not currently defined by its defining entity.
    pub fn find_index(&self, ctx: &Context) -> usize {
        self.try_find_index(ctx)
            .expect("Value is not currently defined by its defining entity")
    }

    /// How many uses does this definition have?
    pub fn num_uses(&self, ctx: &Context) -> usize {
        self.get_defnode_ref(ctx).num_uses()
    }

    /// Get all uses of this value.
    pub fn uses(&self, ctx: &Context) -> Vec<Use<Value>> {
        self.get_defnode_ref(ctx).uses(ctx).collect()
    }

    /// PLIRON: lazy use iteration — [Value::uses] collects a `Vec` per
    /// call; this walks the use list with no allocation. Prefer for
    /// `.all()`/`.any()` queries and read-only loops. The use list must
    /// not be mutated while iterating.
    pub fn uses_iter<'a>(&self, ctx: &'a Context) -> impl Iterator<Item = Use<Value>> + 'a {
        self.get_defnode_ref(ctx).uses(ctx)
    }

    /// PLIRON: [`Value::uses`] into a caller-owned buffer so per-node
    /// queries in a worklist walk share one allocation. The copy is still
    /// required — the defnode sits behind a RefCell.
    pub fn uses_into(&self, ctx: &Context, out: &mut Vec<Use<Value>>) {
        out.clear();
        out.extend(self.get_defnode_ref(ctx).uses(ctx));
    }

    /// Does this definition have any [Use]?
    pub fn is_used(&self, ctx: &Context) -> bool {
        self.get_defnode_ref(ctx).is_used()
    }

    /// Replace uses of the underlying definition, that satisfy `pred`, with `other`.
    pub fn replace_some_uses_with<P: Fn(&Context, &Use<Value>) -> bool>(
        &self,
        ctx: &Context,
        predicate: P,
        other: &Value,
    ) {
        // We collect because we don't want to keep the defnode locked up.
        let touched_uses: Vec<_> = self
            .get_defnode_ref(ctx)
            .uses(ctx)
            .filter(|r#use| predicate(ctx, r#use))
            .collect();
        for r#use in &touched_uses {
            DefNode::replace_use_with(ctx, self, r#use, other);
        }
    }

    /// Replace all uses of the underlying definition with `other`.
    pub fn replace_all_uses_with(&self, ctx: &Context, other: &Value) {
        self.replace_some_uses_with(ctx, |_, _| true, other);
        // PLIRON: the debug-info notes follow the value (`Context::dbg_notes`).
        let mut notes = ctx.dbg_notes.borrow_mut();
        if !notes.is_empty() && self != other {
            if let Some(n) = notes.remove(self) {
                notes.entry(*other).or_default().extend(n);
            }
        }
    }

    /// Replace the given use of `this` [Value] with `other`.
    pub fn replace_use_with(&self, ctx: &Context, r#use: Use<Value>, other: &Value) {
        DefNode::replace_use_with(ctx, self, &r#use, other);
    }

    /// Get this value's location
    pub fn loc(&self, ctx: &Context) -> Location {
        match self.defining_entity {
            DefiningEntity::Op(op) => op.deref(ctx).loc(),
            DefiningEntity::Block(block) => block.deref(ctx).loc(),
        }
    }

    /// Set this value's type.
    pub fn set_type(&self, ctx: &Context, ty: TypeHandle) {
        let index = self.find_index(ctx);
        match self.defining_entity {
            DefiningEntity::Op(op) => op.deref(ctx).results_slice_mut(ctx)[index].set_type(ty),
            DefiningEntity::Block(block) => {
                block.deref(ctx).args_slice_mut(ctx)[index].set_type(ty)
            }
        }
    }

    /// Set this value's name.
    /// This corresponds to the [given_name](Named::given_name) of the value.
    pub fn set_name(&self, ctx: &Context, name: Option<Identifier>) {
        let index = self.find_index(ctx);
        match self.defining_entity {
            DefiningEntity::Op(op) => given_names::set_operation_result_name(ctx, op, index, name),
            DefiningEntity::Block(block) => {
                given_names::set_block_arg_name(ctx, block, index, name)
            }
        }
    }

    /// Get the defining block of this value.
    pub fn get_defining_block(&self, ctx: &Context) -> Option<Ptr<BasicBlock>> {
        match self.defining_entity {
            DefiningEntity::Op(op) => op.deref(ctx).get_parent_block(),
            DefiningEntity::Block(block) => Some(block),
        }
    }
}

impl Verify for Value {
    // Check that the value's uses point back to it,
    fn verify(&self, ctx: &Context) -> Result<()> {
        for r#use in self.uses_iter(ctx) {
            let opd_idx = <Self as UseTrait>::try_find_index(&r#use, ctx)
                .map_err(|_| verify_error!(self.loc(ctx), DefUseVerifyErr::OperandNotUseOfDef))?;
            let use_operand = r#use.user_op.deref(ctx).get_operand(ctx, opd_idx);
            if use_operand != *self {
                verify_err!(self.loc(ctx), DefUseVerifyErr::OperandNotUseOfDef)?;
            }
        }
        self.get_type(ctx).verify(ctx)
    }
}

impl Typed for Value {
    fn get_type(&self, ctx: &Context) -> TypeHandle {
        let index = self.find_index(ctx);
        match self.defining_entity {
            DefiningEntity::Op(op) => op.deref(ctx).results_slice(ctx)[index].get_type(),
            DefiningEntity::Block(block) => {
                block.deref(ctx).args_slice(ctx)[index].get_type()
            }
        }
    }
}

impl Named for Value {
    fn given_name(&self, ctx: &Context) -> Option<Identifier> {
        let index = self.find_index(ctx);
        match self.defining_entity {
            DefiningEntity::Op(op) => given_names::get_operation_result_name(ctx, op, index),
            DefiningEntity::Block(block) => given_names::get_block_arg_name(ctx, block, index),
        }
    }

    fn id(&self, _ctx: &Context) -> Identifier {
        Identifier::try_from(format!("v{}", self.val_uid)).unwrap()
    }
}

impl Printable for Value {
    fn fmt(
        &self,
        ctx: &Context,
        _state: &crate::printable::State,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        write!(f, "{}", self.unique_name(ctx))
    }
}

impl DefTrait for Value {
    fn get_defnode_ref<'a>(&self, ctx: &'a Context) -> Ref<'a, DefNode<Self>> {
        let index = self.find_index(ctx);
        match self.defining_entity {
            DefiningEntity::Op(op) => {
                let span = op.deref(ctx).results;
                let pool = ctx.op_result_pool.borrow();
                Ref::map(pool, |p| &p[span.range()][index].def)
            }
            DefiningEntity::Block(block) => {
                let span = block.deref(ctx).args;
                let pool = ctx.block_arg_pool.borrow();
                Ref::map(pool, |p| &p[span.range()][index].def)
            }
        }
    }

    fn get_defnode_mut<'a>(&self, ctx: &'a Context) -> RefMut<'a, DefNode<Self>> {
        let index = self.find_index(ctx);
        match self.defining_entity {
            DefiningEntity::Op(op) => {
                let span = op.deref(ctx).results;
                let pool = ctx.op_result_pool.borrow_mut();
                RefMut::map(pool, |p| &mut p[span.range()][index].def)
            }
            DefiningEntity::Block(block) => {
                let span = block.deref(ctx).args;
                let pool = ctx.block_arg_pool.borrow_mut();
                RefMut::map(pool, |p| &mut p[span.range()][index].def)
            }
        }
    }
}

impl UseTrait for Value {
    fn try_find_index(r#use: &Use<Self>, ctx: &Context) -> Result<usize> {
        let op = r#use.user_op.deref(ctx);
        op.operands_slice(ctx)
            .iter()
            .position(|operand| operand.use_uid == r#use.use_uid)
            .ok_or_else(|| arg_error!(op.loc(), UseError::OperandNotInUserOp))
    }
    fn get_usenode_ref<'a>(r#use: &Use<Self>, ctx: &'a Context) -> Ref<'a, UseNode<Self>> {
        let index = <Self as UseTrait>::find(r#use, ctx);
        let span = r#use.user_op.deref(ctx).operands;
        let pool = ctx.op_operand_pool.borrow();
        Ref::map(pool, |p| &p[span.range()][index].r#use)
    }
    fn get_operand_mut<'a>(
        r#use: &Use<Self>,
        ctx: &'a Context,
    ) -> RefMut<'a, Operand<Value>> {
        let index = <Self as UseTrait>::find(r#use, ctx);
        let span = r#use.user_op.deref(ctx).operands;
        let pool = ctx.op_operand_pool.borrow_mut();
        RefMut::map(pool, |p| &mut p[span.range()][index])
    }
}

impl Ptr<BasicBlock> {
    /// Does this block a predecessor?
    pub fn has_pred(&self, ctx: &Context) -> bool {
        self.deref(ctx).preds.is_used()
    }

    /// Is `pred` a predecessor of this block?
    /// *Note*: O(n) in the number of predecessors of this block.
    pub fn is_pred(&self, ctx: &Context, pred: Ptr<BasicBlock>) -> bool {
        self.deref(ctx)
            .preds
            .uses(ctx)
            .any(|r#use| r#use.user_op.deref(ctx).get_parent_block() == Some(pred))
    }

    /// Number of predecessors to this block.
    pub fn num_preds(&self, ctx: &Context) -> usize {
        self.deref(ctx).preds.num_uses()
    }

    /// Get the `i`-th predecessor of this block. Panics if `i` is out of bounds.
    /// *Note*:
    ///     1. The order of predecessors is not guaranteed to be deterministic
    ///     across different platforms or rust compiler versions.
    ///     2. O(n) in the number of predecessors of this block.
    pub fn get_pred(&self, ctx: &Context, i: usize) -> Ptr<BasicBlock> {
        self.deref(ctx)
            .preds
            .uses(ctx)
            .nth(i)
            .expect("Predecessor index out of bounds")
            .user_op
            .deref(ctx)
            .get_container()
            .expect("Terminator branching to this block is not in any basic block")
    }

    /// Get all predecessors of this block.
    /// *Note*: The order of predecessors is not guaranteed to be deterministic
    /// across different platforms or rust compiler versions.
    pub fn preds(&self, ctx: &Context) -> Vec<Ptr<BasicBlock>> {
        self.deref(ctx)
            .preds
            .uses(ctx)
            .map(|r#use| {
                r#use
                    .user_op
                    .deref(ctx)
                    .get_container()
                    .expect("Terminator branching to this block is not in any basic block")
            })
            .collect()
    }

    /// Get uses (as successor operands in an Operation) of this block.
    pub fn uses(&self, ctx: &Context) -> Vec<Use<Ptr<BasicBlock>>> {
        self.deref(ctx).preds.uses(ctx).collect()
    }

    /// PLIRON: lazy use iteration — see [`Value::uses_iter`].
    pub fn uses_iter<'a>(
        &self,
        ctx: &'a Context,
    ) -> impl Iterator<Item = Use<Ptr<BasicBlock>>> + 'a {
        self.deref(ctx).preds.uses(ctx)
    }

    /// Checks whether self is a successor of `pred`.
    /// O(n) in the number of successors of `pred`.
    pub fn is_succ_of(&self, ctx: &Context, pred: Ptr<BasicBlock>) -> bool {
        // We do not check [Self::get_defnode_ref].uses here because
        // we'd have to go through them all. We do not have a Use<_>
        // object to directly check membership.
        pred.deref(ctx)
            .get_tail()
            .is_some_and(|pred_term| {
                pred_term
                    .deref(ctx)
                    .successors(ctx)
                    .any(|succ| self == &succ)
            })
    }
    /// Retarget predecessors (that satisfy pred) to `other`.
    pub fn retarget_some_preds_to<P: Fn(&Context, Ptr<BasicBlock>) -> bool>(
        &self,
        ctx: &Context,
        predicate: P,
        other: Ptr<BasicBlock>,
    ) {
        let predicate = |ctx: &Context, r#use: &Use<Ptr<BasicBlock>>| {
            let pred_block = r#use
                .user_op
                .deref(ctx)
                .get_container()
                .expect("Predecessor block must be in a Region");
            predicate(ctx, pred_block)
        };

        // We collect because we don't want to keep the defnode locked up.
        let touched_uses: Vec<_> = self
            .get_defnode_ref(ctx)
            .uses(ctx)
            .filter(|r#use| predicate(ctx, r#use))
            .collect();

        for r#use in &touched_uses {
            DefNode::replace_use_with(ctx, self, r#use, &other);
        }
    }

    /// Retarget all predecessors to `other`.
    pub fn retarget_all_preds_to(&self, ctx: &Context, other: Ptr<BasicBlock>) {
        self.retarget_some_preds_to(ctx, |_, _| true, other);
    }

    /// Retarget the given pred (`block_use`) of `this` [`Ptr<BasicBlock>`] to `other`.
    pub fn retarget_pred_to(
        &self,
        ctx: &Context,
        block_use: Use<Ptr<BasicBlock>>,
        other: Ptr<BasicBlock>,
    ) {
        DefNode::replace_use_with(ctx, self, &block_use, &other);
    }
}

impl Named for Ptr<BasicBlock> {
    fn given_name(&self, ctx: &Context) -> Option<Identifier> {
        self.deref(ctx).given_name(ctx)
    }
    fn id(&self, ctx: &Context) -> Identifier {
        self.deref(ctx).id(ctx)
    }
}

impl DefTrait for Ptr<BasicBlock> {
    fn get_defnode_ref<'a>(&self, ctx: &'a Context) -> Ref<'a, DefNode<Self>> {
        let block = self.deref(ctx);
        Ref::map(block, |blockref| &blockref.preds)
    }

    fn get_defnode_mut<'a>(&self, ctx: &'a Context) -> RefMut<'a, DefNode<Self>> {
        let block = self.deref_mut(ctx);
        RefMut::map(block, |blockref| &mut blockref.preds)
    }
}

impl UseTrait for Ptr<BasicBlock> {
    fn try_find_index(r#use: &Use<Self>, ctx: &Context) -> Result<usize> {
        let op = r#use.user_op.deref(ctx);
        op.successors_slice(ctx)
            .iter()
            .position(|succ| succ.use_uid == r#use.use_uid)
            .ok_or_else(|| arg_error!(op.loc(), UseError::SuccessorNotInUserOp))
    }
    fn get_usenode_ref<'a>(r#use: &Use<Self>, ctx: &'a Context) -> Ref<'a, UseNode<Self>> {
        let succ_idx = <Self as UseTrait>::find(r#use, ctx);
        let span = r#use.user_op.deref(ctx).successors;
        let pool = ctx.op_successor_pool.borrow();
        Ref::map(pool, |p| &p[span.range()][succ_idx].r#use)
    }
    fn get_operand_mut<'a>(
        r#use: &Use<Ptr<BasicBlock>>,
        ctx: &'a Context,
    ) -> RefMut<'a, Operand<Ptr<BasicBlock>>> {
        let succ_idx = <Self as UseTrait>::find(r#use, ctx);
        let span = r#use.user_op.deref(ctx).successors;
        let pool = ctx.op_successor_pool.borrow_mut();
        RefMut::map(pool, |p| &mut p[span.range()][succ_idx])
    }
}

/// A use node contains a pointer to its definition.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UseNode<T: DefUseParticipant> {
    /// The definition that this is a use of.
    def: T,
}

impl<T: DefUseParticipant> UseNode<T> {
    pub(crate) fn get_def(&self) -> T {
        self.def
    }
}

/// Describes a [Value] or [BasicBlock] use.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
#[allow(private_bounds)]
pub struct Use<T: DefUseParticipant> {
    /// Uses of a def can only be in an operation.
    pub(crate) user_op: Ptr<Operation>,
    /// The global (in the context) unique id of the use we're describing.
    pub(crate) use_uid: u64,
    /// Phantom data to keep track of whether this is a Value use or a BasicBlock use.
    pub(crate) _dummy: PhantomData<T>,
}

#[allow(private_bounds)]
impl<T: UseTrait> Use<T> {
    /// Find index of the operand/successor in the user operation that corresponds to this [Use].
    /// Returns [Err] if the [Use] is not in its user operation's operands/successors.
    pub fn try_find_index(&self, ctx: &Context) -> Result<usize> {
        T::try_find_index(self, ctx)
    }

    /// Find index of the operand/successor in the user operation that corresponds to this [Use].
    /// Panics if the [Use] is not in its user operation's operands/successors.
    pub fn find_index(&self, ctx: &Context) -> usize {
        T::find(self, ctx)
    }

    /// Get the definition that this is a use of.
    pub fn get_def(&self, ctx: &Context) -> T {
        UseTrait::get_usenode_ref(self, ctx).get_def()
    }

    /// Get the operation that is the user of this use.
    pub fn user_op(&self) -> Ptr<Operation> {
        self.user_op
    }
}

impl Typed for Use<Value> {
    fn get_type(&self, ctx: &Context) -> TypeHandle {
        self.get_def(ctx).get_type(ctx)
    }
}
