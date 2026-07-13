//! The merging receiver.

use core::net::SocketAddr;

use crate::error::Error;
use crate::merger::{DmxMerger, SourceId};
use crate::packet::{Packet, Payload};
use crate::storage::{HeapStorage, MapLike, VecLike};
use crate::time::Instant;
use crate::types::{Cid, NetintId, Priority, SourceName, Universe};

use super::event::{ListenOutcome, SourceInfoRef, StopOutcome, UniverseDataRef};
use super::{
    BasicReceiverCore, BasicReceiverResources, DMX_NULL_START_CODE, LostSource, PAP_START_CODE,
    PacketOutcome, ReceiverConfig, ReceiverStorage, SOURCE_LOSS_TIMEOUT,
};

mod event;

#[cfg(feature = "alloc")]
pub use event::{MergedData, MergedSource, ReceiverEvent};
pub use event::{
    MergedDataRef, MergedLostSource, MergedPacketOutcome, MergedPollOutcome, MergedSourceRef,
    ReceiverPollEvent, SyncRelease,
};

#[cfg(test)]
#[path = "merging_tests.rs"]
mod tests;

// --- Per-source and per-universe state ---------------------------------------

/// Per-source bookkeeping the merging receiver keeps alongside its merger.
#[doc(hidden)]
#[derive(Debug)]
pub struct MergeSource {
    /// Handle into this universe's [`DmxMerger`], allocated lazily the first
    /// time the source contributes levels or per-address priority. A source seen
    /// only via an alternate START code is tracked for its identity (so its name
    /// is known if it is later lost) but holds no merger slot.
    id: Option<SourceId>,
    /// The source's most recent name.
    name: SourceName,
    /// The address the source was last seen at.
    addr: SocketAddr,
    /// The source's most recent universe (packet) priority.
    universe_priority: u8,
    /// Whether the source is currently sending per-address priority.
    pap_active: bool,
    /// Whether the source has ever contributed NULL-start-code levels. A source
    /// that has only ever sent per-address priority owns no slots and is not an
    /// active source.
    levels_active: bool,
    /// The synchronization address declared by the source's most recent data
    /// packet (`0` if unsynchronized).
    sync_address: u16,
    /// The Force_Synchronization bit from the source's most recent synchronized
    /// data packet.
    force_sync: bool,
}

/// The merge state for one listened universe.
#[doc(hidden)]
#[derive(Debug)]
pub struct UniverseMerge<S: ReceiverStorage> {
    merger: DmxMerger<S>,
    /// Live sources on this universe, keyed by CID.
    sources: S::Sources,
    /// Whether the universe is still in its sampling period.
    sampling: bool,
    /// The universe's agreed synchronization address: `Some(s)` when every
    /// live source contributing levels declares the same nonzero `s`, else
    /// `None` (sync is ignored on the universe). Recomputed whenever a source
    /// is added, removed, or changes its declared address.
    agreed_sync: Option<u16>,
    /// The Force_Synchronization policy governing this universe: the bit from
    /// the most recent synchronized data packet. Decides the sync-loss behavior.
    governing_force_sync: bool,
    /// Whether this universe's merged data has changed since the last
    /// [`MergedDataChanged`](ReceiverPollEvent::MergedDataChanged) event was
    /// delivered.
    merged_change_pending: bool,
}

impl<S: ReceiverStorage> UniverseMerge<S> {
    fn new() -> Self {
        Self {
            merger: DmxMerger::default(),
            sources: S::Sources::default(),
            sampling: true,
            agreed_sync: None,
            governing_force_sync: false,
            merged_change_pending: false,
        }
    }

    /// Recomputes the universe's agreed synchronization address from its
    /// level-contributing sources. Returns whether it changed.
    fn recompute_sync_agreement(&mut self) -> bool {
        let mut declared = self
            .sources
            .values()
            .filter(|s| s.levels_active)
            .map(|s| s.sync_address);
        let agreed = match declared.next() {
            Some(first) if first != 0 && declared.all(|s| s == first) => Some(first),
            _ => None,
        };
        let changed = self.agreed_sync != agreed;
        self.agreed_sync = agreed;
        changed
    }

    /// Recomputes the agreed synchronization address after a change in source
    /// membership and refreshes the merger's hold: the universe withholds while
    /// its (possibly new) agreed address is Active. Returns whether it is now
    /// withholding.
    fn refresh_sync_hold(&mut self, sync_addresses: &S::SyncAddresses, now: Instant) -> bool {
        self.recompute_sync_agreement();
        let withhold = self
            .agreed_sync
            .is_some_and(|s| addr_active(sync_addresses, s, now));
        self.merger.set_deferred(withhold);
        withhold
    }

