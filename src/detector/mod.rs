//! The sACN source detector.

use core::marker::PhantomData;

use crate::log::{debug, warning};
use crate::packet::{Packet, Payload};
use crate::storage::{HeapStorage, MapLike, VecLike, coherence_check};
use crate::time::{Duration, Instant};
use crate::types::{Cid, SourceName};

mod event;

#[cfg(feature = "alloc")]
pub use event::SourceDetectorEvent;
pub use event::{
    DetectorPacketOutcome, DetectorPollOutcome, LimitExceeded, SourceDetectorEventRef,
    SourceDetectorPollEvent, SourceUpdateRef,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

// --- Storage types ----------------------------------------------------------

/// Storage types for a [`SourceDetector`].
///
/// Use [`static_storage!`](crate::static_storage!) to produce a type that
/// implements this trait for statically-allocated storage, or use
/// [`HeapStorage`] for heap-based storage.
pub trait DetectorStorage: Sized {
    /// The tracked sources, keyed by CID.
    type Sources: MapLike<Cid, DetectedSource<Self>>;
    /// One source's universe list (both the reported list and the in-progress
    /// page reassembly use a backing of this type).
    type Universes: VecLike<u16>;
    /// The reusable buffer of poll events.
    type EventBuffer: VecLike<SourceDetectorPollEvent>;
}

coherence_check! {
    /// Capacity coherence assertions for [`SourceDetector`].
    AssertCoherent<S: DetectorStorage> = {
        assert!(
            <S::EventBuffer as VecLike<SourceDetectorPollEvent>>::CAPACITY
                >= <S::Sources as MapLike<Cid, DetectedSource<S>>>::CAPACITY,
            "DetectorStorage::EventBuffer capacity must be >= Sources capacity",
        );
    }
}

#[cfg(feature = "alloc")]
impl DetectorStorage for HeapStorage {
    type Sources = alloc::collections::BTreeMap<Cid, DetectedSource<HeapStorage>>;
    type Universes = alloc::vec::Vec<u16>;
    type EventBuffer = alloc::vec::Vec<SourceDetectorPollEvent>;
}

#[cfg(not(feature = "alloc"))]
impl DetectorStorage for HeapStorage {
    type Sources = crate::storage::SortedVecMap<Cid, DetectedSource<HeapStorage>, 0>;
    type Universes = heapless::Vec<u16, 0>;
    type EventBuffer = heapless::Vec<SourceDetectorPollEvent, 0>;
}

// --- Timing constants --------------------------------------------------------

/// How long a source may go without sending a universe discovery packet before
/// it is considered expired.
///
/// E1.31 requires a source to send universe discovery packets at least every 10
/// seconds (the E131_UNIVERSE_DISCOVERY_INTERVAL). Allowing two intervals before
/// declaring a source gone tolerates a single missed announcement.
const DEFAULT_SOURCE_TIMEOUT: Duration = Duration::from_millis(20_000);

// --- Configuration -----------------------------------------------------------

/// Configuration for a [`SourceDetector`].
///
/// Construct the defaults with [`SourceDetectorConfig::new`] and adjust with the
/// `with_*` methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceDetectorConfig {
    source_timeout: Duration,
    source_limit: Option<usize>,
    universes_per_source_limit: Option<usize>,
}

impl Default for SourceDetectorConfig {
    fn default() -> Self {
        Self {
            source_timeout: DEFAULT_SOURCE_TIMEOUT,
            source_limit: None,
            universes_per_source_limit: None,
        }
    }
}

impl SourceDetectorConfig {
    /// Constructs a new [`SourceDetectorConfig`] with default settings: no source
    /// or universe limits, and the standard source-expiry timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how long a source may go silent before it is declared expired.
    ///
    /// Defaults to 20 seconds (two E1.31 universe discovery intervals), which
    /// tolerates one missed announcement. Shortening it makes expiry more
    /// responsive at the cost of false positives when an announcement is dropped.
    /// Shortening to ~10 seconds or lower is not recommended; it is likely to
    /// result in constant timeouts.
    #[must_use]
    pub fn with_source_timeout(mut self, timeout: Duration) -> Self {
        self.source_timeout = timeout;
        self
    }

    /// Limits the number of sources the detector tracks. Once the limit is
    /// reached, further new sources are dropped and a
    /// [`SourceLimitExceeded`](SourceDetectorEvent::SourceLimitExceeded) is
    /// emitted.
    ///
    /// Defaults to no limit.
    #[must_use]
    pub fn with_source_limit(mut self, limit: usize) -> Self {
        self.source_limit = Some(limit);
        self
    }

