// SPDX-License-Identifier: GPL-2.0
//! # Utility library
//!
//! This module provides utilities that can be used independently of the core
//! module.

use kernel::prelude::*;
use kernel::sync::Arc;

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
