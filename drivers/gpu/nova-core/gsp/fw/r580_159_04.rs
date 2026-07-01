// SPDX-License-Identifier: GPL-2.0

//! Firmware bindings.
//!
//! Imports the generated bindings by `bindgen`.
//!
//! This module may not be directly used. Please abstract or re-export the needed symbols in the
//! parent module instead.

#![allow(
    dead_code,
    clippy::all,
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::ref_as_ptr,
    missing_docs,
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    improper_ctypes,
    unreachable_pub,
    unsafe_op_in_unsafe_fn
)]
use kernel::ffi;
use pin_init::MaybeZeroable;

include!("r580_159_04/bindings.rs");

// SAFETY: This type has a size of zero, so its inclusion into another type should not affect their
// ability to implement `Zeroable`.
unsafe impl<T> kernel::prelude::Zeroable for __IncompleteArrayField<T> {}

// Renamed upstream from `GSP_FW_HEAP_PARAM_SIZE_PER_GB_FB`.
pub use self::GSP_FW_HEAP_PARAM_SIZE_PER_GB as GSP_FW_HEAP_PARAM_SIZE_PER_GB_FB;
