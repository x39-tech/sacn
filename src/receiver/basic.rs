//! The basic receiver.

use core::marker::PhantomData;
use core::net::SocketAddr;

use crate::error::Error;
use crate::log::{debug, warning};
use crate::packet::{Packet, Payload};
use crate::storage::{HeapStorage, MapLike, VecLike};
use crate::time::Instant;
use crate::types::{NetintId, Universe};

use super::event::{ListenOutcome, SourceInfoRef, StopOutcome, UniverseDataRef};
use super::loss;
use super::source::TrackedSource;
use super::{BasicReceiverStorage, DMX_NULL_START_CODE, PAP_START_CODE, ReceiverConfig};

mod event;

#[cfg(feature = "alloc")]
pub use event::BasicReceiverEvent;
pub use event::{BasicReceiverPollEvent, LostSource, PacketOutcome, PollOutcome};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "property_tests.rs"]
mod property_tests;

// --- Per-universe state ------------------------------------------------------

/// State the receiver tracks for one listened universe.
#[doc(hidden)]
#[derive(Debug)]
pub struct BasicUniverseState<S: BasicReceiverStorage> {
    /// Sources currently being tracked, keyed by CID.
    sources: S::BasicSources,
    /// Whether a sampling period is in progress.
    sampling: bool,
    /// When the current sampling period ends.
    sample_end: Instant,
    /// Open source-loss termination sets being settled.
    term_sets: S::TermSets,
    /// Whether a source-limit-exceeded notification has already been emitted and
    /// should be suppressed until the count drops again.
    suppress_limit_exceeded: bool,
}

// --- The receiver ------------------------------------------------------------

/// A sACN basic receiver state machine: the bottom layer of the receive path,
/// without source merging.
///
/// `BasicReceiver` tracks the sources transmitting on each universe it listens
/// to and forwards their data to the application **per-source** - reconciling
/// priorities and merging across sources on the same universe belongs to the
/// higher [`Receiver`](super::Receiver) layer. It encodes the behavior an
/// interoperable and robust sACN receiver needs:
///
/// - **Sampling periods:** When a universe starts being listened to, a short
///   (configurable) window collects the sources present on the network before
///   the data is treated as authoritative, avoiding flicker as sources are
///   discovered.
/// - **Sequence numbering:** Out-of-order and duplicate packets are discarded
///   per E1.31 §6.7.2.
/// - **Source loss with settling:** Sources lost in quick succession are
///   grouped into a single notification.
/// - **Per-address priority (PAP, `0xDD`):** A new source is given time to
///   reveal whether it sends per-address priority, with fallback to packet
///   priority and a lost-PAP notification.
///
/// # Driving the state machine
///
/// The receiver holds no socket and no clock. The caller (an adapter, or a
/// test) drives it:
///
/// - [`listen`](Self::listen) / [`stop_listening`](Self::stop_listening)
///   register interest in a universe.
/// - [`handle_packet`](Self::handle_packet) feeds in a parsed packet and
///   returns the events it produced, borrowing the packet.
/// - [`poll`](Self::poll) advances time, running the periodic source-loss,
///   sampling and timeout logic; it returns the events produced and the next
///   instant at which calling it again could change something.
///
/// Every operation returns a type representing its possible outcomes.
/// Sometimes representing these outcomes requires a borrow of a type that was
/// passed in (as with [`handle_packet`](Self::handle_packet)) or a borrow of a
/// buffer held internally to `BasicReceiver` (as with [`poll`](Self::poll)). In
/// the latter case, the returned data must be used and dropped before the same
/// method is called again.
///
/// ```
/// use sacn::{BasicReceiver, ReceiverConfig, NetintId, Universe};
/// use sacn::receiver::{BasicReceiverPollEvent, PacketOutcome};
/// use sacn::packet::Packet;
/// use sacn::time::Instant;
/// use std::net::SocketAddr;
///
/// let mut receiver = BasicReceiver::new(ReceiverConfig::new());
/// let now = Instant::EPOCH;
/// receiver.listen(now, Universe::new(1).unwrap()).unwrap();
///
/// // A datagram has arrived on the socket; parse it and feed it in. The core
/// // never touches the socket itself, so the adapter supplies the bytes and
/// // the sender's address.
/// # let datagram = doctest_helper::dmx_datagram();
/// let from: SocketAddr = "192.0.2.10:5568".parse().unwrap();
/// let packet = Packet::parse(&datagram).unwrap();
/// match receiver.handle_packet(now, from, NetintId::UNKNOWN, &packet) {
///     PacketOutcome::Data { universe, data, .. } => {
///         // `data` is `Some` once a frame is ready to deliver. A new source's
///         // first frame can be briefly withheld while the receiver waits to see
///         // whether it also sends per-address priority.
///         if let Some(data) = data {
///             // deliver `data.values` from `data.source` on `data.universe`
///             let _ = data.values;
///         }
///         let _ = universe;
///     }
///     // A sync packet, a source-limit notification, or an ignored packet
///     // arrive as other variants; see `PacketOutcome` for the full set.
///     _ => {}
/// }
///
/// // Periodically advance timers: this ends sampling periods and settles
/// // source loss. Always drain every event the poll produced.
/// let mut poll = receiver.poll(now);
/// while let Some(event) = poll.next_event() {
///     match event {
///         BasicReceiverPollEvent::SamplingEnded { universe } => { let _ = universe; }
///         BasicReceiverPollEvent::SourcesLost { .. } => {}
///         _ => {}
///     }
/// }
/// # mod doctest_helper {
/// #     use sacn::{Source, SourceConfig, UniverseConfig, Cid, Universe};
/// #     use sacn::time::Instant;
/// #     pub fn dmx_datagram() -> Vec<u8> {
/// #         let mut src = Source::new(SourceConfig::new(Cid::from_bytes([7; 16]), "src"));
/// #         let u = Universe::new(1).unwrap();
/// #         src.add_universe(UniverseConfig::new(u)).unwrap();
/// #         src.update_levels(u, &[255, 128, 0]);
/// #         let mut poll = src.poll(Instant::EPOCH);
/// #         loop {
/// #             let tx = poll.next_transmission().unwrap();
/// #             if matches!(tx.route, sacn::Route::Universe(_)) {
/// #                 return tx.data.to_vec();
/// #             }
/// #         }
/// #     }
/// # }
/// ```
#[derive(Debug)]
pub struct BasicReceiver<S: BasicReceiverStorage = HeapStorage> {
    core: BasicReceiverCore<S>,
    store: BasicReceiverResources<S>,
}

