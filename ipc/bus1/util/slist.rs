// SPDX-License-Identifier: GPL-2.0
//! # Intrusive Single-Linked Lists
//!
//! This module implements an intrusive single linked list. It follows the
//! intrusive design described in [`intrusive`](crate::util::intrusive).
//!
//! [`List`] represents a single linked list and maintains a pointer to the
//! first element in a list. It is an owning list, which takes ownership of a
//! reference to each element stored in the list. Elements must embed a
//! [`Node`], which is used by the list to store metadata. Nodes effectively
//! store just a pointer to the next node in the list.
//!
//! Since elements of a single linked list do not have a pointer to their
//! previous element, they generally cannot be unlinked ad-hoc. Instead, they
//! can only be unlinked during iteration or if they are the first element.
//! Therefore, this implementation does not provide any way to test list
//! association in O(1). It is possible to check whether an element is linked
//! or not, but you cannot check whether it is linked into a specific list.

// XXX: Since `kernel::atomic::Atomic<*mut T>` was not yet stabilized, this
//     implementation uses `Atomic<usize>` instead, exposing provenance. This
//     will change once atomic pointers are stabilized.

use core::ptr::NonNull;
use kernel::prelude::*;
use kernel::sync::atomic;

use crate::util;

/// Intrusive single linked list to store elements.
///
/// A [`List`] is a single-linked list, where each element only knows its
/// following element. The list maintains a pointer to the first element only.
///
/// Elements stored in a [`List`] are owned by that list, but are not moved,
/// nor allocated by the list. Instead, the list takes ownership of a pointer
/// to the element (either via smart pointers, or via references that have a
/// lifetime that exceeds the lifetime of the list). Once an element is
/// removed, ownership is transferred back to the caller.
///
/// [`List`] is intrusive in nature, meaning it relies on metadata on each
/// element to manage list internals. This metadata must be a member field of
/// type [`Node`]. The exact member field that is used for a list is provided
/// via a generic field representing type (`FRT`). All elements stored in a
/// single list will use the same member field. But a single element can have
/// multiple different member fields of type [`Node`], and thus be linked into
/// multiple different lists simultaneously.
pub struct List<Ref, Frt>
where
    Ref: util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
    // Pointer to the first node in a single-linked list. This is never `NULL`,
    // but can be `END`, in which case it marks the end of the list. This
    // pointer always represents a pinned owned reference to the entry, gained
    // via `Ref::pin_into_deref()`.
    // Only uses atomics to allow `CursorMut` to treat it as `Node.next`.
    first: atomic::Atomic<usize>,
    // Lists effectively own their entries of type `Ref`. Ensure this is
    // reflected in this type.
    _ref: core::marker::PhantomData<Ref>,
    // Different lists can store entries of type `Ref` via different nodes. By
    // pinning the field-representing-type, it is always clear which node a
    // list is using. `Field<T>` is prohibited from exposing any subtyping, so
    // we can directly embed `Frt` here.
    _frt: core::marker::PhantomData<Frt>,
}

/// Metadata required for elements of a [`List`].
///
/// Every element that is stored in a [`List`] must have a member field of
/// type [`Node`]. [`List`] is generic over the member field used, so a
/// single element can have multiple nodes to be stored in multiple different
/// lists. All elements stored in the same list will use the same member field
/// for that list, though.
pub struct Node {
    // A pointer to the next node. This is `NULL` if the node is unlinked,
    // `END` if it is the last element in the list. This pointer always
    // represents a pinned owned reference to the entry, gained via
    // `Ref::pin_into_deref()`.
    // This is an atomic to allow acquiring unused nodes. Once acquired, a list
    // can use non-atomic reads. Writes must be atomic still, to prevent
    // temporary releases.
    pub(crate) next: atomic::Atomic<usize>,
    // List nodes store pointers to other nodes, so nodes must always be pinned
    // when linked.
    _pin: core::marker::PhantomPinned,
}

