// SPDX-License-Identifier: GPL-2.0
//! # Intrusive Lockless Linked Lists
//!
//! This module implements an intrusive lockless linked list. It is similar
//! to `linux/llist.h`, but implemented in pure Rust.
//!
//! This follows the intrusive design described in
//! [`intrusive`](crate::util::intrusive). However, it only offers a very
//! limited API surface. For a general purpose single linked list list API,
//! use [`util::slist`].
//!
//! The core entrypoint is [`List`], which maintains a single pointer to the
//! last entry in a single linked list. Elements are stored by their [`Node`]
//! metadata field, which again is just a single pointer to the respective
//! previous element in a list.
//!
//! More generally, [`List`] can be seen as a multi-producer/multi-consumer
//! channel, similar to (but very much reduced in scope)
//! `std::sync::mpsc` in the Rust standard library.

use kernel::prelude::*;
use kernel::sync::atomic;

use crate::util;

/// Intrusive lockless single linked list to store elements.
///
/// A [`List`] effectively provides two operations:
/// 1) Push a new element to the front of the list.
/// 2) Remove all elements from the list and return them.
///
/// Both operations can be performed without any locks but only via hardware
/// atomic operations.
///
/// This list is mainly used for multi-producer / single-or-multi-consumer
/// (mpsc/mpmc) channels. That is, it serves as handover of items from
/// producers to a consumer / consumers. The list does not provide any
/// iterators, cursors, or other utilities to modify or inspect a list. If
/// those are needed, proper locked lists are the better option.
///
/// Elements stored in a [`List`] are owned by that list, but are not moved,
/// nor allocated by the list. Instead, the list takes ownership of a pointer
/// to the element (either via smart pointers, or via references that have a
/// lifetime that exceeds the lifetime of the list). Once an element is
/// removed, ownership is transferred back to the caller.
///
/// [`List`] uses the same nodes as [`util::slist::Node`], and thus can move
/// nodes from one to another.
pub struct List<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    // Pointer to the first node in the list. Set to `END` if the list is
    // empty, `NULL` if it was sealed and can no longer be pused to. All other
    // pointers always represent a pinned owned reference to an entry, gained
    // via `Ref::pin_into_deref()`.
    first: atomic::Atomic<usize>,
    // Different lists can store entries of type `Ref` via different nodes. By
    // pinning the field-representing-type, it is always clear which node a
    // list is using.
    _lrt: core::marker::PhantomData<Lrt>,
}

/// Metadata required for elements of a [`List`].
///
/// This is an alias for [`util::slist::Node`]. That is, this uses the same
/// node type as the general purpose single-linked list provided by
/// [`util::slist`].
pub type Node = util::slist::Node;

#[doc(hidden)]
#[macro_export]
macro_rules! util_lll_node_of {
    ($ref:ty, $field:ident $(,)?) => {
        $crate::util::intrusive::link_of!{$ref, $field, $crate::util::lll::Node}
    }
}

/// Alias of [`link_of!()`](util::intrusive::link_of) for [`Node`] members.
#[doc(inline)]
pub use util_lll_node_of as node_of;

// Marks a sealed list. This is different than `slist::END` in that no more
// entries can be pushed to a sealed list. Otherwise, it is treated like an
// empty list.
pub(crate) const SEAL: usize = 0;