    /// Limits the number of universes the detector records for any one source.
    /// A source advertising more than this has its list truncated and a
    /// [`UniverseLimitExceeded`](SourceDetectorEvent::UniverseLimitExceeded) is
    /// emitted.
    ///
    /// Defaults to no limit.
    #[must_use]
    pub fn with_universes_per_source_limit(mut self, limit: usize) -> Self {
        self.universes_per_source_limit = Some(limit);
        self
    }
}

// --- Per-source state --------------------------------------------------------

/// State the detector tracks for one discovered source.
#[doc(hidden)]
#[derive(Debug)]
pub struct DetectedSource<S: DetectorStorage> {
    /// The source's most recent name.
    name: SourceName,
    /// The current, complete universe list last reported to the application.
    /// Empty until a full page sequence has been reassembled.
    universes: S::Universes,
    /// Universes accumulated for the page sequence currently being reassembled.
    partial: S::Universes,
    /// The next page number expected to continue the in-progress reassembly.
    next_page: u8,
    /// When this source expires if it stays silent.
    expiry: Instant,
    /// Whether a universe-limit-exceeded notification has already been emitted
    /// for this source and should be suppressed until its universe count drops.
    suppress_universe_limit_exceeded: bool,
}

impl<S: DetectorStorage> DetectedSource<S> {
    fn new(name: &str, expiry: Instant) -> Self {
        Self {
            name: SourceName::from_str_lossy(name),
            universes: S::Universes::default(),
            partial: S::Universes::default(),
            next_page: 0,
            expiry,
            suppress_universe_limit_exceeded: false,
        }
    }

    /// Updates the stored name if the source now reports a different one.
    fn set_name(&mut self, name: &str) {
        if self.name.as_str() != name {
            self.name.set(name);
        }
    }

    /// Discards any in-progress page reassembly.
    fn reset_reassembly(&mut self) {
        self.partial.clear();
        self.next_page = 0;
    }
}

/// Returns whether `list` is strictly ascending (and thus also free of
/// duplicates), as E1.31 requires of a discovery universe list.
fn is_ascending(list: &[u16]) -> bool {
    list.windows(2).all(|w| w[0] < w[1])
}

// --- The detector ------------------------------------------------------------

/// A sACN source detector state machine.
///
/// `SourceDetector` consumes universe discovery packets - the periodic
/// announcements sources make on the reserved discovery universe - and tracks
/// which sources are present and which universes each one transmits. It emits:
///
/// - [`SourceUpdated`](SourceDetectorEvent::SourceUpdated) when a source is first
///   seen or changes its universe list,
/// - [`SourceExpired`](SourceDetectorEvent::SourceExpired) when a source goes
///   silent, and
/// - [`SourceLimitExceeded`](SourceDetectorEvent::SourceLimitExceeded) or
///   [`UniverseLimitExceeded`](SourceDetectorEvent::UniverseLimitExceeded) when a
///   configured source or per-source universe limit is hit.
///
/// # Page reassembly
///
/// A source advertises its universes across one or more discovery *pages*.
/// The detector reassembles a source's list from a run of consecutive pages,
/// starting at page 0 and ending at the last page; pages are assumed to arrive
/// in order, and a page arriving out of sequence restarts the reassembly. A
/// source is reported updated only once a complete list has been reassembled
/// and only when that list differs from the one last reported, so a stable
/// source announcing the same universes repeatedly produces a single update.
/// The completed list must be in ascending order; a non-conformant unordered
/// list is dropped rather than reported.
///
/// # Driving the state machine
///
/// The detector holds no socket and no clock. The caller (an adapter, or a
/// test) drives it:
///
/// - [`handle_packet`](Self::handle_packet) feeds in a parsed packet and returns
///   the events it produced, borrowing the detector's own storage.
/// - [`poll`](Self::poll) advances time, expiring silent sources, and returns
///   the events produced plus the next instant at which calling it again could
///   change something.
///
/// Non-discovery packets are ignored, so a caller may feed it every packet it
/// receives on the discovery universe without pre-filtering.
///
/// ```
/// use sacn::{SourceDetector, SourceDetectorConfig};
/// use sacn::detector::SourceDetectorPollEvent;
/// use sacn::packet::Packet;
/// use sacn::time::Instant;
///
/// let mut detector = SourceDetector::new(SourceDetectorConfig::new());
/// let now = Instant::EPOCH;
///
/// // Feed in a packet received on the discovery universe.
/// # let datagram = doctest_helper::discovery_datagram();
/// let packet = Packet::parse(&datagram).unwrap();
/// let outcome = detector.handle_packet(now, &packet);
/// if let Some(update) = outcome.updated {
///     // a source appeared or changed the universe list it advertises
///     let _ = (update.cid, update.name, update.universes);
/// }
///
/// // Advance time to expire sources that have stopped announcing themselves.
/// let poll = detector.poll(now);
/// for event in poll.events() {
///     match event {
///         SourceDetectorPollEvent::SourceExpired { cid, name } => { let _ = (cid, name); }
///         _ => {}
///     }
/// }
/// let _next = poll.deadline;
/// # mod doctest_helper {
/// #     use sacn::{Source, SourceConfig, UniverseConfig, Cid, Universe, Route};
/// #     use sacn::time::Instant;
/// #     pub fn discovery_datagram() -> Vec<u8> {
/// #         let mut src = Source::new(SourceConfig::new(Cid::from_bytes([7; 16]), "src"));
/// #         let u = Universe::new(1).unwrap();
/// #         src.add_universe(UniverseConfig::new(u)).unwrap();
/// #         src.update_levels(u, &[255, 128, 0]);
/// #         let mut poll = src.poll(Instant::EPOCH);
/// #         loop {
/// #             let tx = poll.next_transmission().unwrap();
/// #             if matches!(tx.route, Route::Discovery) {
/// #                 return tx.data.to_vec();
/// #             }
/// #         }
/// #     }
/// # }
/// ```
#[derive(Debug)]
pub struct SourceDetector<S: DetectorStorage = HeapStorage> {
    core: SourceDetectorCore<S>,
    store: SourceDetectorResources<S>,
}

