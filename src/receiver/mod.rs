//! The sACN receive path.
//!
//! Two tiers of receiver live here, sharing the same configuration and source
//! tracking:
//!
//! - [`BasicReceiver`] tracks the sources on each universe and forwards their
//!   data **per-source**, leaving priority reconciliation to the application. It
//!   is the foundation the merging receiver is built on.
//! - [`Receiver`] wires a [`BasicReceiver`] to a per-universe [DMX
//!   merger](crate::merger) and emits a single **merged** result per universe -
//!   the highest-priority level for each slot, the owning source, and the list
//!   of active sources.

mod basic;
mod event;
mod loss;
mod merging;
mod source;

use crate::merger::MergerStorage;
use crate::packet::{DMX_NULL_START_CODE, PAP_START_CODE};
use crate::storage::{HeapStorage, MapLike, VecLike, coherence_check};
use crate::time::{Duration, Instant};
use crate::types::{Cid, Universe};

#[cfg(feature = "alloc")]
pub use basic::BasicReceiverEvent;
pub use basic::{
    BasicReceiver, BasicReceiverCore, BasicReceiverEventRef, BasicReceiverPollEvent,
    BasicReceiverResources, BasicUniverseState, LostSource, PacketOutcome, PollOutcome,
};
pub use event::{ListenOutcome, SourceInfoRef, StopOutcome, UniverseDataRef};
#[cfg(feature = "alloc")]
pub use event::{SourceInfo, UniverseData};
pub use loss::{TerminationSet, TerminationSetSource};
pub use merging::{MergeSource, UniverseMerge};
#[cfg(feature = "alloc")]
pub use merging::{MergedData, MergedSource, ReceiverEvent};
pub use merging::{
    MergedDataRef, MergedLostSource, MergedPacketOutcome, MergedPollOutcome, MergedSourceRef,
    Receiver, ReceiverCore, ReceiverEventRef, ReceiverPollEvent, ReceiverResources, SyncRelease,
};
pub use source::TrackedSource;

// --- Storage types ----------------------------------------------------------

/// Storage types for a [`BasicReceiver`] (and the merging [`Receiver`] by
/// extension)
///
/// Use [`static_storage!`](crate::static_storage!) to produce a type that
/// implements this trait for statically-allocated storage, or use
/// [`HeapStorage`] for heap-based storage.
pub trait BasicReceiverStorage: Sized {
    /// The listened universes and their per-universe state.
    type BasicUniverses: MapLike<Universe, basic::BasicUniverseState<Self>>;
    /// The sources tracked on one universe.
    type BasicSources: MapLike<Cid, source::TrackedSource>;
    /// The open source-loss termination sets of one universe.
    type TermSets: VecLike<loss::TerminationSet<Self>>;
    /// The sources captured in one termination set.
    type TermSetSources: MapLike<Cid, loss::TerminationSetSource>;
    /// The per-poll snapshot of listened universe keys, walked lazily while the
    /// poll's events are drained.
    type PollKeys: VecLike<Universe>;
    /// The reusable scratch of sources lost in one settled termination set,
    /// borrowed by the `SourcesLost` event that reports them.
    type LossList: VecLike<basic::LostSource>;
    /// Transient per-poll scratch for sources confirmed offline this tick.
    type OfflineScratch: VecLike<(Cid, bool)>;
    /// Transient per-poll scratch for CIDs (online, unknown, or to-remove).
    type CidScratch: VecLike<Cid>;
}