/// Mutable cursor to move over the elements of a [`List`].
///
/// Mutable cursors mutably borrow a list and then allow moving over the list
/// and accessing the elements. Unlike immutable cursors, mutable cursors allow
/// linking new elements, and unlinking existing elements.
///
/// Single linked lists can only be iterated in one direction, so this cursor
/// behaves very similar to standard iterators.
pub struct CursorMut<'list, Ref, Frt>
where
    Ref: util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
    // Current position of the cursor. Always refers to the pointer to an
    // element, rather than the element itself, to allow removal of the
    // element.
    pos: NonNull<atomic::Atomic<usize>>,
    // Cursors borrow their list mutably, yet that borrow is never used,
    // so provide as phantom data.
    _list: core::marker::PhantomData<&'list mut List<Ref, Frt>>,
}

#[doc(hidden)]
#[macro_export]
macro_rules! util_slist_impl_pin_node {
    ($base:ty, $field:ident $(,)?) => {
        $crate::util::field::impl_pin_field!{$base, $field, $crate::util::slist::Node}
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! util_slist_node_of {
    ($base:ty, $field:ident $(,)?) => {
        $crate::util::field::typed_field_of!{$base, $field, $crate::util::slist::Node}
    }
}

/// Alias of [`impl_pin_field!()`](util::field::impl_pin_field) with a fixed
/// member field type of [`Node`].
#[doc(inline)]
pub use util_slist_impl_pin_node as impl_pin_node;

/// Alias of [`typed_field_of!()`](util::field::typed_field_of) with a fixed
/// member field type of [`Node`].
#[doc(inline)]
pub use util_slist_node_of as node_of;

// Marks the end of a list, to be able to distinguish unlinked nodes from tail
// nodes. Since the initial page is reserved, this cannot match real nodes.
pub(crate) const END: usize = core::mem::align_of::<Node>();

impl<Ref, Frt> List<Ref, Frt>
where
    Ref: util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
    pub(crate) const fn with(first: usize) -> Self {
        Self {
            first: atomic::Atomic::new(first),
            _ref: core::marker::PhantomData,
            _frt: core::marker::PhantomData,
        }
    }

    /// Create a new empty list.
    ///
    /// The new list has no entries linked and is completely independent of
    /// other lists.
    pub const fn new() -> Self {
        Self::with(END)
    }

    /// Check whether the list is empty.
    ///
    /// This returns `true` is no entries are linked, `false` if at least one
    /// entry is linked.
    ///
    /// Note that the list does not maintain a counter of how many elements are
    /// linked.
    pub fn is_empty(&self) -> bool {
        self.first.load(atomic::Relaxed) == END
    }

    /// Return a mutable cursor for this list, starting at the front.
    ///
    /// Create a new mutable cursor for the list, which initially points at the
    /// first element. The cursor mutably borrows the list for its entire
    /// lifetime.
    pub fn cursor_mut(&mut self) -> CursorMut<'_, Ref, Frt> {
        CursorMut {
            pos: util::nonnull_from_ref(&self.first),
            _list: core::marker::PhantomData,
        }
    }

    /// Link a node at the front of the list.
    ///
    /// On success, `Ok` is returned and the node is linked at the front of the
    /// list, with ownership transferred to the list.
    ///
    /// If the node is already on another list, this will return `Err` and
    /// return ownership of the entry to the caller.
    pub fn try_link_front(
        &mut self,
        ent: Pin<Ref>,
    ) -> Result<Pin<&Ref::Target>, Pin<Ref>> {
        self.cursor_mut().try_link_consume(ent)
    }

    /// Unlink the first element of the list.
    ///
    /// If the list is empty, this will return `None`. Otherwise, the first
    /// entry is removed from the list and ownership is transferred to the
    /// caller.
    pub fn unlink_front(&mut self) -> Option<Pin<Ref>> {
        self.cursor_mut().unlink()
    }

    /// Clear the list and move ownership of all entries into a closure.
    ///
    /// This will invoke `clear_fn` once for each entry in the list. The entry
    /// is removed and ownership is transferred into the closure.
    ///
    /// Entries are removed sequentially starting from the front of the list.
    pub fn clear_with<ClearFn>(
        &mut self,
        mut clear_fn: ClearFn,
    )
    where
        ClearFn: FnMut(Pin<Ref>),
    {
        while let Some(v) = self.unlink_front() {
            clear_fn(v);
        }
    }
}

