//! The tokio runtime adapter.
//!
//! Async wrappers turn the core state machines into ergonomic APIs driven by the
//! tokio runtime:
//!
//! - [`BasicReceiver`] wraps [`crate::receiver::BasicReceiver`] and delivers
//!   per-source data.
//! - [`Receiver`] wraps the merging [`crate::receiver::Receiver`] and delivers
//!   merged universe data.
//! - [`Source`] wraps [`crate::source::Source`] and transmits sACN.
//! - [`SourceDetector`] wraps [`crate::detector::SourceDetector`] and reports the
//!   sources present on the network and the universes each one transmits.
//!
//! Each owns a `tokio::net::UdpSocket` and the tokio clock, runs an async loop
//! over the socket and the core's timers, and performs the real multicast joins
//! and leaves for the groups the core is interested in.
//!
//! The `examples/` directory in the repository holds complete terminal programs
//! built on these types (a source console, basic and merging receivers, and a
//! source detector).
//!
//! ```no_run
//! use sacn::tokio::Receiver;
//! use sacn::{ReceiverConfig, ReceiverEvent, Universe};
//!
//! # async fn demo() -> Result<(), sacn::AdapterError> {
//! let mut rx = Receiver::bind(ReceiverConfig::new()).await?;
//! rx.listen(Universe::new(1).unwrap()).await?;
//! while let Some(event) = rx.next_event().await {
//!     match event {
//!         ReceiverEvent::MergedData(data) => { /* use data.levels() */ }
//!         ReceiverEvent::SourcesLost { .. } => {}
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```

mod basic;
mod detector;
mod merging;
mod source;

use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::adapters::{AdapterError, MulticastInterface};
use crate::log::debug;
use crate::proto::{ipv4_multicast, ipv6_multicast};
use crate::types::Universe;

pub use basic::BasicReceiver;
pub use detector::SourceDetector;
pub use merging::Receiver;
pub use source::Source;

/// Builds an [`AdapterError::Io`] tagged with the operation that failed.
fn io_error(operation: &'static str) -> impl FnOnce(io::Error) -> AdapterError {
    move |source| AdapterError::Io { operation, source }
}

/// Enumerates the system's usable multicast interfaces.
///
/// Each up, non-loopback interface contributes its first IPv4 address. The
/// loopback interface is skipped because it does not carry multicast on every
/// platform (notably Linux, where `lo` lacks the multicast flag). IPv6 is not
/// enumerated yet.
fn system_multicast_interfaces() -> Vec<MulticastInterface> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut interfaces = Vec::new();
    // `if-addrs` returns one entry per address, so an interface with several
    // IPv4 addresses appears multiple times; dedup by name keeps us to the first
    // IPv4 address of each interface.
    let mut seen = HashSet::new();
    for iface in ifaces {
        if iface.is_loopback() || !iface.is_oper_up() {
            continue;
        }
        if let if_addrs::IfAddr::V4(v4) = iface.addr
            && seen.insert(iface.name)
        {
            interfaces.push(MulticastInterface::V4(v4.ip));
        }
    }
    interfaces
}

/// How a multi-interface listen reacts to a failed join.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinPolicy {
    /// Try every interface; succeed as long as at least one join works. Used by
    /// automatic interface selection, where some enumerated interfaces may not
    /// be usable.
    Continue,
    /// Fail and roll back at the first error. Used when the caller named the
    /// interfaces explicitly and expects all of them to work.
    Rollback,
}

/// Creates a sACN receive socket bound to `addr` with address/port reuse, so
/// multiple sACN receivers can coexist on the same host.
fn bind_socket(addr: SocketAddr) -> Result<UdpSocket, AdapterError> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(io_error("creating socket"))?;
    socket
        .set_reuse_address(true)
        .map_err(io_error("setting SO_REUSEADDR"))?;
    // SO_REUSEPORT only exists on some platforms and is not essential, so a
    // failure (or its absence) is not fatal.
    #[cfg(unix)]
    let _ = socket.set_reuse_port(true);
    socket
        .set_nonblocking(true)
        .map_err(io_error("setting non-blocking mode"))?;
    socket
        .bind(&addr.into())
        .map_err(io_error("binding socket"))?;
    UdpSocket::from_std(socket.into()).map_err(io_error("registering socket"))
}

/// Joins the multicast group for the universe number `group` on `interface`.
///
/// Takes a raw `u16` rather than a [`Universe`] so it can also join the reserved
/// discovery universe, which lies outside the valid data-universe range.
fn join(socket: &UdpSocket, group: u16, interface: MulticastInterface) -> Result<(), AdapterError> {
    let result = match interface {
        MulticastInterface::V4(addr) => socket.join_multicast_v4(ipv4_multicast(group), addr),
        MulticastInterface::V6(index) => socket.join_multicast_v6(&ipv6_multicast(group), index),
    };
    result.map_err(io_error("joining multicast group"))
}

/// Leaves the multicast group for the universe number `group` on `interface`.
///
/// Takes a raw `u16` rather than a [`Universe`] so it can also leave the reserved
/// discovery universe, which lies outside the valid data-universe range.
fn leave(
    socket: &UdpSocket,
    group: u16,
    interface: MulticastInterface,
) -> Result<(), AdapterError> {
    let result = match interface {
        MulticastInterface::V4(addr) => socket.leave_multicast_v4(ipv4_multicast(group), addr),
        MulticastInterface::V6(index) => socket.leave_multicast_v6(&ipv6_multicast(group), index),
    };
    result.map_err(io_error("leaving multicast group"))
}

/// Switches a universe's joined multicast interfaces from `old` to `new`,
/// applying the rollback `policy` on a failed join.
///
/// The previous set is left first (a re-listen replaces it), then the new set is
/// joined. On success the interfaces actually joined are returned (at least one,
/// per `policy`), so the caller can record exactly what must be left later. On
/// failure the interfaces this call joined are left again before the error is
/// returned; the caller is responsible for clearing the core's state.
fn execute_listen(
    socket: &UdpSocket,
    universe: Universe,
    old: &[MulticastInterface],
    new: &[MulticastInterface],
    policy: JoinPolicy,
) -> Result<Vec<MulticastInterface>, AdapterError> {
    // Leave the previous interface set first (a re-listen replaces it).
    for &interface in old {
        let _ = leave(socket, universe.get(), interface);
    }

    // Join the new set, stopping early under the rollback policy.
    let mut joined = Vec::new();
    let mut first_error = None;
    for &interface in new {
        match join(socket, universe.get(), interface) {
            Ok(()) => joined.push(interface),
            Err(error) => match policy {
                JoinPolicy::Rollback => {
                    first_error = Some(error);
                    break;
                }
                JoinPolicy::Continue => {
                    debug!("failed to join a multicast interface; continuing");
                }
            },
        }
    }

    if first_error.is_some() || joined.is_empty() {
        for interface in joined {
            let _ = leave(socket, universe.get(), interface);
        }
        return Err(first_error.unwrap_or(AdapterError::NoNetwork));
    }

    Ok(joined)
}
