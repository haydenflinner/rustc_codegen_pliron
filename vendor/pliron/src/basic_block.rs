// SPDX-License-Identifier: Apache-2.0
// Copyright (c) The pliron contributors

//! A [BasicBlock] is a list of [Operation]s.

use alloc::{string::String, vec::Vec};
use thiserror::Error;

use crate::{
    operation::OpSpan,
    attribute::{AttributeDict, verify_attr},
    builtin::{
        given_names,
        op_interfaces::{IsTerminatorInterface, NoTerminatorInterface},
    },
    combine::{
        optional,
        parser::{Parser, char::spaces},
        sep_by, token,
    },
    common_traits::{Named, RcShare, Verify},
    context::{Arena, Context, Ptr, private::ArenaObj},
    identifier::Identifier,
    irfmt::{
        outlined::{preprint_outline_block, register_block_for_outline},
        parsers::{delimited_list_parser, location, spaced, type_parser},
        printers::{iter_with_sep, iter_with_sep_by},
    },
    linked_list::{ContainsLinkedList, LinkedList, private},
    location::{Located, Location},
    op::op_impls,
    operation::{DefUseVerifyErr, OpDbg, Operation, OperationParserConfig},
    parsable::{self, IntoParseResult, Parsable, ParseResult},
    printable::{self, ListSeparator, Printable, indented_nl},
    region::Region,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::{DefNode, DefiningEntity, Value},
    verify_err, verify_error,
};

/// Argument to a [BasicBlock]
pub(crate) struct BlockArgument {
    /// The def containing the list of this argument's uses.
    pub(crate) def: DefNode<Value>,
    /// Unique ID of this value in [Context].
    pub(crate) val_uid: u64,
    /// The [Type](crate::type::Type) of this argument.
    pub(crate) ty: TypeHandle,
}

impl BlockArgument {
    /// Create a new block argument with the given type and a fresh value UID.
    pub(crate) fn new(ctx: &Context, ty: TypeHandle) -> Self {
        Self {
            def: DefNode::new(),
            val_uid: ctx.get_new_value_uid(),
            ty,
        }
    }

    /// Get the [Type](crate::type::Type) of this argument.
    pub(crate) fn get_type(&self) -> TypeHandle {
        self.ty
    }

    /// Set the [Type](crate::type::Type) of this argument.
    pub(crate) fn set_type(&mut self, ty: TypeHandle) {
        self.ty = ty;
    }

    /// Build a [Value] corresponding to this argument.
    pub(crate) fn as_value(&self, block: Ptr<BasicBlock>) -> Value {
        Value {
            defining_entity: DefiningEntity::Block(block),
            val_uid: self.val_uid,
        }
    }
}

impl Typed for BlockArgument {
    fn get_type(&self, _ctx: &Context) -> TypeHandle {
        self.get_type()
    }
}

/// PLIRON: block-arg source for [`crate::operation::PoolIter`] — the
/// `block_arg_pool` span, mapped through [`BlockArgument::as_value`].
#[derive(Clone, Copy)]
struct ArgVal;
impl<'a> crate::operation::PoolSrc<'a, Ptr<BasicBlock>> for ArgVal {
    type Item = Value;
    #[inline]
    fn get(
        ctx: &'a Context,
        blk: Ptr<BasicBlock>,
        span: OpSpan,
        i: usize,
    ) -> Option<Value> {
        ctx.block_arg_pool
            .borrow()
            .get(span.off as usize + i)
            .map(|e| e.as_value(blk))
    }
}

/// [Operation]s contained in this [BasicBlock]
#[derive(Default)]
pub struct OpsInBlock {
    first: Option<Ptr<Operation>>,
    last: Option<Ptr<Operation>>,
}

/// Links a [BasicBlock] with other blocks and the container [Region].
/// PLIRON: next/prev moved to the dense [NodeLinks](crate::linked_list::NodeLinks)
/// table; only the container stays on the node.
#[derive(Default)]
struct RegionLinks {
    /// Parent region of this block.
    parent_region: Option<Ptr<Region>>,
}