impl<Lrt> List<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Create a new empty list.
    ///
    /// The new list has no entries linked and is completely independent of
    /// other lists.
    pub const fn new() -> Self {
        Self {
            first: atomic::Atomic::new(util::slist::END),
            _lrt: core::marker::PhantomData,
        }
    }

    /// Check whether the list is empty.
    ///
    /// This returns `true` is no entries are linked, `false` if at least one
    /// entry is linked.
    ///
    /// Note that the list does not maintain a counter of how many elements are
    /// linked.
    pub fn is_empty(&self) -> bool {
        match self.first.load(atomic::Relaxed) {
            SEAL | util::slist::END => true,
            _ => false,
        }
    }

    /// Check whether the list is sealed.
    ///
    /// This returns `true` if the list is sealed. A sealed list is always
    /// empty and cannot be modified, anymore (nor can the seal be removed).
    pub fn is_sealed(&self) -> bool {
        self.first.load(atomic::Relaxed) == SEAL
    }

    /// Link a node at the front of a list.
    ///
    /// On success, `Ok` is returned and the node is linked at the front of the
    /// list, with ownership transferred to the list.
    ///
    /// If the node is already on another list, this will return `Err` and
    /// return ownership of the entry to the caller.
    ///
    /// On success, this ensures a release memory barrier before linking it
    /// into the list, matching the acquire memory barrier in
    /// [`List::clear()`].
    pub fn try_link_front(
        &self,
        ent: Pin<Lrt::Ref>,
    ) -> Result<(), Pin<Lrt::Ref>> {
        let mut first = self.first.load(atomic::Relaxed);
        if first == SEAL {
            // Sealed lists cannot be linked to.
            return Err(ent);
        }

        let ent_node = Lrt::acquire(ent);
        // SAFETY: `ent_node` is convertible to a shared reference as long as
        //     we do not call `Lrt::release()`.
        let ent_node_r = unsafe { ent_node.as_ref() };

        let Ok(_) = ent_node_r.next.cmpxchg(0, first, atomic::Relaxed) else {
            // `ent_node_r` becomes invalid once `end_deref` is released.
            #[expect(dropping_references)]
            drop(ent_node_r);
            // `ent` is already linked into another list, return ownership to
            // the caller wrapped in an `Err`.
            //
            // SAFETY: `ent_node` was just acquired from `pin_into_deref()`
            //     and is no longer used afterwards.
            return Err(unsafe { Lrt::release(ent_node) });
        };

        // Expose provenance, until `Atomic<*mut T>` is here.
        let ent_node_addr = ent_node.as_ptr() as usize;

        // Try updating the list-front until it succeeds.
        loop {
            // Use release barrier, since we want all operations on the node
            // to be ordered before the node is pushed to the list. The
            // matching acquire barrier is in `Self::clear()`.
            match self.first.cmpxchg(
                first,
                ent_node_addr,
                atomic::Release,
            ) {
                Ok(_) => break Ok(()),
                Err(v) => {
                    // If the list is sealed, no more entries can be linked.
                    if v == SEAL {
                        // SAFETY: `ent_node` was just acquired from
                        // `pin_into_deref()` and is no longer used afterwards.
                        break Err(unsafe { Lrt::release(ent_node) });
                    }
                    first = v;
                    ent_node_r.next.store(first, atomic::Relaxed);
                },
            }
        }
    }

    /// Clear the entire list and return the entries to the caller.
    ///
    /// This will atomically remove all entries from the list, and return those
    /// entries as a general purpose single linked list to the caller.
    ///
    /// Note that [`List`] only supports adding entries at the front. Hence,
    /// the returned list will be in LIFO (last-in-first-out) order.
    ///
    /// This ensures an acquire memory barrier matching the release memory
    /// barrier in [`List::try_link_front()`].
    pub fn clear(&self) -> util::slist::List<Lrt> {
        let mut first = self.first.load(atomic::Relaxed);
        loop {
            if first == SEAL {
                break util::slist::List::new();
            }
            // Use acquire barrier to ensure writes to the nodes are
            // visible, if done before they were linked. The matching
            // release barrier is in `Self::try_link_front()`.
            match self.first.cmpxchg(
                first,
                util::slist::END,
                atomic::Acquire,
            ) {
                Ok(v) => {
                    // SAFETY: By clearing `self.first` we acquire the list.
                    // Since it uses the same nodes as `slist`, we can create
                    // one from it.
                    break unsafe { util::slist::List::with(v) };
                },
                Err(v) => first = v,
            }
        }
    }

    /// Seal the entire list and return all entries to the caller.
    ///
    /// This will atomically remove all entries from the list and seal it, so
    /// any new attempt to link more entries will fail.
    ///
    /// A sealed list will remain sealed and cannot be unsealed. This also
    /// implies that the list will remain empty.
    ///
    /// If the list is already sealed, this is a no-op and will return an empty
    /// list.
    pub fn seal(&self) -> util::slist::List<Lrt> {
        let v = self.first.xchg(SEAL, atomic::Acquire);
        if v == SEAL {
            util::slist::List::new()
        } else {
            // SAFETY: By clearing `self.first` we acquire the list. Since it
            // uses the same nodes as `slist`, we can create one from it.
            unsafe { util::slist::List::with(v) }
        }
    }
}

