// SPDX-License-Identifier: GPL-2.0
//! # Utility library
//!
//! This module provides utilities that can be used independently of the core
//! module.

use kernel::prelude::*;
use kernel::sync::{Arc, ArcBorrow};

pub mod convert;
pub mod field;

/// Convert an Arc to its pinned version.
///
/// All [`Arc`] instances are unconditionally pinned. It is always safe to
/// convert from their unpinned variant to their pinned variant.
///
/// Most kernel APIs just use a plain [`Arc`], even if they rely on pinning.
/// If another API needs a `Pin<T>`, this converter can provide it for [`Arc`],
/// even though most other kernel APIs do not use `Pin<Arc<T>>`.
pub fn arc_pin<T>(v: Arc<T>) -> Pin<Arc<T>> {
    // SAFETY: `Arc<T>` guarantees its target is pinned.
    unsafe { Pin::new_unchecked(v) }
}

/// Convert an Arc to its unpinned version.
///
/// All [`Arc`] instances are unconditionally pinned. It is always safe to
/// convert from their pinned variant to their unpinned variant.
///
/// Most kernel APIs just use a plain [`Arc`], even if they rely on pinning.
/// This converter allows getting an [`Arc`] if some other API returned a
/// generic `Pin<T>` with `T = Arc<U>`.
pub fn arc_unpin<T>(v: Pin<Arc<T>>) -> Arc<T> {
    // SAFETY: `Arc<T>` guarantees its target is pinned, even if not wrapped.
    unsafe { Pin::into_inner_unchecked(v) }
}

/// Convert an ArcBorrow to its pinned version.
///
/// All [`Arc`] instances are unconditionally pinned. It is always safe to
/// convert from their unpinned variant to their pinned variant.
pub fn arc_borrow_pin<T>(v: ArcBorrow<'_, T>) -> Pin<ArcBorrow<'_, T>> {
    // SAFETY: `Arc<T>` guarantees its target is pinned.
    unsafe { Pin::new_unchecked(v) }
}

/// Create a [`NonNull`] from a reference.
///
/// This is a backport of [`core::ptr::NonNull::from_ref()`].
pub fn nonnull_from_ref<T: ?Sized>(v: &T) -> core::ptr::NonNull<T> {
    // SAFETY: A reference cannot be NULL.
    unsafe { core::ptr::NonNull::new_unchecked(core::ptr::from_ref(v).cast_mut()) }
}

/// Create a [`NonNull`] from a reference.
///
/// This is a backport of [`core::ptr::NonNull::from_mut()`].
pub fn nonnull_from_mut<T: ?Sized>(v: &mut T) -> core::ptr::NonNull<T> {
    // SAFETY: A reference cannot be NULL.
    unsafe { core::ptr::NonNull::new_unchecked(v) }
}

/// Return the memory address part of a pointer without exposing provenance.
///
/// This returns the same value as an `as usize` cast. However, this function
/// is meant to not expose provenance, and rather behave like
/// `<*mut T>::addr()`. Unfortunately, the latter requires an MSRV of 1.84,
/// which is not yet available upstream. Until then, this serves as a
/// replacement.
pub fn ptr_addr<T: ?Sized>(v: *const T) -> usize {
    // Simply expose the provenance until. A transmute would avoid the
    // exposition, but is not a stable API.
    v.cast::<()>() as usize
}

/// Compare two pointers.
///
/// This is equivalent to `<*const T as Ord>::cmp()`. Unlike the trait-based
/// solution, this has fixed pointer types and thus can be called with
/// references, which are then coerced to pointers.
///
/// This serves the same purpose as `core::ptr::eq()`, but for `Ord` rather
/// than `Eq`.
pub fn ptr_cmp<T: ?Sized>(a: *const T, b: *const T) -> core::cmp::Ordering {
    // Even though `PartialOrd for *mut T` documents that it uses
    // `<*mut T>::addr()` for comparisons, clippy still warns about it. Cast
    // to `()` to ensure metadata is ignored.
    a.cast::<()>().cmp(&b.cast::<()>())
}