/// A basic block contains a list of [Operation]s. It may have arguments.
pub struct BasicBlock {
    pub(crate) self_ptr: Ptr<BasicBlock>,
    pub(crate) label: Option<Identifier>,
    pub(crate) ops_list: OpsInBlock,
    /// PLIRON: span into `Context::block_arg_pool`.
    pub(crate) args: OpSpan,
    pub(crate) preds: DefNode<Ptr<BasicBlock>>,
    /// Links to the parent [Region] and
    /// previous and next [BasicBlock]s in the block.
    region_links: RegionLinks,
    /// A dictionary of attributes.
    pub attributes: AttributeDict,
    loc: Location,
}

impl Named for BasicBlock {
    fn given_name(&self, _ctx: &Context) -> Option<Identifier> {
        self.label.clone()
    }
    fn id(&self, _ctx: &Context) -> Identifier {
        self.self_ptr.make_name("block")
    }
}

impl BasicBlock {
    /// Create a new Basic Block.
    pub fn new(
        ctx: &mut Context,
        label: Option<Identifier>,
        arg_types: Vec<TypeHandle>,
    ) -> Ptr<BasicBlock> {
        let f = |self_ptr: Ptr<BasicBlock>| BasicBlock {
            self_ptr,
            label,
            args: OpSpan::default(),
            ops_list: OpsInBlock::default(),
            preds: DefNode::new(),
            region_links: RegionLinks::default(),
            attributes: AttributeDict::default(),
            loc: Location::Unknown,
        };
        let newblock = Self::alloc(ctx, f);
        // Let's update the args of the new block. Easier to do it here than during creation.
        let args: Vec<BlockArgument> = arg_types
            .into_iter()
            .map(|ty| BlockArgument::new(ctx, ty))
            .collect();
        {
            let mut pool = ctx.block_arg_pool.borrow_mut();
            let off = pool.len() as u32;
            let len = args.len() as u32;
            pool.extend(args);
            newblock.deref_mut(ctx).args = OpSpan { off, len };
        }
        // We're done.
        newblock
    }

    /// PLIRON: attribute dictionary accessor. Prefer this over direct
    /// `.attributes` field reads so the backing store can change shape.
    #[inline]
    pub fn attributes(&self) -> &crate::attribute::AttributeDict {
        &self.attributes
    }

    /// PLIRON: mutable attribute dictionary accessor.
    #[inline]
    pub fn attributes_mut(&mut self) -> &mut crate::attribute::AttributeDict {
        &mut self.attributes
    }

