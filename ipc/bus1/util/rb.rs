// SPDX-License-Identifier: GPL-2.0
//! # Intrusive Red-Black Trees
//!
//! This module implements an intrusive Red-Black Tree. Internally, it uses the
//! common infrastructure provided by the C implementation `lib/rbtree.c` and
//! is designed to work in a very similar manner.
//!
//! The entire API is meant to be completely safe to use. However, in the C API
//! you cannot assert whether a node is attached to a specific instance of a
//! tree. This makes ad-hoc operations unsafe or expensive, since object
//! ownership has to be verified. Therefore, this implementation extents all
//! nodes with a tag that asserts ownership and thus circumvents this
//! restriction. The cost is an additional pointer-sized field in each node.
//!
//! The API is designed to be very similar to the API of common Rust
//! collections that own their entries (e.g., `alloc::collections::BTreeMap`).
//! That is, [`Tree`] is the entry-point of every rb-tree operation, and it
//! owns all entries stored in this tree. However, unlike the standard Rust
//! collections, [`Tree`] only stores smart-pointers, and never allocates or
//! moves entries. Instead, it relies on
//! [`IntoDeref`](crate::util::convert::IntoDeref) to convert smart pointers
//! into raw pointers. Furthermore, it uses an intrusive design, so it relies
//! on metadata on the nodes to link/unlink. It uses field projections to be
//! generic over where this metadata is stored (see
//! [`Field`](crate::util::field::Field) for details).

use core::ptr::NonNull;
use kernel::prelude::*;
use kernel::sync::atomic;

use crate::util;

/// Red-Black Tree that stores and manages elements.
///
/// A [`Tree`] can be used to link and unlink elements, and thus transfer
/// ownership of an element into a tree. Those elements can then be searched
/// for, or can be iterated, similar to other standard Rust collections.
///
/// Elements stored in a [`Tree`] are owned by that tree, but are not moved,
/// nor allocated by the tree. Instead, the tree takes ownership of a pointer
/// to the element (either via smart pointers, or via references that have a
/// lifetime that exceeds the lifetime of the tree). Once an element is
/// removed, ownership is transferred back to the caller.
///
/// Trees are intrusive in nature, meaning they rely on metadata on each
/// element to manage tree internals. This metadata must be a member field of
/// type [`Node`]. The exact member field that is used for a tree is provided
/// via a generic field representing type (`FRT`). All elements stored in a
/// single tree will use the same member field. But a single element can have
/// multiple different member fields of type [`Node`], and thus be linked into
/// multiple different trees simultaneously.
pub struct Tree<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    // Rb-tree metadata for the entire tree. In their most basic form, this is
    // just a pointer to the root node (but caching variants are an option to
    // improve lookup of the first and last entry).
    root: kernel::bindings::rb_root,
    // We need a unique identifier to store as `owner` marker in nodes. The
    // address of the owning tree is a reasonable default that can be acquired
    // without external interference. Downside is that the tree needs to be
    // pinned. Since pinning is ubiquitious in kernel APIs, this seems like an
    // acceptable price to pay.
    _pin: core::marker::PhantomPinned,
    // Different trees can store entries of type `Ref` via different nodes. By
    // pinning the field-representing-type, it is always clear which node a
    // tree is using.
    _lrt: core::marker::PhantomData<Lrt>,
}

/// Red-Black Tree metadata required on each element.
///
/// Every element that is stored in a [`Tree`] must have a member field of type
/// [`Node`]. A [`Tree`] is generic over the member field used, so a single
/// element can have multiple nodes to be stored in multiple different trees.
/// All elements stored in the same tree will use the same member field for
/// that tree, though.
pub struct Node {
    // Since this is an owning intrusive collection, we carefully ensure to
    // never create multiple references to a single node. With a non-owning
    // intrusive collection, we would need an `UnsafePinned` here, to ensure
    // references retained by the caller do not alias with references created
    // during tree introspection or manipulation. Yet, for owning collections
    // this is unnecessary.
    // We still use `Opaque` over `UnsafeCell` here, since interior mutability
    // is required, and we really don't want to rely too much on the
    // implementation details of `rbtree.c`. And we get `UnsafePinned` that way
    // as well, so future non-owning extensions to this API would need no
    // adjustments.
    bindings: kernel::types::Opaque<kernel::bindings::rb_node>,
    // The owner field is a tag that uniquely identifies the tree that owns the
    // entry. Anything could be used as tag, but we decided on the tree address
    // as it is trivially unique for each pinned tree.
    // A value of 0 means the entry is unlinked. Any other value marks the
    // entry as owned by the tree with the given tag. The value can only be set
    // or cleared when holding a mutable reference to the owning tree. Acquire
    // and Release semantics are guaranteed by any link/unlink functions, so
    // entries can be moved from one tree to another, even across tasks.
    owner: atomic::Atomic<usize>,
    // RB-Tree node bindings store pointers to other nodes, so nodes must
    // always be pinned when linked.
    _pin: core::marker::PhantomPinned,
}