/// The sACN basic receiver state machine: the receive-path logic, separated from
/// its working memory.
///
/// [`BasicReceiver`] contains one of these as well as a
/// [`BasicReceiverResources`]. Usually, just using [`BasicReceiver`] is the right
/// choice. Use this type alongside [`BasicReceiverResources`] if you need maximum
/// control of your memory layout; [`BasicReceiverResources`] contains all of the
/// bulk memory associated with a receiver, and can be const-initialized
/// statically.
///
/// This has all the same functionality as [`BasicReceiver`]; the only difference
/// is that each method takes a mutable reference to a separate
/// [`BasicReceiverResources`]. Each [`BasicReceiverCore`] should be associated
/// with exactly one [`BasicReceiverResources`] and you should pass the same
/// [`BasicReceiverResources`] instance to every call to a [`BasicReceiverCore`]
/// method.
#[derive(Debug)]
pub struct BasicReceiverCore<S: BasicReceiverStorage = HeapStorage> {
    config: ReceiverConfig,
    _marker: PhantomData<S>,
}

/// The mutable working memory a [`BasicReceiverCore`] operates on.
///
/// This struct holds everything about a receiver that scales with the number of
/// universes and their tracked sources, so it is the potentially large
/// allocation. It can be constructed in a const expression with
/// statically-allocated storage (see below).
///
/// Most users should just use [`BasicReceiver`] rather than [`BasicReceiverCore`]
/// and [`BasicReceiverResources`].
///
/// To construct:
///
/// - **Heap:** construct with [`BasicReceiverResources::default`].
/// - **Fixed-capacity:** use the [`static_storage!`](crate::static_storage!)
///   macro, which emits a `const fn` `basic_receiver_resources()` returning an
///   empty `BasicReceiverResources`, suitable for static allocation in a const
///   context.
#[derive(Debug)]
pub struct BasicReceiverResources<S: BasicReceiverStorage = HeapStorage> {
    universes: S::BasicUniverses,
    poll_keys: S::PollKeys,
    loss_scratch: S::LossList,
}