// Convenience helpers
impl<Ref, Frt> List<Ref, Frt>
where
    Ref: util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
    /// Link a node at the front of the list.
    ///
    /// Works like [`List::try_link_front()`] but panics on error.
    pub fn link_front(&mut self, ent: Pin<Ref>) -> Pin<&Ref::Target> {
        self.try_link_front(ent).unwrap_or_else(|v| {
            core::panic!(
                "attempting to link a foreign node: {:?}",
                core::ptr::from_ref(&*v)
            );
        })
    }

    /// Clear the list and drop all elements.
    ///
    /// Works like [`List::clear_with()`] but uses `core::mem::drop` as
    /// closure.
    pub fn clear(&mut self) {
        self.clear_with(|_| {})
    }
}

// SAFETY: Lists have no interior mutability, nor do they otherwise care for
//     their calling CPU. They can be freely sent across CPUs, only limited by
//     the stored type.
unsafe impl<Ref, Frt> Send for List<Ref, Frt>
where
    Ref: Send + util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
}

// SAFETY: Lists have no interior mutability, nor do they otherwise care for
//     their calling CPU. They can be shared across CPUs, only limited by
//     the stored type.
unsafe impl<Ref, Frt> Sync for List<Ref, Frt>
where
    Ref: Sync + util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
}

impl<Ref, Frt> core::default::Default for List<Ref, Frt>
where
    Ref: util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
    /// Return a new empty list.
    fn default() -> Self {
        Self::new()
    }
}

impl<Ref, Frt> core::ops::Drop for List<Ref, Frt>
where
    Ref: util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
    /// Clear a list before dropping it.
    fn drop(&mut self) {
        self.clear();
    }
}

impl Node {
    /// Create a new unlinked node.
    ///
    /// The new node is marked as unlinked and not associated with any other
    /// node or list.
    ///
    /// Note that nodes must be pinned to be linked into a list. Therefore,
    /// the result must either be pinned in place or moved into a pinned
    /// structure to make proper use of it.
    pub const fn new() -> Self {
        Self {
            next: atomic::Atomic::new(0),
            _pin: core::marker::PhantomPinned,
        }
    }

    /// Check whether this node is linked into a list.
    ///
    /// This returns `true` if this node is linked into any list. It returns
    /// `false` if the node is currently unlinked.
    ///
    /// Note that a node can be linked into a list at any time. That is,
    /// validity of the returned boolean can change spuriously, unless the
    /// caller otherwise ensures exclusive access to the node.
    /// Furthermore, no memory barriers are guaranteed by this call, so data
    /// dependence must be considered separately.
    pub fn is_linked(&self) -> bool {
        self.next.load(atomic::Relaxed) != 0
    }
}

// SAFETY: Nodes are only ever modified through their owning list, or through
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
    /// Create a new unlinked node.
    fn default() -> Self {
        Self::new()
    }
}

impl core::ops::Drop for Node {
    /// Drop a node and verify it is unlinked.
    ///
    /// No special cleanup is required when dropping nodes. However, linked
    /// nodes are owned by their respective list. So if a linked node is
    /// dropped, someone screwed up and this will warn loudly.
    fn drop(&mut self) {
        kernel::warn_on!(self.is_linked());
    }
}