enum CursorPos {
    Empty,
    Front(NonNull<Node>),
    At(NonNull<Node>),
    Back(NonNull<Node>),
}

/// Mutable cursor over entries of an RB-Tree.
///
/// This cursor either points at an empty tree, directly at a node in a
/// non-empty tree, before the first node, or after the last node. The cursor
/// can be moved back and forth, and elements can be inserted and removed at
/// will.
pub struct CursorMut<'tree, Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    tree: Pin<&'tree mut Tree<Lrt>>,
    pos: CursorPos,
}

/// Mutable slot in an RB-Tree.
///
/// This slot points at a location in an RB-Tree, which can either be an
/// existing element, or an empty slot. It is usually obtained by searching
/// a tree for a specific key. If an element matching the key is found, the
/// slot of that element is returned. Otherwise, an empty slot suitable for
/// insertion of an element with such a key is returned.
///
/// Slots mutably borrow the tree they reference, and as such allow insertion
/// of new elements into the tree.
pub struct Slot<'tree, Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    tree: Pin<&'tree mut Tree<Lrt>>,
    anchor: *mut kernel::bindings::rb_node,
    slot: *mut *mut kernel::bindings::rb_node,
}

#[doc(hidden)]
#[macro_export]
macro_rules! util_rb_node_of {
    ($ref:ty, $field:ident $(,)?) => {
        $crate::util::intrusive::link_of!{$ref, $field, $crate::util::rb::Node}
    }
}

/// Alias of [`link_of!()`](util::intrusive::link_of) for [`Node`] members.
#[doc(inline)]
pub use util_rb_node_of as node_of;