    /// Tracks `cid` as a source on this universe, refreshing its stored name,
    /// address and universe priority. Does not allocate a merger handle: a
    /// source is registered with the merger only once it contributes levels or
    /// per-address priority (see [`merger_handle`](Self::merger_handle)), so a
    /// source sending only an alternate START code is known by identity without
    /// occupying a merger slot.
    ///
    /// Returns whether the source is tracked after the call.
    fn track_source(
        &mut self,
        cid: Cid,
        name: &str,
        addr: SocketAddr,
        priority: u8,
        sync_address: u16,
        force_sync: bool,
    ) -> bool {
        if let Some(src) = self.sources.get_mut(&cid) {
            if src.name != name {
                src.name.set(name);
            }
            src.addr = addr;
            src.universe_priority = priority;
            src.sync_address = sync_address;
            if sync_address != 0 {
                src.force_sync = force_sync;
            }
            return true;
        }
        self.sources
            .upsert(
                cid,
                MergeSource {
                    id: None,
                    name: SourceName::from(name),
                    addr,
                    universe_priority: priority,
                    pap_active: false,
                    levels_active: false,
                    sync_address,
                    force_sync,
                },
            )
            .is_ok()
    }

    /// Returns the source's merger handle, allocating one on first use. The
    /// source must already be tracked via [`track_source`](Self::track_source).
    fn merger_handle(&mut self, cid: Cid) -> SourceId {
        let Self {
            merger, sources, ..
        } = self;
        let src = sources
            .get_mut(&cid)
            .expect("source tracked before merging");
        if let Some(id) = src.id {
            return id;
        }
        // The receiver caps tracked sources per universe below the merger's
        // source capacity, so a tracked source always has room in the merger.
        let id = merger
            .add_source()
            .expect("merger capacity covers the tracked-source limit");
        src.id = Some(id);
        id
    }

    /// Feeds a tracked source's NULL-start-code levels into the merge.
    fn apply_levels(&mut self, cid: Cid, priority: u8, values: &[u8]) {
        let id = self.merger_handle(cid);
        self.merger.update_levels(id, values);
        self.merger
            .update_universe_priority(id, clamp_priority(priority));
        if let Some(src) = self.sources.get_mut(&cid) {
            src.levels_active = true;
        }
    }

    /// Feeds a tracked source's per-address priorities into the merge.
    fn apply_pap(&mut self, cid: Cid, values: &[u8]) {
        let id = self.merger_handle(cid);
        self.merger.update_per_address_priorities(id, values);
        if let Some(src) = self.sources.get_mut(&cid) {
            src.pap_active = true;
        }
    }

    /// Reverts a source to its universe priority after it stopped sending PAP.
    /// Returns whether the source was tracked.
    fn revert_pap(&mut self, cid: Cid) -> bool {
        let Some(src) = self.sources.get_mut(&cid) else {
            return false;
        };
        src.pap_active = false;
        let id = src.id;
        if let Some(id) = id {
            self.merger.remove_per_address_priorities(id);
        }
        true
    }