impl<'list, Ref, Frt> CursorMut<'list, Ref, Frt>
where
    Ref: util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
    fn as_pos(&self) -> &atomic::Atomic<usize> {
        // SAFETY: `Self.pos` always refers to a valid atomic on a valid node.
        unsafe { self.pos.as_ref() }
    }

    fn get_ptr(&self) -> Option<NonNull<Node>> {
        let pos = self.as_pos().load(atomic::Relaxed);
        if pos == END {
            None
        } else {
            // Recreate the pointer with the exposed provenance.
            let ptr = pos as *mut Node;
            // SAFETY: NULL is never stored in a list, but only used to denote
            //     unlinked nodes. Hence, `node` cannot be NULL.
            Some(unsafe { NonNull::new_unchecked(ptr) })
        }
    }

    fn link(
        &mut self,
        ent: Pin<Ref>,
    ) -> Result<NonNull<Ref::Target>, Pin<Ref>> {
        let pos = self.as_pos();
        let (ent_deref, ent_node) = Frt::acquire(ent);

        // Nothing is dependent on the value of `Node.next`, except for the
        // fact whether this operation succeeded, so perform a relaxed cmpxchg.
        // Any data dependence behind `Ref` is ordered on `self` (and `ent`).
        //
        // SAFETY: `ent_node` points to a valid immutable node as long as we
        //     hold `ent_deref`.
        if let Ok(_) = unsafe { ent_node.as_ref() }.next.cmpxchg(
            0,
            pos.load(atomic::Relaxed),
            atomic::Relaxed,
        ) {
            // Expose provenance, until `Atomic<*mut T>` is here.
            let ent_node_addr = ent_node.as_ptr() as usize;
            // All nodes are owned by the list, so no ordering needed. Atomics
            // are only used to prevent temporary releases of the nodes.
            pos.store(ent_node_addr, atomic::Relaxed);
            Ok(ent_deref)
        } else {
            // `ent` is already linked into another list, return ownership to
            // the caller wrapped in an `Err`.
            //
            // SAFETY: `ent_deref` was just acquired from `acquire()` and is
            //     no longer used afterwards.
            Err(unsafe { Frt::release(ent_deref) })
        }
    }

    /// Get a reference to the element the cursor points to.
    ///
    /// If the cursor points past the last element, `None` is returned.
    /// Otherwise, a reference to the element is returned.
    pub fn get(&self) -> Option<Pin<&Ref::Target>> {
        let ent_node = self.get_ptr()?;
        // SAFETY: `ent_node` was taken from the list, and a list ensures
        //     those were acquired via `Frt::acquire()` and thus valid
        //     until released. Since the cursor is immutably borrowed, the
        //     entry is valid for that lifetime.
        Some(unsafe { Pin::new_unchecked(Frt::from_node(ent_node).as_ref()) })
    }

    /// Get a mutable reference to the element the cursor points to.
    ///
    /// If the cursor points past the last element, `None` is returned.
    /// Otherwise, a mutable reference to the element is returned.
    pub fn get_mut(&mut self) -> Option<Pin<&mut Ref::Target>>
    where
        Ref: core::ops::DerefMut,
    {
        let ent_node = self.get_ptr()?;
        // SAFETY: `ent_node` was taken from the list, and a list ensures
        //     those were acquired via `Frt::acquire()` and thus valid
        //     until released. Since the cursor is immutably borrowed, the
        //     entry is valid for that lifetime.
        //     Since we own a `Ref`, we can mutably borrow the entire list to
        //     get a mutable reference to the reference target.
        Some(unsafe { Pin::new_unchecked(Frt::from_node(ent_node).as_mut()) })
    }

    /// Move the cursor to the next element.
    ///
    /// If the cursor already points past the last element, this is a no-op.
    /// Otherwise, the cursor is moved to the next element.
    pub fn move_next(&mut self) {
        if let Some(ent_node) = self.get_ptr() {
            // SAFETY: `ent_node` was taken from the list, and a list ensures
            //     those were acquired via `Frt::acquire()` and thus valid
            //     until released. Since cursors always move forward, the entry
            //     is valid until destruction of the cursor.
            self.pos = unsafe {
                NonNull::new_unchecked(
                    (&raw const (*ent_node.as_ptr()).next).cast_mut(),
                )
            };
        }
    }

    /// Link a node at the cursor position.
    ///
    /// On success, `Ok` is returned and the node is linked at the cursor
    /// position (i.e., the cursor points to the node), with ownership
    /// transferred to the list.
    ///
    /// If the node is already on another list, this will return `Err` and
    /// return ownership of the entry to the caller. The cursor and list remain
    /// unmodified.
    pub fn try_link(
        &mut self,
        ent: Pin<Ref>,
    ) -> Result<Pin<&Ref::Target>, Pin<Ref>> {
        self.link(ent).map(|v| {
            // SAFETY: The entry is convertible to a pinned shared reference
            //     for as long as we do not call `Ref::release()`. By holding
            //     `self`, we prevent `Cursor` from doing so.
            unsafe { Pin::new_unchecked(v.as_ref()) }
        })
    }

    /// Unlink the current element without moving the cursor.
    ///
    /// If the cursor points past the last element, this is a no-op and returns
    /// `None`. Otherwise, the element is unlinked and ownership
    /// transferred to the caller. The cursor now points to the following
    /// element.
    pub fn unlink(&mut self) -> Option<Pin<Ref>> {
        let ent_node = self.get_ptr()?;

        // Borrow `ent_node` as reference. Update the list position to skip
        // the node and then update the node to be marked as unlinked.
        {
            // SAFETY: `ent_node` was taken from the list, and a list ensures
            //     those were acquired via `Frt::acquire()` and thus valid
            //     until released. A temporary conversion to reference is thus
            //     safe.
            let ent_node_r = unsafe { ent_node.as_ref() };

            // Unlink the node from the list. The load could be non-atomic,
            // since the node is owned by the list. The store must be atomic,
            // to ensure no temporary releases of the node.
            // No data dependence, since everything is still list-owned and
            // ordered through `self._list`.
            self.as_pos().store(
                ent_node_r.next.load(atomic::Relaxed),
                atomic::Relaxed,
            );

            // Release the node. No ordering required, since any data
            // dependence is either ordered on `self` or up to the caller.
            ent_node_r.next.store(0, atomic::Relaxed);
        }

        // SAFETY: `ent_node` was taken from the list, and a list
        //     guarantees it was acquired via `Frt::acquire()` and thus
        //     embedded in a reference target and valid until release.
        let ent_deref = unsafe { Frt::from_node(ent_node) };
        // SAFETY: `ent_deref` was taken from the list, and thus guaranteed
        //     to be acquired via `Frt::acquire()`. Since the previous entry
        //     was updated to point to the next, the pointer is no longer
        //     stored in the list.
        Some(unsafe { Frt::release(ent_deref) })
    }
}

