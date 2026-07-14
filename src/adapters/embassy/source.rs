//! The embassy source adapter.

use core::fmt;

use embassy_net::udp::UdpSocket;
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_time::{Instant as EmbassyInstant, Timer};

use crate::HeapStorage;
use crate::proto::SACN_PORT;
use crate::source::{OnSyncLoss, SourceConfig, SourceCore, UniverseConfig};
use crate::storage::VecLike;
use crate::time::Instant;
use crate::types::{Priority, SourceName, StartCode, Universe};

use super::error::EmbassyError;
use super::sending::Routes;
use super::storage::{Fanout, SourceResources, SourceStorage};
use super::{from_embassy_duration, to_embassy_duration};

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
/// methods), and drive transmission by calling [`process`](Self::process) or
/// [`run`](Self::run) in a loop.
///
/// The source's working memory, including the embassy stack's socket buffers,
/// lives in a separate [`SourceResources`] borrowed for the source's
/// whole lifetime. This type can be given fixed, heapless limits and const-
/// constructed in static memory using the macro
/// [`embassy_static_storage!`](crate::embassy_static_storage!).
///
/// A source must keep transmitting on its own schedule even when the application
/// is idle (for keep-alive packets). [`process`](Self::process) sends every
/// packet that is currently due and returns the instant the next one falls due;
/// the application waits until then (interleaving any data updates) before
/// calling it again. [`run`](Self::run) is a convenience wrapper around that
/// loop, meant to be raced against your application's events with
/// `embassy_futures::select`.
///
/// ```no_run
/// use sacn::embassy::{SourceResources, Source};
/// use sacn::{Cid, SourceConfig, Universe, UniverseConfig};
/// use static_cell::ConstStaticCell;
///
/// // A fixed-capacity storage policy, plus a const constructor for its memory.
/// sacn::embassy_static_storage! {
///     pub struct Caps {
///         rx_universes: 0,
///         rx_sources_per_universe: 0,
///         rx_sync_addresses: 0,
///         tx_universes: 4,
///         tx_unicast_per_universe: 2,
///         det_sources: 0,
///         det_universes_per_source: 0,
///     }
/// }
///
/// static RESOURCES: ConstStaticCell<SourceResources<Caps>> =
///     ConstStaticCell::new(Caps::embassy_source_resources());
///
/// # fn my_cid() -> Cid {
/// #     Cid::from_bytes([1; 16])
/// # }
/// #
/// # async fn demo(stack: embassy_net::Stack<'static>) -> Result<(), sacn::embassy::EmbassyError> {
/// let resources = RESOURCES.take();
/// // my_cid() should return a CID that is stable over your device's lifetime
/// // (i.e. through power cycles, etc).
/// let config = SourceConfig::new(my_cid(), "demo");
/// // `stack` is your already-initialized `embassy_net::Stack`.
/// let mut source = Source::new(stack, resources, config)?;
///
/// let universe = Universe::new(1).unwrap();
/// source.add_universe(UniverseConfig::new(universe))?;
/// source.update_levels(universe, &[255, 128, 0]);
///
/// loop {
///     match source.process().await? {
///         Some(at) => embassy_time::Timer::at(at).await,
///         None => break,
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct Source<'d, S: SourceStorage = HeapStorage> {
    socket: UdpSocket<'d>,
    stack: Stack<'d>,
    core: SourceCore<S>,
    core_store: &'d mut crate::source::SourceResources<S>,
    routes: Routes<'d, S>,
    epoch: EmbassyInstant,
    in_flight: &'d mut Option<Fanout<S>>,
}

