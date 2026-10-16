// SPDX-License-Identifier: Apache-2.0
// Copyright (c) The pliron contributors

//! Provide linked-list operations for `Ptr<T: LinkedList>`.

use crate::context::{Context, Ptr, private::ArenaObj};

/// The setter methods on [LinkedList] and [ContainsLinkedList]
/// are not safe for using from anywhere except the impls here.
/// But Rust doesn't allow private trait functions. Hence this
/// [workaround](https://jack.wrenn.fyi/blog/private-trait-methods/)
pub(crate) mod private {
    use crate::context::{Context, Ptr, private::ArenaObj};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    pub trait ContainsLinkedList<T: LinkedList> {
        /// Simply set the head pointer.
        fn set_head(&mut self, head: Option<Ptr<T>>);
        /// Simply set the tail pointer
        fn set_tail(&mut self, tail: Option<Ptr<T>>);
    }

    pub trait LinkedList: ArenaObj + PartialEq {
        type ContainerType: super::ContainsLinkedList<Self> + ArenaObj
        where
            Self: super::LinkedList;
        /// PLIRON: the dense side-table holding this node type's list
        /// links — see [super::NodeLinks].
        fn links_store(ctx: &Context) -> &RefCell<Vec<super::NodeLinks<Self>>>;
        /// Simply set the container for this node.
        fn set_container(&mut self, container: Option<Ptr<Self::ContainerType>>)
        where
            Self: super::LinkedList;
    }
}

/// PLIRON: next/prev links for one linked-list node, stored in a dense
/// per-node-kind table ([private::LinkedList::links_store]) indexed by
/// the node's `ArenaIndex` index component. List walks read this array
/// (~24 bytes/entry, contiguous) instead of dereferencing each ~250-byte
/// node through the slotmap just to follow one pointer — a
/// dependent-load-per-hop chase measured at several percent of total
/// compile time. The `container` link intentionally stays inside the
/// node: `get_parent_*` accessors have no `Context` to reach this table.
///
/// `tag` stores the node's full `ArenaIndex` so a stale entry left by a
/// deallocated node (whose slot index was reused) reads as unlinked
/// rather than handing out the old node's links.
pub struct NodeLinks<T: private::LinkedList> {
    tag: Option<crate::context::ArenaIndex>,
    next: Option<Ptr<T>>,
    prev: Option<Ptr<T>>,
}

// Manual impls: derives would impose `T: Clone/Copy/Default`, which the
// node types don't satisfy.
impl<T: private::LinkedList> Clone for NodeLinks<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: private::LinkedList> Copy for NodeLinks<T> {}
impl<T: private::LinkedList> Default for NodeLinks<T> {
    fn default() -> Self {
        Self {
            tag: None,
            next: None,
            prev: None,
        }
    }
}

/// A node's index inside its dense side tables — the `ArenaIndex`
/// index component (`as_ffi` packs `index` in the low 32 bits).
/// Shared by the links and op-dispatch tables.
#[inline]
pub(crate) fn links_index<T: crate::context::private::ArenaObj>(ptr: Ptr<T>) -> usize {
    use slotmap::Key;
    ptr.idx.data().as_ffi() as u32 as usize
}

impl<T: private::LinkedList> NodeLinks<T> {
    /// `ptr`'s links, or the all-`None` default when the entry is absent
    /// or belongs to a previous occupant of the slot (tag mismatch).
    #[inline]
    fn get(ctx: &Context, ptr: Ptr<T>) -> Self {
        T::links_store(ctx)
            .borrow()
            .get(links_index(ptr))
            .filter(|links| links.tag == Some(ptr.idx))
            .copied()
            .unwrap_or_default()
    }
}

/// Run `f` on `ptr`'s links entry, growing the table as needed.
fn update_links<T: private::LinkedList>(
    ctx: &Context,
    ptr: Ptr<T>,
    f: impl FnOnce(&mut NodeLinks<T>),
) {
    let mut links = T::links_store(ctx).borrow_mut();
    let i = links_index(ptr);
    if links.len() <= i {
        links.resize(i + 1, NodeLinks::default());
    }
    let links = &mut links[i];
    links.tag = Some(ptr.idx);
    f(links);
}