    /// Applies one data packet to the merge, honoring the synchronization hold.
    ///
    /// Levels and per-address priority are fed into the merger; data carrying any
    /// other START code is returned as `passthrough`. `sync_addresses` and `now`
    /// resolve whether the universe's agreed sync address is currently Active.
    /// Returns whether the merged output changed and any passthrough data.
    fn apply_data<'p>(
        &mut self,
        data: UniverseDataRef<'p>,
        pap_enabled: bool,
        sync_universe: u16,
        force_sync: bool,
        sync_addresses: &S::SyncAddresses,
        now: Instant,
    ) -> (bool, Option<UniverseDataRef<'p>>) {
        let cid = data.source.cid;
        if !self.track_source(
            cid,
            data.source.name,
            data.addr,
            data.priority,
            sync_universe,
            force_sync,
        ) {
            // The source table is full; the basic receiver has already reported
            // the limit. Nothing enters the merge.
            return (false, None);
        }
        if sync_universe != 0 {
            self.governing_force_sync = force_sync;
        }

        let is_levels = data.start_code == DMX_NULL_START_CODE;
        let is_pap = data.start_code == PAP_START_CODE && pap_enabled;

        // Mark a levels source as level-contributing before recomputing
        // agreement, so its declared sync address is counted this frame.
        // (`apply_levels` sets the same flag, but only after the recompute.)
        if is_levels && let Some(src) = self.sources.get_mut(&cid) {
            src.levels_active = true;
        }
        self.recompute_sync_agreement();

        // A universe withholds while its agreed address is Active. Under
        // when force_sync is false, it also keeps withholding after a sync-loss
        // timeout - the address has gone Inactive but we were already
        // withholding and agreement still holds - so the output stays frozen on
        // the last coherent frame until sync resumes, instead of leaking the
        // next live frame.
        let was_withholding = self.merger.is_deferred();
        let hold_last_look = !self.governing_force_sync;
        let withhold = self.agreed_sync.is_some_and(|s| {
            addr_active(sync_addresses, s, now) || (was_withholding && hold_last_look)
        });
        // Set the hold before feeding the merger so a withheld frame never
        // briefly leaks into the output.
        self.merger.set_deferred(withhold);
        let released = was_withholding && !withhold;

        let mut passthrough = None;
        if is_levels {
            self.apply_levels(cid, data.priority, data.values);
        } else if is_pap {
            self.apply_pap(cid, data.values);
        } else {
            // An alternate START code (PAP handling may be off): passthrough.
            passthrough = Some(data);
        }

        let merged_changed = !withhold && (is_levels || is_pap || released);
        (merged_changed, passthrough)
    }

    /// Removes a lost source from the merge. Returns whether it was tracked.
    fn remove_source(&mut self, cid: Cid) -> bool {
        let Some(id) = self.sources.get(&cid).map(|src| src.id) else {
            return false;
        };
        self.sources.remove(&cid);
        if let Some(id) = id {
            self.merger.remove_source(id);
        }
        true
    }

    /// The stored name of a tracked source, if present.
    fn source_name(&self, cid: Cid) -> Option<&str> {
        self.sources.get(&cid).map(|src| src.name.as_str())
    }

    /// Builds a borrowed view of the current merge result.
    fn merged_ref(&self, universe: Universe) -> MergedDataRef<'_, S> {
        let out = self.merger.output();
        MergedDataRef::new(
            universe,
            out.levels(),
            out.priorities(),
            out.owners(),
            &self.sources,
        )
    }

    /// A borrowed identity for a tracked source, if present.
    fn source_info_ref(&self, cid: Cid) -> Option<SourceInfoRef<'_>> {
        self.sources.get(&cid).map(|src| SourceInfoRef {
            cid,
            name: src.name.as_str(),
        })
    }
}

// --- The receiver ------------------------------------------------------------

/// A merging sACN receiver: the top tier of the receive path.
///
/// `Receiver` combines a [`BasicReceiver`](super::BasicReceiver) with a per-universe
/// [`DmxMerger`](crate::merger::DmxMerger). The basic receiver tracks the
/// sources on each universe and forwards their data per-source; the merging
/// receiver feeds that data into a merger and emits a single **merged** result
/// for the universe: the highest-priority level for each slot
/// (Highest-Takes-Precedence among the top priority), the source that owns it,
/// and the list of active sources. See the [DMX merger](crate::merger) for the
/// merge semantics.
///
/// Levels and per-address priorities are routed into the merge; data carrying
/// any other START code is forwarded untouched as a [`UniverseDataRef`]
/// passthrough, exactly as the basic receiver delivers it.
///
/// # Sampling
///
/// Like the basic receiver, the merging receiver opens a sampling period when a
/// universe starts being listened to. While a universe samples, incoming data
/// still updates the merge, but no [`MergedData`](ReceiverEvent::MergedData) is
/// emitted; a single merged result is produced when the period ends. This gives
/// the application a flicker-free first frame.
///
/// # Driving the state machine
///
/// The interface mirrors [`BasicReceiver`](super::BasicReceiver):
/// [`listen`](Self::listen) / [`stop_listening`](Self::stop_listening) register
/// interest, [`handle_packet`](Self::handle_packet) feeds in a parsed packet,
/// and [`poll`](Self::poll) advances time. Outcomes borrow either the packet
/// passed in or buffers held inside the receiver (notably the merger's output);
/// a borrowed outcome must be used and dropped before the next call.
///
/// ```
/// use sacn::{Receiver, ReceiverConfig, NetintId, Universe};
/// use sacn::receiver::{MergedPacketOutcome, ReceiverPollEvent};
/// use sacn::packet::Packet;
/// use sacn::time::Instant;
/// use std::net::SocketAddr;
///
/// let mut receiver = Receiver::new(ReceiverConfig::new());
/// let now = Instant::EPOCH;
/// receiver.listen(now, Universe::new(1).unwrap()).unwrap();
///
/// // Feed in a datagram received from the socket. Levels and per-address
/// // priorities are routed into the universe's merge rather than delivered
/// // per-source.
/// # let datagram = doctest_helper::dmx_datagram();
/// let from: SocketAddr = "192.0.2.10:5568".parse().unwrap();
/// let packet = Packet::parse(&datagram).unwrap();
/// match receiver.handle_packet(now, from, NetintId::UNKNOWN, &packet) {
///     MergedPacketOutcome::Data { merged, .. } => {
///         // `merged` is `Some` once a fresh result is ready; it is withheld
///         // while the universe is still sampling. When present, `merged.levels()`
///         // is the post-merge value for every slot.
///         if let Some(merged) = merged {
///             let _ = merged.levels();
///         }
///     }
///     // Alternate START codes, synchronized releases and capacity limits
///     // arrive as other variants; see `MergedPacketOutcome` for the full set.
///     _ => {}
/// }
///
/// // Advance timers. A poll event does not carry the merged data itself; a
/// // `MergedDataChanged` only names a universe whose result changed. The poll
/// // borrows the receiver for as long as you are draining it, so a result
/// // cannot be looked up mid-drain: record the changed universes, end the
/// // drain, then resolve them.
/// let mut changed = Vec::new();
/// {
///     let mut poll = receiver.poll(now);
///     while let Some(event) = poll.next_event() {
///         match event {
///             ReceiverPollEvent::MergedDataChanged { universe } => changed.push(universe),
///             // `SamplingEnded` and `SourcesLost` are delivered here too; see
///             // `ReceiverPollEvent` for the full set.
///             _ => {}
///         }
///     }
/// }
/// for universe in changed {
///     if let Some(merged) = receiver.merged(universe) {
///         let _ = merged.levels();
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
///
/// # Synchronization
///
/// Receivers support E1.31 universe synchronization (§11 and Appendix B). It
/// is enabled by default (disable with
/// [`ReceiverConfig::with_synchronization`](crate::receiver::ReceiverConfig::with_synchronization)).
/// When enabled, the merging receiver holds a synchronized universe's merged
/// output rather than publishing it as it changes. The held results for every
/// universe sharing a synchronization address are released together when the
/// matching sync packet arrives, delivered as one
/// [`SyncMergedData`](ReceiverEvent::SyncMergedData) event (or
/// [`MergedPacketOutcome::Sync`] from [`handle_packet`](Self::handle_packet)).
/// If the sync stream is lost, the universe follows the source's
/// [`OnSyncLoss`](crate::source::OnSyncLoss) policy: hold the last synchronized
/// look, or revert to live output.
///
/// This holding behavior is exclusive to this type - a
/// [`BasicReceiver`](super::BasicReceiver) only forwards sync packets to its
/// caller and never holds data itself.
///
/// Two deliberate limitations keep the state handling tractable:
///
/// - If several sources on one universe disagree on their synchronization
///   address, synchronization is ignored for that universe until they agree.
///   (Running multiple synchronized sources on one universe is discouraged.)
/// - Only NULL START code (DMX) and per-address priority data are synchronized;
///   alternate START codes are always passed through live.
#[derive(Debug)]
pub struct Receiver<S: ReceiverStorage = HeapStorage> {
    core: ReceiverCore<S>,
    store: ReceiverResources<S>,
}