impl<'d, S: SourceStorage> Source<'d, S> {
    /// Binds a source, ready to transmit, on the standard sACN port, using the
    /// working memory and socket buffers in `resources`.
    ///
    /// The resources must outlive the source. No universes are transmitted
    /// until one is added with [`add_universe`](Self::add_universe) and given
    /// data with [`update_levels`](Self::update_levels).
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Bind`] if the socket cannot be bound.
    pub fn new(
        stack: Stack<'d>,
        resources: &'d mut SourceResources<S>,
        config: SourceConfig,
    ) -> Result<Self, EmbassyError> {
        // Force the storage capacity coherence assertions at monomorphization.
        let () = super::storage::AssertEmbassyCoherent::<S>::CHECK;

        let SourceResources {
            source: core_store,
            destinations,
            in_flight,
            tx_meta,
            tx_buffer,
        } = resources;

        let mut socket = UdpSocket::new(
            stack,
            &mut [],
            &mut [],
            tx_meta.as_mut(),
            tx_buffer.as_mut(),
        );
        socket.bind(0).map_err(EmbassyError::Bind)?;
        socket.set_hop_limit(Some(MULTICAST_HOP_LIMIT));
        Ok(Self {
            socket,
            stack,
            core: SourceCore::with_config(config),
            core_store,
            routes: Routes::new(destinations),
            epoch: EmbassyInstant::now(),
            in_flight,
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
        self.add_universe_with(config, true)
    }

    /// Adds a unicast-only universe: one whose data is sent solely to the unicast
    /// destinations added with [`add_unicast`](Self::add_unicast), never to a
    /// multicast group. Add at least one destination for it to transmit anything.
    ///
    /// Returns `true` if it was added, or `false` if it was already present (left
    /// unchanged). Whether an existing universe multicasts is fixed when it is
    /// added; remove and re-add it to change that.
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Protocol`] wrapping
    /// [`Error::NoCapacity`](crate::Error::NoCapacity) if a fixed-capacity
    /// source's universe table is full.
    pub fn add_unicast_only_universe(
        &mut self,
        config: UniverseConfig,
    ) -> Result<bool, EmbassyError> {
        self.add_universe_with(config, false)
    }

    /// Adds a universe to the core and records whether it multicasts, unless it
    /// was already present (in which case the core leaves it unchanged and the
    /// destination table is not touched).
    fn add_universe_with(
        &mut self,
        config: UniverseConfig,
        multicast: bool,
    ) -> Result<bool, EmbassyError> {
        let universe = config.universe();
        let sync_universe = config.sync_universe();
        let added = self.core.add_universe(self.core_store, config)?;
        if added {
            self.routes.add_universe(universe, multicast, sync_universe);
        }
        Ok(added)
    }

    /// Begins terminating a universe (the E1.31 three-packet stream-terminated
    /// sequence), after which it is dropped. Returns `false` if the universe was
    /// not present. Keep driving [`run`](Self::run) / [`process`](Self::process)
    /// until the universe is gone to actually send the termination packets.
    pub fn remove_universe(&mut self, universe: Universe) -> bool {
        self.core.remove_universe(self.core_store, universe)
    }

    /// Whether the source is currently transmitting (or terminating) `universe`.
    pub fn has_universe(&self, universe: Universe) -> bool {
        self.core.has_universe(&*self.core_store, universe)
    }

    /// The universes the source currently has, in ascending order.
    pub fn universes(&self) -> impl Iterator<Item = Universe> + '_ {
        self.core.universes(&*self.core_store)
    }

    /// Sets the NULL-start-code levels for a universe. See
    /// [`Source::update_levels`](crate::source::Source::update_levels).
    pub fn update_levels(&mut self, universe: Universe, levels: &[u8]) {
        self.core.update_levels(self.core_store, universe, levels);
    }

    /// Sets both the levels and per-address priorities for a universe. See
    /// [`Source::update_levels_and_pap`](crate::source::Source::update_levels_and_pap).
    pub fn update_levels_and_pap(&mut self, universe: Universe, levels: &[u8], pap: &[u8]) {
        self.core
            .update_levels_and_pap(self.core_store, universe, levels, pap);
    }

    /// Stops sending per-address priority for a universe.
    pub fn remove_pap(&mut self, universe: Universe) {
        self.core.remove_pap(self.core_store, universe);
    }

    /// Changes a universe's priority.
    pub fn set_priority(&mut self, universe: Universe, priority: Priority) {
        self.core.set_priority(self.core_store, universe, priority);
    }

    /// Changes a universe's preview-data flag.
    pub fn set_preview(&mut self, universe: Universe, preview: bool) {
        self.core.set_preview(self.core_store, universe, preview);
    }

    /// Changes the source name carried in every packet. Only the first 63 bytes
    /// are transmitted, truncated at a character boundary.
    pub fn set_name(&mut self, name: impl Into<SourceName>) {
        self.core.set_name(self.core_store, name);
    }

    /// Restarts transmission of a universe's current data promptly, without
    /// changing it. See [`Source::resend`](crate::source::Source::resend).
    pub fn resend(&mut self, universe: Universe) {
        self.core.resend(self.core_store, universe);
    }

    /// Starts, changes, or stops synchronization for a universe at runtime. See
    /// [`Source::set_synchronization`](crate::source::Source::set_synchronization).
    pub fn set_synchronization(
        &mut self,
        universe: Universe,
        sync: Option<(Universe, OnSyncLoss)>,
    ) {
        let sync_universe = sync.map_or(0, |(u, _)| u.get());
        self.core
            .set_synchronization(self.core_store, universe, sync);
        self.routes.set_sync(universe, sync_universe);
    }

    /// Adds a unicast destination for a universe, on the standard sACN port, in
    /// addition to any multicast. Returns `false` if the universe is not present,
    /// the address was already a destination, or the destination table is full.
    pub fn add_unicast(&mut self, universe: Universe, addr: IpAddress) -> bool {
        self.add_unicast_to_port(universe, addr, SACN_PORT)
    }

    /// Adds a unicast destination for a universe on an explicit destination
    /// `port`, in addition to any multicast. Returns `false` if the universe is
    /// not present, the address was already a destination, or the destination
    /// table is full.
    ///
    /// The same address may be a destination on more than one port; each
    /// `(address, port)` pair is tracked independently.
    pub fn add_unicast_to_port(&mut self, universe: Universe, addr: IpAddress, port: u16) -> bool {
        let endpoint = IpEndpoint::new(addr, port);
        if !self.routes.add_unicast(universe, endpoint) {
            return false;
        }
        // Nudge the core to re-send promptly so the new destination gets data
        // without waiting for the next keep-alive.
        self.core.resend(self.core_store, universe);
        true
    }

    /// Removes a unicast destination on the standard sACN port from a universe.
    /// Returns `false` if the universe or address was not present. For a
    /// destination added with [`add_unicast_to_port`](Self::add_unicast_to_port),
    /// use [`remove_unicast_from_port`](Self::remove_unicast_from_port).
    pub fn remove_unicast(&mut self, universe: Universe, addr: IpAddress) -> bool {
        self.remove_unicast_from_port(universe, addr, SACN_PORT)
    }

    /// Removes a unicast destination on an explicit `port` from a universe.
    /// Returns `false` if the universe or `(address, port)` pair was not present.
    pub fn remove_unicast_from_port(
        &mut self,
        universe: Universe,
        addr: IpAddress,
        port: u16,
    ) -> bool {
        let endpoint = IpEndpoint::new(addr, port);
        self.routes.remove_unicast(universe, endpoint)
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
            core_store: source,
            routes,
            epoch,
            in_flight,
        } = self;

        // Finish delivering a packet whose fan-out a previous cancellation left
        // partway through.
        if in_flight.is_some() {
            flush_fanout(socket, source.current_packet(), in_flight, routes).await;
        }

        let (v4, v6) = (stack.config_v4().is_some(), stack.config_v6().is_some());
        let now = Instant::from_epoch(from_embassy_duration(epoch.elapsed()));
        let mut poll = core.poll(source, now);
        let deadline = poll.deadline;

        while let Some(transmission) = poll.next_transmission() {
            // Resolve this packet's destinations up front so an interrupted
            // fan-out resumes against a stable list.
            let targets = routes.targets_for(transmission.route, v4, v6);
            if targets.is_empty() {
                // No address family exists on `stack` and there is no unicast
                // destination, so there is nothing to send to.
                continue;
            }
            **in_flight = Some(Fanout { targets, next: 0 });
            flush_fanout(socket, transmission.data, in_flight, routes).await;
        }

        // Retire the destinations of any universe the core dropped this poll,
        // along with any tracked failure state for its endpoints.
        for &universe in poll.removed() {
            routes.remove_universe(universe);
        }

        Ok(deadline.map(|deadline| *epoch + to_embassy_duration(deadline.since_epoch())))
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
    /// - [`EmbassyError::Protocol`] wrapping [`Error::ReservedStartCode`] if
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
    ) -> Result<(), EmbassyError> {
        let Self {
            socket,
            stack,
            core,
            core_store: source,
            routes,
            in_flight,
            ..
        } = self;

        // Finish any interrupted fan-out first.
        if in_flight.is_some() {
            flush_fanout(socket, source.current_packet(), in_flight, routes).await;
        }

        let transmission = core.send_now(source, universe, start_code, data)?;
        let (v4, v6) = (stack.config_v4().is_some(), stack.config_v6().is_some());
        let targets = routes.targets_for(transmission.route, v4, v6);
        if !targets.is_empty() {
            **in_flight = Some(Fanout { targets, next: 0 });
            flush_fanout(socket, transmission.data, in_flight, routes).await;
        }
        Ok(())
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

#[cfg(feature = "alloc")]
impl<'d> Source<'d, HeapStorage> {
    /// Binds a heap-backed source that owns its working memory.
    ///
    /// It might be convenient to use this form if you are using an allocator
    /// and want more flexibility for how [`Source`] and [`SourceResources`]
    /// are stored. In this case, [`SourceResources`] will be created with
    /// [`Box::leak`], so it will live for the lifetime of the program and
    /// cannot be reclaimed.
    ///
    /// ```no_run
    /// use sacn::embassy::Source;
    /// use sacn::{Cid, SourceConfig};
    ///
    /// # async fn demo<'d>(stack: embassy_net::Stack<'d>) -> Result<(), sacn::embassy::EmbassyError> {
    /// let config = SourceConfig::new(Cid::from_bytes([1; 16]), "demo");
    /// // Where `stack` is your `embassy_net::Stack<'d>`
    /// let source: Source<'d> = Source::new_boxed(stack, config)?;
    /// # let _ = source;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Bind`] if the socket cannot be bound. Note that
    /// on this error path the freshly allocated resources are leaked (not
    /// reclaimed).
    pub fn new_boxed(stack: Stack<'d>, config: SourceConfig) -> Result<Self, EmbassyError> {
        // Leak up front: `Source` borrows the resources, so the borrow checker
        // will not let us reclaim the box after handing it out. A `bind` failure
        // therefore leaks the allocation, which keeps this constructor free of
        // `unsafe` at the cost of a one-time leak on a fatal startup error.
        let resources = alloc::boxed::Box::leak(alloc::boxed::Box::new(SourceResources::<
            HeapStorage,
        >::default()));
        Self::new(stack, resources, config)
    }
}

impl<S: SourceStorage> fmt::Debug for Source<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The socket, stack and core hold no publicly meaningful debug state
        // (and the socket is not `Debug`); surface the adapter's own fields.
        f.debug_struct("Source")
            .field("routes", &self.routes)
            .field("epoch", &self.epoch)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

/// Delivers `bytes` to the destinations recorded in `fanout`, resuming from
/// wherever a prior call left off and clearing `fanout` once every destination
/// has been reached. Individual send failures are logged (once, on the
/// transition into and out of failure) and skipped.
async fn flush_fanout<S: SourceStorage>(
    socket: &UdpSocket<'_>,
    bytes: &[u8],
    fanout: &mut Option<Fanout<S>>,
    routes: &mut Routes<'_, S>,
) {
    let Some(f) = fanout else {
        return;
    };
    while f.next < f.targets.len() {
        let target = f.targets.as_slice()[f.next];
        let result = socket.send_to(bytes, target).await;
        routes.report(target, result);
        f.next += 1;
    }
    *fanout = None;
}