    /// PLIRON: raw block-argument slice (crate-internal store escape hatch).
    #[inline]
    pub(crate) fn args_slice<'a>(
        &self,
        ctx: &'a Context,
    ) -> core::cell::Ref<'a, [BlockArgument]> {
        core::cell::Ref::map(ctx.block_arg_pool.borrow(), |pool| {
            &pool[self.args.range()]
        })
    }

    /// PLIRON: mutable arg store (crate-internal).
    #[inline]
    pub(crate) fn args_slice_mut<'a>(
        &self,
        ctx: &'a Context,
    ) -> core::cell::RefMut<'a, [BlockArgument]> {
        core::cell::RefMut::map(ctx.block_arg_pool.borrow_mut(), |pool| {
            &mut pool[self.args.range()]
        })
    }

    /// PLIRON: rebuild `block`'s argument list into a fresh tail span
    /// of `new_len` BlockArguments seeded with `seed_ty`, moving old
    /// node `i` to `map_old(i)` (`None` drops it). Preserves
    /// val_uid/DefNode identity without Clone; unmapped nodes die
    /// with the orphaned span (same as Vec::remove).
    fn rewrite_args(
        block: Ptr<BasicBlock>,
        ctx: &Context,
        new_len: usize,
        seed_ty: TypeHandle,
        map_old: impl Fn(usize) -> Option<usize>,
    ) {
        let span = block.deref(ctx).args;
        let old = span.range();
        let seeds: Vec<BlockArgument> = (0..new_len)
            .map(|_| BlockArgument::new(ctx, seed_ty))
            .collect();
        let mut pool = ctx.block_arg_pool.borrow_mut();
        let new_off = pool.len();
        pool.extend(seeds);
        for i in (0..span.len as usize).rev() {
            if let Some(dst) = map_old(i) {
                pool.swap(old.start + i, new_off + dst);
            }
        }
        drop(pool);
        block.deref_mut(ctx).args = OpSpan {
            off: new_off as u32,
            len: new_len as u32,
        };
    }

    /// Set the block's label.
    /// This corresponds to the [given_name](Named::given_name) of this block.
    pub fn set_label(&mut self, label: Option<Identifier>) {
        self.label = label;
    }

    /// Get parent region.
    pub fn get_parent_region(&self) -> Option<Ptr<Region>> {
        self.region_links.parent_region
    }

    /// Get parent operation.
    pub fn get_parent_op(&self, ctx: &Context) -> Option<Ptr<Operation>> {
        self.get_parent_region()
            .map(|region| region.deref(ctx).get_parent_op())
    }

    /// Get parent block.
    pub fn get_parent_block(&self, ctx: &Context) -> Option<Ptr<BasicBlock>> {
        self.get_parent_op(ctx)
            .and_then(|op| op.deref(ctx).get_parent_block())
    }

    /// Get idx'th argument as a Value. Panics on invalid index.
    pub fn get_argument(&self, ctx: &Context, arg_idx: usize) -> Value {
        self.args_slice(ctx)[arg_idx].as_value(self.self_ptr)
    }

    /// Get an iterator over the arguments
    pub fn arguments<'a>(&self, ctx: &'a Context) -> impl Iterator<Item = Value> + Clone + 'a {
        crate::operation::PoolIter::<ArgVal, Ptr<BasicBlock>>::new(
            ctx,
            self.self_ptr,
            self.args,
        )
    }

    /// Add an argument to the end of the argument list, returning its index.
    pub fn push_argument(block: Ptr<BasicBlock>, ctx: &Context, ty: TypeHandle) -> usize {
        let len = block.deref(ctx).get_num_arguments();
        Self::rewrite_args(block, ctx, len + 1, ty, |i| Some(i));
        len
    }

    /// Remove the last argument. Panics if there are no arguments or if the argument has uses.
    /// Any [Value] referring to the removed argument is invalidated.
    pub fn pop_argument(block: Ptr<BasicBlock>, ctx: &Context) {
        let len = block.deref(ctx).get_num_arguments();
        assert!(
            len > 0,
            "Can't pop argument from block {} with no arguments",
            block.deref(ctx).unique_name(ctx)
        );
        Self::remove_argument(block, ctx, len - 1);
    }

    /// Insert a new argument at `arg_idx`, shifting existing arguments, from `arg_idx`, to the right.
    /// Panics on invalid index.
    pub fn insert_argument(block: Ptr<BasicBlock>, ctx: &Context, arg_idx: usize, ty: TypeHandle) {
        let len = block.deref(ctx).get_num_arguments();
        Self::rewrite_args(block, ctx, len + 1, ty, |i| {
            Some(if i < arg_idx { i } else { i + 1 })
        });
        given_names::insert_block_arg_name(ctx, block, arg_idx, None);
    }

    /// Remove the argument at `arg_idx`, shifting subsequent arguments to the left.
    /// Panics on invalid index or if the argument has uses.
    /// Any [Value] referring to the removed argument is invalidated.
    pub fn remove_argument(block: Ptr<BasicBlock>, ctx: &Context, arg_idx: usize) {
        let value = block.deref(ctx).get_argument(ctx, arg_idx);
        assert!(
            !value.is_used(ctx),
            "Can't remove argument {} from block {} with uses",
            arg_idx,
            block.deref(ctx).unique_name(ctx)
        );
        given_names::remove_block_arg_name(ctx, block, arg_idx);
        let seed_ty = block.deref(ctx).args_slice(ctx)[arg_idx].ty;
        let len = block.deref(ctx).get_num_arguments();
        Self::rewrite_args(block, ctx, len - 1, seed_ty, |i| {
            if i == arg_idx {
                None
            } else {
                Some(i - (i > arg_idx) as usize)
            }
        });
    }

    /// Get the number of arguments.
    pub fn get_num_arguments(&self) -> usize {
        self.args.len as usize
    }

    /// Get all successors of this block.
    pub fn succs(&self, ctx: &Context) -> Vec<Ptr<BasicBlock>> {
        self.get_terminator(ctx)
            .map(|term| term.deref(ctx).successors(ctx).collect())
            .unwrap_or_default()
    }

    /// Does this block have any successors?
    pub fn has_succ(&self, ctx: &Context) -> bool {
        self.get_terminator(ctx)
            .map(|term| term.deref(ctx).get_num_successors() > 0)
            .unwrap_or(false)
    }

    /// Is `succ` a successor of this block?
    /// O(n) in the number of successors.
    pub fn is_succ(&self, ctx: &Context, succ: Ptr<BasicBlock>) -> bool {
        self.get_terminator(ctx)
            .is_some_and(|term| term.deref(ctx).successors(ctx).any(|s| s == succ))
    }

    /// Get the `i`-th successor of this block.
    /// Panics if there is no terminator or if `i` is out of bounds.
    pub fn get_succ(&self, ctx: &Context, i: usize) -> Ptr<BasicBlock> {
        self.get_terminator(ctx)
            .expect("Can't get successor of block with no terminator")
            .deref(ctx)
            .get_successor(ctx, i)
    }

    /// Get the number of successors of this block. Returns 0 if there is no terminator.
    pub fn num_succ(&self, ctx: &Context) -> usize {
        self.get_terminator(ctx)
            .map(|term| term.deref(ctx).get_num_successors())
            .unwrap_or(0)
    }

    /// Get the block terminator, if one exists.
    pub fn get_terminator(&self, ctx: &Context) -> Option<Ptr<Operation>> {
        let last_opr = self.get_tail()?;
        let last_op = Operation::get_op_dyn(last_opr, ctx);
        op_impls::<dyn IsTerminatorInterface>(last_op.as_ref()).then_some(last_opr)
    }

    /// Drop all uses that this block holds.
    pub fn drop_all_uses(ptr: Ptr<Self>, ctx: &Context) {
        let ops: Vec<_> = ptr.deref(ctx).iter(ctx).collect();
        for op in ops {
            Operation::drop_all_uses(op, ctx);
        }
    }

    /// Unlink and deallocate this block and everything that it contains.
    /// Panics if there are any uses outside the block.
    pub fn erase(ptr: Ptr<Self>, ctx: &mut Context) {
        Self::drop_all_uses(ptr, ctx);
        assert!(
            !ptr.has_pred(ctx),
            "BasicBlock {} with predecessor {} being erased",
            ptr.deref(ctx).unique_name(ctx),
            ptr.preds(ctx).first().unwrap().deref(ctx).unique_name(ctx)
        );
        if let Some(arg) = ptr.deref(ctx).arguments(ctx).find(|arg| arg.is_used(ctx)) {
            panic!(
                "Attempting to erase block {} whose argument {} has use outside the block",
                ptr.deref(ctx).unique_name(ctx),
                arg.unique_name(ctx)
            );
        }
        if let Some(op) = ptr.deref(ctx).iter(ctx).find(|op| op.deref(ctx).has_use(ctx)) {
            panic!(
                "Attempting to erase block {} which contains {} with use outside the block",
                ptr.deref(ctx).unique_name(ctx),
                OpDbg { op, ctx }
            );
        }
        if ptr.is_linked(ctx) {
            ptr.unlink(ctx);
        }
        ArenaObj::dealloc(ptr, ctx);
    }
}