impl<Lrt> Tree<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Create a new empty tree.
    ///
    /// Note that empty trees do not have to be pinned, but to insert elements
    /// into a tree, it must be pinned. Therefore, the returned tree must be
    /// either pinned in place, or moved into a pinned structure to be used for
    /// insertions.
    pub fn new() -> Self {
        Self {
            root: kernel::bindings::rb_root {
                rb_node: core::ptr::null_mut(),
            },
            _pin: core::marker::PhantomPinned,
            _lrt: core::marker::PhantomData,
        }
    }

    fn panic_acquire(_v: Pin<Lrt::Ref>) -> ! {
        core::panic!("attempting to link a foreign node");
    }

    fn panic_claim(_v: Pin<&Lrt::Target>) -> ! {
        core::panic!("attempting to claim a foreign node");
    }

    // Return the memory address of this tree as an integer. This is used to
    // tag nodes belonging to this tree.
    //
    // Note that the address of a tree is only stable if the tree is pinned.
    // Since all modifications to a tree take a `Pin<&mut Self>`, this is
    // given. We avoid taking a pinned tree here, to allow read-only queries to
    // work with `&self` for simpler APIs. It is up to the caller to ensure
    // that the result is used coherently.
    //
    // Also note that the drop handler of a tree ensures all entries are
    // unlinked before a tree is unpinned. Therefore, owner tags are usually
    // never used longer than a tree is pinned.
    fn as_owner(&self) -> usize {
        util::ptr_addr(core::ptr::from_ref(self))
    }

    fn root_mut(self: Pin<&mut Self>) -> &mut kernel::bindings::rb_root {
        // SAFETY: `Self.root` is not structurally pinned.
        unsafe { &mut Pin::into_inner_unchecked(self).root }
    }

    /// Check whether the tree is empty.
    ///
    /// This returns `true` is no entries are linked, `false` if at least one
    /// entry is linked.
    ///
    /// Note that the tree does not maintain a counter of how many elements are
    /// linked.
    pub fn is_empty(&self) -> bool {
        self.root.rb_node.is_null()
    }

    /// Check whether this tree contains a given node.
    ///
    /// This returns `true` if the node is linked into this tree. It returns
    /// `false` if the node is currently unlinked or linked into another tree.
    ///
    /// This operation is performed in O(1), regardless of the number of
    /// elements in the tree.
    ///
    /// While holding a reference to [`Tree`], no node can be linked to, or
    /// unlinked from, the tree. Hence, unlike [`Node::is_linked()`] the return
    /// value of this method is stable for at least the lifetime of `self`.
    pub fn contains(
        &self,
        ent_target: &Lrt::Target,
    ) -> bool {
        let ent_node = Lrt::project(ent_target);
        // SAFETY: `ent_node` is convertible to a reference.
        let v = unsafe {
            (*Node::owner(ent_node)).load(atomic::Relaxed)
        };
        v == self.as_owner()
    }

    /// Create a mutable cursor at the first element.
    ///
    /// The new cursor will point to the first element. If the tree is empty,
    /// the cursor points to no element at all.
    pub fn cursor_mut_first(
        mut self: Pin<&mut Self>,
    ) -> CursorMut<'_, Lrt> {
        // SAFETY: `rb_first()` requires a pointer to a valid root, pointing to
        // valid nodes. This invariant is maintained by `Tree`.
        let pos = if let Some(v) = NonNull::new(unsafe {
            kernel::bindings::rb_first(
                self.as_mut().root_mut(),
            )
        }) {
            // SAFETY: `v` points to a valid entry in the tree, thus it is
            // embedded in a `Node`.
            CursorPos::At(unsafe { Node::from_rb(v) })
        } else {
            CursorPos::Empty
        };

        CursorMut {
            tree: self,
            pos,
        }
    }

    /// Create a mutable cursor at the last element.
    ///
    /// The new cursor will point to the last element. If the tree is empty,
    /// the cursor points to no element at all.
    pub fn cursor_mut_last(
        mut self: Pin<&mut Self>,
    ) -> CursorMut<'_, Lrt> {
        // SAFETY: `rb_last()` requires a pointer to a valid root, pointing to
        // valid nodes. This invariant is maintained by `Tree`.
        let pos = if let Some(v) = NonNull::new(unsafe {
            kernel::bindings::rb_last(
                self.as_mut().root_mut(),
            )
        }) {
            // SAFETY: `v` points to a valid entry in the tree, thus it is
            // embedded in a `Node`.
            CursorPos::At(unsafe { Node::from_rb(v) })
        } else {
            CursorPos::Empty
        };

        CursorMut {
            tree: self,
            pos,
        }
    }

    /// Try creating a mutable cursor at an explicit element.
    ///
    /// This tries to create a [`CursorMut`] at the given element. If the
    /// element is not linked in this tree, this will return `None` instead.
    pub fn try_cursor_mut_at(
        mut self: Pin<&mut Self>,
        ent_target: Pin<&Lrt::Target>,
    ) -> Option<CursorMut<'_, Lrt>> {
        let ent_node = Lrt::project(&ent_target);
        // SAFETY: `end_node` points to a valid allocation.
        let v = unsafe {
            (*Node::owner(ent_node)).load(atomic::Relaxed)
        };
        if v == self.as_mut().as_owner() {
            Some(CursorMut {
                tree: self,
                pos: CursorPos::At(ent_node),
            })
        } else {
            None
        }
    }

    /// Find a slot in the tree.
    ///
    /// Search through the tree with the given comparison function, looking for
    /// a specific slot. Regardless whether the slot is occupied or not, this
    /// will return a `Slot` object.
    ///
    /// This will perform a search through the binary tree from root to leaf,
    /// using `cmp_fn` on each node. `cmp_fn` can be chosen freely, but should
    /// preferably implement a partial order to ensure a coherent tree order.
    pub fn find_slot_by<CmpFn>(
        mut self: Pin<&mut Self>,
        mut cmp_fn: CmpFn,
    ) -> Slot<'_, Lrt>
    where
        CmpFn: FnMut(Pin<&Lrt::Target>) -> core::cmp::Ordering,
    {
        let mut anchor: *mut kernel::bindings::rb_node;
        let mut slot: &mut *mut kernel::bindings::rb_node;

        anchor = core::ptr::null_mut();
        slot = &mut self.as_mut().root_mut().rb_node;

        while let Some(mut ent_rb) = NonNull::new(*slot) {
            // SAFETY: All rb-entriess in a tree always refer to a valid
            //     rb-entry within a valid node.
            let ent_node = unsafe { Node::from_rb(ent_rb) };
            // SAFETY: All nodes in a tree always refer to a valid node
            //     within a reference target.
            let ent_target = unsafe { Lrt::borrow(ent_node) };

            slot = match cmp_fn(ent_target) {
                core::cmp::Ordering::Less => {
                    // SAFETY: `ent_rb` points to a valid node and no other
                    //     references to it exist.
                    unsafe { &mut ent_rb.as_mut().rb_left }
                },
                core::cmp::Ordering::Greater => {
                    // SAFETY: `ent_rb` points to a valid node and no other
                    //     references to it exist.
                    unsafe { &mut ent_rb.as_mut().rb_left }
                },
                core::cmp::Ordering::Equal => break,
            };
            anchor = ent_rb.as_ptr();
        }

        Slot {
            anchor,
            slot,
            tree: self,
        }
    }

    /// Remove all entries from a tree.
    ///
    /// Clear the entire tree and pass ownership of each entry by invoking
    /// `clear_fn`.
    ///
    /// This will iterate the tree in postorder, without rebalancing. Hence,
    /// this is significantly faster than clearing a tree via [`CursorMut`].
    pub fn clear_with<ClearFn>(
        mut self: Pin<&mut Self>,
        mut clear_fn: ClearFn,
    )
    where
        ClearFn: FnMut(Pin<Lrt::Ref>),
    {
        let mut anchor: *mut kernel::bindings::rb_node;

        // SAFETY: `rb_first_postorder()` requires a pointer to a valid root,
        //     pointing to valid nodes. This invariant is maintained by `Tree`.
        anchor = unsafe {
            kernel::bindings::rb_first_postorder(
                self.as_mut().root_mut(),
            )
        };

        // Clear the tree, so it is considered empty. Since nodes do not
        // contain pointers to the root, this cannot affect the postorder
        // traversal below.
        self.as_mut().root_mut().rb_node = core::ptr::null_mut();

        while let Some(ent_rb) = NonNull::new(anchor) {
            // SAFETY: Same as for `rb_first_postorder()` above, but only cares
            //     for elements following it, not any elements preceding it.
            //     Since we call it before calling `clear_fn`, it is safe.
            anchor = unsafe {
                kernel::bindings::rb_next_postorder(ent_rb.as_ptr())
            };

            // SAFETY: `ent_rb` is a valid rb-entry in a valid node.
            let ent_node = unsafe { Node::from_rb(ent_rb) };
            // SAFETY: `end_node` is a valid node.
            unsafe { (*Node::owner(ent_node)).store(0, atomic::Release) };
            // SAFETY: `end_node` is a valid node in a reference target.
            let ent = unsafe { Lrt::release(ent_node) };
            clear_fn(ent);
        }
    }
}

