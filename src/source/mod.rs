//! The sACN send path: a source that transmits DMX data on one or more
//! universes.

#[cfg(test)]
mod property_tests;
#[cfg(test)]
mod tests;
mod transmit;

use crate::error::{CodecError, CodecErrorKind, Error};
use crate::log::debug;
use crate::packet::{
    DataPacket, Packet, Payload, SyncPacket, UniverseDiscoveryPacket, UniverseList,
};
use crate::packet::{
    DMX_NULL_START_CODE, MAX_PACKET_SIZE, MAX_SLOTS, MAX_UNIVERSES_PER_PAGE, PAP_START_CODE,
};
use crate::storage::{coherence_check, HeapStorage, MapLike, VecLike};
use crate::time::{Duration, Instant};
use crate::types::{Cid, Priority, SequenceNumber, SourceName, StartCode, Universe};

pub use transmit::{Route, SourcePoll, Transmission};

// --- Storage types ----------------------------------------------------------

/// Storage types for a [`Source`].
pub trait SourceStorage: Sized {
    /// The universes the source transmits on, keyed by universe number.
    type TxUniverses: MapLike<Universe, TxUniverseState>;
    /// Per-synchronization-group state, keyed by sync universe.
    type SyncGroups: MapLike<Universe, SyncGroupState>;
    /// The queue of sends planned by the most recent poll.
    type Pending: VecLike<Pending>;
    /// The universes physically dropped by the most recent poll.
    type Removed: VecLike<Universe>;
}

coherence_check! {
    /// Capacity coherence assertions for [`Source`].
    ///
    /// A single poll can queue, per universe, a level and a per-address-priority
    /// packet, plus one synchronization packet per group (at most one group per
    /// universe), plus one discovery page per 512 universes.
    AssertCoherent<S: SourceStorage> = {
        let universes = <S::TxUniverses as MapLike<Universe, TxUniverseState>>::CAPACITY;
        assert!(
            <S::SyncGroups as MapLike<Universe, SyncGroupState>>::CAPACITY >= universes,
            "SourceStorage::SyncGroups capacity must be >= TxUniverses capacity",
        );
        assert!(
            <S::Removed as VecLike<Universe>>::CAPACITY >= universes,
            "SourceStorage::Removed capacity must be >= TxUniverses capacity",
        );
        assert!(
            <S::Pending as VecLike<Pending>>::CAPACITY
                >= universes
                    .saturating_mul(3)
                    .saturating_add(universes / 512)
                    .saturating_add(1),
            "SourceStorage::Pending capacity must be >= 3 * TxUniverses + pages per poll",
        );
    }
}

#[cfg(feature = "alloc")]
impl SourceStorage for HeapStorage {
    type TxUniverses = alloc::collections::BTreeMap<Universe, TxUniverseState>;
    type SyncGroups = alloc::collections::BTreeMap<Universe, SyncGroupState>;
    type Pending = alloc::vec::Vec<Pending>;
    type Removed = alloc::vec::Vec<Universe>;
}

#[cfg(not(feature = "alloc"))]
impl SourceStorage for HeapStorage {
    type TxUniverses = crate::storage::SortedVecMap<Universe, TxUniverseState, 0>;
    type SyncGroups = crate::storage::SortedVecMap<Universe, SyncGroupState, 0>;
    type Pending = heapless::Vec<Pending, 0>;
    type Removed = heapless::Vec<Universe, 0>;
}

// --- Timing constants -------------------------------------------------------

/// The DMX refresh interval; matches the maximum DMX rate (~44 Hz).
const TICK_INTERVAL: Duration = Duration::from_millis(22);

/// The default interval between keep-alive level packets during transmission
/// suppression.
const DEFAULT_KEEP_ALIVE: Duration = Duration::from_millis(900);

/// The default interval between keep-alive per-address-priority packets during
/// transmission suppression.
const DEFAULT_PAP_KEEP_ALIVE: Duration = Duration::from_millis(900);

/// The minimum keep-alive interval permitted by E1.31 (§6.6.2).
const MIN_KEEP_ALIVE: Duration = Duration::from_millis(800);

/// The maximum keep-alive interval permitted by E1.31 (§6.6.2).
const MAX_KEEP_ALIVE: Duration = Duration::from_millis(1000);

/// How often the source announces its universes on the discovery universe
/// (E1.31 Appendix A: `E131_UNIVERSE_DISCOVERY_INTERVAL`).
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);

/// The number of packets sent at the full DMX rate after a change, before
/// transmission is suppressed to the keep-alive rate.
const PRE_SUPPRESSION_PACKETS: u8 = 3;

/// The number of stream-terminated packets sent when a universe is removed
/// (E1.31 §6.2.6).
const TERMINATION_PACKETS: u8 = 3;

// --- Configuration -----------------------------------------------------------

/// Configuration for a [`Source`].
///
/// Construct with [`SourceConfig::new`] and adjust with the `with_*` methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    cid: Cid,
    name: SourceName,
    keep_alive: Duration,
    pap_keep_alive: Duration,
    sync_delay: Duration,
}

impl SourceConfig {
    /// Creates a configuration for a source with the given CID and name.
    ///
    /// The name is a human-readable UTF-8 string; only its first 63 bytes are
    /// transmitted, truncated at a character boundary.
    pub fn new(cid: Cid, name: impl Into<SourceName>) -> Self {
        Self {
            cid,
            name: name.into(),
            keep_alive: DEFAULT_KEEP_ALIVE,
            pap_keep_alive: DEFAULT_PAP_KEEP_ALIVE,
            sync_delay: Duration::from_millis(5),
        }
    }

    /// Sets the interval between keep-alive level packets sent while a universe's
    /// data is unchanged. Defaults to 900ms. Clamped to the E1.31 range of
    /// between 800ms and 1000ms.
    #[must_use]
    pub fn with_keep_alive(mut self, interval: Duration) -> Self {
        self.keep_alive = interval.clamp(MIN_KEEP_ALIVE, MAX_KEEP_ALIVE);
        self
    }

