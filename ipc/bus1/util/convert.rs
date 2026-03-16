//! # Utilities for conversions between types
//!
//! This module contains utilities to help dealing with conversions between
//! types.

use core::ptr::NonNull;
use kernel::prelude::*;
use crate::util;

/// Convert a value into a raw pointer to its dereferenced value.
///
/// This trait extends [`core::ops::Deref`] and allows converting a value
/// into a raw pointer to its dereferenced value. While
/// [`core::ops::Deref`] retains the original value and merely borrows to
/// dereference it, [`IntoDeref`] actually converts the value into a raw
/// pointer to the dereferenced value without retaining the original.
///
/// Note that in many cases this will leak the original value if no extra
/// steps are taken. Usually, you want to restore the original value to ensure
/// the correct drop handlers are run (see [`FromDeref`]).
///
/// This trait is a generic version of
/// [`Box::into_raw()`](kernel::alloc::Box::into_raw),
/// [`Arc::into_raw()`](kernel::sync::Arc::into_raw), and more.
///
/// ## Mutability
///
/// [`IntoDeref`] serves for immutable and mutable conversions. The returned
/// pointer does not reflect whether mutable access is granted. That is, the
/// trait can be used for owned values like [`Box`](kernel::alloc::Box)
/// where mutable access is granted, and shared values like
/// [`Arc`](kernel::sync::Arc) where no mutable access is granted.
///
/// ## Safety
///
/// The implementations of [`Deref`](core::ops::Deref) and [`IntoDeref`] must
/// be compatible. That is, for a given instance, both must agree on what
/// object a deref resolves to.
///
/// Furthermore, for types that provide [pinned](core::pin) variants,
/// [`IntoDeref`] is part of the safety requirements of
/// [`core::pin::Pin::new_unchecked()`] just like
/// [`Deref`](core::ops::Deref) is.
///
/// An implementation must guarantee the safety invariants of the individual
/// method implementations.
pub unsafe trait IntoDeref: Sized + core::ops::Deref {
    /// Convert a value into a raw pointer to its dereferenced value.
    ///
    /// This consumes a dereferencable value and yields a raw pointer
    /// to the dereferenced value.
    ///
    /// The returned pointer is guaranteed to be convertible to a shared
    /// reference for any caller-chosen lifetime `'a` where `Self: 'a`.
    fn into_deref(v: Self) -> NonNull<Self::Target>;

    /// Convert a pinned value into a raw pointer to its dereferenced value.
    ///
    /// This is the pinned equivalent of [`Self::into_deref()`].
    fn pin_into_deref(v: Pin<Self>) -> NonNull<Self::Target> {
        // SAFETY: Pinned types must ensure they uphold pinning guarantees
        //     just like `Deref` does (see trait requirements).
        Self::into_deref(unsafe { Pin::into_inner_unchecked(v) })
    }
}

/// Convert a dereferenced value back to its original value.
///
/// This trait provides the inverse operation of [`IntoDeref`]. It takes a
/// raw pointer to a dereferenced value and restores the original pointer.
/// This operation is unsafe and requires the caller to guarantee that the
/// pointer was acquired via [`IntoDeref`] or similar means.
///
/// This trait is a generic version of
/// [`Box::from_raw()`](kernel::alloc::Box::from_raw),
/// [`Arc::from_raw()`](kernel::sync::Arc::from_raw), and more.
///
/// ## Safety
///
/// The implementations of [`Deref`](core::ops::Deref) and [`FromDeref`] must
/// be compatible. That is, for a given instance, both must agree on what
/// object a deref resolves to.
///
/// Furthermore, for types that provide [pinned](core::pin) variants,
/// [`FromDeref`] is part of the safety requirements of
/// [`core::pin::Pin::new_unchecked()`] just like
/// [`Deref`](core::ops::Deref) is.
///
/// An implementation must guarantee the safety invariants of the individual
/// method implementations.
pub unsafe trait FromDeref: Sized + core::ops::Deref {
    /// Convert a dereferenced value back to its original value.
    ///
    /// ## Safety
    ///
    /// The wrapped pointer must have been acquired via [`IntoDeref`] or a
    /// matching equivalent (i.e., the wrapped pointer must be a valid pointer
    /// for the smart pointer [`Self`]). This implies that it must be valid
    /// for a suitable lifetime for [`Self`].
    ///
    /// If `Self` requires exclusive access to the wrapped pointer, the caller
    /// must guarantee that they do not make use of any retained copies of the
    /// wrapped pointer.
    ///
    /// It is always safe to call this on values obtained via [`IntoDeref`], as
    /// long as the raw pointer is no longer used afterwards.
    unsafe fn from_deref(v: NonNull<Self::Target>) -> Self;

    /// Convert a dereferenced value back to its original pinned value.
    ///
    /// This is the pinned equivalent of [`Self::from_deref()`].
    ///
    /// ## Safety
    ///
    /// The caller must guarantee that the original value was a pinned pointer.
    /// Furthermore, all requirements of [`Self::from_deref()`] apply.
    unsafe fn pin_from_deref(v: NonNull<Self::Target>) -> Pin<Self> {
        // SAFETY: Pinned types must ensure they uphold pinning guarantees
        //     just like `Deref` does (see trait requirements). Also, the
        //     caller must ensure the original value was pinned.
        unsafe { Pin::new_unchecked(Self::from_deref(v)) }
    }
}