// Convenience helpers
impl<Lrt> Tree<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Create a mutable cursor at an explicit element.
    ///
    /// Works like [`Tree::try_cursor_mut_at()`] but panics if the element is
    /// not linked into this tree.
    pub fn cursor_mut_at(
        self: Pin<&mut Self>,
        ent_target: Pin<&Lrt::Target>,
    ) -> CursorMut<'_, Lrt> {
        self.try_cursor_mut_at(ent_target).unwrap_or_else(
            || Self::panic_claim(ent_target),
        )
    }

    /// Try linking a new entry in this tree.
    ///
    /// This combines [`Tree::find_slot_by()`] with [`Slot::try_link()`].
    ///
    /// On success, the ownership of the new entry is transferred to the tree
    /// and the dereferenced entry is returned as a borrow wrapped in `Ok`. If
    /// there either already is a matching entry linked into the tree, or if
    /// `ent` is already linked into any tree, this function will fail and
    /// return ownership of the new entry to the caller wrapped in `Err`.
    ///
    /// If this function fails, the slot is immediately dropped. If a retry
    /// attempt is desired, the slot should be retrieved via `find_slot_by()`
    /// instead. This avoids repeated traversals.
    ///
    /// ## Comparator
    ///
    /// The comparator `cmp_fn` gets the dereferenced to-be-linked entry `ent`
    /// as first argument and an existing entry in the tree as second argument.
    /// It shall return an order describing the relationship of the first
    /// argument compared to the second.
    pub fn try_link_by<CmpFn>(
        self: Pin<&mut Self>,
        ent: Pin<Lrt::Ref>,
        mut cmp_fn: CmpFn,
    ) -> Result<Pin<&'_ Lrt::Target>, Pin<Lrt::Ref>>
    where
        CmpFn: FnMut(&Pin<Lrt::Ref>, Pin<&Lrt::Target>) -> core::cmp::Ordering,
    {
        self.find_slot_by(
            |other| cmp_fn(&ent, other),
        ).try_link(ent)
    }

    /// Try unlinking a specific element from the tree.
    ///
    /// This chains [`Tree::try_cursor_mut_at()`] and [`Slot::try_unlink()`].
    ///
    /// This returns the element if it was unlinked from the tree, or `None` if
    /// the element was not linked into this tree.
    pub fn try_unlink(
        self: Pin<&mut Self>,
        ent_target: Pin<&Lrt::Target>,
    ) -> Option<Pin<Lrt::Ref>> {
        self.try_cursor_mut_at(ent_target).and_then(|v| {
            v.try_unlink()
        })
    }

    /// Unlink a specific element from the tree.
    ///
    /// Works like [`Tree::try_unlink()`] but panics if the entry was not
    /// linked into this tree.
    pub fn unlink(
        self: Pin<&mut Self>,
        ent_target: Pin<&Lrt::Target>,
    ) -> Pin<Lrt::Ref> {
        self.try_unlink(ent_target).unwrap_or_else(
            || core::panic!("attempting to unlink foreign entry"),
        )
    }

    /// Remove all entries from a tree.
    ///
    /// Works like [`Tree::clear_with()`] but uses [`core::mem::drop()`] as
    /// callback.
    pub fn clear(self: Pin<&mut Self>) {
        self.clear_with(|_| {});
    }
}

// SAFETY: Trees have no interior mutability, nor do they otherwise care for
//     their calling CPU. They can be freely sent across CPUs, only limited by
//     the stored type.
unsafe impl<Lrt> Send for Tree<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
    Lrt::Ref: Send,
{
}

// SAFETY: Trees have no interior mutability, nor do they otherwise care for
//     their calling CPU. They can be shared across CPUs, only limited by
//     the stored type.
unsafe impl<Lrt> Sync for Tree<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
    Lrt::Ref: Sync,
{
}

impl<Lrt> core::default::Default for Tree<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Return a new empty tree.
    ///
    /// The new tree has no entries linked and is completely independent of
    /// other trees.
    fn default() -> Self {
        Self::new()
    }
}