/// The sACN source detector state machine: the discovery-tracking logic,
/// separated from its working memory.
///
/// [`SourceDetector`] contains one of these as well as a
/// [`SourceDetectorResources`]. Usually, just using [`SourceDetector`] is the
/// right choice. Use this type alongside [`SourceDetectorResources`] if you need
/// maximum control of your memory layout; [`SourceDetectorResources`] contains
/// all of the bulk memory associated with a detector, and can be
/// const-initialized statically.
///
/// This has all the same functionality as [`SourceDetector`]; the only difference
/// is that each method takes a mutable reference to a separate
/// [`SourceDetectorResources`]. Each [`SourceDetectorCore`] should be associated
/// with exactly one [`SourceDetectorResources`] and you should pass the same
/// [`SourceDetectorResources`] instance to every call to a [`SourceDetectorCore`]
/// method.
#[derive(Debug)]
pub struct SourceDetectorCore<S: DetectorStorage = HeapStorage> {
    config: SourceDetectorConfig,
    _marker: PhantomData<S>,
}

/// The mutable working memory a [`SourceDetectorCore`] operates on.
///
/// This struct holds everything about a detector that scales with the number of
/// tracked sources and their universe lists, so it is the potentially large
/// allocation. It can be constructed in a const expression with
/// statically-allocated storage (see below).
///
/// Most users should just use [`SourceDetector`] rather than
/// [`SourceDetectorCore`] and [`SourceDetectorResources`].
///
/// To construct:
///
/// - **Heap:** construct with [`SourceDetectorResources::default`].
/// - **Fixed-capacity:** use the [`static_storage!`](crate::static_storage!)
///   macro, which emits a `const fn` `detector_resources()` returning an empty
///   `SourceDetectorResources`, suitable for static allocation in a const
///   context.
#[derive(Debug)]
pub struct SourceDetectorResources<S: DetectorStorage = HeapStorage> {
    sources: S::Sources,
    /// Whether a source-limit-exceeded notification has already been emitted and
    /// should be suppressed until a tracked source expires.
    suppress_source_limit_exceeded: bool,
    event_buffer: S::EventBuffer,
}

#[cfg(feature = "alloc")]
impl SourceDetector<HeapStorage> {
    /// Creates a heap-backed detector with the given configuration.
    ///
    /// For a fixed-capacity detector, construct with
    /// `SourceDetector::<Caps>::with_config(config)`, constructing the
    /// capacity policy `Caps` using [`static_storage!`](crate::static_storage!).
    pub fn new(config: SourceDetectorConfig) -> Self {
        Self::with_config(config)
    }
}

impl<S: DetectorStorage> SourceDetector<S> {
    /// Creates a detector with the given configuration, backed by the storage
    /// policy `S`.
    pub fn with_config(config: SourceDetectorConfig) -> Self {
        Self {
            core: SourceDetectorCore::with_config(config),
            store: SourceDetectorResources::default(),
        }
    }