// Convenience helpers
impl<Lrt> List<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Link a node at the front of a list.
    ///
    /// Works like [`List::try_link_front()`] but warns on error and leaks the
    /// entry.
    pub fn link_front(&self, ent: Pin<Lrt::Ref>) {
        self.try_link_front(ent).unwrap_or_else(|v| {
            // Warn if the entry is already used elsewhere, and then leak the
            // reference to avoid cascading failures.
            kernel::warn_on!(true);
            core::mem::forget(v);
        })
    }
}

// SAFETY: `List` can be sent along CPUs, as long as the data it contains can
//     also be sent along. `List` never cares about the CPU it is called on.
unsafe impl<Lrt> Send for List<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
    Lrt::Ref: Send,
{
}

// SAFETY: `List` is meant to be shared across CPUs and safely handles parallel
//     accesses through atomics. It never hands out references to stored
//     elements, so it is `Sync` as long as the data it sends along is `Send`.
unsafe impl<Lrt> Sync for List<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
    Lrt::Ref: Send,
{
}

impl<Lrt> core::default::Default for List<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Return a new empty list.
    fn default() -> Self {
        Self::new()
    }
}

impl<Lrt> core::ops::Drop for List<Lrt>
where
    Lrt: util::intrusive::Link<Node>,
{
    /// Clear a list before dropping it.
    ///
    /// This drops all elements in the list via [`List::clear()`], before
    /// dropping the list. This ensures that the elements of a list are
    /// not leaked.
    fn drop(&mut self) {
        self.clear();
    }
}

#[kunit_tests(bus1_util_lll)]
mod test {
    use super::*;

    #[derive(Default)]
    struct Entry {
        key: u8,
        node: Node,
    }

    util::field::impl_pin_field!(Entry, node, Node);

    #[test]
    fn test_basic() {
        let e0 = core::pin::pin!(Entry { key: 0, ..Default::default() });
        let e1 = core::pin::pin!(Entry { key: 1, ..Default::default() });

        let list: List<node_of!(&Entry, node)> = List::new();

        assert!(list.is_empty());
        assert!(!list.is_sealed());
        assert!(!e0.node.is_linked());
        assert!(!e1.node.is_linked());

        list.link_front(e0.as_ref());
        list.link_front(e1.as_ref());

        assert!(!list.is_empty());
        assert!(!list.is_sealed());
        assert!(e0.node.is_linked());
        assert!(e1.node.is_linked());

        assert!(list.try_link_front(e0.as_ref()).is_err());
        assert!(list.try_link_front(e1.as_ref()).is_err());

        let mut r = list.clear();
        assert_eq!(r.unlink_front().unwrap().key, 1);
        assert_eq!(r.unlink_front().unwrap().key, 0);
        assert!(r.unlink_front().is_none());

        assert!(list.is_empty());
        assert!(!list.is_sealed());
        assert!(!e0.node.is_linked());
        assert!(!e1.node.is_linked());

        list.link_front(e0.as_ref());
        assert!(!list.is_empty());
        assert!(!list.is_sealed());
        assert!(e0.node.is_linked());
        assert!(!e1.node.is_linked());

        assert!(!list.is_sealed());
        let mut r = list.seal();
        assert!(list.is_empty());
        assert!(list.is_sealed());
        assert!(e0.node.is_linked());
        assert!(!e1.node.is_linked());
        assert!(list.try_link_front(e0.as_ref()).is_err());
        assert!(list.try_link_front(e1.as_ref()).is_err());

        assert_eq!(r.unlink_front().unwrap().key, 0);
        assert!(r.unlink_front().is_none());

        assert!(list.is_empty());
        assert!(list.is_sealed());
        assert!(!e0.node.is_linked());
        assert!(!e1.node.is_linked());
    }
}