/// The sACN merging receiver state machine: the receive-path logic, separated
/// from its working memory.
///
/// [`Receiver`] contains one of these as well as a [`ReceiverResources`].
/// Usually, just using [`Receiver`] is the right choice. Use this type alongside
/// [`ReceiverResources`] if you need maximum control of your memory layout;
/// [`ReceiverResources`] contains all of the bulk memory associated with a
/// receiver, and can be const-initialized statically.
///
/// This has all the same functionality as [`Receiver`]; the only difference is
/// that each method takes a mutable reference to a separate [`ReceiverResources`].
/// Each [`ReceiverCore`] should be associated with exactly one
/// [`ReceiverResources`] and you should pass the same [`ReceiverResources`]
/// instance to every call to a [`ReceiverCore`] method.
#[derive(Debug)]
pub struct ReceiverCore<S: ReceiverStorage = HeapStorage> {
    basic: BasicReceiverCore<S>,
}

/// The mutable working memory a [`ReceiverCore`] operates on.
///
/// This struct holds everything about a merging receiver that scales with the
/// number of universes, their tracked sources, and the per-universe merge, so it
/// is the potentially large allocation. It can be constructed in a const
/// expression with statically-allocated storage (see below).
///
/// Most users should just use [`Receiver`] rather than [`ReceiverCore`] and
/// [`ReceiverResources`].
///
/// To construct:
///
/// - **Heap:** construct with [`ReceiverResources::default`].
/// - **Fixed-capacity:** use the [`static_storage!`](crate::static_storage!)
///   macro, which emits a `const fn` `receiver_resources()` returning an empty
///   `ReceiverResources`, suitable for static allocation in a const context.
#[derive(Debug)]
pub struct ReceiverResources<S: ReceiverStorage = HeapStorage> {
    basic: BasicReceiverResources<S>,
    universes: S::Universes,
    sync_addresses: S::SyncAddresses,
    sync_release: S::SyncReleases,
    loss_scratch: S::MergeLossList,
}