impl Located for BasicBlock {
    fn loc(&self) -> Location {
        self.loc.clone()
    }

    fn set_loc(&mut self, loc: Location) {
        self.loc = loc;
    }
}

impl private::ContainsLinkedList<Operation> for BasicBlock {
    fn set_head(&mut self, head: Option<Ptr<Operation>>) {
        self.ops_list.first = head;
    }

    fn set_tail(&mut self, tail: Option<Ptr<Operation>>) {
        self.ops_list.last = tail;
    }
}

impl ContainsLinkedList<Operation> for BasicBlock {
    fn get_head(&self) -> Option<Ptr<Operation>> {
        self.ops_list.first
    }

    fn get_tail(&self) -> Option<Ptr<Operation>> {
        self.ops_list.last
    }
}

impl PartialEq for BasicBlock {
    fn eq(&self, other: &Self) -> bool {
        self.self_ptr == other.self_ptr
    }
}

impl private::LinkedList for BasicBlock {
    type ContainerType = Region;
    fn links_store(
        ctx: &Context,
    ) -> &core::cell::RefCell<Vec<crate::linked_list::NodeLinks<Self>>> {
        &ctx.block_links
    }
    fn set_container(&mut self, container: Option<Ptr<Self::ContainerType>>) {
        self.region_links.parent_region = container;
    }
}