    /// Sets the interval between keep-alive per-address-priority packets sent
    /// while a universe's data is unchanged. Defaults to 900ms. Clamped to the
    /// E1.31 range of between 800ms and 1000ms.
    #[must_use]
    pub fn with_pap_keep_alive(mut self, interval: Duration) -> Self {
        self.pap_keep_alive = interval.clamp(MIN_KEEP_ALIVE, MAX_KEEP_ALIVE);
        self
    }

    /// Sets the delay between a synchronized universe's data packets and the
    /// synchronization packet that releases them. Default 5ms.
    #[must_use]
    pub fn with_sync_delay(mut self, delay: Duration) -> Self {
        self.sync_delay = delay;
        self
    }

    /// The source's CID.
    pub fn cid(&self) -> Cid {
        self.cid
    }

    /// The source's name.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }
}

/// How receivers should behave if a universe's synchronization stream is lost.
/// This maps to the E1.31 Force_Synchronization flag (§6.2.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnSyncLoss {
    /// Freeze on the last synchronized frame, ignoring data until sync resumes
    /// (`Force_Synchronization = 0`).
    HoldLastLook,
    /// Fall back to live, unsynchronized streaming (`Force_Synchronization = 1`).
    RevertToLive,
}

impl OnSyncLoss {
    /// The E1.31 Force_Synchronization bit value for this policy.
    fn force_sync(self) -> bool {
        match self {
            OnSyncLoss::HoldLastLook => false,
            OnSyncLoss::RevertToLive => true,
        }
    }
}

/// Configuration for a universe added to a [`Source`].
///
/// Construct with [`UniverseConfig::new`] and adjust with the `with_*` methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UniverseConfig {
    universe: Universe,
    priority: Priority,
    preview: bool,
    sync_universe: u16,
    force_sync: bool,
}

impl UniverseConfig {
    /// Creates a configuration for `universe` with default settings: the default
    /// priority, preview off, and no synchronization.
    pub fn new(universe: Universe) -> Self {
        Self {
            universe,
            priority: Priority::DEFAULT,
            preview: false,
            sync_universe: 0,
            force_sync: false,
        }
    }

    /// Sets the universe priority sent in each packet. Defaults to
    /// [`Priority::DEFAULT`].
    #[must_use]
    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Sets the preview-data flag. When set, receivers treat the data as
    /// intended for visualization only. Defaults to `false`.
    #[must_use]
    pub fn with_preview(mut self, preview: bool) -> Self {
        self.preview = preview;
        self
    }

    /// Synchronizes this universe on `sync_universe`: its data packets advertise
    /// that address, so receivers hold the data until a synchronization packet
    /// arrives there, and the source emits those sync packets after the
    /// universe's data. `on_loss` sets how receivers behave if the sync stream
    /// dies.
    ///
    /// Synchronization is disabled by default. Call this with a nonzero universe
    /// to enable it.
    #[must_use]
    pub fn synchronized_on(mut self, sync_universe: Universe, on_loss: OnSyncLoss) -> Self {
        self.sync_universe = sync_universe.get();
        self.force_sync = on_loss.force_sync();
        self
    }

    /// The universe number.
    pub fn universe(&self) -> Universe {
        self.universe
    }

    /// The configured synchronization universe as a raw `u16`, or `0` if the
    /// universe is not synchronized.
    #[allow(dead_code)]
    pub(crate) fn sync_universe(&self) -> u16 {
        self.sync_universe
    }
}

// --- Per-universe state ------------------------------------------------------

/// A universe's DMX slot data (levels or per-address priorities): an inline,
/// fixed-capacity buffer which holds up to [`MAX_SLOTS`] bytes.
///
/// `len` is used as an overall presence marker for the levels or PAPs stored
/// here.
#[derive(Debug)]
struct SlotBuffer {
    data: [u8; MAX_SLOTS],
    len: Option<u16>,
}

impl SlotBuffer {
    const fn new() -> Self {
        Self {
            data: [0; MAX_SLOTS],
            len: None,
        }
    }

    fn is_set(&self) -> bool {
        self.len.is_some()
    }

    fn get(&self) -> Option<&[u8]> {
        self.len.map(|n| &self.data[..n as usize])
    }

    fn set(&mut self, values: &[u8]) {
        let n = values.len();
        self.data[..n].copy_from_slice(values);
        self.len = Some(n as u16);
    }

    fn clear(&mut self) {
        self.len = None;
    }
}

/// State the source tracks for one universe it transmits on.
#[doc(hidden)]
#[derive(Debug)]
pub struct TxUniverseState {
    universe: Universe,
    priority: u8,
    preview: bool,
    sync_universe: u16,
    force_sync: bool,

    /// The current NULL-start-code levels, unset until levels are first set.
    levels: SlotBuffer,
    /// The current per-address priorities, unset if not sending PAP.
    pap: SlotBuffer,

    next_seq: SequenceNumber,

    level_pre_suppression: u8,
    pap_pre_suppression: u8,
    level_next_send: Instant,
    pap_next_send: Instant,
    level_last_send: Option<Instant>,
    pap_last_send: Option<Instant>,

    terminating: bool,
    term_count: u8,
    term_next_send: Instant,
}

impl TxUniverseState {
    fn new(config: UniverseConfig) -> Self {
        Self {
            universe: config.universe,
            priority: config.priority.get(),
            preview: config.preview,
            sync_universe: config.sync_universe,
            force_sync: config.force_sync,
            levels: SlotBuffer::new(),
            pap: SlotBuffer::new(),
            next_seq: SequenceNumber::new(0),
            level_pre_suppression: 0,
            pap_pre_suppression: 0,
            level_next_send: Instant::EPOCH,
            pap_next_send: Instant::EPOCH,
            level_last_send: None,
            pap_last_send: None,
            terminating: false,
            term_count: 0,
            term_next_send: Instant::EPOCH,
        }
    }

    fn has_levels(&self) -> bool {
        self.levels.is_set()
    }