#[cfg(feature = "alloc")]
impl Receiver<HeapStorage> {
    /// Creates a heap-backed merging receiver with the given configuration. It
    /// listens to no universes until [`listen`](Self::listen) is called.
    ///
    /// For a fixed-capacity receiver, construct with
    /// `Receiver::<Caps>::with_config(config)` using a policy from
    /// [`static_storage!`](crate::static_storage!).
    pub fn new(config: ReceiverConfig) -> Self {
        Self::with_config(config)
    }
}

impl<S: ReceiverStorage> Receiver<S> {
    /// Creates a merging receiver with the given configuration, backed by the
    /// storage policy `S`. It listens to no universes until
    /// [`listen`](Self::listen) is called.
    pub fn with_config(config: ReceiverConfig) -> Self {
        Self {
            core: ReceiverCore::with_config(config),
            store: ReceiverResources::default(),
        }
    }

    /// The synchronization universes the receiver is currently interested in:
    /// the valid, nonzero addresses declared by any live source. An adapter
    /// generally joins these multicast groups so it can receive their sync
    /// packets.
    ///
    /// The same address may be yielded more than once (once per source that
    /// declares it), so a caller that wants a set should collect into one. The
    /// iterator is empty when synchronization is disabled.
    pub fn sync_group_interest(&self) -> impl Iterator<Item = Universe> + '_ {
        self.core.sync_group_interest(&self.store)
    }

    /// Get the config with which this receiver was created.
    pub fn config(&self) -> &ReceiverConfig {
        self.core.config()
    }

    /// Begins listening for a universe.
    ///
    /// For a universe not yet listened to, this opens a sampling period (a
    /// [`SamplingStarted`](ReceiverEvent::SamplingStarted) event).
    ///
    /// Calling it again for a universe already being listened to is a no-op that
    /// leaves the sampling period and tracked sources untouched (the returned
    /// [`ListenOutcome`] reports no new sampling period).
    pub fn listen(&mut self, now: Instant, universe: Universe) -> Result<ListenOutcome, Error> {
        self.core.listen(&mut self.store, now, universe)
    }

    /// Stops listening for a universe. The returned [`StopOutcome`] reports
    /// whether the universe was being listened to. All merge state for the
    /// universe is discarded.
    pub fn stop_listening(&mut self, universe: Universe) -> StopOutcome {
        self.core.stop_listening(&mut self.store, universe)
    }

    /// Feeds in a parsed packet received from `from`, routing it through the
    /// basic receiver and into the universe's merge.
    ///
    /// A NULL-start-code (levels) or per-address-priority packet updates the
    /// merge and, unless in a sampling period or synchronization is active,
    /// immediately yields a fresh merged frame as [`MergedPacketOutcome::Data`].
    /// A packet with any other START code is forwarded as a
    /// [`MergedPacketOutcome::Passthrough`], and a synchronization packet
    /// as a [`MergedPacketOutcome::Sync`]. The returned outcome borrows the
    /// packet and the receiver's internal merge buffers, so it must be used
    /// and dropped before the next call.
    pub fn handle_packet<'r, 'p>(
        &'r mut self,
        now: Instant,
        from: SocketAddr,
        netint: NetintId,
        packet: &Packet<'p>,
    ) -> MergedPacketOutcome<'r, 'p, S> {
        self.core
            .handle_packet(&mut self.store, now, from, netint, packet)
    }

    /// Advances time to `now`, running the basic receiver's sampling, timeout and
    /// source-loss logic and translating the results into merged poll events.
    ///
    /// When a sampling period ends, or a source loss changes a live universe, a
    /// [`MergedDataChanged`](ReceiverPollEvent::MergedDataChanged) signal is
    /// emitted for the affected universe; resolve it to the borrowed result with
    /// [`MergedPollOutcome::merged`]. Returns the next timer deadline alongside
    /// the events.
    pub fn poll(&mut self, now: Instant) -> MergedPollOutcome<'_, S> {
        self.core.poll(&mut self.store, now)
    }

    /// The current merged result for a universe, or `None` if the universe is
    /// not listened to or is still in its sampling period.
    ///
    /// This borrows the receiver's merger buffers and source table, valid until
    /// the next mutating call. It resolves a
    /// [`MergedDataChanged`](ReceiverPollEvent::MergedDataChanged) event and can
    /// also be queried at any time to read a universe's latest merge.
    #[must_use]
    pub fn merged(&self, universe: Universe) -> Option<MergedDataRef<'_, S>> {
        merged(&self.store.universes, universe)
    }
}

impl<S: ReceiverStorage> ReceiverResources<S> {
    /// Assembles resources from already-constructed (empty) collections.
    ///
    /// Not used directly; used only from [`static_storage!`](crate::static_storage!)
    /// or [`Default::default()`].
    #[doc(hidden)]
    pub const fn from_parts(
        basic: BasicReceiverResources<S>,
        universes: S::Universes,
        sync_addresses: S::SyncAddresses,
        sync_release: S::SyncReleases,
        loss_scratch: S::MergeLossList,
    ) -> Self {
        Self {
            basic,
            universes,
            sync_addresses,
            sync_release,
            loss_scratch,
        }
    }