impl<Lrt> core::ops::Drop for Tree<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Clear a tree before dropping it.
    ///
    /// This drops all elements in the tree via [`Tree::clear()`], before
    /// dropping the tree. This ensures that the elements of a tree are
    /// not leaked.
    fn drop(&mut self) {
        // SAFETY: We treat `self` as pinned unconditionally.
        let this = unsafe { Pin::new_unchecked(self) };
        this.clear();
    }
}

impl Node {
    /// Create a new unlinked node.
    ///
    /// Note that unlinked nodes do not need to be pinned. However, to link a
    /// node into a tree, it must be pinned. Therefore, you need to pin the
    /// returned value in place, or move it into a pinned structure, to make
    /// use of it.
    pub fn new() -> Self {
        Self {
            owner: atomic::Atomic::new(0),
            bindings: kernel::types::Opaque::new(
                kernel::bindings::rb_node {
                    __rb_parent_color: 0,
                    rb_right: core::ptr::null_mut(),
                    rb_left: core::ptr::null_mut(),
                },
            ),
            _pin: core::marker::PhantomPinned,
        }
    }

    /// Return a pointer to the atomic owner tag of a node.
    ///
    /// The owner tag of a node can be accessed at any time, as long as the
    /// allocation of the node does not get deallocated. That is, the owner tag
    /// can even be accessed if another part holds a mutable reference to the
    /// node. The transparent wrapper around `UnsafeCell` in an atomic
    /// guarantee that such accesses are safe.
    ///
    /// ## Safety
    ///
    /// The node pointer must refer to a valid and initialized allocation of a
    /// node.
    unsafe fn owner(node: NonNull<Self>) -> *mut atomic::Atomic<usize> {
        // SAFETY: Delegated to caller.
        unsafe { &raw mut (*node.as_ptr()).owner }
    }

    /// Get a node pointer from an rb-entry pointer.
    ///
    /// ## Safety
    ///
    /// The rb-entry must refer to a valid allocation inside of a node. The
    /// allocation does not have to be initialized.
    unsafe fn from_rb(rb: NonNull<kernel::bindings::rb_node>) -> NonNull<Self> {
        // SAFETY: Delegated to caller.
        unsafe {
            NonNull::new_unchecked(
                kernel::container_of!(
                    kernel::types::Opaque::cast_from(rb.as_ptr()),
                    Self,
                    bindings
                ).cast_mut(),
            )
        }
    }

    /// Check whether this node is linked into a tree.
    ///
    /// This returns `true` if this node is linked into any tree. It returns
    /// `false` if the node is currently unlinked.
    ///
    /// Note that a node can be linked into a tree at any time. That is,
    /// validity of the returned boolean can change spuriously, unless the
    /// caller otherwise ensures exclusive access to the node.
    pub fn is_linked(&self) -> bool {
        // SAFETY: `self` trivially points to a valid allocation of a node.
        let v = unsafe {
            (*Self::owner(util::nonnull_from_ref(self))).load(atomic::Relaxed)
        };
        v != 0
    }
}

// SAFETY: Nodes are only ever modified through their owning tree, or through
//     atomics. Hence, they can be freely sent across CPUs.
unsafe impl Send for Node {
}

// SAFETY: Shared references to a node always use atomics for any data access.
//     They can be freely shared across CPUs.
unsafe impl Sync for Node {
}

impl core::clone::Clone for Node {
    /// Returns a clean and unlinked node.
    ///
    /// Cloning a node always yields a new node that is unlinked and in no way
    /// tied to the original node.
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl core::default::Default for Node {
    /// Return a clean and unlinked node.
    ///
    /// The default state for nodes is an unlinked state. Such nodes are in no
    /// way tied to a tree or any other node.
    fn default() -> Self {
        Self::new()
    }
}

impl core::ops::Drop for Node {
    /// Drop a node and verify it is unlinked.
    ///
    /// No special cleanup is required when dropping nodes. However, linked
    /// nodes are owned by their respective tree and as such must never be
    /// dropped. In an owning tree, this cannot happen, but in non-owning trees
    /// it is the responsibility of the caller to ensure nodes are unlinked
    /// before they are dropped.
    ///
    /// Since this is an owning tree implementation, this drop handler is a
    /// safety net to ensure a correct implementation.
    ///
    /// ## Background
    ///
    /// The drop handler could attempt to disassociate the node. However, this
    /// only works if node and tree are owned by the same thread. Since
    /// [`Node`] was designed with [`Send`], it can be dropped by another
    /// thread (possibly in parallel with a drop of the tree). Any attempt to
    /// unlink would thus race.
    ///
    /// In case of non-owning trees, neither tree nor node can ensure the other
    /// is valid for even the shortest interval, and thus cannot attempt any
    /// unlink operation. Instead, validity of nodes is an invariant that must
    /// be upheld by the user, and is protected by this drop implementation. In
    /// case of an owning tree, nodes are always valid while linked, and thus
    /// this drop implementation will hopefully be a no-op.
    fn drop(&mut self) {
        // SAFETY: The allocation behind `self` is valid.
        let owner = unsafe {
            (*Node::owner(util::nonnull_from_mut(self))).load(atomic::Relaxed)
        };
        if owner != 0 {
            core::panic!(
                "attempting drop of a claimed node: {:?}",
                core::ptr::from_ref(&*self),
            );
        }
    }
}

impl<'tree, Lrt> CursorMut<'tree, Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Unlink a specific node from the tree.
    ///
    /// ## Safety
    ///
    /// `ent_node` must point to a valid entry in `tree`.
    unsafe fn unlink_at(
        mut tree: Pin<&mut Tree<Lrt>>,
        ent_node: NonNull<Node>,
    ) -> Pin<Lrt::Ref> {
        // SAFETY: `rb_erase` only reshuffles a tree. So it is enough to
        //     ensure it is passed a valid root with only valid nodes. This
        //     invariant is always maintained by `Tree`.
        unsafe {
            kernel::bindings::rb_erase(
                ent_node.as_ref().bindings.get(),
                tree.as_mut().root_mut(),
            )
        };

        // SAFETY: `ent_node` refers to a valid entry.
        unsafe {
            (*Node::owner(ent_node)).store(0, atomic::Release);
        }

        // SAFETY: `ent_node` was removed from the tree, as such it is
        //     guaranteed to not be used any further.
        unsafe { Lrt::release(ent_node) }
    }