impl LinkedList for BasicBlock {
    fn get_container(&self) -> Option<Ptr<Self::ContainerType>> {
        self.region_links.parent_region
    }
}

impl ArenaObj for BasicBlock {
    fn get_arena(ctx: &Context) -> &Arena<Self> {
        &ctx.basic_blocks
    }
    fn get_arena_mut(ctx: &mut Context) -> &mut Arena<Self> {
        &mut ctx.basic_blocks
    }
    fn dealloc_sub_objects(ptr: Ptr<Self>, ctx: &mut Context) {
        let ops: Vec<_> = ptr.deref_mut(ctx).iter(ctx).collect();
        for op in ops {
            ArenaObj::dealloc(op, ctx);
        }
    }
    fn get_self_ptr(&self, _ctx: &Context) -> Ptr<Self> {
        self.self_ptr
    }
}

impl Verify for BasicBlock {
    fn verify(&self, ctx: &Context) -> Result<()> {
        // Ensure that the block has a terminator
        // (unless the enclosing [Op] is marked [NoTerminatorInterface].
        let label: String = self.unique_name(ctx).into();
        let parent_op = self.get_parent_op(ctx).ok_or_else(|| {
            verify_error!(self.loc(), BasicBlockVerifyErr::NoParent(label.clone()))
        })?;
        let parent_op = Operation::get_op_dyn(parent_op, ctx);
        if !op_impls::<dyn NoTerminatorInterface>(parent_op.as_ref())
            && self.get_terminator(ctx).is_none()
        {
            verify_err!(self.loc(), BasicBlockVerifyErr::MissingTerminator(label))?;
        }
        self.attributes()
            .iter(ctx)
            .try_for_each(|(_, attr)| verify_attr(&**attr, ctx))?;
        // Check that every use is of this block.
        for r#use in self.self_ptr.uses_iter(ctx) {
            if r#use.get_def(ctx) != self.self_ptr {
                verify_err!(self.loc(), DefUseVerifyErr::OperandNotUseOfDef)?;
            }
        }
        // Check that every predecessor points back to this block.
        // (This is almost a repetition of the previous check).
        for pred in self.self_ptr.preds(ctx) {
            if !pred.deref(ctx).is_succ(ctx, self.self_ptr) {
                verify_err!(self.loc(), DefUseVerifyErr::OperandNotUseOfDef)?;
            }
        }
        self.args_slice(ctx)
            .iter()
            .try_for_each(|arg| arg.as_value(self.self_ptr).verify(ctx))?;
        self.iter(ctx).try_for_each(|op| op.deref(ctx).verify(ctx))
    }
}

/// Error indicating that a basic block is missing a terminator.
#[derive(Debug, Error)]

pub enum BasicBlockVerifyErr {
    #[error("Basic block \"{0}\" is missing a terminator")]
    MissingTerminator(String),
    #[error("Basic block \"{0}\" has a terminator that is not the last operation in the block")]
    TerminatorNotLast(String),
    #[error("Basic block \"{0}\" has no parent operation")]
    NoParent(String),
}