    /// The universes latched by the most recent synchronization packet, borrowed
    /// from the receiver's reusable scratch. Resolves the frames yielded by
    /// [`SyncRelease::merged_frames`](event::SyncRelease::merged_frames).
    pub(super) fn sync_release(&self) -> &[Universe] {
        self.sync_release.as_slice()
    }
}

impl<S: ReceiverStorage> Default for ReceiverResources<S> {
    /// Empty resources with empty collections. For a fixed-capacity policy this
    /// builds the value at runtime; prefer the macro-generated
    /// `receiver_resources()` `const fn` to place it in static memory without a
    /// stack copy.
    fn default() -> Self {
        Self::from_parts(
            BasicReceiverResources::default(),
            S::Universes::default(),
            S::SyncAddresses::default(),
            S::SyncReleases::default(),
            S::MergeLossList::default(),
        )
    }
}

impl<S: ReceiverStorage> ReceiverCore<S> {
    /// Creates a merging receiver controller with the given configuration, backed
    /// by the storage policy `S`. It listens to no universes until
    /// [`listen`](Self::listen) is called.
    ///
    /// The controller holds only the configuration; its working memory lives in
    /// a separate [`ReceiverResources`] passed to each method. Most users should
    /// use [`Receiver`] instead of [`ReceiverCore`] and [`ReceiverResources`].
    pub fn with_config(config: ReceiverConfig) -> Self {
        let () = super::AssertMergingCoherent::<S>::CHECK;
        let () = crate::merger::AssertCoherent::<S>::CHECK;
        Self {
            basic: BasicReceiverCore::with_config(config),
        }
    }

    /// Get the config with which this receiver was created.
    pub fn config(&self) -> &ReceiverConfig {
        self.basic.config()
    }