#[cfg(feature = "alloc")]
impl BasicReceiver<HeapStorage> {
    /// Creates a heap-backed receiver with the given configuration. It listens
    /// to no universes until [`listen`](Self::listen) is called.
    ///
    /// For a fixed-capacity receiver, construct with
    /// `BasicReceiver::<Caps>::with_config(config)` using a policy from
    /// [`static_storage!`](crate::static_storage!).
    pub fn new(config: ReceiverConfig) -> Self {
        Self::with_config(config)
    }
}

impl<S: BasicReceiverStorage> BasicReceiver<S> {
    /// Creates a receiver with the given configuration, backed by the storage
    /// policy `S`. It listens to no universes until [`listen`](Self::listen) is
    /// called.
    pub fn with_config(config: ReceiverConfig) -> Self {
        Self {
            core: BasicReceiverCore::with_config(config),
            store: BasicReceiverResources::default(),
        }
    }

    /// Get the config with which this receiver was created.
    pub fn config(&self) -> &ReceiverConfig {
        self.core.config()
    }

    /// Begins listening for a universe.
    ///
    /// For a universe not yet listened to, this opens a sampling period (a
    /// [`SamplingStarted`](BasicReceiverEvent::SamplingStarted) event).
    ///
    /// Calling it again for a universe already being listened to is a no-op that
    /// leaves the sampling period and tracked sources untouched (the returned
    /// [`ListenOutcome`] reports no new sampling period).
    ///
    /// Returns [`Error::NoCapacity`] when a fixed-capacity receiver's universe
    /// table is full and this universe is not already listened to.
    pub fn listen(&mut self, now: Instant, universe: Universe) -> Result<ListenOutcome, Error> {
        self.core.listen(&mut self.store, now, universe)
    }

    /// Stops listening for a universe. The returned [`StopOutcome`] reports
    /// whether the universe was being listened to.
    ///
    /// All sources tracked on this universe are considered lost and no
    /// notifications will be delivered.
    pub fn stop_listening(&mut self, universe: Universe) -> StopOutcome {
        self.core.stop_listening(&mut self.store, universe)
    }

    /// Feeds in a parsed packet received from `from` on interface `netint`,
    /// returning the events it produced (at most a per-address-priority-lost
    /// notification and a data delivery, both borrowing `packet`).
    ///
    /// Only data packets on a currently-listened universe are acted upon;
    /// anything else (sync, discovery, an unlistened universe, an out-of-range
    /// universe) is ignored and yields an outcome with no universe.
    ///
    /// `netint` identifies the interface the packet arrived on. It is not yet
    /// acted upon (sampling is currently per-universe); it is threaded through so
    /// per-interface re-sampling on network changes can be added without a
    /// signature change. Adapters that do not attribute packets to an interface
    /// pass [`NetintId::UNKNOWN`].
    pub fn handle_packet<'p>(
        &mut self,
        now: Instant,
        from: SocketAddr,
        netint: NetintId,
        packet: &Packet<'p>,
    ) -> PacketOutcome<'p> {
        self.core
            .handle_packet(&mut self.store, now, from, netint, packet)
    }

    /// Advances time to `now`, running the periodic sampling, source-timeout and
    /// source-loss settling logic, and returning the events it produced.
    ///
    /// The returned [`PollOutcome`] carries the next timer deadline (final as
    /// soon as `poll` returns) and a lazily-drained sequence of events. Calling
    /// `poll` earlier than the deadline is harmless; calling it later only
    /// delays notifications.
    ///
    /// Each `poll` first classifies every listened universe's sources and
    /// updates its termination sets eagerly, then leaves the settled losses and
    /// ended sampling periods to be drawn out one universe at a time by
    /// [`PollOutcome::next_event`]. A caller that stops draining early simply
    /// has those universes handled on the next `poll` rather than losing their
    /// events.
    pub fn poll(&mut self, now: Instant) -> PollOutcome<'_, S> {
        self.core.poll(&mut self.store, now)
    }
}

impl<S: BasicReceiverStorage> BasicReceiverResources<S> {
    /// Assembles resources from already-constructed (empty) collections.
    ///
    /// Not used directly; used only from [`static_storage!`](crate::static_storage!)
    /// or [`Default::default()`].
    #[doc(hidden)]
    pub const fn from_parts(
        universes: S::BasicUniverses,
        poll_keys: S::PollKeys,
        loss_scratch: S::LossList,
    ) -> Self {
        Self {
            universes,
            poll_keys,
            loss_scratch,
        }
    }

    /// The sources lost by the most recent settled termination set.
    fn lost_sources(&self) -> &[LostSource] {
        self.loss_scratch.as_slice()
    }