/// An object that contains a linked list.
pub trait ContainsLinkedList<T: LinkedList>: private::ContainsLinkedList<T> {
    /// Simply get the head of the list.
    fn get_head(&self) -> Option<Ptr<T>>;
    /// Simply get the tail of the list.
    fn get_tail(&self) -> Option<Ptr<T>>;
    /// Get an iterator over the items. Context is borrowed throughout.
    fn iter<'a>(&self, ctx: &'a Context) -> Iter<'a, T> {
        Iter {
            next: self.get_head(),
            next_back: self.get_tail(),
            ctx,
        }
    }
}

/// An iterator over the elements of a [LinkedList]
pub struct Iter<'a, T: LinkedList> {
    next: Option<Ptr<T>>,
    next_back: Option<Ptr<T>>,
    ctx: &'a Context,
}

impl<T: LinkedList> Clone for Iter<'_, T> {
    fn clone(&self) -> Self {
        Self {
            next: self.next,
            next_back: self.next_back,
            ctx: self.ctx,
        }
    }
}

impl<T: LinkedList> Iterator for Iter<'_, T> {
    type Item = Ptr<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next.inspect(|curr| {
            if *curr
                == self
                    .next_back
                    .expect("Some(next) => Some(next_back) violated")
            {
                self.next = None;
                self.next_back = None;
            } else {
                self.next = curr.get_next(self.ctx);
            }
        })
    }

    fn last(mut self) -> Option<Self::Item> {
        self.next_back()
    }
}

impl<T: LinkedList> DoubleEndedIterator for Iter<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.next_back.inspect(|curr| {
            if *curr == self.next.expect("Some(next_back) => Some(next) violated") {
                self.next_back = None;
                self.next = None;
            } else {
                self.next_back = curr.get_prev(self.ctx);
            }
        })
    }
}

/// Implements a linked list based on [Ptr]
/// PLIRON: next/prev links live in a dense per-kind side table
/// ([NodeLinks]); only the container link stays in the node, so
/// `get_container` remains a node getter while list operations are
/// implemented for `Ptr<T: LinkedList>`.
pub trait LinkedList: private::LinkedList {
    /// Get a reference to the object that contains this linked list.
    fn get_container(&self) -> Option<Ptr<Self::ContainerType>>;
}

/// Append all nodes from `donor` to the back of `donee`, leaving `donor` empty.
pub fn append_linked_list<C: ContainsLinkedList<U> + ArenaObj, U: LinkedList<ContainerType = C>>(
    donee: Ptr<C>,
    donor: Ptr<C>,
    ctx: &Context,
) {
    let mut current = donor.deref(ctx).get_head();
    while let Some(node) = current {
        let next = node.get_next(ctx);
        node.unlink(ctx);
        node.insert_at_back(donee, ctx);
        current = next;
    }
}