coherence_check! {
    /// Capacity coherence assertions for [`BasicReceiver`].
    AssertReceiverCoherent<S: BasicReceiverStorage> = {
        let sources = <S::BasicSources as MapLike<Cid, source::TrackedSource>>::CAPACITY;
        let universes =
            <S::BasicUniverses as MapLike<Universe, basic::BasicUniverseState<S>>>::CAPACITY;
        assert!(
            <S::TermSets as VecLike<loss::TerminationSet<S>>>::CAPACITY >= sources,
            "ReceiverStorage::TermSets capacity must be >= Sources capacity",
        );
        assert!(
            <S::TermSetSources as MapLike<Cid, loss::TerminationSetSource>>::CAPACITY >= sources,
            "ReceiverStorage::TermSetSources capacity must be >= Sources capacity",
        );
        assert!(
            <S::PollKeys as VecLike<Universe>>::CAPACITY >= universes,
            "ReceiverStorage::PollKeys capacity must be >= Universes capacity",
        );
        assert!(
            <S::LossList as VecLike<basic::LostSource>>::CAPACITY >= sources,
            "ReceiverStorage::LossList capacity must be >= Sources capacity",
        );
        assert!(
            <S::OfflineScratch as VecLike<(Cid, bool)>>::CAPACITY >= sources,
            "ReceiverStorage::OfflineScratch capacity must be >= Sources capacity",
        );
        assert!(
            <S::CidScratch as VecLike<Cid>>::CAPACITY >= sources,
            "ReceiverStorage::CidScratch capacity must be >= Sources capacity",
        );
    }
}

#[cfg(feature = "alloc")]
impl BasicReceiverStorage for HeapStorage {
    type BasicUniverses =
        alloc::collections::BTreeMap<Universe, basic::BasicUniverseState<HeapStorage>>;
    type BasicSources = alloc::collections::BTreeMap<Cid, source::TrackedSource>;
    type TermSets = alloc::vec::Vec<loss::TerminationSet<HeapStorage>>;
    type TermSetSources = alloc::collections::BTreeMap<Cid, loss::TerminationSetSource>;
    type PollKeys = alloc::vec::Vec<Universe>;
    type LossList = alloc::vec::Vec<basic::LostSource>;
    type OfflineScratch = alloc::vec::Vec<(Cid, bool)>;
    type CidScratch = alloc::vec::Vec<Cid>;
}

#[cfg(not(feature = "alloc"))]
impl BasicReceiverStorage for HeapStorage {
    type BasicUniverses =
        crate::storage::SortedVecMap<Universe, basic::BasicUniverseState<HeapStorage>, 0>;
    type BasicSources = crate::storage::SortedVecMap<Cid, source::TrackedSource, 0>;
    type TermSets = heapless::Vec<loss::TerminationSet<HeapStorage>, 0>;
    type TermSetSources = crate::storage::SortedVecMap<Cid, loss::TerminationSetSource, 0>;
    type PollKeys = heapless::Vec<Universe, 0>;
    type LossList = heapless::Vec<basic::LostSource, 0>;
    type OfflineScratch = heapless::Vec<(Cid, bool), 0>;
    type CidScratch = heapless::Vec<Cid, 0>;
}

/// Storage types for the merging [`Receiver`].
///
/// Use [`static_storage!`](crate::static_storage!) to produce a type that
/// implements this trait for statically-allocated storage, or use
/// [`HeapStorage`] for heap-based storage.
pub trait ReceiverStorage: BasicReceiverStorage + MergerStorage {
    /// The listened universes and their per-universe merge state.
    type Universes: MapLike<Universe, merging::UniverseMerge<Self>>;
    /// The sources contributing to one universe's merge, keyed by CID.
    type Sources: MapLike<Cid, merging::MergeSource>;
    /// The active synchronization addresses and their loss deadlines.
    type SyncAddresses: MapLike<u16, Instant>;
    /// The reusable scratch holding the enriched loss list of the `SourcesLost`
    /// event currently being drained from a poll.
    type MergeLossList: VecLike<merging::MergedLostSource>;
    /// Transient per-sync scratch of the universes latched by a sync packet.
    type SyncReleases: VecLike<Universe>;
}

