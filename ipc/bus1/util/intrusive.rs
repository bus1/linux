//! # Utilities for Intrusive Data Structures
//!
//! Intrusive data structures store metadata of elements they manage in the
//! element itself, rather than using support data structures. This requires
//! users to embed the respective metadata types in their types, and annotate
//! the data structures with sufficient information about this embedding.
//!
//! Intrusive data structures encode connections between elements and
//! containing collections in the type system. Furthermore, they can often
//! reduce or fully eliminate dynamic allocations, as well as reduce the
//! number of pointer chases necessary to traverse a collection.
//! On the flip side, intrusive data structures often reduce cache locality and
//! can thus be more expensive to traverse.
//!
//! In general, performance and allocation pressure highly depend on the
//! implemented algorithm, rather than on the fact whether a data structure is
//! intrusive. But if dynamic collections free of allocations are needed,
//! intrusive data structures are usually the only option.
//!
//! The biggest advantage of intrusive data structures, though, is their use
//! of the type system to encode the possible relationships of different
//! data types. An instance of a given data type can only be linked into a
//! statically known number of intrusive collections at a time. The fact that
//! metadata is directly embedded in a type can be used to deduce how a type
//! can be hooked up into other collections. Furthermore, such relationships
//! can be queried at runtime in constant time.
//!
//! ## Examples
//!
//! The following pseudo code shows how intrusive collections behave, based on
//! a fictional intrusive collection called `col`. Most collections that use
//! the utilities of this module behave in a very similar manner.
//!
//! ```rust,ignore
//! // Import the fictional collection `col`.
//! use some::intrusive::collection::col;
//!
//! // Data type that will be stored in the collection. The payload describes
//! // the user controlled data that can be put into the entry. `md` is the
//! // mandatory metadata used to store it as a node in the collection.
//! struct Entry {
//!     payload: u8,
//!     md: col::Node,
//! }
//!
//! // Encode that `Entry` has a node as member field called `md`. The unstable
//! // field-projections feature of Rust would make this obsolete.
//! col::impl_pin_node!(Entry, md);
//!
//! // Create 3 entries, and then pin them on the stack. As an alternative, the
//! // entries could also be dynamically allocated via `Box`, `Arc`, etc..
//! let e0_o = Entry { payload: 0, ..Default::default() };
//! let e1_o = Entry { payload: 1, ..Default::default() };
//! let e2_o = Entry { payload: 2, ..Default::default() };
//! let e0 = core::pin::pin!(e0_o);
//! let e1 = core::pin::pin!(e1_o);
//! let e2 = core::pin::pin!(e2_o);
//!
//! // Create a fictional collection called `Map`, which stores elements of
//! // type `Entry` using the `md` member field.
//! let map = col::Map::<&Entry, col::node_of!(Entry, md)>::new();
//!
//! // Nodes can be queried for their state at any time.
//! assert!(!e0.md.is_linked());
//! // Collections can often be queried for their relationship to a node. This
//! // can usually be performed in O(1), but depends on the implementation.
//! assert!(!map.contains(&e0));
//!
//! // Collections take ownership of a reference to an element, rather than
//! // moving the element into the collection. In this case, a shared reference
//! // is used. Dynamic allocations would move a `Box`, `Arc`, etc. into the
//! // collection.
//! map.push(&e0);
//!
//! // Since the used reference type implements `Clone`, the caller retains a
//! // reference and can use it to query the collection for it.
//! assert!(e0.md.is_linked());
//! assert!(map.contains(&e0));
//!
//! // Many elements can be pushed into a collection, but a single element
//! // cannot be in multiple collections that use the same member field node.
//! // Assuming `push()` panics on failure, we use `try_push()` to verify that
//! // `e0` was already pushed before.
//! map.push(&e1);
//! map.push(&e2);
//! assert!(map.try_push(&e0).is_err());
//!
//! // Collections usually provide cursors that allow traversals that can
//! // optionally modify the collection. In this case it is used to drop all
//! // elements with an even payload number.
//! let mut cursor = map.first_mut();
//! while let Some(v) = cursor.get() {
//!     if (v.payload % 2) == 0 {
//!         cursor.move_next_unlink();
//!     } else {
//!         cursor.move_next();
//!     }
//! }
//! assert!(!map.contains(&e0));
//! assert!(map.contains(&e1));
//! assert!(!map.contains(&e2));
//!
//! // Since collections own references to their elements, they can be dropped
//! // and will automatically drop all references to contained elements.
//! // Here, this means the elements are no longer linked anywhere and we can
//! // get mutable access to them again (verified by using `Pin::set()`).
//! drop(map);
//! e0.set(Default::default());
//! e1.set(Default::default());
//! e2.set(Default::default());
//! ```

use core::ptr::NonNull;
use kernel::prelude::*;
use crate::util::{self, field};

/// Trait alias for reference types in an intrusive data structure.
///
/// This trait is used as trait-alias for the combination of
/// [`IntoDeref`](util::convert::IntoDeref) and
/// [`FromDeref`](util::convert::FromDeref). Note that both those
/// traits imply [`Deref`](core::ops::Deref) and [`Sized`].
///
/// This trait is auto-implemented for all qualifying types.
pub trait Reference
where
    Self: util::convert::IntoDeref,
    Self: util::convert::FromDeref,
{
}