    /// The listened universe recorded at `index` in this poll's key snapshot.
    fn polled_universe(&self, index: usize) -> Option<&Universe> {
        self.poll_keys.as_slice().get(index)
    }
}

impl<S: BasicReceiverStorage> Default for BasicReceiverResources<S> {
    /// Empty resources with empty collections. For a fixed-capacity policy this
    /// builds the value at runtime; prefer the macro-generated
    /// `basic_receiver_resources()` `const fn` to place it in static memory
    /// without a stack copy.
    fn default() -> Self {
        Self::from_parts(
            S::BasicUniverses::default(),
            S::PollKeys::default(),
            S::LossList::default(),
        )
    }
}

impl<S: BasicReceiverStorage> BasicReceiverCore<S> {
    /// Creates a receiver controller with the given configuration, backed by the
    /// storage policy `S`. It listens to no universes until
    /// [`listen`](Self::listen) is called.
    ///
    /// The controller holds only the configuration; its working memory lives in
    /// a separate [`BasicReceiverResources`] passed to each method. Most users
    /// should use [`BasicReceiver`] instead of [`BasicReceiverCore`] and
    /// [`BasicReceiverResources`].
    pub fn with_config(config: ReceiverConfig) -> Self {
        let () = super::AssertReceiverCoherent::<S>::CHECK;
        Self {
            config,
            _marker: PhantomData,
        }
    }

    /// Get the config with which this receiver was created.
    pub fn config(&self) -> &ReceiverConfig {
        &self.config
    }

    /// Begins listening for a universe.
    ///
    /// See [`BasicReceiver::listen`].
    pub fn listen(
        &self,
        store: &mut BasicReceiverResources<S>,
        now: Instant,
        universe: Universe,
    ) -> Result<ListenOutcome, Error> {
        if store.universes.contains_key(&universe) {
            return Ok(ListenOutcome::new(false));
        }
        let state = BasicUniverseState {
            sources: S::BasicSources::default(),
            sampling: true,
            sample_end: now.saturating_add(self.config.sample_period),
            term_sets: S::TermSets::default(),
            suppress_limit_exceeded: false,
        };
        match store.universes.upsert(universe, state) {
            Ok(()) => {
                debug!(
                    "began listening on universe {}, sampling period started",
                    universe
                );
                Ok(ListenOutcome::new(true))
            }
            Err(_) => Err(Error::NoCapacity),
        }
    }

    /// Stops listening for a universe.
    ///
    /// See [`BasicReceiver::stop_listening`].
    pub fn stop_listening(
        &self,
        store: &mut BasicReceiverResources<S>,
        universe: Universe,
    ) -> StopOutcome {
        let was_listening = store.universes.remove(&universe);
        StopOutcome::new(was_listening)
    }