coherence_check! {
    /// Carrier for the compile-time coherence check on a
    /// [`MergingReceiverStorage`] policy: the merging receiver's own collections
    /// must be sized coherently against the leaf universe and source counts.
    /// Forced by [`Receiver`]'s constructor. `SyncAddresses` is a leaf capacity of
    /// its own (it has no derived bound), so it is not checked here.
    AssertMergingCoherent<S: ReceiverStorage> = {
        let sources = <S::BasicSources as MapLike<Cid, source::TrackedSource>>::CAPACITY;
        let universes =
            <S::BasicUniverses as MapLike<Universe, basic::BasicUniverseState<S>>>::CAPACITY;
        assert!(
            <S::Universes as MapLike<Universe, merging::UniverseMerge<S>>>::CAPACITY
                >= universes,
            "MergingReceiverStorage::Universes capacity must be >= BasicUniverses capacity",
        );
        assert!(
            <S::Sources as MapLike<Cid, merging::MergeSource>>::CAPACITY >= sources,
            "MergingReceiverStorage::Sources capacity must be >= BasicSources capacity",
        );
        assert!(
            <S::MergeLossList as VecLike<merging::MergedLostSource>>::CAPACITY >= sources,
            "MergingReceiverStorage::MergeLossList capacity must be >= Sources capacity",
        );
        assert!(
            <S::SyncReleases as VecLike<Universe>>::CAPACITY >= universes,
            "MergingReceiverStorage::SyncReleases capacity must be >= Universes capacity",
        );
    }
}

#[cfg(feature = "alloc")]
impl ReceiverStorage for HeapStorage {
    type Universes = alloc::collections::BTreeMap<Universe, merging::UniverseMerge<HeapStorage>>;
    type Sources = alloc::collections::BTreeMap<Cid, merging::MergeSource>;
    type SyncAddresses = alloc::collections::BTreeMap<u16, Instant>;
    type MergeLossList = alloc::vec::Vec<merging::MergedLostSource>;
    type SyncReleases = alloc::vec::Vec<Universe>;
}

#[cfg(not(feature = "alloc"))]
impl ReceiverStorage for HeapStorage {
    type Universes = crate::storage::SortedVecMap<Universe, merging::UniverseMerge<HeapStorage>, 0>;
    type Sources = crate::storage::SortedVecMap<Cid, merging::MergeSource, 0>;
    type SyncAddresses = crate::storage::SortedVecMap<u16, Instant, 0>;
    type MergeLossList = heapless::Vec<merging::MergedLostSource, 0>;
    type SyncReleases = heapless::Vec<Universe, 0>;
}

// --- Timing constants -------------------------------------------------------

/// How long a source may be silent before it is considered lost. E1.31 calls
/// this the network data loss timeout.
const SOURCE_LOSS_TIMEOUT: Duration = Duration::from_millis(2500);

/// The default time to wait for a new source's first per-address-priority
/// (`0xDD`) packet before falling back to packet priority. Tunable via
/// [`ReceiverConfig::with_per_address_priority_wait_time`].
const DEFAULT_PAP_WAIT: Duration = Duration::from_millis(1500);

/// The default length of a universe's sampling period. Tunable via
/// [`ReceiverConfig::with_sample_period`].
const DEFAULT_SAMPLE_PERIOD: Duration = Duration::from_millis(1500);

/// A set of DMX START codes, stored as a 256-bit bitmap.
///
/// Used to configure which alternate START codes a receiver tracks beyond the
/// always-handled NULL and per-address-priority codes.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StartCodeSet {
    bits: [u64; 4],
}

impl StartCodeSet {
    const fn empty() -> Self {
        Self { bits: [0; 4] }
    }

    fn insert(&mut self, code: u8) {
        self.bits[(code >> 6) as usize] |= 1 << (code & 0x3f);
    }

    fn contains(&self, code: u8) -> bool {
        self.bits[(code >> 6) as usize] & (1 << (code & 0x3f)) != 0
    }
}

impl core::fmt::Debug for StartCodeSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_set()
            .entries((0u16..=255).filter(|&c| self.contains(c as u8)))
            .finish()
    }
}

// --- Configuration -----------------------------------------------------------