// Convenience helpers
impl<'list, Ref, Frt> CursorMut<'list, Ref, Frt>
where
    Ref: util::intrusive::Reference,
    Frt: util::intrusive::Field<Ref, Node = Node>,
{
    /// Consume the cursor and return the element it pointed to.
    ///
    /// Works like [`Self::get()`], but consumes the cursor and can thus
    /// return a borrow for `'list`.
    pub fn get_consume(self) -> Option<Pin<&'list Ref::Target>> {
        let ent_node = self.get_ptr()?;
        drop(self);
        // SAFETY: `ent_node` was taken from the list, and a list ensures
        //     those were acquired via `Frt::acquire()` and thus valid
        //     until released. Since the cursor was consumed, the entry is
        //     valid for `'list`.
        Some(unsafe { Pin::new_unchecked(Frt::from_node(ent_node).as_ref()) })
    }

    /// Get a reference to the element the cursor points to and move forward.
    ///
    /// Works like [`Self::get()`] but calls [`Self::move_next()`] afterwards,
    /// thus returning a longer borrow.
    pub fn get_and_move_next(&mut self) -> Option<Pin<&'list Ref::Target>> {
        let ent_node = self.get_ptr()?;
        self.move_next();
        // SAFETY: `ent_node` was taken from the list, and a list ensures
        //     those were acquired via `Frt::acquire()` and thus valid
        //     until released. Since the cursor was moved forward and can never
        //     move backwards, this node is valid for `'list`.
        Some(unsafe { Pin::new_unchecked(Frt::from_node(ent_node).as_ref()) })
    }

    /// Link a node at the cursor position, consuming the cursor.
    ///
    /// Works like [`Self::try_link()`] but consumes the cursor, thus returning
    /// a longer borrow.
    pub fn try_link_consume(
        mut self,
        ent: Pin<Ref>,
    ) -> Result<Pin<&'list Ref::Target>, Pin<Ref>> {
        match self.link(ent) {
            Ok(v) => {
                drop(self);
                // SAFETY: The entry is convertible to a pinned shared
                //     reference for as long as we do not call
                //     `Ref::release()`. By dropping `self`, no-one can modify
                //     the tree for as long as `'list`.
                Ok(unsafe { Pin::new_unchecked(v.as_ref()) })
            },
            Err(v) => Err(v),
        }
    }
}