    fn has_pap(&self) -> bool {
        self.pap.is_set()
    }

    /// Whether this universe is announced in universe discovery. E1.31
    /// requires a source to enumerate every universe it is actively transmitting
    /// data on, so this is exactly the universes that have level data.
    fn in_discovery(&self) -> bool {
        self.has_levels()
    }

    /// Resets level transmission suppression so the levels are re-sent and a fresh
    /// pre-suppression burst begins, scheduling the re-send as soon as the minimum
    /// inter-packet spacing ([`TICK_INTERVAL`]) allows.
    fn reset_levels(&mut self) {
        self.level_pre_suppression = 0;
        self.level_next_send = next_send_after(self.level_last_send);
    }

    /// Resets per-address-priority transmission suppression, similar to
    /// [`reset_levels`](Self::reset_levels).
    fn reset_pap(&mut self) {
        self.pap_pre_suppression = 0;
        self.pap_next_send = next_send_after(self.pap_last_send);
    }

    /// Cancels a pending termination if the universe is being kept alive (its
    /// data was changed before the termination sequence finished).
    fn cancel_termination(&mut self) {
        if self.terminating {
            self.terminating = false;
            self.term_count = 0;
        }
    }

    /// Whether the universe has finished its termination sequence and is awaiting
    /// removal. It is logically gone (excluded from queries and scheduling) but
    /// kept in the table for one more poll so its already-queued final
    /// termination packet stays resolvable while the poll is drained.
    ///
    /// A terminating universe always has levels: one without data is dropped
    /// immediately by [`remove_universe`](Source::remove_universe) instead.
    fn is_finished(&self) -> bool {
        self.terminating && self.term_count >= TERMINATION_PACKETS
    }

    /// Whether this universe is actively advertising synchronization group
    /// `sync_universe`, and is thus a live member of it. A terminating universe
    /// advertises `sync_address = 0`, so it has already left its group.
    fn syncs_on(&self, sync_universe: u16) -> bool {
        !self.terminating && self.sync_universe == sync_universe
    }

    /// Returns this universe's next sequence number, advancing the counter. The
    /// same counter is shared across level, per-address-priority and ad-hoc
    /// [`send_now`](Source::send_now) packets, so receivers see one monotonic
    /// sequence per universe.
    fn take_seq(&mut self) -> SequenceNumber {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.next();
        seq
    }
}

// --- A queued transmission ---------------------------------------------------

/// A single queued send: a description of the packet to serialize plus where
/// it is going. The bytes are produced on demand by
/// [`Source::pull_transmission`], so a queue of these holds no packet payloads.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Pending {
    emission: Emission,
    route: Route,
}

/// A lightweight description of one packet the source will serialize on demand.
/// Carries no payload bytes: the levels, per-address priorities and discovery
/// list are read from the source's live state when the packet is serialized.
#[derive(Clone, Copy, Debug)]
enum Emission {
    /// A NULL-start-code level packet for a universe. `terminated` sets the
    /// stream-terminated flag (the termination sequence).
    Level {
        universe: Universe,
        terminated: bool,
    },
    /// A per-address-priority (`0xDD`) packet for a universe.
    Pap { universe: Universe },
    /// One universe-discovery page: `page` of `last_page`. The page's universe
    /// list is rebuilt from the source's live universe set when the packet is
    /// serialized.
    Discovery { page: u8, last_page: u8 },
    /// A universe-synchronization packet on `sync_universe`. The sequence
    /// number is a separate sequence tracked for sync packets only.
    Sync {
        sync_universe: Universe,
        seq: SequenceNumber,
    },
}

// --- Per-sync-group state ----------------------------------------------------

/// A sync group is the implicit set of data universes sharing a sync
/// address; this holds the timing and sequencing the group needs.
#[doc(hidden)]
#[derive(Debug)]
pub struct SyncGroupState {
    /// The group's own sequence counter, independent of any member universe's.
    next_seq: SequenceNumber,
    /// When the next synchronization packet is due, or `None` if no sync is
    /// currently armed for the group.
    pending_deadline: Option<Instant>,
}

impl SyncGroupState {
    fn new() -> Self {
        Self {
            next_seq: SequenceNumber::new(0),
            pending_deadline: None,
        }
    }

    /// Returns the group's next sequence number, advancing the counter.
    fn take_seq(&mut self) -> SequenceNumber {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.next();
        seq
    }
}

// --- The source --------------------------------------------------------------