/// Configuration shared by [`BasicReceiver`] and [`Receiver`].
///
/// Construct the defaults with [`ReceiverConfig::new`] and adjust with the
/// `with_*` methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverConfig {
    sample_period: Duration,
    extra_hold_time: Duration,
    pap_handling: bool,
    pap_wait: Duration,
    allowed_start_codes: StartCodeSet,
    source_limit: Option<usize>,
    filter_preview: bool,
    synchronization: bool,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        let mut allowed_start_codes = StartCodeSet::empty();
        allowed_start_codes.insert(DMX_NULL_START_CODE);
        allowed_start_codes.insert(PAP_START_CODE);
        Self {
            sample_period: DEFAULT_SAMPLE_PERIOD,
            extra_hold_time: Duration::ZERO,
            pap_handling: true,
            pap_wait: DEFAULT_PAP_WAIT,
            allowed_start_codes,
            source_limit: None,
            filter_preview: false,
            synchronization: true,
        }
    }
}

impl ReceiverConfig {
    /// Construct a new [`ReceiverConfig`] with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the length of the sampling period opened when a universe starts
    /// being listened to.
    ///
    /// During this window, all received data packets are forwarded without
    /// applying any special logic, such as
    /// [waiting for per-address priority](Self::with_per_address_priority_handling)
    /// or [grouping terminations](BasicReceiverEvent::SourcesLost). Applications
    /// should cache data received during the sampling period (after
    /// [`SamplingStarted`](BasicReceiverEvent::SamplingStarted) is received and
    /// before [`SamplingEnded`](BasicReceiverEvent::SamplingEnded) is received)
    /// and not act on it until the sampling period has ended. (The merging
    /// [`Receiver`] does this for you, withholding merged data until the period
    /// ends.)
    ///
    /// Longer periods discover more slow-to-appear sources before the period
    /// ends, at the cost of a longer wait for data. More than 1 second is
    /// recommended. Defaults to 1.5s.
    #[must_use]
    pub fn with_sample_period(mut self, period: Duration) -> Self {
        self.sample_period = period;
        self
    }

    /// Sets an additional hold time applied before a source loss is reported,
    /// on top of the 2.5s network-data-loss timeout that is always in effect.
    ///
    /// A source is detected as lost after it has been silent for the (fixed,
    /// standard-mandated) loss timeout; this extends the delay before the
    /// [`SourcesLost`](BasicReceiverEvent::SourcesLost) notification is emitted.
    /// The extra window also lets a source that returns within it cancel its own
    /// pending loss silently. It does **not** affect the grouping of
    /// near-simultaneous losses, which always happens.
    ///
    /// Defaults to zero: loss is reported as soon as it is detected and the
    /// termination grouping logic has resolved. Hold-last-look policy is best
    /// implemented at the application layer, but this knob is available to set
    /// a per-source hold at the receiver level if preferred.
    #[must_use]
    pub fn with_extra_hold_time(mut self, hold: Duration) -> Self {
        self.extra_hold_time = hold;
        self
    }

    /// Sets whether per-address-priority (`0xDD`) handling is enabled. Defaults
    /// to `true`.
    ///
    /// Per-address priority began as an ETC extension to sACN and is now being
    /// standardized in BSR E1.31-1. When this option is enabled, the following
    /// behaviors are added to the receiver state machine:
    ///
    /// - When receiving DMX data from a new source, the receiver waits for a
    ///   configurable timeout to receive per-address priority data before
    ///   forwarding DMX data, so that apps do not act on level data without
    ///   the accompanying priorities (see
    ///   [`with_per_address_priority_wait_time`](Self::with_per_address_priority_wait_time)).
    /// - When a source that was previously sending per-address priority data
    ///   stops sending it, the receiver generates a
    ///   [`SourcePapLost`](BasicReceiverEvent::SourcePapLost) notification, so
    ///   that the application knows to stop applying the last received
    ///   per-address priorities and fall back to universe priority.
    ///
    /// This should be enabled in networks where sources are likely to send
    /// per-address priority, and even when sources do not send it, the only
    /// effect of this option is to add a small delay before receiving DMX data
    /// from a new source. Disable it if per-address priority is not a concern
    /// and maximum performance is needed.
    ///
    /// Has no effect when `0xDD` is excluded from the START code allow-list (see
    /// [`with_allowed_start_codes`](Self::with_allowed_start_codes)): with no PAP
    /// packets to act on, the handling is implicitly off.
    #[must_use]
    pub fn with_per_address_priority_handling(mut self, enabled: bool) -> Self {
        self.pap_handling = enabled;
        self
    }