#[kunit_tests(bus1_util_slist)]
mod test {
    use super::*;

    #[derive(Default)]
    struct Entry {
        key: u8,
        node: Node,
    }

    impl_pin_node!(Entry, node);

    // Create a list that stores shared references. This allows access to
    // the elements even if stored in the list. Once the list is dropped,
    // mutable access to the elements is possible again.
    #[test]
    fn shared_refs() {
        let mut e0 = core::pin::pin!(Entry { key: 0, ..Default::default() });
        let mut e1 = core::pin::pin!(Entry { key: 1, ..Default::default() });

        let mut list: List<&Entry, node_of!(Entry, node)> = List::new();

        assert!(list.is_empty());
        assert!(!e0.node.is_linked());
        assert!(!e1.node.is_linked());

        list.link_front(e0.as_ref());
        list.link_front(e1.as_ref());

        assert!(!list.is_empty());
        assert!(e0.node.is_linked());
        assert!(e1.node.is_linked());

        assert!(list.try_link_front(e0.as_ref()).is_err());
        assert!(list.try_link_front(e1.as_ref()).is_err());

        let mut c = list.cursor_mut();
        assert_eq!(c.get().unwrap().key, 1);
        assert_eq!(c.get_and_move_next().unwrap().key, 1);
        assert_eq!(c.get().unwrap().key, 0);
        assert_eq!(c.get_and_move_next().unwrap().key, 0);
        assert!(c.get().is_none());

        assert_eq!(list.unlink_front().unwrap().key, 1);
        assert_eq!(list.unlink_front().unwrap().key, 0);

        assert!(list.unlink_front().is_none());
        assert!(list.is_empty());

        drop(list);
        assert!(!e0.as_mut().node.is_linked());
        assert!(!e1.as_mut().node.is_linked());
    }

    // Create a `List` that stores mutable references. This prevents any use of
    // the entries while linked. But once the list is dropped, they can be used
    // again.
    #[test]
    fn mutable_refs() {
        let mut e0 = core::pin::pin!(Entry { key: 0, ..Default::default() });
        let mut e1 = core::pin::pin!(Entry { key: 1, ..Default::default() });

        let mut list: List<&mut Entry, node_of!(Entry, node)> = List::new();

        assert!(list.is_empty());
        assert!(!e0.node.is_linked());
        assert!(!e1.node.is_linked());

        list.link_front(e0.as_mut());
        list.link_front(e1.as_mut());

        assert!(!list.is_empty());

        let mut c = list.cursor_mut();
        assert_eq!(c.get_mut().unwrap().key, 1);
        c.move_next();
        assert_eq!(c.get_mut().unwrap().key, 0);
        c.move_next();
        assert!(c.get().is_none());

        let v = list.unlink_front().unwrap();
        assert_eq!(v.key, 1);
        let v = list.unlink_front().unwrap();
        assert_eq!(v.key, 0);

        assert!(list.unlink_front().is_none());
        assert!(list.is_empty());

        drop(list);
        assert!(!e0.node.is_linked());
        assert!(!e1.node.is_linked());
    }
}