/// A sACN source state machine: the send path.
///
/// `Source` transmits sACN data on one or more universes. It encodes the
/// behavior an interoperable and robust sACN transmitter needs:
///
/// - **Transmission suppression.** When a universe's Null START Code (DMX)
///   data changes, the new values are sent in a short burst of packets at the
///   DMX rate, after which transmission is suppressed down to a low-rate
///   stream of identical keep-alive packets (every 900ms). This behavior is
///   also applied to alternate START Code data, including per-address
///   priority, although this is not required by the standard.
/// - **Per-universe sequence numbering**.
/// - **Per-address priority (PAP, `0xDD`).** A universe can carry per-slot
///   priorities alongside its levels; a slot's per-address priority of zero
///   means "not sourcing this slot", and receivers ignore that slot's level.
/// - **Termination.** Removing a universe sends the E1.31 three-packet
///   stream-terminated sequence before the universe is dropped.
/// - **Universe discovery.** The source periodically announces the universes it
///   is transmitting on the reserved discovery universe.
/// - **Ad-hoc START codes.** [`send_now`](Self::send_now) transmits a one-shot
///   packet with an arbitrary START code (application data outside the managed
///   NULL and per-address-priority streams), with no rate limiting or
///   suppression.
///
/// # Driving the state machine
///
/// The source holds no socket and no clock; the caller (an adapter, or a test)
/// drives it:
///
/// - [`add_universe`](Self::add_universe) /
///   [`remove_universe`](Self::remove_universe) register and tear down
///   universes.
/// - [`update_levels`](Self::update_levels) and the other `update_*` / `set_*`
///   methods change the data being transmitted.
/// - [`poll`](Self::poll) advances time and returns the packets due to be sent
///   now, plus the next instant at which polling could produce more.
///
/// ```
/// use sacn::{Source, SourceConfig, UniverseConfig, Cid, Universe};
/// use sacn::time::Instant;
///
/// // Build a source and start transmitting one universe.
/// let cid = Cid::from_bytes([0x11; 16]);
/// let mut source = Source::new(SourceConfig::new(cid, "My Source"));
/// let universe = Universe::new(1).unwrap();
/// source.add_universe(UniverseConfig::new(universe)).unwrap();
/// source.update_levels(universe, &[255, 128, 0]);
///
/// // Poll to collect the packets due now, then drain them onto the wire. An
/// // adapter would expand each `route` into concrete multicast/unicast
/// // destinations and send the bytes; here we just count them.
/// let now = Instant::EPOCH;
/// let mut poll = source.poll(now);
/// let mut sent = 0;
/// while let Some(tx) = poll.next_transmission() {
///     // send `tx.data` to the destinations implied by `tx.route`
///     let _ = (tx.route, tx.data);
///     sent += 1;
/// }
/// assert!(sent > 0);
///
/// // `deadline` is the earliest instant another poll could produce more (a
/// // keep-alive, the next suppression packet, or a discovery announcement).
/// let _next = poll.deadline;
/// ```
///
/// # Synchronization
///
/// A universe may be [synchronized](UniverseConfig::synchronized_on) on a
/// synchronization universe: its data packets advertise that address so
/// receivers hold the data, and the source emits a
/// [synchronization packet](Route::Sync) after the group's data to release the
/// held frame atomically.
#[derive(Debug)]
pub struct Source<S: SourceStorage = HeapStorage> {
    config: SourceConfig,
    universes: S::TxUniverses,
    /// Per-synchronization-group state, keyed by sync universe. Groups are
    /// implicitly derived from universes sharing a sync address, and have
    /// some timing and state to track separately.
    sync_groups: S::SyncGroups,
    /// The queue of sends planned by the most recent poll.
    pending: S::Pending,
    /// How far the drain has advanced through `pending`. A transmission is
    /// consumed (the cursor advances past it) the moment it is handed out by
    /// [`SourcePoll::next_transmission`]; the caller is then responsible for
    /// getting it onto the wire. Un-handed-out entries survive an abandoned
    /// drain: the next [`poll`](Self::poll) resumes from here rather than
    /// rebuilding.
    cursor: usize,
    /// The single packet buffer every transmission is serialized into, reused
    /// across the whole drain. Holds the most recently serialized packet until
    /// the next serialization overwrites it; until then, it can be re-read with
    /// [`current_packet`](Self::current_packet).
    packet_buf: [u8; MAX_PACKET_SIZE],
    /// The length of the packet currently held in `packet_buf`.
    packet_len: usize,
    /// Universes physically dropped by the most recent poll, reported to the
    /// adapter via [`SourcePoll::removed`]. Rebuilt each poll.
    removed: S::Removed,
    /// When the next discovery packet should be sent.
    discovery_next_send: Instant,
}

#[cfg(feature = "alloc")]
impl Source<HeapStorage> {
    /// Creates a heap-backed source with the given configuration. It transmits
    /// nothing until a universe is added with
    /// [`add_universe`](Self::add_universe) and given data with
    /// [`update_levels`](Self::update_levels).
    ///
    /// For a fixed-capacity source, construct with
    /// `Source::<Caps>::with_config(config)` using a policy from
    /// [`static_storage!`](crate::static_storage!).
    pub fn new(config: SourceConfig) -> Self {
        Self::with_config(config)
    }
}

impl<S: SourceStorage> Source<S> {
    /// Creates a source with the given configuration, backed by the storage
    /// policy `S`. It transmits nothing until a universe is added with
    /// [`add_universe`](Self::add_universe) and given data with
    /// [`update_levels`](Self::update_levels).
    pub fn with_config(config: SourceConfig) -> Self {
        let () = AssertCoherent::<S>::CHECK;
        Self {
            config,
            universes: S::TxUniverses::default(),
            sync_groups: S::SyncGroups::default(),
            pending: S::Pending::default(),
            cursor: 0,
            packet_buf: [0; MAX_PACKET_SIZE],
            packet_len: 0,
            removed: S::Removed::default(),
            discovery_next_send: Instant::EPOCH,
        }
    }

    /// The configuration this source was created with.
    pub fn config(&self) -> &SourceConfig {
        &self.config
    }

    /// The source's CID.
    pub fn cid(&self) -> Cid {
        self.config.cid
    }

    /// Adds a universe to transmit on.
    ///
    /// Returns `true` if the universe was added, or `false` if it was already
    /// present (in which case it is left unchanged; reconfigure it with the
    /// `set_*` methods, or remove and re-add it). The universe transmits nothing
    /// until its levels are set with [`update_levels`](Self::update_levels).
    ///
    /// Returns [`Error::NoCapacity`] when a fixed-capacity source's universe
    /// table is full and this universe is not already present.
    pub fn add_universe(&mut self, config: UniverseConfig) -> Result<bool, Error> {
        let universe = config.universe;
        if self
            .universes
            .get(&universe)
            .is_some_and(|state| !state.is_finished())
        {
            return Ok(false);
        }
        match self
            .universes
            .upsert(universe, TxUniverseState::new(config))
        {
            Ok(()) => {
                debug!("added universe {} to source", universe);
                Ok(true)
            }
            Err(_) => Err(Error::NoCapacity),
        }
    }

