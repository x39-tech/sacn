//! The tokio source adapter.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use socket2::SockRef;
use tokio::net::UdpSocket;
use tokio::time::Instant as TokioInstant;

use crate::adapters::net::ToMulticastInterfaces;
use crate::adapters::sending::{Routes, SendTarget};
use crate::adapters::{AdapterError, MulticastInterface};
use crate::proto::SACN_PORT;
use crate::source::{OnSyncLoss, Source as Core, SourceConfig, UniverseConfig};
use crate::time::Instant;
use crate::types::{Priority, SourceName, StartCode, Universe};

use super::{bind_socket, io_error, ipv4_multicast, ipv6_multicast, system_multicast_interfaces};

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;

/// The multicast TTL sources transmit with.
const MULTICAST_TTL: u32 = 64;

/// An asynchronous sACN source driven by the tokio runtime.
///
/// Construct one with [`bind`](Self::bind), register universes with
/// [`add_universe`](Self::add_universe) / [`add_universe_on`](Self::add_universe_on),
/// set their data with [`update_levels`](Self::update_levels) (and the other
/// `update_*` / `set_*` methods), and drive transmission by calling
/// [`process`](Self::process) or [`run`](Self::run) in a loop.
///
/// This wraps the [`crate::source::Source`] core, which documents the transmit
/// behavior (suppression and keep-alives, termination, discovery and
/// synchronization).
///
/// A source must keep transmitting on its own schedule even when the
/// application is idle (for keep-alive packets). [`process`](Self::process)
/// sends every packet that is currently due and returns the instant the next
/// one falls due; the application waits until then (interleaving any data
/// updates) before calling it again:
///
/// ```no_run
/// use sacn::tokio::Source;
/// use sacn::{Cid, SourceConfig, UniverseConfig, Universe};
///
/// # async fn demo() -> Result<(), sacn::AdapterError> {
/// let mut source = Source::bind(SourceConfig::new(Cid::from_bytes([1; 16]), "demo")).await?;
/// let universe = Universe::new(1).unwrap();
/// source.add_universe(UniverseConfig::new(universe))?;
/// source.update_levels(universe, &[255, 128, 0]);
///
/// loop {
///     let deadline = source.process().await?;
///     match deadline {
///         Some(at) => tokio::time::sleep_until(at).await,
///         None => break,
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Source {
    socket: UdpSocket,
    core: Core,
    epoch: TokioInstant,
    routes: Routes,
    fanout: Option<Fanout>,
}

/// A packet's in-progress delivery to its destinations. Used to implement
/// cancel-safety for sending operations.
#[derive(Debug)]
struct Fanout {
    /// The ordered concrete destinations, resolved once when the fan-out begins.
    targets: Vec<SendTarget>,
    /// The index of the next destination still to be sent to. Advanced only
    /// after a send completes.
    next: usize,
}

