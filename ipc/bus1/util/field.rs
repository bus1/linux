//! # Field Projections
//!
//! This module allows generalizing over the fields of a structure. At its
//! core is the [`Field`] trait, allowing limited type reflection.
//!
//! This trait is just enough to get intrusive collections working. For
//! generalized versions of this, see
//! [field projections](https://github.com/rust-lang/rust/issues/145383).

use kernel::prelude::*;

/// Authoritative information about a field of another type.
///
/// This trait asserts that [`Self::Base`] has a member field of type
/// [`Self::Type`] at byte offset [`Self::OFFSET`]. This information is
/// authoritative. As such, implementing this trait on *any* type must be
/// subject to this condition.
///
/// All subtypes of an implementation always carry the same trait
/// implementation. That is, an implementing type cannot be coerced into
/// another type with a deviating implementation.
///
/// Commonly, this trait is automatically implemented on Field Representing
/// Types (FRTs) by the compiler, or manually via [`impl_field`].
///
/// # Unsized Types
///
/// All involved types currently must be `Sized`. In particular, the
/// implementing type `Self`, `Field::Base`, and `Field::Type` must be `Sized`.
///
/// The trait could allow unsized types, but the helpers that convert from base
/// to field pointer cannot calculate pointer metadata without external
/// input. Unless there is a solid design to pass around metadata, this is left
/// for a future extension.
///
/// ## Safety
///
/// Implementing this type is only safe, if for a given valid value of type
/// [`Self::Base`] there exists a valid value of type [`Self::Type`] at byte
/// offset `OFFSET`, and this value is represented by a direct member field
/// on [`Self::Base`].
///
/// Any subtypes of the implementing type must have an equal trait
/// implementation. Type coercion must never lead to a deviating trait
/// implementation.
pub unsafe trait Field: Send + Sync + Copy {
    /// Base containing type this field exists in.
    type Base;

    /// Type of the field.
    type Type;

    /// Offset of this field in bytes relative to the start of the base type.
    const OFFSET: usize;
}

/// Authoritative information about a structurally pinned field.
///
/// This trait is an extension of [`Field`] and guarantees that the field is
/// [structurally pinned](https://rust.docs.kernel.org/core/pin/index.html#projections-and-structural-pinning).
///
/// ## Safety
///
/// The implementation must guarantee that the field is structurally pinned.
pub unsafe trait PinField: Field {
}

/// Reflection metadata about a field of a base type.
///
/// This type is used as implementing type for generated [`Field`]
/// implementations. It is a 1-ZST and used only to represent reflection
/// metadata about a field of a type.
///
/// See [`impl_field`] for its main user.
///
/// This type is invariant over all its type parameters.
///
/// ## Limitations
///
/// If multiple zero-sized member fields share the same offset, only a single
/// one can be represented with this type. The compiler generated alternative
/// in the standard library can circumvent this limitation. Without compiler
/// support, auto-generation of such types requires other external enumerations
/// that make usage needlessly complex. Hence, this uses the field offset
/// as distinguisher, and thus limits the implementation.
#[repr(C, packed)]
pub struct FieldRepr<Base: ?Sized, Type: ?Sized, const OFFSET: usize> {
    _base: [*mut Base; 0],
    _type: [*mut Type; 0],
    _offset: [(); OFFSET],
}

// SAFETY: `FieldRepr` doesn't contain any values. No subtypes exist.
unsafe impl<Base: ?Sized, Type: ?Sized, const OFFSET: usize> Send
for FieldRepr<Base, Type, OFFSET> {
}

// SAFETY: `FieldRepr` doesn't contain any values. No subtypes exist.
unsafe impl<Base: ?Sized, Type: ?Sized, const OFFSET: usize> Sync
for FieldRepr<Base, Type, OFFSET> {
}

impl<Base: ?Sized, Type: ?Sized, const OFFSET: usize> Clone
for FieldRepr<Base, Type, OFFSET> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Base: ?Sized, Type: ?Sized, const OFFSET: usize> Copy
for FieldRepr<Base, Type, OFFSET> {
}

/// Turn a base pointer into a member field pointer.
///
/// This is equivalent to taking a raw pointer to a member field
/// `&raw mut (*v).field`. Note that the base is not dereferenced for this
/// operation.
///
/// ## Safety
///
/// The pointer `v` must point to an allocation of `BaseTy`, but that value can
/// be uninitialized.
pub unsafe fn field_of_ptr<Frt: Field>(v: *mut Frt::Base) -> *mut Frt::Type {
    // SAFETY: Validity of the allocation behind `v` is delegated to the
    //     caller. The offset calculation is guaranteed by the `Field` trait.
    unsafe { v.byte_offset(Frt::OFFSET as isize).cast() }
}

/// Turn a field pointer into a base pointer.
///
/// This is the inverse of [`field_of_ptr()`]. It recreates the base pointer
/// from the member field pointer.
///
/// ## Miri Stacked & Tree Borrows
///
/// If you require compatibility with Stacked Borrows as used in Miri, you must
/// ensure that the field pointer was created from a reference to the base,
/// rather than from a reference to the field. In other words, make sure that
/// you use [`field_of_ptr()`] and then retain that raw field pointer until you
/// need it for [`base_of_ptr()`]. Otherwise, your code will likely not be
/// compatible with Stacked Borrows.
///
/// If you only require compatibility with Tree Borrows, this is not an issue.
///
/// ## Safety
///
/// The pointer `v` must point into an allocation of `BaseTy` at the offset of
/// the member field described by `Field`, but the value can be uninitialized.
pub unsafe fn base_of_ptr<Frt: Field>(v: *mut Frt::Type) -> *mut Frt::Base {
    // SAFETY: Validity of the allocation behind `v` is delegated to the
    //     caller. The offset calculation is guaranteed by the `Field` trait.
    unsafe { v.byte_offset(-(Frt::OFFSET as isize)).cast() }
}