    /// Begins terminating a universe: the E1.31 three-packet stream-terminated
    /// sequence is sent over the next few polls, after which the universe is
    /// dropped. Returns `false` if the universe was not present.
    ///
    /// A universe with no level data is dropped immediately (there is nothing to
    /// terminate).
    pub fn remove_universe(&mut self, universe: Universe) -> bool {
        let Some(state) = self.universes.get_mut(&universe) else {
            return false;
        };
        if state.has_levels() {
            state.terminating = true;
            state.term_count = 0;
            state.term_next_send = Instant::EPOCH;
            debug!("terminating universe {}", universe);
        } else {
            // Nothing to terminate; drop it now.
            self.universes.remove(&universe);
            debug!("dropped universe {} (no data to terminate)", universe);
        }
        true
    }

    /// Whether the source is currently transmitting (or terminating) `universe`.
    pub fn has_universe(&self, universe: Universe) -> bool {
        self.universes
            .get(&universe)
            .is_some_and(|state| !state.is_finished())
    }

    /// The universes the source currently has, in ascending order. Includes
    /// universes that are mid-termination, but not ones that have finished
    /// terminating and are awaiting removal.
    pub fn universes(&self) -> impl Iterator<Item = Universe> + '_ {
        self.universes
            .iter()
            .filter(|(_, state)| !state.is_finished())
            .map(|(&universe, _)| universe)
    }

    /// Sets the NULL-start-code levels for a universe. If the levels are
    /// different than previous ones, a send will be scheduled as soon as is
    /// permissible. At most [`MAX_SLOTS`] (512) levels are used; any beyond
    /// that are ignored. A no-op if the universe is not present.
    ///
    /// [`MAX_SLOTS`]: crate::packet::MAX_SLOTS
    pub fn update_levels(&mut self, universe: Universe, levels: &[u8]) {
        let Some(state) = self.universes.get_mut(&universe) else {
            return;
        };
        let n = levels.len().min(MAX_SLOTS);
        // Only an actual change (or resuming a terminating universe) restarts the
        // burst. Re-sending identical data must not reset suppression.
        let changed = state.levels.get() != Some(&levels[..n]);
        let resumed = state.terminating;
        state.cancel_termination();
        if changed {
            state.levels.set(&levels[..n]);
        }
        if changed || resumed {
            state.reset_levels();
        }
    }

    /// Sets both the levels and the per-address priorities for a universe.
    ///
    /// A per-address priority of zero means the source is not controlling that
    /// slot, and a receiver ignores the slot's level accordingly. At most
    /// [`MAX_SLOTS`] of each are used. A no-op if the universe is not present.
    ///
    /// [`MAX_SLOTS`]: crate::packet::MAX_SLOTS
    pub fn update_levels_and_pap(&mut self, universe: Universe, levels: &[u8], pap: &[u8]) {
        let Some(state) = self.universes.get_mut(&universe) else {
            return;
        };
        let levels_n = levels.len().min(MAX_SLOTS);
        let pap_n = pap.len().min(MAX_SLOTS);
        // As in `update_levels`, only a real change restarts the burst. Levels and
        // per-address priority travel in separate packets, so each resets only its
        // own transmission.
        let levels_changed = state.levels.get() != Some(&levels[..levels_n]);
        let pap_changed = state.pap.get() != Some(&pap[..pap_n]);
        let resumed = state.terminating;
        state.cancel_termination();
        if levels_changed {
            state.levels.set(&levels[..levels_n]);
        }
        if pap_changed {
            state.pap.set(&pap[..pap_n]);
        }
        if levels_changed || resumed {
            state.reset_levels();
        }
        if pap_changed || resumed {
            state.reset_pap();
        }
    }

    /// Stops sending per-address priority for a universe, falling back to its
    /// universe priority. Receivers see the per-address priority time out. A
    /// no-op if the universe is not present or was not sending PAP.
    pub fn remove_pap(&mut self, universe: Universe) {
        let Some(state) = self.universes.get_mut(&universe) else {
            return;
        };
        // Dropping the PAP data stops its packets from being queued; receivers
        // see the per-address priority time out. Levels are unaffected.
        state.pap.clear();
    }

    /// Changes a universe's priority, resetting its transmission so the change
    /// propagates promptly. A no-op if the universe is not present.
    pub fn set_priority(&mut self, universe: Universe, priority: Priority) {
        if let Some(state) = self.universes.get_mut(&universe) {
            state.priority = priority.get();
            state.reset_levels();
            state.reset_pap();
        }
    }

    /// Changes a universe's preview-data flag. A no-op if the universe is not
    /// present.
    pub fn set_preview(&mut self, universe: Universe, preview: bool) {
        if let Some(state) = self.universes.get_mut(&universe) {
            state.preview = preview;
            state.reset_levels();
            state.reset_pap();
        }
    }

    /// Changes the source name carried in every packet, resetting transmission
    /// on all universes so the change propagates promptly.
    pub fn set_name(&mut self, name: impl Into<SourceName>) {
        self.config.name = name.into();
        for state in self.universes.values_mut() {
            state.reset_levels();
            state.reset_pap();
        }
    }

    /// Restarts transmission of a universe's current data, re-sending its levels
    /// and per-address priority promptly (as soon as the minimum inter-packet
    /// spacing allows) and beginning a fresh pre-suppression burst, without
    /// changing the data itself. A no-op if the universe is not present.
    ///
    /// Generally this should only be called if a new destination has been
    /// added which needs data sent promptly.
    pub fn resend(&mut self, universe: Universe) {
        if let Some(state) = self.universes.get_mut(&universe) {
            state.reset_levels();
            state.reset_pap();
        }
    }

    /// Starts, changes, or stops (`None`) synchronization for a universe at
    /// runtime, resetting its transmission so the change propagates promptly. A
    /// no-op if the universe is not present.
    ///
    /// `Some((sync_universe, on_loss))` synchronizes the universe on
    /// `sync_universe` with the given failure policy, exactly as
    /// [`UniverseConfig::synchronized_on`] does at configuration time.
    ///
    /// `None` stops synchronization by transmitting `sync_address = 0`,
    /// which tells receivers to desynchronize and resume live output.
    pub fn set_synchronization(
        &mut self,
        universe: Universe,
        sync: Option<(Universe, OnSyncLoss)>,
    ) {
        if let Some(state) = self.universes.get_mut(&universe) {
            match sync {
                Some((sync_universe, on_loss)) => {
                    state.sync_universe = sync_universe.get();
                    state.force_sync = on_loss.force_sync();
                }
                None => {
                    state.sync_universe = 0;
                    state.force_sync = false;
                }
            }
            state.reset_levels();
            state.reset_pap();
        }
    }

    /// Advances time to `now` and returns the packets due to be sent now, plus
    /// the next instant at which polling again could produce more.
    ///
    /// Calling `poll` more often than necessary is harmless (it simply finds
    /// nothing due); calling it later only delays transmissions. The returned
    /// [`SourcePoll`] borrows the source mutably; drain its transmissions before
    /// polling or mutating the source again.
    pub fn poll(&mut self, now: Instant) -> SourcePoll<'_, S> {
        // Resume draining if a previous drain was abandoned before it finished.
        if self.cursor < self.pending.len() {
            return SourcePoll::new(Some(now), self);
        }

        // Physically drop universes that finished terminating on a previous poll.
        {
            let Self {
                universes, removed, ..
            } = &mut *self;
            removed.clear();
            universes.retain(|universe, state| {
                let finished = state.is_finished();
                if finished {
                    debug!("universe {} removed (termination complete)", universe);
                    removed.push_expect(*universe);
                }
                !finished
            });
        }

        self.pending.clear();
        self.cursor = 0;

        self.poll_discovery(now);

        // Poll each universe's data scheduling, arming the sync group of any
        // universe that queued a packet this poll.
        {
            let Self {
                config,
                universes,
                sync_groups,
                pending,
                ..
            } = self;
            for state in universes.values_mut() {
                let queued = poll_universe(state, config, now, pending);
                // A terminating universe advertises `sync_address = 0`, so it is
                // no longer a member of its group and must not (re)arm it.
                if queued && !state.terminating {
                    if let Ok(sync_universe) = Universe::new(state.sync_universe) {
                        arm_sync_group(sync_groups, sync_universe, now, config.sync_delay);
                    }
                }
            }
        }

        self.poll_sync_groups(now);

        let deadline = self.next_deadline(now);
        SourcePoll::new(deadline, self)
    }

    /// Fires and prunes synchronization groups after the per-universe scheduling
    /// (which armed them) has run.
    ///
    /// A group whose timer has come due queues one [`Emission::Sync`], which
    /// lands after this poll's data packets. Groups with no members and no
    /// pending sync are dropped.
    fn poll_sync_groups(&mut self, now: Instant) {
        let Self {
            universes,
            sync_groups,
            pending,
            ..
        } = self;

        // Drop groups whose last active member has left (terminated or removed)
        // before firing: a terminated universe advertises `sync_address = 0`, so
        // its pending sync is moot. This bounds the live group count by the
        // active-universe count, so arming a group never overflows.
        sync_groups.retain(|sync_universe, _| {
            universes
                .values()
                .any(|state| state.syncs_on(sync_universe.get()))
        });

        // Fire any surviving group whose sync is now due.
        for (&sync_universe, group) in sync_groups.iter_mut() {
            if group
                .pending_deadline
                .is_some_and(|deadline| now >= deadline)
            {
                let seq = group.take_seq();
                pending.push_expect(Pending {
                    emission: Emission::Sync { sync_universe, seq },
                    route: Route::Sync(sync_universe),
                });
                group.pending_deadline = None;
            }
        }
    }

    /// Emits universe-discovery pages if the discovery interval has elapsed and
    /// the source has at least one universe to announce.
    fn poll_discovery(&mut self, now: Instant) {
        let announce = self.universes.values().any(TxUniverseState::in_discovery);
        if !announce || now < self.discovery_next_send {
            return;
        }

        // Count the distinct announced universes to size the page run. The
        // pages themselves are rebuilt from the live universe set when each
        // discovery packet is serialized, so no full-list scratch is kept.
        let total_universes = announced_count(&self.universes);
        let page_count = total_universes.div_ceil(MAX_UNIVERSES_PER_PAGE).max(1);
        let last_page = (page_count - 1) as u8;

        // One page per emission; the adapter sends each to the discovery group on
        // the interfaces it transmits on.
        for page in 0..page_count {
            self.pending.push_expect(Pending {
                emission: Emission::Discovery {
                    page: page as u8,
                    last_page,
                },
                route: Route::Discovery,
            });
        }

        self.discovery_next_send = now.saturating_add(DISCOVERY_INTERVAL);
    }

    /// Computes the earliest future instant at which polling could produce a
    /// transmission.
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut consider = |deadline: Instant| {
            if deadline > now {
                next = Some(match next {
                    Some(current) => current.min(deadline),
                    None => deadline,
                });
            }
        };

        let announces = self
            .universes
            .values()
            .any(|state| !state.is_finished() && state.in_discovery());
        if announces {
            consider(self.discovery_next_send);
        }
        for state in self.universes.values() {
            if state.is_finished() {
                // Logically gone; awaiting removal at the next poll.
                continue;
            }
            if state.terminating {
                if state.has_levels() {
                    consider(state.term_next_send);
                }
            } else {
                if state.has_levels() {
                    consider(state.level_next_send);
                }
                if state.has_pap() {
                    consider(state.pap_next_send);
                }
            }
        }
        for group in self.sync_groups.values() {
            if let Some(deadline) = group.pending_deadline {
                consider(deadline);
            }
        }
        next
    }

    /// Serializes the queued transmission at `idx` into the reusable packet
    /// buffer and returns it. The sequence number for a level or per-address-
    /// priority packet is assigned here (not when queued). The serialized bytes
    /// remain in `packet_buf` until the next serialization, so a caller cut off
    /// mid-send can re-read them via [`current_packet`](Self::current_packet).
    fn serialize_at(&mut self, idx: usize) -> Transmission<'_> {
        let pending = self.pending.as_slice()[idx];

        let Self {
            config,
            universes,
            packet_buf,
            packet_len: last_len,
            ..
        } = self;

        let mut disc_page: heapless::Vec<u8, { MAX_UNIVERSES_PER_PAGE * 2 }> = heapless::Vec::new();

        let packet = match pending.emission {
            Emission::Level {
                universe,
                terminated,
            } => {
                let state = universes
                    .get_mut(&universe)
                    .expect("queued universe is present");
                let seq = state.take_seq();
                let values = state.levels.get().expect("level packet has levels");
                data_packet(
                    config,
                    state,
                    universe,
                    seq,
                    DMX_NULL_START_CODE,
                    terminated,
                    values,
                )
            }
            Emission::Pap { universe } => {
                let state = universes
                    .get_mut(&universe)
                    .expect("queued universe is present");
                let seq = state.take_seq();
                let pap = state.pap.get().expect("pap packet has pap data");
                data_packet(config, state, universe, seq, PAP_START_CODE, false, pap)
            }
            Emission::Discovery { page, last_page } => {
                build_discovery_page(universes, page, &mut disc_page);
                Packet {
                    cid: config.cid,
                    payload: Payload::UniverseDiscovery(UniverseDiscoveryPacket {
                        source_name: config.name.as_str(),
                        page,
                        last_page,
                        universes: UniverseList::from_bytes(&disc_page),
                    }),
                }
            }
            Emission::Sync { sync_universe, seq } => Packet {
                cid: config.cid,
                payload: Payload::Sync(SyncPacket {
                    sequence_number: seq,
                    sync_address: sync_universe.get(),
                }),
            },
        };

        let n = serialize_into(packet_buf, &packet);
        *last_len = n;
        Transmission {
            route: pending.route,
            data: &packet_buf[..n],
        }
    }

    /// The bytes of the packet currently held in the reusable buffer: the one
    /// most recently returned by [`SourcePoll::next_transmission`] or
    /// [`send_now`](Self::send_now).
    ///
    /// An adapter can use this to finish delivering a packet whose fan-out to
    /// multiple destinations was interrupted (e.g. its send future was
    /// cancelled). It is invalidated by operations that produce a new
    /// serialized packet (e.g. the next call to [`SourcePoll::next_transmission`]
    /// or [`send_now`](Self::send_now)).
    pub fn current_packet(&self) -> &[u8] {
        &self.packet_buf[..self.packet_len]
    }

    /// Serializes a one-shot packet with an arbitrary START code for immediate
    /// transmission on `universe`, for application data that falls outside the
    /// managed NULL and per-address-priority streams. The packet carries the
    /// universe's configured priority, preview flag and sync universe, and
    /// takes the universe's next sequence number so it stays in order with the
    /// scheduled stream.
    ///
    /// Unlike [`update_levels`](Self::update_levels), this performs no rate
    /// limiting, transmission suppression or keep-alive: the packet is serialized
    /// once, and the caller sends it (or repeats the call) at whatever rate it
    /// likes. The returned [`Transmission`] is handled exactly like one drained
    /// from a [`poll`](Self::poll), including re-reading it via
    /// [`current_packet`](Self::current_packet).
    ///
    /// # Errors
    ///
    /// - [`Error::ReservedStartCode`] if `start_code` is [`StartCode::NULL`] or
    ///   [`StartCode::PAP`], which the source manages itself.
    /// - [`Error::NoSuchUniverse`] if the universe is not present (or is
    ///   terminating).
    /// - [`Error::Codec`] if `data` exceeds [`MAX_SLOTS`].
    ///
    /// [`MAX_SLOTS`]: crate::packet::MAX_SLOTS
    pub fn send_now(
        &mut self,
        universe: Universe,
        start_code: StartCode,
        data: &[u8],
    ) -> Result<Transmission<'_>, Error> {
        if start_code.is_reserved() {
            return Err(Error::ReservedStartCode {
                start_code: start_code.get(),
            });
        }
        if data.len() > MAX_SLOTS {
            return Err(Error::Codec(CodecError {
                offset: 0,
                kind: CodecErrorKind::TooManyValues {
                    count: data.len(),
                    max: MAX_SLOTS,
                },
            }));
        }

        let Self {
            config,
            universes,
            packet_buf,
            packet_len: last_len,
            ..
        } = self;

        let state = universes
            .get_mut(&universe)
            .filter(|state| !state.is_finished())
            .ok_or(Error::NoSuchUniverse {
                universe: universe.get(),
            })?;
        let seq = state.take_seq();
        let packet = data_packet(config, state, universe, seq, start_code.get(), false, data);

        let n = serialize_into(packet_buf, &packet);
        *last_len = n;
        Ok(Transmission {
            route: Route::Universe(universe),
            data: &packet_buf[..n],
        })
    }
}

