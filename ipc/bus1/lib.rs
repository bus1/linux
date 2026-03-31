// SPDX-License-Identifier: GPL-2.0
//! # Kernel Bus1 Crate
//!
//! This is the in-kernel implementation of the Bus1 communication system in
//! rust. Any user-space API is outside the scope of this module.

#[allow(
    dead_code,
    missing_docs,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
)]
pub mod capi {
    include!(env!("BUS1_CAPI_PATH"));
}

const __LOG_PREFIX: &[u8] = b"bus1\0";