    /// Get the config with which this detector was created.
    pub fn config(&self) -> &SourceDetectorConfig {
        self.core.config()
    }

    /// Feeds in a parsed packet, returning the events it produced.
    ///
    /// Only universe discovery packets are acted upon; any other packet is
    /// ignored and yields an empty outcome. A discovery page refreshes its
    /// source's expiry, is folded into the source's page reassembly, and - if it
    /// completes a universe list that differs from the last one reported -
    /// produces a [`SourceUpdated`](SourceDetectorEvent::SourceUpdated). The
    /// outcome borrows the detector's storage.
    pub fn handle_packet<'d>(
        &'d mut self,
        now: Instant,
        packet: &Packet<'_>,
    ) -> DetectorPacketOutcome<'d> {
        self.core.handle_packet(&mut self.store, now, packet)
    }

    /// Advances time to `now`, expiring any source that has been silent past its
    /// timeout, and enqueuing a
    /// [`SourceExpired`](SourceDetectorPollEvent::SourceExpired) for each.
    ///
    /// Returns the earliest instant at which calling `poll` again could produce a
    /// different result (the next source-expiry deadline), or `None` if no
    /// sources are being tracked. Calling it earlier is harmless; calling it
    /// later only delays notifications.
    pub fn poll(&mut self, now: Instant) -> DetectorPollOutcome<'_> {
        self.core.poll(&mut self.store, now)
    }

    /// The earliest source-expiry deadline, or `None` if no sources are tracked.
    #[cfg(test)]
    fn next_deadline(&self) -> Option<Instant> {
        self.store.next_deadline()
    }
}

impl<S: DetectorStorage> SourceDetectorResources<S> {
    /// Assembles resources from already-constructed (empty) collections.
    ///
    /// Not used directly; used only from [`static_storage!`](crate::static_storage!)
    /// or [`Default::default()`].
    #[doc(hidden)]
    pub const fn from_parts(sources: S::Sources, event_buffer: S::EventBuffer) -> Self {
        Self {
            sources,
            suppress_source_limit_exceeded: false,
            event_buffer,
        }
    }

    /// The earliest source-expiry deadline, or `None` if no sources are tracked.
    fn next_deadline(&self) -> Option<Instant> {
        self.sources.values().map(|source| source.expiry).min()
    }

    /// The expiry events produced by the most recent poll.
    #[cfg(feature = "embassy")]
    fn poll_events(&self) -> &[SourceDetectorPollEvent] {
        self.event_buffer.as_slice()
    }
}

impl<S: DetectorStorage> Default for SourceDetectorResources<S> {
    /// Empty resources with empty collections. For a fixed-capacity policy this
    /// builds the value at runtime; prefer the macro-generated
    /// `detector_resources()` `const fn` to place it in static memory without a
    /// stack copy.
    fn default() -> Self {
        Self::from_parts(S::Sources::default(), S::EventBuffer::default())
    }
}

impl<S: DetectorStorage> SourceDetectorCore<S> {
    /// Creates a detector controller with the given configuration, backed by the
    /// storage policy `S`.
    ///
    /// The controller holds only the configuration; its working memory lives in
    /// a separate [`SourceDetectorResources`] passed to each method. Most users
    /// should use [`SourceDetector`] instead of [`SourceDetectorCore`] and
    /// [`SourceDetectorResources`].
    pub fn with_config(config: SourceDetectorConfig) -> Self {
        let () = AssertCoherent::<S>::CHECK;
        Self {
            config,
            _marker: PhantomData,
        }
    }

    /// Get the config with which this detector was created.
    pub fn config(&self) -> &SourceDetectorConfig {
        &self.config
    }