// --- Free helpers ------------------------------------------------------------

/// Arms the sync group for `sync_universe` at `now + sync_delay`, creating the
/// group if it does not yet exist. Leaves an already-armed group untouched.
fn arm_sync_group(
    sync_groups: &mut impl MapLike<Universe, SyncGroupState>,
    sync_universe: Universe,
    now: Instant,
    sync_delay: Duration,
) {
    if let Some(group) = sync_groups.get_mut(&sync_universe) {
        if group.pending_deadline.is_none() {
            group.pending_deadline = Some(now.saturating_add(sync_delay));
        }
    } else {
        let mut group = SyncGroupState::new();
        group.pending_deadline = Some(now.saturating_add(sync_delay));
        sync_groups.upsert_expect(sync_universe, group);
    }
}

/// The number of distinct universes the source announces in discovery: every
/// data universe holding data, plus the synchronization universe of each if
/// applicable, deduplicated.
fn announced_count(universes: &impl MapLike<Universe, TxUniverseState>) -> usize {
    let mut count = 0;
    let mut prev = None;
    while let Some(v) = next_announced(universes, prev) {
        prev = Some(v);
        count += 1;
    }
    count
}

/// Finds the smallest announced universe strictly greater than `prev` (or the
/// smallest of all when `prev` is `None`), or `None` once they are exhausted.
///
/// Announced universes are the data universes holding data plus their
/// synchronization universes.
fn next_announced(
    universes: &impl MapLike<Universe, TxUniverseState>,
    prev: Option<u16>,
) -> Option<u16> {
    let mut best: Option<u16> = None;
    let mut consider = |v: u16| {
        if prev.is_none_or(|p| v > p) {
            best = Some(best.map_or(v, |b| b.min(v)));
        }
    };
    for state in universes.values() {
        if state.in_discovery() {
            consider(state.universe.get());
            if state.sync_universe != 0 {
                consider(state.sync_universe);
            }
        }
    }
    best
}