impl Source {
    /// Binds a source, ready to transmit, on an ephemeral local port.
    ///
    /// No universes are transmitted until one is added with
    /// [`add_universe`](Self::add_universe) and given data with
    /// [`update_levels`](Self::update_levels).
    ///
    /// # Errors
    ///
    /// Returns an [`AdapterError::Io`] if the socket cannot be created or bound.
    pub async fn bind(config: SourceConfig) -> Result<Self, AdapterError> {
        let socket = bind_socket(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
        // Give multicast a useful default TTL; best-effort.
        let _ = SockRef::from(&socket).set_multicast_ttl_v4(MULTICAST_TTL);
        Ok(Self {
            socket,
            core: Core::new(config),
            epoch: TokioInstant::now(),
            routes: Routes::new(),
            fanout: None,
        })
    }

    /// The configuration this source was created with.
    pub fn config(&self) -> &SourceConfig {
        self.core.config()
    }

    /// Adds a universe to transmit on, using all usable system interfaces for
    /// multicast.
    ///
    /// To transmit on an explicit interface set (or unicast-only, with none),
    /// use [`add_universe_on`](Self::add_universe_on). Returns `true` if the
    /// universe was added, or `false` if it was already present (left
    /// unchanged). The universe transmits nothing until its levels are set
    /// with [`update_levels`](Self::update_levels).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::NoNetwork`] if no usable interface is found, or
    /// [`AdapterError::Protocol`] if add failed on the core state machine.
    pub fn add_universe(&mut self, config: UniverseConfig) -> Result<bool, AdapterError> {
        let interfaces = system_multicast_interfaces();
        if interfaces.is_empty() {
            return Err(AdapterError::NoNetwork);
        }
        self.add_universe_with(config, interfaces)
    }

    /// Adds a universe to transmit on, using an explicit set of interfaces for
    /// multicast. An empty set is permitted for a unicast-only universe; add its
    /// destinations with [`add_unicast`](Self::add_unicast).
    ///
    /// Returns `true` if the universe was added, or `false` if it was already
    /// present.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Io`] if an interface cannot be resolved, or
    /// [`AdapterError::Protocol`] if add failed on the core state machine.
    pub fn add_universe_on(
        &mut self,
        config: UniverseConfig,
        interfaces: impl ToMulticastInterfaces,
    ) -> Result<bool, AdapterError> {
        let interfaces: Vec<_> = interfaces
            .to_multicast_interfaces()
            .map_err(io_error("resolving interfaces"))?
            .collect();
        self.add_universe_with(config, interfaces)
    }

    /// Adds a universe to the core and records its multicast interfaces, unless
    /// it was already present (in which case the core leaves it unchanged and the
    /// destination table is not touched).
    fn add_universe_with(
        &mut self,
        config: UniverseConfig,
        interfaces: Vec<MulticastInterface>,
    ) -> Result<bool, AdapterError> {
        let universe = config.universe();
        let sync_universe = config.sync_universe();
        match self.core.add_universe(config) {
            Ok(added) => {
                if added {
                    self.routes
                        .add_universe(universe, interfaces, sync_universe);
                }
                Ok(added)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Begins terminating a universe (the E1.31 three-packet stream-terminated
    /// sequence), after which it is dropped. Returns `false` if the universe was
    /// not present. Keep driving [`run`](Self::run) / [`process`](Self::process)
    /// until the universe is gone to actually send the termination packets.
    pub fn remove_universe(&mut self, universe: Universe) -> bool {
        self.core.remove_universe(universe)
    }

    /// Whether the source is currently transmitting (or terminating) `universe`.
    pub fn has_universe(&self, universe: Universe) -> bool {
        self.core.has_universe(universe)
    }

    /// The universes the source currently has, in ascending order.
    pub fn universes(&self) -> impl Iterator<Item = Universe> + '_ {
        self.core.universes()
    }

    /// Sets the NULL-start-code levels for a universe. See
    /// [`Source::update_levels`](crate::source::Source::update_levels).
    pub fn update_levels(&mut self, universe: Universe, levels: &[u8]) {
        self.core.update_levels(universe, levels);
    }

    /// Sets both the levels and per-address priorities for a universe. See
    /// [`Source::update_levels_and_pap`](crate::source::Source::update_levels_and_pap).
    pub fn update_levels_and_pap(&mut self, universe: Universe, levels: &[u8], pap: &[u8]) {
        self.core.update_levels_and_pap(universe, levels, pap);
    }

    /// Stops sending per-address priority for a universe.
    pub fn remove_pap(&mut self, universe: Universe) {
        self.core.remove_pap(universe);
    }

    /// Changes a universe's priority.
    pub fn set_priority(&mut self, universe: Universe, priority: Priority) {
        self.core.set_priority(universe, priority);
    }

    /// Changes a universe's preview-data flag.
    pub fn set_preview(&mut self, universe: Universe, preview: bool) {
        self.core.set_preview(universe, preview);
    }

    /// Changes the source name carried in every packet.
    ///
    /// The name is a human-readable UTF-8 string; only its first 63 bytes are
    /// transmitted, truncated at a character boundary.
    pub fn set_name(&mut self, name: impl Into<SourceName>) {
        self.core.set_name(name);
    }

    /// Starts, changes, or stops synchronization for a universe at runtime. See
    /// [`Source::set_synchronization`](crate::source::Source::set_synchronization).
    pub fn set_synchronization(
        &mut self,
        universe: Universe,
        sync: Option<(Universe, OnSyncLoss)>,
    ) {
        let sync_universe = sync.map_or(0, |(u, _)| u.get());
        self.core.set_synchronization(universe, sync);
        self.routes.set_sync(universe, sync_universe);
    }

    /// Adds a unicast destination for a universe, on the standard sACN port, in
    /// addition to any multicast. Returns `false` if the universe is not present
    /// or the address was already a destination.
    pub fn add_unicast(&mut self, universe: Universe, addr: IpAddr) -> bool {
        self.add_unicast_to_port(universe, addr, SACN_PORT)
    }

    /// Adds a unicast destination for a universe on an explicit destination
    /// `port`, in addition to any multicast. Returns `false` if the universe is
    /// not present or the address was already a destination.
    ///
    /// The same address may be a destination on more than one port; each
    /// `(address, port)` pair is tracked independently.
    pub fn add_unicast_to_port(&mut self, universe: Universe, addr: IpAddr, port: u16) -> bool {
        if !self
            .routes
            .add_unicast(universe, SocketAddr::new(addr, port))
        {
            return false;
        }
        // Nudge the core to re-send promptly so the new destination gets data
        // without waiting for the next keep-alive.
        self.core.resend(universe);
        true
    }

    /// Removes a unicast destination on the standard sACN port from a universe.
    /// Returns `false` if the universe or address was not present. For a
    /// destination added with [`add_unicast_to_port`](Self::add_unicast_to_port),
    /// use [`remove_unicast_from_port`](Self::remove_unicast_from_port).
    pub fn remove_unicast(&mut self, universe: Universe, addr: IpAddr) -> bool {
        self.remove_unicast_from_port(universe, addr, SACN_PORT)
    }

    /// Removes a unicast destination on an explicit `port` from a universe.
    /// Returns `false` if the universe or `(address, port)` pair was not present.
    pub fn remove_unicast_from_port(
        &mut self,
        universe: Universe,
        addr: IpAddr,
        port: u16,
    ) -> bool {
        self.routes
            .remove_unicast(universe, SocketAddr::new(addr, port))
    }

    /// Sends every packet currently due and returns the instant at which the
    /// next one falls due, or `None` if the source has nothing left to transmit.
    ///
    /// Call this in a loop, sleeping until the returned deadline (and applying
    /// any data updates) between calls, or use [`run`](Self::run) as a
    /// convenience wrapper around the same. Individual send failures are logged
    /// (once, on the transition into and out of failure).
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe. Dropping the future and calling again will
    /// never result in an operation (e.g. a state update or a transmission of
    /// a packet to a specific destination) being lost or duplicated.
    pub async fn process(&mut self) -> Result<Option<TokioInstant>, AdapterError> {
        let Self {
            socket,
            core,
            epoch,
            routes,
            fanout,
        } = self;

        // Finish delivering a packet whose fan-out a previous cancellation left
        // partway through.
        if fanout.is_some() {
            flush_fanout(socket, core.current_packet(), fanout, routes).await;
        }

        let now = Instant::from_epoch(epoch.elapsed());
        let mut poll = core.poll(now);
        let deadline = poll.deadline;
        while let Some(transmission) = poll.next_transmission() {
            // Resolve this packet's destinations up front so an interrupted
            // fan-out can resume against a stable list, then deliver it. The bytes
            // live in the core buffer for the duration of the fan-out.
            *fanout = Some(Fanout {
                targets: routes.targets_for(transmission.route),
                next: 0,
            });
            flush_fanout(socket, transmission.data, fanout, routes).await;
        }

        for &universe in poll.removed() {
            // Retire the universe's destinations (and, with them, any tracked
            // failure state for its endpoints) now that the core has dropped it.
            routes.remove_universe(universe);
        }

        Ok(deadline.map(|d| *epoch + d.since_epoch()))
    }

    /// Immediately sends a one-shot packet with an arbitrary START code on
    /// `universe`, for application data outside the managed NULL-level and
    /// per-address-priority streams - for example a text packet or a
    /// manufacturer-specific message. See
    /// [`Source::send_now`](crate::source::Source::send_now).
    ///
    /// Unlike the level streams, this is not rate limited or suppressed: the
    /// packet is sent once, to the universe's multicast groups and unicast
    /// destinations, at whatever rate the application calls this. It takes the
    /// universe's next sequence number, so it stays ordered with the scheduled
    /// stream.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe. Dropping the future and calling again will
    /// never result in an operation (e.g. a state update or a transmission of
    /// a packet to a specific destination) being lost or duplicated.
    ///
    /// # Errors
    ///
    /// - [`AdapterError::Protocol`] wrapping [`Error::ReservedStartCode`] if
    ///   `start_code` is [`StartCode::NULL`] or [`StartCode::PAP`],
    ///   [`Error::NoSuchUniverse`] if the universe is not present, or
    ///   [`Error::Codec`] if `data` exceeds the 512-slot maximum.
    ///
    /// [`Error::ReservedStartCode`]: crate::Error::ReservedStartCode
    /// [`Error::NoSuchUniverse`]: crate::Error::NoSuchUniverse
    /// [`Error::Codec`]: crate::Error::Codec
    pub async fn send_now(
        &mut self,
        universe: Universe,
        start_code: StartCode,
        data: &[u8],
    ) -> Result<(), AdapterError> {
        let Self {
            socket,
            core,
            routes,
            fanout,
            ..
        } = self;

        // Finish any interrupted fan-out first
        if fanout.is_some() {
            flush_fanout(socket, core.current_packet(), fanout, routes).await;
        }

        let transmission = core.send_now(universe, start_code, data)?;
        *fanout = Some(Fanout {
            targets: routes.targets_for(transmission.route),
            next: 0,
        });
        flush_fanout(socket, transmission.data, fanout, routes).await;
        Ok(())
    }

    /// Drives the source continuously: repeatedly sends every packet currently
    /// due and waits until the next one falls due.
    ///
    /// This function only returns if a send fails (see Errors), so it is meant
    /// to be raced against other futures in a [`tokio::select!`]; it is
    /// cancel-safe.
    ///
    /// ```no_run
    /// # use sacn::tokio::Source;
    /// # use sacn::Universe;
    /// # async fn demo(mut source: Source, mut rx: tokio::sync::mpsc::Receiver<(Universe, Vec<u8>)>) -> Result<(), sacn::AdapterError> {
    /// loop {
    ///     tokio::select! {
    ///         result = source.run() => { result?; }
    ///         Some((universe, levels)) = rx.recv() => source.update_levels(universe, &levels),
    ///     }
    /// }
    /// # }
    /// ```
    ///
    /// While the source has nothing left to transmit (its deadline is `None`) this
    /// resolves to a pending future and will never resolve; it can only be
    /// cancelled. Practically speaking, this happens when the source has no
    /// universes with any active data.
    ///
    /// # Errors
    ///
    /// Propagates the first error from [`process`](Self::process).
    pub async fn run(&mut self) -> Result<core::convert::Infallible, AdapterError> {
        loop {
            match self.process().await? {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        }
    }
}

/// Delivers `bytes` to the destinations recorded in `fanout`, resuming from
/// wherever a prior call left off and clearing `fanout` once every destination
/// has been reached. Individual send failures are logged (once, on the
/// transition into and out of failure) and skipped.
async fn flush_fanout(
    socket: &UdpSocket,
    bytes: &[u8],
    fanout: &mut Option<Fanout>,
    routes: &mut Routes,
) {
    let Some(f) = fanout else {
        return;
    };
    while f.next < f.targets.len() {
        let target = f.targets[f.next];
        let result = send_to_target(socket, target, bytes).await;
        routes.report(target, result);
        f.next += 1;
    }
    *fanout = None;
}

/// Sends `bytes` to a single resolved destination.
async fn send_to_target(
    socket: &UdpSocket,
    target: SendTarget,
    bytes: &[u8],
) -> std::io::Result<usize> {
    match target {
        SendTarget::Multicast {
            universe,
            interface,
        } => send_multicast(socket, universe, interface, bytes).await,
        SendTarget::Unicast(addr) => socket.send_to(bytes, addr).await,
    }
}

/// Sends `bytes` to `universe`'s multicast group out of `interface`, setting the
/// outgoing interface first.
async fn send_multicast(
    socket: &UdpSocket,
    universe: u16,
    interface: MulticastInterface,
    bytes: &[u8],
) -> std::io::Result<usize> {
    set_multicast_interface(socket, interface)?;
    let group = match interface {
        MulticastInterface::V4(_) => SocketAddr::new(ipv4_multicast(universe).into(), SACN_PORT),
        MulticastInterface::V6(_) => SocketAddr::new(ipv6_multicast(universe).into(), SACN_PORT),
    };
    socket.send_to(bytes, group).await
}

/// Sets the socket's outgoing multicast interface for the next send.
fn set_multicast_interface(
    socket: &UdpSocket,
    interface: MulticastInterface,
) -> std::io::Result<()> {
    let sock = SockRef::from(socket);
    match interface {
        MulticastInterface::V4(addr) => sock.set_multicast_if_v4(&addr),
        MulticastInterface::V6(index) => sock.set_multicast_if_v6(index),
    }
}