    /// Move to the next entry, if any.
    ///
    /// Move the cursor to point to the next entry. If the tree is empty, or if
    /// the cursor points to the last element, this is a no-op.
    pub fn move_next(&mut self) {
        match self.pos {
            CursorPos::Empty => {},
            CursorPos::Front(ent_node) => {
                self.pos = CursorPos::At(ent_node);
            },
            CursorPos::Back(_ent_node) => {},
            CursorPos::At(ent_node) => {
                // SAFETY: `ent_node` points to a valid entry in the tree.
                if let Some(v) = NonNull::new(unsafe {
                    kernel::bindings::rb_next(
                        ent_node.as_ref().bindings.get(),
                    )
                }) {
                    // SAFETY: `v` points to a valid entry in the tree, thus
                    // it is embedded in a `Node`.
                    self.pos = CursorPos::At(unsafe { Node::from_rb(v) });
                } else {
                    self.pos = CursorPos::Back(ent_node);
                }
            },
        }
    }

    /// Move to the previous entry, if any.
    ///
    /// Move the cursor to point to the previous entry. If the tree is empty,
    /// or if the cursor points to the first element, this is a no-op.
    pub fn move_prev(&mut self) {
        match self.pos {
            CursorPos::Empty => {},
            CursorPos::Front(_ent_node) => {},
            CursorPos::Back(ent_node) => {
                self.pos = CursorPos::At(ent_node);
            },
            CursorPos::At(ent_node) => {
                // SAFETY: `ent_node` points to a valid entry in the tree.
                if let Some(v) = NonNull::new(unsafe {
                    kernel::bindings::rb_prev(
                        ent_node.as_ref().bindings.get(),
                    )
                }) {
                    // SAFETY: `v` points to a valid entry in the tree, thus
                    // it is embedded in a `Node`.
                    self.pos = CursorPos::At(unsafe { Node::from_rb(v) });
                } else {
                    self.pos = CursorPos::Front(ent_node);
                }
            },
        }
    }

    /// Move to the next entry, trying to unlink the current entry first.
    ///
    /// If the cursor points to an element, the element is unlinked and
    /// returned. Otherwise, `None` is returned. In all cases, the cursor is
    /// moved to point to the next element.
    pub fn try_unlink_and_move_next(&mut self) -> Option<Pin<Lrt::Ref>> {
        if let CursorPos::At(ent_node) = self.pos {
            // SAFETY: `ent_node` points to a valid entry in the tree.
            self.pos = if let Some(v) = NonNull::new(unsafe {
                kernel::bindings::rb_next(
                    ent_node.as_ref().bindings.get(),
                )
            }) {
                // SAFETY: `v` points to a valid entry in the tree, thus
                // it is embedded in a `Node`.
                CursorPos::At(unsafe { Node::from_rb(v) })
            } else if let Some(v) = NonNull::new(unsafe {
                kernel::bindings::rb_prev(
                    ent_node.as_ref().bindings.get(),
                )
            }) {
                // SAFETY: `v` points to a valid entry in the tree, thus
                // it is embedded in a `Node`.
                CursorPos::Back(unsafe { Node::from_rb(v) })
            } else {
                CursorPos::Empty
            };

            // SAFETY: `ent_node` is a valid entry in `tree` and no longer
            // referenced by the cursor.
            Some(unsafe { Self::unlink_at(self.tree.as_mut(), ent_node) })
        } else {
            self.move_next();
            None
        }
    }