/// Fills `out` with the big-endian universe numbers of discovery page `page`:
/// the announced universes whose ascending rank falls in this page's window.
fn build_discovery_page(
    universes: &impl MapLike<Universe, TxUniverseState>,
    page: u8,
    out: &mut heapless::Vec<u8, { MAX_UNIVERSES_PER_PAGE * 2 }>,
) {
    out.clear();
    let lo = page as usize * MAX_UNIVERSES_PER_PAGE;
    let hi = lo + MAX_UNIVERSES_PER_PAGE;
    let mut idx = 0;
    let mut prev = None;
    while idx < hi {
        let Some(v) = next_announced(universes, prev) else {
            break;
        };
        prev = Some(v);
        if idx >= lo {
            let _ = out.extend_from_slice(&v.to_be_bytes());
        }
        idx += 1;
    }
}

/// Builds a data packet for `universe` carrying `values` under `start_code`. The
/// universe's scalar settings (priority, sync universe, preview) are copied in;
/// `config` supplies the CID and source name.
fn data_packet<'a>(
    config: &'a SourceConfig,
    state: &TxUniverseState,
    universe: Universe,
    seq: SequenceNumber,
    start_code: u8,
    terminated: bool,
    values: &'a [u8],
) -> Packet<'a> {
    let (sync_address, force_sync) = if terminated {
        (0, false)
    } else {
        (state.sync_universe, state.force_sync)
    };
    Packet {
        cid: config.cid,
        payload: Payload::Data(DataPacket {
            source_name: config.name.as_str(),
            priority: state.priority,
            sync_address,
            sequence_number: seq,
            preview: state.preview,
            stream_terminated: terminated,
            force_sync,
            universe: universe.get(),
            start_code,
            values,
        }),
    }
}