/// Trait alias for field-representing-types in an intrusive data structure.
///
/// This trait is used as trait-alias for the combination of a reference
/// type (see [`Reference`]) and a fixed field-representing-type
/// [`PinField`](field::PinField).
///
/// This trait is auto-implemented for all qualifying types.
pub trait Field<Ref>
where
    Self: field::PinField<Base = Ref::Target, Type = Self::Node>,
    Ref: Reference,
{
    /// Type of metadata that is stored in nodes of this intrusive
    /// data structure.
    type Node;

    /// Convert from reference target pointer to node pointer.
    ///
    /// ## Safety
    ///
    /// The reference target pointer must refer to an allocation of its
    /// type, but does not need to be initialized.
    unsafe fn to_node(p: NonNull<Ref::Target>) -> NonNull<Self::Node> {
        // SAFETY: Caller guarantees that `p` points to an allocation
        //     of `Ref::Target`, and thus a field access cannot return NULL.
        unsafe { NonNull::new_unchecked(field::field_of_ptr::<Self>(p.as_ptr())) }
    }

    /// Convert from node pointer to reference target pointer.
    ///
    /// ## Safety
    ///
    /// The node pointer must refer to an allocation of its type embedded
    /// in a reference target object. The allocation does not need to be
    /// initialized.
    unsafe fn from_node(n: NonNull<Self::Node>) -> NonNull<Ref::Target> {
        // SAFETY: Caller guarantees that `p` points to an allocation
        //     of `Self::Node` within a `Ref::Target`. Hence, `base_of_ptr()`
        //     is safe to call and cannot return `NULL`.
        unsafe { NonNull::new_unchecked(field::base_of_ptr::<Self>(n.as_ptr())) }
    }

    /// Acquire a reference target pointer from a reference.
    ///
    /// This will turn the reference into a reference target pointer. It is up
    /// to the caller to ensure calling [`Self::release()`] when releasing the
    /// pointer. Otherwise, the original reference will be leaked.
    ///
    /// This returns [`Self::to_node()`] alongside the reference target
    /// pointer.
    fn acquire(v: Pin<Ref>) -> (NonNull<Ref::Target>, NonNull<Self::Node>) {
        let deref = Ref::pin_into_deref(v);
        (
            deref,
            // SAFETY: `deref` was just acquired from `pin_into_deref()`, which
            //     guarantees that the result is convertible to a reference.
            unsafe { Self::to_node(deref) },
        )
    }

    /// Release a reference target pointer to get back the original reference.
    ///
    /// # Safety
    ///
    /// The reference target pointer must have been acquired via
    /// [`Self::acquire()`], and the caller must cease further use of the
    /// pointer.
    unsafe fn release(v: NonNull<Ref::Target>) -> Pin<Ref> {
        // SAFETY: Caller guarantees that `v` was from `acquire()`, and thus
        //     from `pin_into_deref()`. They also guarantee to cease using `v`.
        unsafe { Ref::pin_from_deref(v) }
    }
}

// Ideally, the nested macros would support trailing commas. However,
// meta-variable expansion is still unstable. This can be tracked upstream in
// issue #83527 (https://github.com/rust-lang/rust/issues/83527).
#[doc(hidden)]
#[macro_export]
macro_rules! util_intrusive_node_macros {
    ($node:ty, $impl_pin_node:ident, $node_of:ident $(,)?) => {
        #[doc(hidden)]
        #[macro_export]
        macro_rules! $impl_pin_node {
            // This should be:
            // ($base:ty, $field:ident $$(,)?) => {
            ($base:ty, $field:ident) => {
                $crate::util::field::impl_pin_field!{
                    $base,
                    $field,
                    $node,
                }
            }
        }

        #[doc(hidden)]
        #[macro_export]
        macro_rules! $node_of {
            // This should be:
            // ($base:ty, $field:ident $$(,)?) => {
            ($base:ty, $field:ident) => {
                $crate::util::field::typed_field_of!(
                    $base,
                    $field,
                    $node,
                )
            }
        }

        /// Implement `PinField` for a structurally pinned member node.
        ///
        /// This works like `util::field::impl_pin_field!()` but assumes the
        /// type of the field to be `$node`.
        ///
        /// ## Safety
        ///
        /// The safety requirements of `impl_pin_field!()` apply.
        #[doc(inline)]
        pub use $impl_pin_node as impl_pin_node;

        /// Resolve to the `FieldRepr` of a specific member node.
        ///
        /// This works like `util::field::typed_field_of!()` but assumes the
        /// type of the field to be `$node`.
        #[doc(inline)]
        pub use $node_of as node_of;
    }
}

/// Define macro aliases for node access.
///
/// This macro takes the following arguments:
/// - $node:ty
/// - $impl_pin_node:ident
/// - $node_of:ident
///
/// This macro will define two macros:
/// 1) A macro called `$impl_pin_node` which is an alias for
///    [`impl_pin_field!`](field::impl_pin_field) but with
///    the member field type fixed to `$node`.
/// 2) A macro called `$node_of` which is an alias for
///    [`typed_field_of!`](field::typed_field_of)` but with the
///    member field type fixed to `$node`.
///
/// This macro will also create the following aliases with documentation
/// attached:
/// 1) `pub use $impl_pin_node` as impl_pin_node;`
/// 2) `pub use $node_of` as node_of;`
#[doc(inline)]
pub use util_intrusive_node_macros as node_macros;

// Blanket impl of the `Reference` alias.
impl<Ref> Reference for Ref
where
    Ref: util::convert::IntoDeref,
    Ref: util::convert::FromDeref,
{
}

// Blanket impl of the `Field<Ref>` alias.
impl<Frt, Ref> Field<Ref> for Frt
where
    Frt: field::PinField<Base = Ref::Target>,
    Ref: Reference,
{
    type Node = Frt::Type;
}
