//! Host-agnostic typed generational resource SDK.
//!
//! This module is the public surface host crates use to allocate, borrow, and
//! close VM resources without reaching into VM private state. It is generic
//! over the concrete resource type: the concrete class is validated at borrow
//! time with [`std::any::TypeId`] and never enumerated by the core.
//!
//! # Ownership model
//!
//! - [`ResourceTable`] is the single owner of every live resource in one
//!   execution scope. A table is `Send + !Sync` and is moved under the sole
//!   mutating owner.
//! - A [`Resource<T>`] is a cheap, `Copy` capability token keyed by a
//!   [`ResourceHandle`]. Duplicating the token does not duplicate ownership of
//!   the underlying resource.
//! - Host functions borrow a resource for the duration of one call through
//!   [`ResourceTable::get`] / [`ResourceTable::get_mut`], returning
//!   [`ResourceRef`] / [`ResourceMut`], which must not outlive the call.
//! - Close is poll-based: [`HostResource::begin_close`] issues the synchronous
//!   cancel/close request, then [`ResourceTable::poll_close`] drives a single
//!   resource to completion and [`ResourceTable::poll_close_all`] drives the
//!   whole table to quiescence (child first) using the caller's waker. Stale
//!   handles and slot reuse after close are rejected by the generation in the
//!   handle.

pub mod close;
pub mod error;
pub mod handle;
pub mod reason;
pub mod table;

pub use self::close::{CloseProgress, HostResource};
pub use self::error::{ResourceError, ResourceErrorCode, ResourceResult};
pub use self::handle::{Resource, ResourceHandle, ResourceMut, ResourceOwned, ResourceRef};
pub use self::reason::ResourceCloseReason;
pub use crate::host_api::ResourceTypeKey;
pub use table::{
    GuestReleaseOutcome, OwnershipRelease, ResourceAccessFrame, ResourceAccessMode,
    ResourceAccessRequest, ResourceOwnership, ResourceTable,
};
