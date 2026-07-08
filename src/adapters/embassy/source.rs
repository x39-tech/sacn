//! The embassy source adapter.

use core::fmt;

use embassy_net::udp::{PacketMetadata, SendError, UdpSocket};
use embassy_net::{IpEndpoint, Stack};
use embassy_time::{Instant as EmbassyInstant, Timer};

use crate::log::{info, warning};
use crate::source::{Source as Core, SourceConfig, SourceStorage, UniverseConfig};
use crate::storage::HeapStorage;
use crate::time::Instant;
use crate::types::{Priority, SourceName, Universe};

use super::error::EmbassyError;
use super::{from_embassy_duration, targets_for, to_embassy_duration, Targets};

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;

/// The multicast hop limit sources transmit with.
const MULTICAST_HOP_LIMIT: u8 = 64;

/// An asynchronous, `no_std` sACN source driven by the embassy runtime.
///
/// Construct one with [`new`](Self::new), register universes with
/// [`add_universe`](Self::add_universe), set their data with
/// [`update_levels`](Self::update_levels) (and the other `update_*` / `set_*`
/// methods), and drive transmission by racing [`run`](Self::run) against your
/// application's events in a `select`. `run` sends every packet that is due and
/// sleeps until the next one falls due; it only returns on error.
///
/// The `S` type parameter is the core [`SourceStorage`] policy. Use a
/// fixed-capacity marker created by
/// [`static_storage!`](crate::static_storage!) on a target with no allocator.
pub struct Source<'d, S: SourceStorage = HeapStorage> {
    socket: UdpSocket<'d>,
    stack: Stack<'d>,
    core: Core<S>,
    epoch: EmbassyInstant,
    in_flight: Option<Fanout>,
    failing: FailingTargets,
}

/// A packet's in-progress delivery to its destinations, plus the cursor that
/// makes resuming an interrupted send cancel safe.
#[derive(Debug)]
struct Fanout {
    /// The resolved destinations, captured once when the fan-out begins.
    targets: Targets,
    /// The index of the next destination still to send to. Advanced only after
    /// a send completes.
    next: usize,
}