    /// Move to the previous entry, trying to unlink the current entry first.
    ///
    /// If the cursor points to an element, the element is unlinked and
    /// returned. Otherwise, `None` is returned. In all cases, the cursor is
    /// moved to point to the previous element.
    pub fn try_unlink_and_move_prev(&mut self) -> Option<Pin<Lrt::Ref>> {
        if let CursorPos::At(ent_node) = self.pos {
            // SAFETY: `ent_node` points to a valid entry in the tree.
            self.pos = if let Some(v) = NonNull::new(unsafe {
                kernel::bindings::rb_prev(
                    ent_node.as_ref().bindings.get(),
                )
            }) {
                // SAFETY: `v` points to a valid entry in the tree, thus
                // it is embedded in a `Node`.
                CursorPos::At(unsafe { Node::from_rb(v) })
            } else if let Some(v) = NonNull::new(unsafe {
                kernel::bindings::rb_next(
                    ent_node.as_ref().bindings.get(),
                )
            }) {
                // SAFETY: `v` points to a valid entry in the tree, thus
                // it is embedded in a `Node`.
                CursorPos::Front(unsafe { Node::from_rb(v) })
            } else {
                CursorPos::Empty
            };

            // SAFETY: `ent_node` is a valid entry in `tree` and no longer
            // referenced by the cursor.
            Some(unsafe { Self::unlink_at(self.tree.as_mut(), ent_node) })
        } else {
            self.move_prev();
            None
        }
    }

    /// Try unlinking the current entry from the tree, consuming the cursor.
    ///
    /// This will unlink the current entry under the cursor from the tree. If
    /// the cursor refers to an empty tree, this is a no-op and `None` is
    /// returned. Otherwise, the removed entry is returned.
    ///
    /// This consumes the cursor. Use [`move_next_try_unlink()`] etc. to unlink
    /// entries while retaining the cursor.
    pub fn try_unlink(mut self) -> Option<Pin<Lrt::Ref>> {
        let CursorPos::At(ent_node) = self.pos else {
            return None;
        };

        // SAFETY: `ent_node` is a valid entry in `tree`.
        Some(unsafe { Self::unlink_at(self.tree.as_mut(), ent_node) })
    }
}

// Convenience helpers
impl<'tree, Lrt> CursorMut<'tree, Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Move to the next entry, unlinking the current entry first.
    ///
    /// Works like [`Self::try_unlink_and_move_next()`] but panics if the tree
    /// is empty.
    pub fn unlink_and_move_next(&mut self) -> Pin<Lrt::Ref> {
        self.try_unlink_and_move_next().unwrap_or_else(
            || core::panic!("attempting to unlink from an empty tree"),
        )
    }

    /// Move to the previous entry, unlinking the current entry first.
    ///
    /// Works like [`Self::try_unlink_and_move_prev()`] but panics if the tree
    /// is empty.
    pub fn unlink_and_move_prev(&mut self) -> Pin<Lrt::Ref> {
        self.try_unlink_and_move_prev().unwrap_or_else(
            || core::panic!("attempting to unlink from an empty tree"),
        )
    }

    /// Unlink the current entry from the tree, consuming the cursor.
    ///
    /// Works like [`Self::try_unlink()`] but panics if the tree is empty.
    pub fn unlink(self) -> Pin<Lrt::Ref> {
        self.try_unlink().unwrap_or_else(
            || core::panic!("attempting to unlink from an empty tree"),
        )
    }
}