/// Linked list operations on Ptr<T: LinkedList> for convenience and safety.
impl<T> Ptr<T>
where
    T: LinkedList,
{
    /// The node after self in its linked list. PLIRON: reads the dense
    /// per-kind [NodeLinks] table — no node deref on the walk.
    pub fn get_next(&self, ctx: &Context) -> Option<Ptr<T>> {
        NodeLinks::get(ctx, *self).next
    }

    /// The node before self in its linked list.
    pub fn get_prev(&self, ctx: &Context) -> Option<Ptr<T>> {
        NodeLinks::get(ctx, *self).prev
    }

    /// Assert self is currently unlinked (no list links, no container).
    fn assert_unlinked(&self, ctx: &Context) {
        let links = NodeLinks::get(ctx, *self);
        assert!(
            links.prev.is_none()
                && links.next.is_none()
                && self.deref(ctx).get_container().is_none(),
            "LinkedList node must be unlinked before relinking"
        );
    }

    /// Insert self after mark.
    pub fn insert_after(&self, ctx: &Context, mark: Ptr<T>) {
        self.assert_unlinked(ctx);
        let container = mark
            .deref(ctx)
            .get_container()
            .expect("insert_after: Mark node itself is unlinked");
        let next = mark.get_next(ctx);
        match next {
            Some(next) => {
                assert!(next.get_prev(ctx).unwrap() == mark);
                update_links(ctx, next, |links| links.prev = Some(*self));
            }
            None => {
                assert!(container.deref(ctx).get_tail().unwrap() == mark);
                private::ContainsLinkedList::set_tail(
                    &mut (*container.deref_mut(ctx)),
                    Some(*self),
                );
            }
        }
        update_links(ctx, mark, |links| links.next = Some(*self));
        update_links(ctx, *self, |links| {
            links.next = next;
            links.prev = Some(mark);
        });
        self.deref_mut(ctx).set_container(Some(container));
    }

    /// Insert self before mark.
    pub fn insert_before(&self, ctx: &Context, mark: Ptr<T>) {
        self.assert_unlinked(ctx);
        let container = mark
            .deref(ctx)
            .get_container()
            .expect("insert_before: Mark node itself is unlinked");
        let prev = mark.get_prev(ctx);
        match prev {
            Some(prev) => {
                assert!(prev.get_next(ctx).unwrap() == mark);
                update_links(ctx, prev, |links| links.next = Some(*self));
            }
            None => {
                assert!(container.deref(ctx).get_head().unwrap() == mark);
                private::ContainsLinkedList::set_head(
                    &mut (*container.deref_mut(ctx)),
                    Some(*self),
                );
            }
        }
        update_links(ctx, mark, |links| links.prev = Some(*self));
        update_links(ctx, *self, |links| {
            links.prev = prev;
            links.next = Some(mark);
        });
        self.deref_mut(ctx).set_container(Some(container));
    }

    /// Insert self as the head of the list.
    pub fn insert_at_front(&self, container: Ptr<T::ContainerType>, ctx: &Context) {
        self.assert_unlinked(ctx);
        let mut container_ref = container.deref_mut(ctx);
        let head = container_ref.get_head();
        match head {
            Some(head) => {
                assert!(head.get_prev(ctx).is_none());
                update_links(ctx, head, |links| links.prev = Some(*self));
            }
            None => {
                private::ContainsLinkedList::set_tail(&mut (*container_ref), Some(*self));
            }
        }
        update_links(ctx, *self, |links| links.next = head);
        private::ContainsLinkedList::set_head(&mut (*container_ref), Some(*self));
        self.deref_mut(ctx).set_container(Some(container));
    }

    /// Insert self as the tail of the list.
    pub fn insert_at_back(&self, container: Ptr<T::ContainerType>, ctx: &Context) {
        self.assert_unlinked(ctx);
        let mut container_ref = container.deref_mut(ctx);
        let tail = container_ref.get_tail();
        match tail {
            Some(tail) => {
                assert!(tail.get_next(ctx).is_none());
                update_links(ctx, tail, |links| links.next = Some(*self));
            }
            None => {
                private::ContainsLinkedList::set_head(&mut (*container_ref), Some(*self));
            }
        }
        update_links(ctx, *self, |links| links.prev = tail);
        private::ContainsLinkedList::set_tail(&mut (*container_ref), Some(*self));
        self.deref_mut(ctx).set_container(Some(container));
    }

    /// Is this node part of a linked list?
    pub fn is_linked(&self, ctx: &Context) -> bool {
        let node = &*self.deref(ctx);
        let has_container = node.get_container().is_some();
        let links = NodeLinks::get(ctx, *self);
        assert!(
            has_container || links.next.is_none() && links.prev.is_none(),
            "LinkedList node has no container, but has next/prev node"
        );
        has_container
    }

    /// Unlink self from list.
    pub fn unlink(&self, ctx: &Context) {
        let container = self
            .deref(ctx)
            .get_container()
            .expect("LinkedList: Attempt to remove unlinked node");
        let links = NodeLinks::get(ctx, *self);
        match links.next {
            Some(next) => update_links(ctx, next, |l| l.prev = links.prev),
            None => {
                private::ContainsLinkedList::set_tail(
                    &mut (*container.deref_mut(ctx)),
                    links.prev,
                );
            }
        }
        match links.prev {
            Some(prev) => update_links(ctx, prev, |l| l.next = links.next),
            None => {
                private::ContainsLinkedList::set_head(
                    &mut (*container.deref_mut(ctx)),
                    links.next,
                );
            }
        }

        update_links(ctx, *self, |l| {
            l.next = None;
            l.prev = None;
        });
        self.deref_mut(ctx).set_container(None);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::boxed::Box;

    use super::*;
    use crate::{
        context::{Arena, Context, Ptr, private::ArenaObj},
        dict_key,
    };
    use alloc::{vec, vec::Vec};
    use core::cell::RefCell;

    #[derive(PartialEq)]
    struct LLNode {
        data: u64,
        parent: Option<Ptr<LLRoot>>,
        self_ptr: Ptr<LLNode>,
    }

    dict_key!(LINKED_LIST_TEST_STORE, "linked_list_test_store");

    impl ArenaObj for LLNode {
        fn get_arena(ctx: &Context) -> &Arena<Self> {
            let index = ctx.aux_data_map[&LINKED_LIST_TEST_STORE];
            &ctx.aux_data[index]
                .downcast_ref::<LinkedListTestArena>()
                .expect("linked_list_store is not LinkedListTestArena")
                .nodes
        }

        fn get_arena_mut(ctx: &mut Context) -> &mut Arena<Self> {
            let index = ctx.aux_data_map[&LINKED_LIST_TEST_STORE];
            &mut ctx.aux_data[index]
                .downcast_mut::<LinkedListTestArena>()
                .expect("linked_list_store is not LinkedListTestArena")
                .nodes
        }

        fn get_self_ptr(&self, _ctx: &Context) -> Ptr<Self> {
            self.self_ptr
        }

        fn dealloc_sub_objects(_ptr: Ptr<Self>, _ctx: &mut Context) {}
    }

    impl LLNode {
        pub fn new(ctx: &mut Context, data: u64) -> Ptr<LLNode> {
            let f = |self_ptr: Ptr<LLNode>| LLNode {
                self_ptr,
                data,
                parent: None,
            };
            Self::alloc(ctx, f)
        }
    }

    impl private::LinkedList for LLNode {
        type ContainerType = LLRoot;

        fn links_store(ctx: &Context) -> &RefCell<Vec<NodeLinks<Self>>> {
            let index = ctx.aux_data_map[&LINKED_LIST_TEST_STORE];
            &ctx.aux_data[index]
                .downcast_ref::<LinkedListTestArena>()
                .expect("linked_list_store is not LinkedListTestArena")
                .node_links
        }

        fn set_container(&mut self, container: Option<Ptr<Self::ContainerType>>) {
            self.parent = container;
        }
    }

    impl LinkedList for LLNode {
        fn get_container(&self) -> Option<Ptr<Self::ContainerType>> {
            self.parent
        }
    }

    struct LLRoot {
        first: Option<Ptr<LLNode>>,
        last: Option<Ptr<LLNode>>,
        self_ptr: Ptr<LLRoot>,
    }

    impl LLRoot {
        pub fn empty(ctx: &mut Context) -> Ptr<LLRoot> {
            let f = |self_ptr: Ptr<LLRoot>| LLRoot {
                self_ptr,
                first: None,
                last: None,
            };
            Self::alloc(ctx, f)
        }
    }

    impl ArenaObj for LLRoot {
        fn get_arena(ctx: &Context) -> &Arena<Self> {
            let index = ctx.aux_data_map[&LINKED_LIST_TEST_STORE];
            &ctx.aux_data[index]
                .downcast_ref::<LinkedListTestArena>()
                .expect("linked_list_store is not LinkedListTestArena")
                .containers
        }

        fn get_arena_mut(ctx: &mut Context) -> &mut Arena<Self> {
            let index = ctx.aux_data_map[&LINKED_LIST_TEST_STORE];
            &mut ctx.aux_data[index]
                .downcast_mut::<LinkedListTestArena>()
                .expect("linked_list_store is not LinkedListTestArena")
                .containers
        }

        fn get_self_ptr(&self, _ctx: &Context) -> Ptr<Self> {
            self.self_ptr
        }

        fn dealloc_sub_objects(_ptr: Ptr<Self>, _ctx: &mut Context) {}
    }

    impl ContainsLinkedList<LLNode> for LLRoot {
        fn get_head(&self) -> Option<Ptr<LLNode>> {
            self.first
        }

        fn get_tail(&self) -> Option<Ptr<LLNode>> {
            self.last
        }
    }

    impl private::ContainsLinkedList<LLNode> for LLRoot {
        fn set_head(&mut self, head: Option<Ptr<LLNode>>) {
            self.first = head;
        }

        fn set_tail(&mut self, tail: Option<Ptr<LLNode>>) {
            self.last = tail;
        }
    }

    #[derive(Default)]
    pub(crate) struct LinkedListTestArena {
        nodes: Arena<LLNode>,
        containers: Arena<LLRoot>,
        node_links: RefCell<Vec<NodeLinks<LLNode>>>,
    }

    fn validate_list(ctx: &Context, root: Ptr<LLRoot>, gold: Vec<u64>) {
        let root_vec: Vec<_> = root
            .deref(ctx)
            .iter(ctx)
            .map(|n| n.deref(ctx).data)
            .collect();
        assert!(
            root_vec == gold,
            "\nExpected: {:?}\nvs\nFound: {:?}",
            gold,
            root_vec
        );
        // Compare reverses, to test the reverse iterator.
        let root_rev_vec: Vec<_> = root
            .deref(ctx)
            .iter(ctx)
            .rev()
            .map(|n| n.deref(ctx).data)
            .collect();
        let gold_rev: Vec<_> = gold.iter().cloned().rev().collect();
        assert!(
            root_rev_vec == gold_rev,
            "\nExpected: {:?}\nvs\nFound: {:?}",
            gold_rev,
            root_rev_vec,
        );
    }

    fn add_arena_to_context(ctx: &mut Context) {
        let arena = LinkedListTestArena::default();
        let index = ctx.aux_data.insert(Box::new(arena));
        ctx.aux_data_map
            .insert(LINKED_LIST_TEST_STORE.clone(), index);
    }

    #[test]
    fn success() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);
        let root = LLRoot::empty(ctx);

        let n1 = LLNode::new(ctx, 1);
        let n2 = LLNode::new(ctx, 2);
        let n3 = LLNode::new(ctx, 3);
        let n4 = LLNode::new(ctx, 4);

        assert!(
            !n1.is_linked(ctx) && !n2.is_linked(ctx) && !n3.is_linked(ctx) && !n4.is_linked(ctx)
        );

        n1.insert_at_front(root, ctx);
        validate_list(ctx, root, vec![1]);

        assert!(
            n1.is_linked(ctx) && !n2.is_linked(ctx) && !n3.is_linked(ctx) && !n4.is_linked(ctx)
        );

        n2.insert_after(ctx, n1);
        validate_list(ctx, root, vec![1, 2]);

        assert!(n1.is_linked(ctx) && n2.is_linked(ctx) && !n3.is_linked(ctx) && !n4.is_linked(ctx));

        n4.insert_after(ctx, n2);
        validate_list(ctx, root, vec![1, 2, 4]);

        assert!(n1.is_linked(ctx) && n2.is_linked(ctx) && !n3.is_linked(ctx) && n4.is_linked(ctx));

        n2.unlink(ctx);
        validate_list(ctx, root, vec![1, 4]);
        n1.unlink(ctx);
        validate_list(ctx, root, vec![4]);
        n4.unlink(ctx);
        validate_list(ctx, root, vec![]);

        n1.insert_at_back(root, ctx);
        validate_list(ctx, root, vec![1]);
        n2.insert_at_back(root, ctx);
        validate_list(ctx, root, vec![1, 2]);

        n1.unlink(ctx);
        validate_list(ctx, root, vec![2]);
        assert!(!n1.is_linked(ctx) && n2.is_linked(ctx) && !n3.is_linked(ctx));
        n3.insert_before(ctx, n2);
        validate_list(ctx, root, vec![3, 2]);
        assert!(!n1.is_linked(ctx) && n2.is_linked(ctx) && n3.is_linked(ctx));
    }

    #[test]
    #[should_panic(expected = "must be unlinked before relinking")]
    fn reinsert_panic() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);
        let root = LLRoot::empty(ctx);

        let n1 = LLNode::new(ctx, 1);
        n1.insert_at_front(root, ctx);
        // Reinserting an exiting node must cause panic.
        n1.insert_at_front(root, ctx);
    }

    #[test]
    #[should_panic(expected = "Attempt to remove unlinked node")]
    fn uninserted_remove_panic() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);
        let n1 = LLNode::new(ctx, 1);
        // Removing an unlinked node must cause panic.
        n1.unlink(ctx);
    }

    #[test]
    #[should_panic(expected = "Attempt to remove unlinked node")]
    fn reremove_panic() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);
        let root = LLRoot::empty(ctx);

        let n1 = LLNode::new(ctx, 1);
        n1.insert_at_front(root, ctx);
        n1.unlink(ctx);
        // Removing an unlinked node must cause panic.
        n1.unlink(ctx);
    }

    #[test]
    #[should_panic(expected = " Mark node itself is unlinked")]
    fn insert_after_unlinked_panic() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);

        let n1 = LLNode::new(ctx, 1);
        let n2 = LLNode::new(ctx, 2);
        // n1 itself is unlinked, so this is a panic.
        n2.insert_after(ctx, n1);
    }

    #[test]
    #[should_panic(expected = " Mark node itself is unlinked")]
    fn insert_before_unlinked_panic() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);

        let n1 = LLNode::new(ctx, 1);
        let n2 = LLNode::new(ctx, 2);
        // n1 itself is unlinked, so this is a panic.
        n2.insert_before(ctx, n1);
    }

    #[test]
    fn test_clear_linked_list() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);
        let root = LLRoot::empty(ctx);

        let n1 = LLNode::new(ctx, 1);
        let n2 = LLNode::new(ctx, 2);
        let n3 = LLNode::new(ctx, 3);

        n1.insert_at_front(root, ctx);
        n2.insert_at_back(root, ctx);
        n3.insert_at_back(root, ctx);
        validate_list(ctx, root, vec![1, 2, 3]);
    }

    #[test]
    fn test_append_linked_list() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);
        let root1 = LLRoot::empty(ctx);
        let root2 = LLRoot::empty(ctx);

        let n1 = LLNode::new(ctx, 1);
        let n2 = LLNode::new(ctx, 2);
        let n3 = LLNode::new(ctx, 3);
        let n4 = LLNode::new(ctx, 4);

        n1.insert_at_back(root1, ctx);
        n2.insert_at_back(root1, ctx);
        n3.insert_at_back(root2, ctx);
        n4.insert_at_back(root2, ctx);

        validate_list(ctx, root1, vec![1, 2]);
        validate_list(ctx, root2, vec![3, 4]);

        super::append_linked_list(root1, root2, ctx);

        validate_list(ctx, root1, vec![1, 2, 3, 4]);
        validate_list(ctx, root2, vec![]);

        assert!(n1.is_linked(ctx) && n2.is_linked(ctx) && n3.is_linked(ctx) && n4.is_linked(ctx));
    }

    #[test]
    fn test_append_empty_to_nonempty() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);
        let root1 = LLRoot::empty(ctx);
        let root2 = LLRoot::empty(ctx);

        let n1 = LLNode::new(ctx, 1);
        n1.insert_at_back(root1, ctx);

        super::append_linked_list(root1, root2, ctx);
        validate_list(ctx, root1, vec![1]);
        validate_list(ctx, root2, vec![]);
    }

    #[test]
    fn test_append_nonempty_to_empty() {
        let ctx = &mut Context::default();
        add_arena_to_context(ctx);
        let root1 = LLRoot::empty(ctx);
        let root2 = LLRoot::empty(ctx);

        let n1 = LLNode::new(ctx, 1);
        n1.insert_at_back(root2, ctx);

        super::append_linked_list(root1, root2, ctx);
        validate_list(ctx, root1, vec![1]);
        validate_list(ctx, root2, vec![]);
    }
}