impl<'d, S: SourceStorage> Source<'d, S> {
    /// Binds a source, ready to transmit, on the standard sACN port, using the
    /// caller-provided socket buffers.
    ///
    /// The buffers must outlive the source (`'d`). A source only transmits,
    /// but still needs receive buffers because `UdpSocket::new` requires them;
    /// they can be minimal. No universes are transmitted until one is added
    /// with [`add_universe`](Self::add_universe) and given data with
    /// [`update_levels`](Self::update_levels).
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Bind`] if the socket cannot be bound.
    pub fn new(
        stack: Stack<'d>,
        rx_meta: &'d mut [PacketMetadata],
        rx_buffer: &'d mut [u8],
        tx_meta: &'d mut [PacketMetadata],
        tx_buffer: &'d mut [u8],
        config: SourceConfig,
    ) -> Result<Self, EmbassyError> {
        let mut socket = UdpSocket::new(stack, rx_meta, rx_buffer, tx_meta, tx_buffer);
        socket.bind(0).map_err(EmbassyError::Bind)?;
        socket.set_hop_limit(Some(MULTICAST_HOP_LIMIT));
        Ok(Self {
            socket,
            stack,
            core: Core::with_config(config),
            epoch: EmbassyInstant::now(),
            in_flight: None,
            failing: FailingTargets::default(),
        })
    }

    /// The configuration this source was created with.
    pub fn config(&self) -> &SourceConfig {
        self.core.config()
    }

    /// Adds a universe to transmit on. Returns `true` if it was added, or
    /// `false` if it was already present (left unchanged). The universe
    /// transmits nothing until its levels are set with
    /// [`update_levels`](Self::update_levels).
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Protocol`] wrapping
    /// [`Error::NoCapacity`](crate::Error::NoCapacity) if a fixed-capacity
    /// source's universe table is full.
    pub fn add_universe(&mut self, config: UniverseConfig) -> Result<bool, EmbassyError> {
        Ok(self.core.add_universe(config)?)
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

    /// Changes the source name carried in every packet. Only the first 63 bytes
    /// are transmitted, truncated at a character boundary.
    pub fn set_name(&mut self, name: impl Into<SourceName>) {
        self.core.set_name(name);
    }

    /// Restarts transmission of a universe's current data promptly, without
    /// changing it. See [`Source::resend`](crate::source::Source::resend).
    pub fn resend(&mut self, universe: Universe) {
        self.core.resend(universe);
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
    pub async fn process(&mut self) -> Result<Option<EmbassyInstant>, EmbassyError> {
        let Self {
            socket,
            stack,
            core,
            epoch,
            in_flight,
            failing,
        } = self;

        // Finish delivering a packet whose fan-out a previous cancellation left
        // partway through.
        if in_flight.is_some() {
            flush_fanout(socket, core.current_packet(), in_flight, failing).await;
        }

        let now = Instant::from_epoch(from_embassy_duration(epoch.elapsed()));
        let mut poll = core.poll(now);
        let deadline = poll.deadline;

        while let Some(transmission) = poll.next_transmission() {
            // Resolve this packet's multicast groups (IPv4/IPv6, per the stack's
            // configured families) up front so an interrupted fan-out resumes
            // against a stable list.
            let targets = targets_for(transmission.route, *stack);
            if targets.is_empty() {
                // No address family is up yet (e.g. DHCP has not completed), so
                // there is nothing to send to.
                continue;
            }
            *in_flight = Some(Fanout { targets, next: 0 });
            flush_fanout(socket, transmission.data, in_flight, failing).await;
        }

        // `removed()` reports universes the core dropped this poll; we
        // currently keep no per-universe state, so there is nothing to clean up.

        Ok(deadline.map(|deadline| *epoch + to_embassy_duration(deadline.since_epoch())))
    }

    /// Drives the source continuously: repeatedly sends every packet currently
    /// due and waits until the next one falls due.
    ///
    /// This only returns on error, so it is meant to be raced against your
    /// application's events with `embassy_futures::select`. It is cancel-safe.
    ///
    /// While the source has nothing left to transmit its deadline is `None`, and
    /// this awaits a pending future that only resolves on cancellation.
    /// Practically speaking, this happens when the source has no universes
    /// with any active data.
    ///
    /// # Errors
    ///
    /// Propagates the first error from [`process`](Self::process).
    pub async fn run(&mut self) -> Result<core::convert::Infallible, EmbassyError> {
        loop {
            match self.process().await? {
                Some(at) => Timer::at(at).await,
                None => core::future::pending().await,
            }
        }
    }
}

impl<S: SourceStorage> fmt::Debug for Source<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The socket, stack and core hold no publicly meaningful debug state
        // (and the socket is not `Debug`); surface the adapter's own fields.
        f.debug_struct("Source")
            .field("epoch", &self.epoch)
            .field("in_flight", &self.in_flight)
            .field("failing", &self.failing)
            .finish_non_exhaustive()
    }
}

/// Delivers `bytes` to the destinations recorded in `fanout`, resuming from
/// wherever a prior call left off and clearing `fanout` once every destination
/// has been reached. Individual send failures are logged (once, on the
/// transition into and out of failure) and skipped.
async fn flush_fanout(
    socket: &UdpSocket<'_>,
    bytes: &[u8],
    fanout: &mut Option<Fanout>,
    failing: &mut FailingTargets,
) {
    let Some(f) = fanout else {
        return;
    };
    while f.next < f.targets.len() {
        let target = f.targets[f.next];
        let result = socket.send_to(bytes, target).await;
        failing.report(target, result);
        f.next += 1;
    }
    *fanout = None;
}

/// The set of destinations whose last send failed. Used to log a persistent
/// error only once on the way into failure and again on recovery.
#[derive(Debug, Default)]
struct FailingTargets(Targets);

impl FailingTargets {
    /// Records the outcome of a send to `target`, logging only on the transition
    /// into a failed state and again on recovery.
    fn report(&mut self, target: IpEndpoint, result: Result<(), SendError>) {
        match result {
            Err(error) => {
                if !self.0.contains(&target) {
                    // The set is keyed by target and capped at the number of
                    // distinct targets, so this push cannot overflow.
                    let _ = self.0.push(target);
                    warning!("sACN source send to {} failed: {:?}", target, error);
                }
            }
            Ok(()) => {
                if let Some(pos) = self.0.iter().position(|t| *t == target) {
                    self.0.remove(pos);
                    info!("sACN source send to {} recovered", target);
                }
            }
        }
    }
}