#[doc(hidden)]
#[macro_export]
macro_rules! util_field_frt {
    ($base:ty, $field:ident, $type:ty $(,)?) => {
        $crate::util::field::FieldRepr<
            $base,
            $type,
            { ::core::mem::offset_of!($base, $field) },
        >
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! util_field_impl_field {
    ($base:ty, $field:ident, $type:ty $(,)?) => {
        // SAFETY: `FieldRepr` exposes no variance. `$field` is verified to be
        //     a member of `$base` via `offset_of!()`, and correctness of its
        //     type is verified apart from coercions (which we accept).
        unsafe impl $crate::util::field::Field
        for $crate::util::field::FieldRepr<
            $base,
            $type,
            { ::core::mem::offset_of!($base, $field) },
        > {
            type Base = $base;
            type Type = $type;
            const OFFSET: usize = const {
                // Verify the type of the member field.
                let mut v = ::core::mem::MaybeUninit::<Self::Base>::uninit();
                let v_ptr = core::ptr::from_mut(&mut v).cast::<Self::Base>();
                // SAFETY: `v` is a valid allocation, a field access is safe.
                let _: *mut Self::Type = unsafe {
                    &raw mut ((*v_ptr).$field)
                };
                ::core::mem::offset_of!(Self::Base, $field)
            };
        }
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! util_field_impl_pin_field {
    ($base:ty, $field:ident, $type:ty $(,)?) => {
        $crate::util::field::impl_field!($base, $field, $type);
        // SAFETY: Structural pinning of `$field` is guaranteed by the caller.
        unsafe impl $crate::util::field::PinField
        for $crate::util::field::FieldRepr<
            $base,
            $type,
            { ::core::mem::offset_of!($base, $field) },
        > {
        }
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! util_field_field_of {
    ($base:ty, $field:ident $(,)?) => {
        $crate::util::field::frt!{$base, $field, _}
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! util_field_typed_field_of {
    ($base:ty, $field:ident, $type:ty $(,)?) => {
        $crate::util::field::frt!{$base, $field, $type}
    }
}

/// Resolve to the field-representing-type (FRT).
///
/// This takes as arguments:
/// - $base:ty
/// - $field:ident
/// - $type:ty
///
/// This resolves to `FieldRepr<$base, $type, ...>` with the last generic
/// parameter set to the offset of `$field` in `$base`.
#[doc(inline)]
pub use util_field_frt as frt;

/// Implement [`Field`] for a specific member field.
///
/// This takes as arguments:
/// - $base:ty
/// - $field:ident
/// - $type:ty
///
/// This implements [`Field`] on [`FieldRepr`] with the given base type, member
/// field name, and member field type.
///
/// ## Safety
///
/// The caller must guarantee that `$type` matches the type of the member 
/// field `$field`. This is verified by this macro, except for possible
/// coercions.
#[doc(inline)]
pub use util_field_impl_field as impl_field;

/// Implement [`PinField`] for a structurally pinned member field.
///
/// This works like [`impl_field!`] but implements [`PinField`] on top.
/// of [`Field`].
///
/// ## Safety
///
/// The safety requirements of [`impl_field!`] apply. On top, the caller
/// must guarantee the field in question is structurally pinned.
#[doc(inline)]
pub use util_field_impl_pin_field as impl_pin_field;

/// Resolve to the [`FieldRepr`] of a specific member field.
///
/// This takes as arguments:
/// - $base:ty
/// - $field:ident
///
/// This resolves to a specific type of [`FieldRepr`] for the specified member
/// field. This lets the compiler auto-derive the type of the field. In
/// situations where an auto-derive is not allowed (e.g., function signatures)
/// use [`typed_field_of!`].
#[doc(inline)]
pub use util_field_field_of as field_of;

/// Resolve to the typed [`FieldRepr`] of a specific member field.
///
/// This takes as arguments:
/// - $base:ty
/// - $field:ident
/// - $type:ty
///
/// This resolves to a specific type of [`FieldRepr`] for the specified member
/// field.
#[doc(inline)]
pub use util_field_typed_field_of as typed_field_of;

#[allow(clippy::undocumented_unsafe_blocks)]
#[kunit_tests(bus1_util_field)]
mod test {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq)]
    #[repr(C, align(4))]
    struct Test {
        a: u16,
        b: u8,
        c: u32,
    }

    impl_field!(Test, a, u16);
    impl_field!(Test, b, u8);
    impl_pin_field!(Test, c, u32);

    // Basic functionality tests for `Field` and its utilities.
    #[test]
    fn field_basics() {
        assert_eq!(core::mem::size_of::<Test>(), 8);

        let mut o = Test { a: 14, b: 11, c: 1444 };
        let o_p = &raw mut o;

        let f_p = unsafe { field_of_ptr::<field_of!(Test, b)>(o_p) };
        let f_r = unsafe { &*f_p };
        let b_p = unsafe { base_of_ptr::<field_of!(Test, b)>(f_p) };
        let b_r = unsafe { &*b_p };

        assert!(core::ptr::eq(o_p, b_p));
        assert_eq!(*f_r, 11);
        assert_eq!(b_r.b, 11);
    }
}