impl<'tree, Lrt> Slot<'tree, Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Check whether the slot is available for insertion.
    ///
    /// If the slot was already occupied by another entry of the tree, or if
    /// the slot was already consumed for an insertion, this will return
    /// `false`. Otherwise, this will return `true`.
    ///
    /// If this returns `true`, a following attempt of [`Self::try_link()`] is
    /// guaranteed to succeed for any unclaimed entry.
    pub fn available(&self) -> bool {
        if let Some(slot) = NonNull::new(self.slot) {
            // SAFETY: If non-null, `slot` is a valid reference to some slot in
            //     this tree. If the slot is non-empty, this slot is already in
            //     use and not available for insertion.
            unsafe { slot.as_ref().is_null() }
        } else {
            // The slot was already used for insertion and is now invalid.
            false
        }
    }

    fn entry_ptr(&self) -> Option<NonNull<Node>> {
        let slot = NonNull::new(self.slot)?;

        // SAFETY: `self.slot` is a valid tree entry.
        let ent_rb = NonNull::new(unsafe { *slot.as_ref() })?;

        // SAFETY: `ent_rb` refers to a valid rb-entry in the tree, and is thus
        //     embedded in a valid node.
        Some(unsafe { Node::from_rb(ent_rb) })
    }

    /// Get a reference to the entry in this slot.
    ///
    /// If the slot is occupied, this will return a reference to the entry in
    /// this slot. Otherwise, this will return `None`.
    pub fn entry(&self) -> Option<Pin<&Lrt::Target>> {
        match self.entry_ptr() {
            None => None,
            // SAFETY: `entry_ptr()` only returns valid entryies.
            Some(v) => Some(unsafe { Lrt::borrow(v) }),
        }
    }

    /// Get a copy of the reference to the entry in this slot.
    ///
    /// If the slot is occupied, this will return a copy of the reference to
    /// the entry in this slot. Otherwise, this will return `None`.
    ///
    /// This is only available if the reference type implements
    /// [`core::clone::Clone`].
    pub fn entry_clone(&self) -> Option<Pin<Lrt::Ref>>
    where
        Lrt::Ref: Clone,
    {
        self.entry_ptr().map(|v| {
            // SAFETY: `v` is a valid tree entry, and as such is from
            //     `acquire()`.
            unsafe { Lrt::borrow_clone(v) }
        })
    }

    /// Try linking a new entry in this slot.
    ///
    /// If the slot is occupied, was already used for linking, or if the passed
    /// entry is already linked into a tree, this will return `Err`, moving
    /// ownership of the new entry back to the caller.
    ///
    /// Otherwise, this will move ownership of the entry to the tree and link
    /// the entry in this slot. A reference to the target is returned via `Ok`.
    ///
    /// Note that a slot can only be used once to link an entry. Once linked
    /// successfully, the slot must be considered unavailable and should be
    /// dropped. Neither [`Slot::entry()`], nor any other operations will be
    /// available on a used slot. The only reason this does not consume the
    /// slot, is to allow re-use of the slot in case the link failed.
    pub fn try_link(
        &mut self,
        ent: Pin<Lrt::Ref>,
    ) -> Result<Pin<&'tree Lrt::Target>, Pin<Lrt::Ref>> {
        // If the entry was already occupied, or already used for insertion, it
        // is no longer available. Refuse to attempt the link.
        if !self.available() {
            return Err(ent);
        }

        // Acquire the entry and get access to the node. This can be converted
        // to a reference as long as we do not release it.
        let ent_node = Lrt::acquire(ent);

        let owner = self.tree.as_mut().as_owner();
        // SAFETY: `ent_node` is a valid node.
        let Ok(_) = (unsafe {
            (*Node::owner(ent_node)).cmpxchg(0, owner, atomic::Acquire)
        }) else {
            // If the cmpxchg fails, the entry is already claimed (either by
            // this tree or another tree). Refuse to use this entry, but return
            // it fully to the caller so it can be reused.
            //
            // SAFETY: The pointer was just obtained from `acquire()`.
            return Err(unsafe { Lrt::release(ent_node) });
        };

        // The entry was successfully claimed. Let `rb_link_node()` and
        // `rb_insert_color()` do their work. Then clear `self.{anchor,slot}`
        // as they are no longer valid for insertion.
        // Note that preferably the function would consume `self`, but that
        // would prevent the caller from re-using the slot on insertion
        // failure.
        //
        // SAFETY: As long as `self.slot` is non-null it points to a valid slot
        //     for insertion with `self.anchor` as the chosen `parent` value.
        //     This was checked via `self.available()` just now.
        //     `ent_node` was just claimed and as such is uniquely owned by
        //     this tree now. Since `self.tree` has a mutable reference, we can
        //     freely link it into the tree.
        unsafe {
            kernel::bindings::rb_link_node(
                ent_node.as_ref().bindings.get(),
                self.anchor,
                self.slot,
            );
            kernel::bindings::rb_insert_color(
                ent_node.as_ref().bindings.get(),
                self.tree.as_mut().root_mut(),
            );
        }

        self.anchor = core::ptr::null_mut();
        self.slot = core::ptr::null_mut();

        // SAFETY: The pointer was just obtained from `acquire()`.
        Ok(unsafe { Lrt::borrow(ent_node) })
    }
}

// Convenience helpers
impl<'tree, Lrt> Slot<'tree, Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Link a new entry into this slot.
    ///
    /// Works like [`Self::try_link()`] but panics if the entry cannot be
    /// linked.
    pub fn link(mut self, ent: Pin<Lrt::Ref>) -> Pin<&'tree Lrt::Target> {
        if !self.available() {
            core::panic!("attempting to link on a used slot");
        }
        match self.try_link(ent) {
            Ok(v) => v,
            Err(v) => Tree::<Lrt>::panic_acquire(v),
        }
    }
}

#[kunit_tests(bus1_util_rb)]
mod test {
    use super::*;

    #[derive(Default)]
    struct Entry {
        key: u8,
        rb: Node,
    }

    util::field::impl_pin_field!(Entry, rb, Node);

    #[test]
    fn test_basic() {
        let e0 = core::pin::pin!(Entry { key: 0, ..Default::default() });
        let e1 = core::pin::pin!(Entry { key: 1, ..Default::default() });

        let tree_o: Tree<node_of!(&Entry, rb)> = Tree::new();
        let mut tree: Pin<&mut Tree<_>> = core::pin::pin!(tree_o);

        assert!(tree.as_mut().is_empty());
        tree.as_mut().find_slot_by(|other| e0.key.cmp(&other.key))
            .link(e0.into_ref());
        assert!(!tree.as_mut().is_empty());
        assert!(
            !tree.as_mut().find_slot_by(|other| 0.cmp(&other.key))
                .available()
        );
        tree.as_mut().find_slot_by(|other| e1.key.cmp(&other.key))
            .link(e1.into_ref());
        assert!(!tree.as_mut().is_empty());

        tree.as_mut().clear();
        assert!(tree.as_mut().is_empty());
    }
}