    /// Feeds in a parsed packet, returning the events it produced.
    ///
    /// See [`SourceDetector::handle_packet`].
    pub fn handle_packet<'d>(
        &self,
        store: &'d mut SourceDetectorResources<S>,
        now: Instant,
        packet: &Packet<'_>,
    ) -> DetectorPacketOutcome<'d> {
        let Payload::UniverseDiscovery(disco) = &packet.payload else {
            return DetectorPacketOutcome::IGNORED;
        };

        let cid = packet.cid;
        let mut limit_exceeded = None;
        let mut updated_cid = None;

        {
            // Find or add the source, enforcing the source limit for new ones.
            if !store.sources.contains_key(&cid) {
                let expiry = now.saturating_add(self.config.source_timeout);
                let refused = self
                    .config
                    .source_limit
                    .is_some_and(|max| store.sources.len() >= max)
                    || store
                        .sources
                        .upsert(cid, DetectedSource::new(disco.source_name, expiry))
                        .is_err();
                if refused {
                    if !store.suppress_source_limit_exceeded {
                        store.suppress_source_limit_exceeded = true;
                        limit_exceeded = Some(LimitExceeded::Source);
                        warning!("source detector source limit exceeded");
                    }
                    return DetectorPacketOutcome {
                        updated: None,
                        limit_exceeded,
                    };
                }
                debug!("source detector tracking new source");
            }

            let config = &self.config;
            let source = store.sources.get_mut(&cid).expect("source just ensured");
            source.set_name(disco.source_name);
            // Any parseable page from the source refreshes its expiry.
            source.expiry = now.saturating_add(config.source_timeout);

            let page = disco.page;
            let last_page = disco.last_page;

            // Determine whether this page continues a valid reassembly.
            let in_sequence = if page > last_page {
                // A page index past the last page is malformed; drop reassembly.
                source.reset_reassembly();
                false
            } else if page == 0 {
                // A first page always starts a fresh reassembly.
                source.reset_reassembly();
                true
            } else if page == source.next_page {
                true
            } else {
                // Out of order: restart and wait for the next page 0.
                source.reset_reassembly();
                false
            };

            if in_sequence {
                // Append this page's universes, honoring the per-source limit and
                // the backing's capacity.
                let mut overflowed = false;
                for universe in disco.universes.iter() {
                    let at_limit = config
                        .universes_per_source_limit
                        .is_some_and(|max| source.partial.len() >= max);
                    if at_limit || source.partial.push(universe).is_err() {
                        overflowed = true;
                        break;
                    }
                }
                if overflowed && !source.suppress_universe_limit_exceeded {
                    source.suppress_universe_limit_exceeded = true;
                    limit_exceeded = Some(LimitExceeded::Universe { cid });
                    warning!("source detector universe limit exceeded for a source");
                }

                if page < last_page {
                    // More pages to come.
                    source.next_page = page + 1;
                } else {
                    // The last page completes the reassembly.
                    source.next_page = 0;
                    if is_ascending(source.partial.as_slice())
                        && source.partial.as_slice() != source.universes.as_slice()
                    {
                        // A shrinking list frees room, so allow a fresh
                        // universe-limit notification later.
                        if source.partial.len() < source.universes.len() {
                            source.suppress_universe_limit_exceeded = false;
                        }
                        core::mem::swap(&mut source.universes, &mut source.partial);
                        updated_cid = Some(cid);
                        debug!("source detector reassembled a changed universe list");
                    }
                    source.partial.clear();
                }
            }
        }

        let updated = updated_cid.map(|cid| {
            let source = store.sources.get(&cid).expect("updated source present");
            SourceUpdateRef {
                cid,
                name: source.name.as_str(),
                universes: source.universes.as_slice(),
            }
        });
        DetectorPacketOutcome {
            updated,
            limit_exceeded,
        }
    }

    /// Advances time to `now`, expiring any source that has been silent past its
    /// timeout.
    ///
    /// See [`SourceDetector::poll`].
    pub fn poll<'a>(
        &self,
        store: &'a mut SourceDetectorResources<S>,
        now: Instant,
    ) -> DetectorPollOutcome<'a> {
        store.event_buffer.clear();

        // A source expires once it has been silent past its timeout. Retain the
        // live sources, emitting an event for each expired one as it is dropped.
        let SourceDetectorResources {
            sources,
            event_buffer,
            suppress_source_limit_exceeded,
        } = &mut *store;
        sources.retain(|&cid, source| {
            if now >= source.expiry {
                // A tracked source left, so allow a fresh source-limit
                // notification.
                *suppress_source_limit_exceeded = false;
                debug!("source detector expired a silent source");
                event_buffer.push_expect(SourceDetectorPollEvent::SourceExpired {
                    cid,
                    name: source.name.clone(),
                });
                false
            } else {
                true
            }
        });

        DetectorPollOutcome::new(store.next_deadline(), store.event_buffer.as_slice())
    }

    /// The expiry events produced by the most recent
    /// [`poll`](SourceDetectorCore::poll), still buffered here until the next
    /// poll clears them. Valid after the [`DetectorPollOutcome`] has been
    /// dropped.
    #[cfg(feature = "embassy")]
    pub(crate) fn poll_events<'a>(
        &self,
        store: &'a SourceDetectorResources<S>,
    ) -> &'a [SourceDetectorPollEvent] {
        store.poll_events()
    }
}