mod impls {
    use super::*;
    use kernel::alloc::{Allocator, Box};
    use kernel::sync::Arc;

    // SAFETY: Coherent with `Deref` and pinning.
    unsafe impl<T: ?Sized> IntoDeref for &T {
        fn into_deref(v: Self) -> NonNull<Self::Target> {
            util::nonnull_from_ref(v)
        }
    }

    // SAFETY: Coherent with `Deref` and pinning.
    unsafe impl<T: ?Sized> FromDeref for &T {
        unsafe fn from_deref(v: NonNull<Self::Target>) -> Self {
            // SAFETY: Delegated to caller.
            unsafe { v.as_ref() }
        }
    }

    // SAFETY: Coherent with `Deref` and pinning.
    unsafe impl<T: ?Sized> IntoDeref for &mut T {
        fn into_deref(v: Self) -> NonNull<Self::Target> {
            util::nonnull_from_mut(v)
        }
    }

    // SAFETY: Coherent with `Deref` and pinning.
    unsafe impl<T: ?Sized> FromDeref for &mut T {
        unsafe fn from_deref(mut v: NonNull<Self::Target>) -> Self {
            // SAFETY: Delegated to caller.
            unsafe { v.as_mut() }
        }
    }

    // SAFETY: Coherent with `Deref` and pinning.
    unsafe impl<T: ?Sized, A: Allocator> IntoDeref for Box<T, A> {
        fn into_deref(v: Self) -> NonNull<Self::Target> {
            // SAFETY: `Box::into_raw()` never returns NULL.
            unsafe { NonNull::new_unchecked(Box::into_raw(v)) }
        }
    }

    // SAFETY: Coherent with `Deref` and pinning.
    unsafe impl<T: ?Sized, A: Allocator> FromDeref for Box<T, A> {
        unsafe fn from_deref(v: NonNull<Self::Target>) -> Self {
            // SAFETY: Delegated to caller.
            unsafe { Box::from_raw(v.as_ptr()) }
        }
    }

    // SAFETY: Coherent with `Deref` and pinning.
    unsafe impl<T: ?Sized> IntoDeref for Arc<T> {
        fn into_deref(v: Self) -> NonNull<Self::Target> {
            // SAFETY: `Arc::into_raw()` never returns NULL.
            unsafe { NonNull::new_unchecked(Arc::into_raw(v).cast_mut()) }
        }
    }

    // SAFETY: Coherent with `Deref` and pinning.
    unsafe impl<T: ?Sized> FromDeref for Arc<T> {
        unsafe fn from_deref(v: NonNull<Self::Target>) -> Self {
            // SAFETY: Delegated to caller.
            unsafe { Arc::from_raw(v.as_ptr()) }
        }
    }
}

#[allow(clippy::undocumented_unsafe_blocks)]
#[kunit_tests(bus1_util_convert)]
mod test {
    use super::*;
    use kernel::alloc::KBox;
    use kernel::sync::Arc;

    #[test]
    fn into_from_deref() {
        let mut v: u64 = 71;

        {
            let p: *const u64 = &raw const v;
            let f: &u64 = &v;

            let d: NonNull<u64> = IntoDeref::into_deref(f);
            assert_eq!(71, unsafe { *d.as_ref() });
            assert!(core::ptr::eq(p, d.as_ptr()));

            let r: &u64 = unsafe { FromDeref::from_deref(d) };
            assert_eq!(71, *r);
            assert!(core::ptr::eq(p, r));
        }

        {
            let p: *mut u64 = &raw mut v;
            let f: &mut u64 = &mut v;

            let d: NonNull<u64> = IntoDeref::into_deref(f);
            assert_eq!(71, unsafe { *d.as_ref() });
            assert!(core::ptr::eq(p, d.as_ptr()));

            let r: &mut u64 = unsafe { FromDeref::from_deref(d) };
            assert_eq!(71, *r);
            assert!(core::ptr::eq(p, r));
        }

        {
            let f: KBox<u64> = KBox::new(v, GFP_KERNEL).unwrap();
            let p: *const u64 = &raw const *f;

            let d: NonNull<u64> = IntoDeref::into_deref(f);
            assert_eq!(71, unsafe { *d.as_ref() });
            assert!(core::ptr::eq(p, d.as_ptr()));

            let r: KBox<u64> = unsafe { FromDeref::from_deref(d) };
            assert_eq!(71, *r);
            assert!(core::ptr::eq(p, &raw const *r));
        }

        {
            let f: Arc<u64> = Arc::new(v, GFP_KERNEL).unwrap();
            let p: *const u64 = &raw const *f;

            let d: NonNull<u64> = IntoDeref::into_deref(f);
            assert_eq!(71, unsafe { *d.as_ref() });
            assert!(core::ptr::eq(p, d.as_ptr()));

            let r: Arc<u64> = unsafe { FromDeref::from_deref(d) };
            assert_eq!(71, *r);
            assert!(core::ptr::eq(p, &raw const *r));
        }
    }
}