/// Serializes `packet` into `buf`, returning its length. `buf` must be at
/// least [`MAX_PACKET_SIZE`].
fn serialize_into(buf: &mut [u8], packet: &Packet) -> usize {
    packet
        .serialize(buf)
        .expect("buffer is sized to the packet")
}

/// Runs one poll tick for a single universe, queueing any due sends. A universe
/// that completes its termination sequence here is left in place (now
/// [`is_finished`](TxUniverseState::is_finished)) and removed at the next poll,
/// so the final termination packet it just queued stays resolvable while this
/// poll is drained.
///
/// Returns whether a synchronizable data packet was queued: `true` only for a
/// non-terminated level or per-address-priority packet.
fn poll_universe(
    state: &mut TxUniverseState,
    config: &SourceConfig,
    now: Instant,
    pending: &mut impl VecLike<Pending>,
) -> bool {
    if state.terminating {
        let more_to_send = state.term_count < TERMINATION_PACKETS;
        if more_to_send && now >= state.term_next_send {
            queue_level(state, true, pending);
            state.term_count += 1;
            state.term_next_send = now.saturating_add(TICK_INTERVAL);
        }
        return false;
    }

    let mut queued = false;
    if state.has_levels() && now >= state.level_next_send {
        queue_level(state, false, pending);
        queued = true;
        state.level_last_send = Some(now);
        state.level_pre_suppression =
            (state.level_pre_suppression + 1).min(PRE_SUPPRESSION_PACKETS);
        let interval = if state.level_pre_suppression < PRE_SUPPRESSION_PACKETS {
            TICK_INTERVAL
        } else {
            config.keep_alive
        };
        state.level_next_send = now.saturating_add(interval);
    }

    if state.has_pap() && now >= state.pap_next_send {
        queue_pap(state, pending);
        queued = true;
        state.pap_last_send = Some(now);
        state.pap_pre_suppression = (state.pap_pre_suppression + 1).min(PRE_SUPPRESSION_PACKETS);
        let interval = if state.pap_pre_suppression < PRE_SUPPRESSION_PACKETS {
            TICK_INTERVAL
        } else {
            config.pap_keep_alive
        };
        state.pap_next_send = now.saturating_add(interval);
    }

    queued
}

/// The earliest instant a re-send may be scheduled given when a packet was last
/// sent: immediately if none ever was, otherwise [`TICK_INTERVAL`] after the last
/// one so the universe never exceeds the maximum DMX rate.
fn next_send_after(last_send: Option<Instant>) -> Instant {
    match last_send {
        Some(last) => last.saturating_add(TICK_INTERVAL),
        None => Instant::EPOCH,
    }
}

/// Queues a level packet for a universe. The sequence number is assigned when
/// the packet is serialized (handed out), not here, so that wire order and
/// sequence order always agree even when an ad-hoc [`send_now`](Source::send_now)
/// is interleaved with the scheduled stream.
fn queue_level(state: &TxUniverseState, terminated: bool, pending: &mut impl VecLike<Pending>) {
    pending.push_expect(Pending {
        emission: Emission::Level {
            universe: state.universe,
            terminated,
        },
        route: Route::Universe(state.universe),
    });
}

/// Queues a per-address-priority packet for a universe. As with
/// [`queue_level`], the sequence number is assigned at serialize time.
fn queue_pap(state: &TxUniverseState, pending: &mut impl VecLike<Pending>) {
    pending.push_expect(Pending {
        emission: Emission::Pap {
            universe: state.universe,
        },
        route: Route::Universe(state.universe),
    });
}