    /// Sets how long a newly discovered source's level data is held while
    /// waiting for its first per-address-priority (`0xDD`) packet, before
    /// falling back to the packet priority. Defaults to 1.5s. More than 1
    /// second is recommended. See
    /// [`with_per_address_priority_handling`](Self::with_per_address_priority_handling).
    ///
    /// Has no effect during a sampling period (where data is reported
    /// immediately) or when per-address-priority handling is disabled.
    #[must_use]
    pub fn with_per_address_priority_wait_time(mut self, wait: Duration) -> Self {
        self.pap_wait = wait;
        self
    }

    /// Limits the number of sources tracked per universe. Once the limit is
    /// reached, further new sources are dropped and a
    /// [`SourceLimitExceeded`](BasicReceiverEvent::SourceLimitExceeded) is
    /// emitted.
    ///
    /// Defaults to no limit.
    #[must_use]
    pub fn with_source_limit(mut self, limit: usize) -> Self {
        self.source_limit = Some(limit);
        self
    }

    /// Sets the complete set of START codes the receiver tracks and forwards.
    /// Defaults to [`DMX_NULL_START_CODE`] (levels) and [`PAP_START_CODE`].
    ///
    /// This is the authoritative allow-list: a packet whose START code is not in
    /// the set is ignored entirely - its data is not forwarded, its source is
    /// not tracked, and it does not count toward the source limit. The set
    /// replaces the default, so to process other codes *alongside* NULL and PAP
    /// you must include them, e.g. `&[0x00, 0xDD, 0x17]`; and NULL or PAP can be
    /// left out to ignore them.
    ///
    /// This composes with
    /// [`with_per_address_priority_handling`](Self::with_per_address_priority_handling):
    /// the allow-list decides whether `0xDD` packets are processed at all, while
    /// that option decides whether processed `0xDD` packets drive the PAP state
    /// machine. Omitting `0xDD` here implicitly disables PAP handling (there is
    /// nothing for it to act on), so a NULL-only receiver gets no PAP wait.
    ///
    /// Each call replaces the set from any previous call.
    #[must_use]
    pub fn with_allowed_start_codes(mut self, start_codes: &[u8]) -> Self {
        let mut set = StartCodeSet::empty();
        for &code in start_codes {
            set.insert(code);
        }
        self.allowed_start_codes = set;
        self
    }

    /// Sets whether packets with the preview-data flag are filtered out instead
    /// of forwarded. Defaults to `false` (preview data is forwarded, with
    /// [`UniverseData::preview`] set).
    #[must_use]
    pub fn with_filter_preview(mut self, filter: bool) -> Self {
        self.filter_preview = filter;
        self
    }

    /// Sets whether universe synchronization is honored. Defaults to `true`.
    ///
    /// When enabled, the merging [`Receiver`] holds a synchronized universe's
    /// output until a synchronization packet releases it, and the
    /// [`BasicReceiver`] surfaces each packet's synchronization address and
    /// reports incoming sync packets. When disabled, synchronization addresses
    /// are ignored, sync packets are dropped, and every packet is delivered
    /// live.
    ///
    /// The [`BasicReceiver`] never holds data regardless of this setting;
    /// disabling only stops it from surfacing synchronization information. The
    /// merging [`Receiver`] is where the holding behavior lives.
    #[must_use]
    pub fn with_synchronization(mut self, enabled: bool) -> Self {
        self.synchronization = enabled;
        self
    }

    /// Whether the receiver acts on packets carrying this START code, per the
    /// allow-list set by [`with_allowed_start_codes`](Self::with_allowed_start_codes)
    /// (default: NULL and per-address priority).
    fn processes(&self, start_code: u8) -> bool {
        self.allowed_start_codes.contains(start_code)
    }

    /// Whether the per-address-priority state machine is actually active. It is
    /// implicitly off when `0xDD` is not in the allow-list, since there are then
    /// no PAP packets to act on - waiting for one would only pointlessly delay a
    /// new source's levels.
    fn pap_active(&self) -> bool {
        self.pap_handling && self.processes(PAP_START_CODE)
    }
}