    /// Feeds in a parsed packet received from `from` on interface `netint`.
    ///
    /// See [`BasicReceiver::handle_packet`].
    pub fn handle_packet<'p>(
        &self,
        store: &mut BasicReceiverResources<S>,
        now: Instant,
        from: SocketAddr,
        netint: NetintId,
        packet: &Packet<'p>,
    ) -> PacketOutcome<'p> {
        let _ = netint;
        let config = &self.config;
        // Sync packets are simply surfaced immediately unless sync is disabled.
        if let Payload::Sync(sync) = &packet.payload {
            if !config.synchronization {
                return PacketOutcome::Ignored;
            }
            return PacketOutcome::Sync {
                sync_address: sync.sync_address,
                cid: packet.cid,
            };
        }
        let Payload::Data(data) = &packet.payload else {
            return PacketOutcome::Ignored;
        };
        // A packet for an out-of-range universe can never match one we listen to.
        let Ok(universe) = Universe::new(data.universe) else {
            return PacketOutcome::Ignored;
        };

        let Some(state) = store.universes.get_mut(&universe) else {
            return PacketOutcome::Ignored;
        };

        // Ignore START codes this receiver does not process.
        if !config.processes(data.start_code) {
            return PacketOutcome::Ignored;
        }

        // From here the packet pertains to `universe`, even if nothing is
        // delivered (a withheld, terminated, or out-of-sequence packet): such a
        // packet yields `Data` with neither `data` nor `pap_lost`.
        let accepted = PacketOutcome::Data {
            universe,
            data: None,
            pap_lost: None,
        };

        let cid = packet.cid;
        let seq = data.sequence_number;
        // Read the config/universe bits the source state machine needs up front,
        // so the source borrow below does not conflict with reading them.
        let sampling = state.sampling;
        let pap_handling = config.pap_active();
        let pap_wait = config.pap_wait;
        let is_new = !state.sources.contains_key(&cid);

        if is_new {
            // A termination from an untracked source brings nothing into being.
            if data.stream_terminated {
                return accepted;
            }
            // The runtime source limit and the storage capacity are the same
            // concept: a new source is refused when either the configured limit
            // is reached or the backing map is full.
            let refused = config
                .source_limit
                .is_some_and(|max| state.sources.len() >= max)
                || state
                    .sources
                    .upsert(cid, TrackedSource::new(now, seq))
                    .is_err();
            if refused {
                if !state.suppress_limit_exceeded {
                    state.suppress_limit_exceeded = true;
                    warning!("source limit exceeded on universe {}", universe);
                    return PacketOutcome::LimitExceeded { universe };
                }
                // The limit was already reported for this universe; suppress the
                // repeat and treat the packet as accepted-but-empty.
                return accepted;
            }
            debug!(
                "tracking new source on universe {} (start code {:#04x})",
                universe, data.start_code
            );
        }

        let src = state
            .sources
            .get_mut(&cid)
            .expect("source present after creation");

        if !is_new {
            if data.stream_terminated {
                src.mark_terminated(now);
            }
            // A terminated source is ignored until it is removed by settling.
            if src.terminated {
                return accepted;
            }
            if !seq.supersedes(src.seq) {
                return accepted;
            }
            src.seq = seq;
        }

        // Any accepted data packet refreshes the network data loss timer,
        // regardless of START code (E1.31 §6.7.1).
        src.register_data_packet(now);

        let mut notify = true;
        let mut pap_lost = false;
        match data.start_code {
            DMX_NULL_START_CODE => {
                let null_outcome = src.process_null(now, sampling, pap_handling, pap_wait);
                notify = null_outcome.notify;
                pap_lost = null_outcome.pap_lost;
            }
            PAP_START_CODE if pap_handling => src.process_pap(now),
            // A PAP packet with handling disabled, or any other allow-listed
            // START code, is forwarded but drives no NULL/PAP state machine.
            _ => {}
        }

        let deliver = notify && !(data.preview && config.filter_preview);
        if deliver {
            // The source has now been reported, so a later loss is notified
            // rather than dropped silently.
            src.ever_delivered = true;
        }

        let pap_lost = pap_lost.then_some(SourceInfoRef {
            cid,
            name: data.source_name,
        });
        let delivered = deliver.then_some(UniverseDataRef {
            universe,
            source: SourceInfoRef {
                cid,
                name: data.source_name,
            },
            addr: from,
            priority: data.priority,
            start_code: data.start_code,
            values: data.values,
            preview: data.preview,
            sync_address: if config.synchronization {
                data.sync_address
            } else {
                0
            },
            is_sampling: sampling,
        });
        PacketOutcome::Data {
            universe,
            data: delivered,
            pap_lost,
        }
    }

    /// Advances time to `now`, running the periodic sampling, source-timeout and
    /// source-loss settling logic.
    ///
    /// See [`BasicReceiver::poll`].
    pub fn poll<'a>(
        &'a self,
        store: &'a mut BasicReceiverResources<S>,
        now: Instant,
    ) -> PollOutcome<'a, S> {
        let mut deadline = None;
        {
            let config = &self.config;
            let BasicReceiverResources {
                universes,
                poll_keys,
                ..
            } = &mut *store;

            poll_keys.clear();
            for (&universe, state) in universes.iter_mut() {
                poll_keys.push_expect(universe);
                deadline = merge_deadline(deadline, Self::mark_universe(config, state, now));
            }
        }

        PollOutcome::new(deadline, self, store, now)
    }

    /// Runs the eager half of one `poll` tick for a single universe: classifies
    /// its sources and updates its termination sets, without firing any settled
    /// set or ending the sampling period. Those event-producing steps are
    /// deferred to [`PollOutcome::next_event`].
    ///
    /// Returns this universe's contribution to the next timer deadline.
    fn mark_universe(
        config: &ReceiverConfig,
        state: &mut BasicUniverseState<S>,
        now: Instant,
    ) -> Option<Instant> {
        // Classify sources as if a sampling period due to end this tick has
        // already ended (so a silent source is captured), but leave the stored
        // `sampling` flag for `next_event` to flip when it delivers the
        // matching `SamplingEnded`.
        let effective_sampling = state.sampling && now < state.sample_end;

        // These scratch lists are transient per universe; their capacity comes
        // from the storage policy.
        let mut offline = S::OfflineScratch::default();
        let mut online = S::CidScratch::default();
        let mut unknown = S::CidScratch::default();
        let mut to_remove = S::CidScratch::default();

        for (&cid, src) in state.sources.iter_mut() {
            if !src.ever_delivered {
                // Never reported to the application (only ever withheld NULL
                // pending PAP). It takes no part in source-loss settling; if it
                // falls silent, drop it quietly rather than as a loss.
                if now >= src.packet_expiry {
                    to_remove.push_expect(cid);
                }
                continue;
            }

            if now >= src.packet_expiry {
                offline.push_expect((cid, src.terminated));
            } else if src.data_received_since_last_tick {
                online.push_expect(cid);
                src.data_received_since_last_tick = false;
            } else if !effective_sampling {
                // A silent-but-not-yet-timed-out source. Outside a sampling
                // period it is captured so a near-simultaneous loss settles with
                // it (avoiding a transient wrong winner in live output). During
                // sampling the output is not yet live, so there is nothing to
                // protect: leaving it uncaptured lets any lost source be
                // reported promptly instead of waiting on this one.
                unknown.push_expect(cid);
            }
        }

        loss::mark_sources_offline::<S>(
            offline.as_slice(),
            unknown.as_slice(),
            &mut state.term_sets,
            config.extra_hold_time,
            now,
        );
        loss::mark_sources_online::<S>(online.as_slice(), &mut state.term_sets);

        for cid in to_remove.iter() {
            state.sources.remove(cid);
        }

        universe_deadline(state, now)
    }

    /// Ends the sampling period for a universe if applicable. Returns whether
    /// the sampling period was ended.
    ///
    /// Invariant: must be called with a valid universe from
    /// [`BasicReceiverResources::polled_universe`].
    fn maybe_end_sampling_period(
        &self,
        store: &mut BasicReceiverResources<S>,
        universe: &Universe,
        now: Instant,
    ) -> bool {
        let state = store
            .universes
            .get_mut(universe)
            .expect("must be called with a valid universe");
        if state.sampling && now >= state.sample_end {
            state.sampling = false;
            debug!("sampling period ended on universe {}", universe);
            true
        } else {
            false
        }
    }

    /// Fires any settled termination set on `universe`, writing the lost sources
    /// into the reusable `loss_scratch` and dropping them from the tracked set.
    /// Returns whether any were lost.
    ///
    /// Invariant: must be called with a valid universe from
    /// [`BasicReceiverResources::polled_universe`].
    fn maybe_fire_lost_sources(
        &self,
        store: &mut BasicReceiverResources<S>,
        universe: &Universe,
        now: Instant,
    ) -> bool {
        let BasicReceiverResources {
            universes,
            loss_scratch,
            ..
        } = store;
        let state = universes
            .get_mut(universe)
            .expect("must be called with a valid universe");
        loss::get_expired_sources::<S>(&mut state.term_sets, now, loss_scratch);
        if loss_scratch.is_empty() {
            return false;
        }
        for source in loss_scratch.iter() {
            state.sources.remove(&source.cid);
        }
        // The tracked-source count dropped, so allow a fresh limit-exceeded
        // notification.
        state.suppress_limit_exceeded = false;
        debug!(
            "{} source(s) lost on universe {}",
            loss_scratch.len(),
            universe
        );
        true
    }
}

/// The earliest future timer deadline for a single universe, or `None` if it has
/// none pending after `now`.
fn universe_deadline<S: BasicReceiverStorage>(
    state: &BasicUniverseState<S>,
    now: Instant,
) -> Option<Instant> {
    let mut next: Option<Instant> = None;
    let mut consider = |deadline: Instant| {
        if deadline > now {
            next = Some(match next {
                Some(current) => current.min(deadline),
                None => deadline,
            });
        }
    };

    if state.sampling {
        consider(state.sample_end);
    }
    for src in state.sources.values() {
        consider(src.packet_expiry);
    }
    for ts in state.term_sets.iter() {
        consider(ts.wait_expiry());
    }
    next
}

/// Combines two per-universe deadlines, taking the earlier of the two.
fn merge_deadline(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}