    /// The synchronization universes the receiver is currently interested in.
    ///
    /// See [`Receiver::sync_group_interest`].
    pub fn sync_group_interest<'a>(
        &self,
        store: &'a ReceiverResources<S>,
    ) -> impl Iterator<Item = Universe> + 'a {
        let enabled = self.config().synchronization;
        store
            .universes
            .values()
            .flat_map(|um| um.sources.values())
            .filter_map(move |src| {
                enabled
                    .then(|| Universe::new(src.sync_address).ok())
                    .flatten()
            })
    }

    /// Begins listening for a universe.
    ///
    /// See [`Receiver::listen`].
    pub fn listen(
        &self,
        store: &mut ReceiverResources<S>,
        now: Instant,
        universe: Universe,
    ) -> Result<ListenOutcome, Error> {
        let outcome = self.basic.listen(&mut store.basic, now, universe)?;
        if !store.universes.contains_key(&universe) {
            // The basic receiver accepted a new universe and the merge map is
            // asserted at least as large, so it is guaranteed to have room.
            store
                .universes
                .upsert_expect(universe, UniverseMerge::new());
        }
        Ok(outcome)
    }

    /// Stops listening for a universe.
    ///
    /// See [`Receiver::stop_listening`].
    pub fn stop_listening(
        &self,
        store: &mut ReceiverResources<S>,
        universe: Universe,
    ) -> StopOutcome {
        store.universes.remove(&universe);
        self.basic.stop_listening(&mut store.basic, universe)
    }

    /// Feeds in a parsed packet received from `from`.
    ///
    /// See [`Receiver::handle_packet`].
    pub fn handle_packet<'r, 'p>(
        &self,
        store: &'r mut ReceiverResources<S>,
        now: Instant,
        from: SocketAddr,
        netint: NetintId,
        packet: &Packet<'p>,
    ) -> MergedPacketOutcome<'r, 'p, S> {
        let basic = self
            .basic
            .handle_packet(&mut store.basic, now, from, netint, packet);

        let (universe, data, pap_lost) = match basic {
            PacketOutcome::Ignored => return MergedPacketOutcome::Ignored,
            PacketOutcome::Sync { sync_address, .. } => {
                return self.on_sync(store, now, sync_address);
            }
            PacketOutcome::LimitExceeded { universe } => {
                return MergedPacketOutcome::LimitExceeded { universe };
            }
            PacketOutcome::Data {
                universe,
                data,
                pap_lost,
            } => (universe, data, pap_lost),
        };

        let pap_enabled = self.config().pap_active();
        let sync_enabled = self.config().synchronization;

        // Copy the per-source outputs out of the packet-borrowing basic outcome
        // (these borrow the packet, not the receiver).
        let pap_lost_cid = pap_lost.map(|src| src.cid);
        let data: Option<UniverseDataRef<'p>> = data;

        let (sync_universe, force_sync) = match &packet.payload {
            Payload::Data(d) if sync_enabled => (d.sync_address, d.force_sync),
            _ => (0, false),
        };

        let ReceiverResources {
            universes,
            sync_addresses,
            ..
        } = store;
        let um = universes
            .get_mut(&universe)
            .expect("a listened universe has merge state");

        // Apply the packet to the universe's merge, honoring the hold.
        let mut merged_changed = false;
        if let Some(cid) = pap_lost_cid
            && um.revert_pap(cid)
        {
            merged_changed = true;
        }
        let mut passthrough: Option<UniverseDataRef<'p>> = None;
        if let Some(data) = data {
            let (changed, pt) = um.apply_data(
                data,
                pap_enabled,
                sync_universe,
                force_sync,
                sync_addresses,
                now,
            );
            merged_changed |= changed;
            passthrough = pt;
        }

        // Build the outcome from the now-settled merge state.
        let merged = (merged_changed && !um.sampling).then(|| um.merged_ref(universe));

        match passthrough {
            Some(data) => MergedPacketOutcome::Passthrough { data, merged },
            None => {
                let pap_lost = pap_lost_cid.and_then(|cid| um.source_info_ref(cid));
                MergedPacketOutcome::Data {
                    universe,
                    merged,
                    pap_lost,
                }
            }
        }
    }

    /// Advances time to `now`, running the periodic sampling, timeout and
    /// source-loss logic and translating the results into merged poll events.
    ///
    /// See [`Receiver::poll`].
    pub fn poll<'a>(
        &'a self,
        store: &'a mut ReceiverResources<S>,
        now: Instant,
    ) -> MergedPollOutcome<'a, S> {
        let sync_deadline = self.eager_sync_pass(store, now);

        let ReceiverResources {
            basic,
            universes,
            sync_addresses,
            loss_scratch,
            ..
        } = store;
        let basic_outcome = self.basic.poll(basic, now);
        let deadline = fold_deadline(sync_deadline, basic_outcome.deadline);
        MergedPollOutcome::new(
            deadline,
            basic_outcome,
            universes,
            sync_addresses,
            loss_scratch,
            now,
        )
    }

    /// Handles a received synchronization packet on `sync_address`: marks the
    /// address Active (refreshing its 2.5s loss timer) and releases the held
    /// frame of every universe agreed on it.
    ///
    /// The very first sync on an address releases nothing - it only flips the
    /// address Active so that subsequent data starts being withheld. Each later
    /// sync latches the accumulated frame for its agreed universes.
    fn on_sync<'r, 'p>(
        &self,
        store: &'r mut ReceiverResources<S>,
        now: Instant,
        sync_address: u16,
    ) -> MergedPacketOutcome<'r, 'p, S> {
        store.sync_release.clear();
        let was_active = addr_active(&store.sync_addresses, sync_address, now);
        if store
            .sync_addresses
            .upsert(sync_address, now.saturating_add(SOURCE_LOSS_TIMEOUT))
            .is_err()
        {
            return MergedPacketOutcome::SyncLimitExceeded { sync_address };
        }

        // Record each latched universe so the returned `SyncRelease` can read its
        // (uncopied) coherent frame back.
        let ReceiverResources {
            universes,
            sync_release,
            ..
        } = &mut *store;
        for (&universe, um) in universes.iter_mut() {
            if um.agreed_sync != Some(sync_address) {
                continue;
            }
            if was_active {
                // Latch: publish the accumulated coherent frame, but keep
                // withholding subsequent data until the next sync.
                um.merger.recompute();
                if !um.sampling {
                    sync_release.push_expect(universe);
                }
            } else {
                // First sync: begin withholding from now on; release nothing.
                um.merger.set_deferred(true);
            }
        }

        MergedPacketOutcome::Sync(SyncRelease::new(store))
    }

    /// Runs the synchronization-loss timeout pass and returns its contribution to
    /// the poll deadline. An address with no sync for the loss timeout goes
    /// Inactive; each universe agreed on it either reverts to live output
    /// (`force_sync == true`, recording a merged-change) or stays frozen
    /// (`force_sync == false`). Expired addresses are then dropped.
    fn eager_sync_pass(&self, store: &mut ReceiverResources<S>, now: Instant) -> Option<Instant> {
        let ReceiverResources {
            universes,
            sync_addresses,
            ..
        } = store;

        for (&addr, &expiry) in sync_addresses.iter() {
            if now < expiry {
                continue;
            }
            for (_universe, um) in universes.iter_mut() {
                if um.agreed_sync != Some(addr) {
                    continue;
                }
                if um.governing_force_sync {
                    // force_sync == true: publish the accumulated frame and
                    // resume live, unsynchronized output.
                    um.merger.set_deferred(false);
                    if !um.sampling {
                        um.merged_change_pending = true;
                    }
                }
                // force_sync == false: keep the output frozen and keep
                // withholding; silent (bounded by the source-data-loss timer).
            }
        }
        sync_addresses.retain(|_, expiry| now < *expiry);

        // Fold the earliest still-active sync expiry into the deadline.
        let mut deadline = None;
        for &expiry in sync_addresses.values() {
            if expiry > now {
                deadline = Some(match deadline {
                    Some(d) => Instant::min(d, expiry),
                    None => expiry,
                });
            }
        }
        deadline
    }
}

