//! A full-featured, production-ready implementation of the sACN (Streaming ACN
//! / ANSI E1.31) protocol for sending and receiving DMX over IP networks.
//!
//! # Getting started
//!
//! Typically, you should start with an adapter like [`tokio`] which suits your
//! application. These are the highest-level abstractions over sACN concepts.
//! In the case of [`tokio`], receive merged DMX with [`tokio::Receiver`],
//! receive per-source data with [`tokio::BasicReceiver`], transmit with
//! [`tokio::Source`], and discover the sources present on a network with
//! [`tokio::SourceDetector`]. The [`tokio`] module has runnable examples for
//! each, and the `examples/` directory in the repository holds complete
//! terminal programs.
//!
//! Other adapters exist for different application paradigms, and for
//! advanced usage and/or `no_std` targets, the I/O-free protocol cores can
//! be used directly (see [Internal architecture](#internal-architecture)) or
//! an adapter over them can be hand-written for your runtime.
//!
//! # Feature flags
//!
//! | Feature   | Default        | Description                                                                                                          |
//! | --------- | -------------- | -------------------------------------------------------------------------------------------------------------------- |
//! | `std`     | yes            | Standard-library support; enables `alloc`.                                                                            |
//! | `alloc`   | via `std`      | Heap-backed storage (the default [`HeapStorage`]). Without it every type uses fixed-capacity storage built with [`static_storage!`]. |
//! | `tokio`   | yes            | The [`tokio`] runtime adapter. Implies `std`.                                                                        |
//! | `embassy` | no             | A `no_std` runtime adapter for the [Embassy](https://embassy.dev/) framework.                                        |
//! | `tracing` | no             | Structured logging via the `tracing` crate.                                                                          |
//! | `log`     | no             | Logging via the `log` crate facade.                                                                                  |
//! | `defmt`   | no             | Logging via `defmt`, for embedded targets.                                                                           |
//! | `uuid`    | no             | `From`/`Into` conversions between [`Cid`] and `uuid::Uuid`.                                                          |
//!
//! # Internal architecture
//!
//! The crate is organised in three layers:
//!
//! - **Core.** A pure protocol state machine with no `async`, no socket and
//!   no clock. It is `#![no_std]` and is a function of `(packets, time) ->
//!   events`, dealing only in universes and opaque interface tokens. The
//!   [`error`], [`time`] and [`proto`] modules and various foundational
//!   protocol types live here.
//! - **Adapters.** Runtime drivers that own the real socket and clock and
//!   translate the core's universe-level requests into real I/O (interface
//!   resolution, multicast-group derivation, joins/leaves and sends).
//! - **Public API.** Thin re-exports of the adapter APIs; the core is also
//!   exposed for advanced/embedded users.
#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]
#![deny(unreachable_pub)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod error;
pub mod packet;
pub mod proto;
pub mod time;

pub mod detector;
pub mod merger;
pub mod receiver;
pub mod source;
pub mod storage;

mod log;
mod types;

// Re-exported so the `static_storage!` macro can name `heapless::Vec` through
// `$crate` without the user's crate depending on `heapless` directly.
#[doc(hidden)]
pub use heapless;

pub use error::{Error, Result};
pub use packet::Packet;
pub use proto::{ipv4_multicast, ipv6_multicast};
pub use storage::{HeapStorage, MapLike, SortedVecMap, VecLike};
pub use types::{Cid, NetintId, Priority, SequenceNumber, SourceName, StartCode, Universe};

#[cfg(feature = "alloc")]
pub use detector::SourceDetectorEvent;
pub use detector::{DetectorStorage, SourceDetector, SourceDetectorConfig};

pub use merger::{DmxMerger, MergeOutput, MergerStorage, SlotOwner, SourceId};

pub use receiver::{
    BasicReceiver, BasicReceiverStorage, Receiver, ReceiverConfig, ReceiverStorage,
};
#[cfg(feature = "alloc")]
pub use receiver::{BasicReceiverEvent, MergedData, MergedSource, ReceiverEvent};

pub use source::{
    OnSyncLoss, Route, Source, SourceConfig, SourceStorage, Transmission, UniverseConfig,
};

mod static_storage;

#[cfg(any(feature = "std", feature = "embassy"))]
mod adapters;

#[cfg(feature = "std")]
pub use adapters::{AdapterError, MulticastInterface, ToMulticastInterfaces};

#[cfg(feature = "tokio")]
pub use adapters::tokio;

#[cfg(feature = "embassy")]
pub use adapters::embassy;