impl Printable for BasicBlock {
    fn fmt(
        &self,
        ctx: &Context,
        state: &printable::State,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        let args_slice = self.args_slice(ctx);
        let args = iter_with_sep_by(
            args_slice.iter(),
            ListSeparator::CharSpace(','),
            |arg, ctx, state, f| {
                write!(
                    f,
                    "{}: {}",
                    arg.as_value(self.self_ptr).print(ctx, state),
                    arg.get_type().print(ctx, state)
                )
            },
        );
        write!(
            f,
            "^{}({})",
            self.unique_name(ctx).print(ctx, state),
            args.print(ctx, state)
        )?;

        // Print non-outlined attributes inline.
        let inline_attrs = self.attributes().clone_skip_outlined(ctx);
        if !inline_attrs.0.is_empty() {
            write!(f, " ")?;
            inline_attrs.fmt(ctx, state, f)?;
        }

        // Print the outline index (!N) if there is a location or any outlined attributes.
        preprint_outline_block(ctx, self.self_ptr, state.share(), f)?;

        write!(f, ":")?;

        let _indent = state.indent();
        write!(
            f,
            "{}{}",
            indented_nl(state),
            iter_with_sep(self.iter(ctx), ListSeparator::CharNewline(';')).print(ctx, state),
        )
    }
}

impl Parsable for BasicBlock {
    type Arg = ();
    type Parsed = Ptr<BasicBlock>;

    //  A basic block is
    // ```
    //  label(arg_1: type_1, ..., arg_n: type_n) [optional_inline_attrs] [!N]:
    //    op_1;
    //    ... ;
    //    op_n
    // ```
    fn parse<'a>(
        state_stream: &mut parsable::StateStream<'a>,
        _arg: Self::Arg,
    ) -> ParseResult<'a, Self::Parsed> {
        let loc = state_stream.loc();

        let block_arg = (
            (location(), Identifier::parser(())).skip(spaced(token(':'))),
            type_parser().skip(spaces()),
        );

        // Parse the entire block header as a single combine chain so that failures
        // after consuming the label+args are reported as committed errors.
        // Format: ^label(args) [optional_attrs] [!N]:
        let outline_loc = state_stream.loc();
        let (label, args, attrs_opt, outindex_opt) = (
            spaced(token('^').with(Identifier::parser(()))),
            spaced(delimited_list_parser('(', ')', ',', block_arg)),
            spaced(optional(AttributeDict::parser(()))),
            spaces().with(optional(token('!').with(usize::parser(())))),
        )
            .skip(spaced(token(':')))
            .parse_stream(state_stream)
            .into_result()?
            .0;

        // We've parsed the components. Now construct the result.
        let (arg_names, arg_types): (Vec<_>, Vec<_>) = args.into_iter().unzip();
        let block = BasicBlock::new(state_stream.state.ctx, Some(label.clone()), arg_types);

        // Set the location of the block to be from where we just parsed it.
        // If it has a different one specified as part of its outlined information,
        // that will be overwritten later.
        block.deref_mut(state_stream.state.ctx).set_loc(loc.clone());

        // Set seen attributes.
        if let Some(attrs) = attrs_opt {
            *block.deref_mut(state_stream.state.ctx).attributes_mut() = attrs;
        }

        for (arg_idx, (arg_loc, name)) in arg_names.into_iter().enumerate() {
            let def: Value = block
                .deref(state_stream.state.ctx)
                .args_slice(state_stream.state.ctx)[arg_idx]
                .as_value(block);
            state_stream.state.name_tracker.ssa_def(
                state_stream.state.ctx,
                &(name.clone(), arg_loc),
                def,
            )?;
            given_names::set_block_arg_name(state_stream.state.ctx, block, arg_idx, Some(name));
        }

        // Register in outline parse state if !N was found.
        if let Some(outindex) = outindex_opt {
            register_block_for_outline(state_stream, outindex, block, outline_loc)?;
        }

        state_stream
            .state
            .name_tracker
            .block_def(state_stream.state.ctx, &(label, loc), block)?;

        // Parse the ops in the block
        let ops: Vec<_> = spaces()
            .with(sep_by::<Vec<_>, _, _, _>(
                Operation::parser(OperationParserConfig {
                    look_for_outlined_attrs: false,
                })
                .skip(spaces()),
                token(';').skip(spaces()),
            ))
            .parse_stream(state_stream)
            .into_result()?
            .0;
        for op in ops {
            op.insert_at_back(block, state_stream.state.ctx);
        }

        Ok(block).into_parse_result()
    }
}