// --- Helpers ----------------------------------------------------------------

/// Clamps a raw wire priority to the valid range, treating an out-of-range
/// (non-conformant) value as the maximum.
fn clamp_priority(raw: u8) -> Priority {
    Priority::new(raw.min(Priority::MAX)).expect("clamped value is in range")
}

/// Whether synchronization address `addr` is Active in `map`: a sync packet was
/// seen on it within the loss timeout.
fn addr_active<M: MapLike<u16, Instant>>(map: &M, addr: u16, now: Instant) -> bool {
    map.get(&addr).is_some_and(|&expiry| now < expiry)
}

/// Combines two poll deadlines, taking the earlier of the two.
fn fold_deadline(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Delivers the merge-side effect of the basic receiver's `SamplingEnded`:
/// finalizes the universe so its accumulated merge becomes its first live
/// result, unless it is synchronized on an active address (then the frame is
/// held for the next sync). Records a pending merged-change when a live result
/// becomes available.
fn deliver_sampling_ended<S: ReceiverStorage>(
    universes: &mut S::Universes,
    sync_addresses: &S::SyncAddresses,
    universe: Universe,
    now: Instant,
) {
    if let Some(um) = universes.get_mut(&universe) {
        um.sampling = false;
        let withhold = um.refresh_sync_hold(sync_addresses, now);
        if !um.sources.is_empty() && !withhold {
            um.merged_change_pending = true;
        }
    }
}

/// Delivers the merge-side effect of the basic receiver's `SourcesLost`:
/// enriches each lost source with its merge-layer name into `scratch`, removes
/// it from the merge, and refreshes the universe's synchronization hold. Records
/// a pending merged-change when the published output changed.
///
/// `scratch` holds the enriched loss list for the returned event to borrow; it
/// is cleared on entry and reused across universes.
fn deliver_sources_lost<S: ReceiverStorage>(
    universes: &mut S::Universes,
    scratch: &mut S::MergeLossList,
    sync_addresses: &S::SyncAddresses,
    universe: Universe,
    sources: &[LostSource],
    now: Instant,
) {
    scratch.clear();
    let Some(um) = universes.get_mut(&universe) else {
        // A universe with no merge state (should not happen, since listen and
        // stop_listening keep the two in step) still yields a loss report named
        // only by CID.
        for source in sources {
            scratch.push_expect(MergedLostSource {
                cid: source.cid,
                name: SourceName::new(),
                terminated: source.terminated,
            });
        }
        return;
    };

    let was_withholding = um.merger.is_deferred();
    let mut removed_any = false;
    for source in sources {
        // Enrich the loss with the name from the merge layer's own source table
        // before removing it.
        scratch.push_expect(MergedLostSource {
            cid: source.cid,
            name: SourceName::from(um.source_name(source.cid).unwrap_or_default()),
            terminated: source.terminated,
        });
        if um.remove_source(source.cid) {
            removed_any = true;
        }
    }
    // A removal can form agreement (start withholding) or dissolve it (release to
    // live); refresh the hold. The published output changed unless it stayed
    // frozen throughout (withholding before and after).
    let withhold = um.refresh_sync_hold(sync_addresses, now);
    if removed_any && !um.sampling && !(was_withholding && withhold) {
        um.merged_change_pending = true;
    }
}

/// Finds the next universe owing a merged-change notification, clearing its flag
/// so the following call finds the next. Returns `None` once all are drained.
/// The signal is payload-free and collapses repeats: several conditions setting
/// one universe's flag in a poll yield a single event.
fn take_pending_change<S: ReceiverStorage>(universes: &mut S::Universes) -> Option<Universe> {
    universes.iter_mut().find_map(|(universe, um)| {
        core::mem::take(&mut um.merged_change_pending).then_some(*universe)
    })
}

/// Gets the merged data for a universe.
fn merged<'a, S: ReceiverStorage + 'a>(
    universes: &'a S::Universes,
    universe: Universe,
) -> Option<MergedDataRef<'a, S>> {
    let um = universes.get(&universe)?;
    if um.sampling {
        return None;
    }
    Some(um.merged_ref(universe))
}
