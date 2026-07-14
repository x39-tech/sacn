//! Fixed-capacity storage for the embassy source adapter.
//!
//! The core [`SourceStorage`](crate::source::SourceStorage) sizes the protocol
//! state machine, but the adapter keeps some state of its own that the core
//! knows nothing about, such as the unicast destinations configured per
//! universe. Those need their own compile-time capacities on a `no_std` target
//! with no allocator.
//!
//! [`SourceStorage`] extends [`crate::source::SourceStorage`] with additional
//! collections and buffers needed by the embassy adapter. [`SourceResources`]
//! is the working memory a source operates on, and
//! [`embassy_static_storage!`](crate::embassy_static_storage!) builds a marker
//! type plus a `const fn` that places an `SourceResources` in static memory.

use embassy_net::IpEndpoint;
use embassy_net::udp::PacketMetadata;

use crate::detector::{
    DetectorStorage as CoreDetectorStorage, SourceDetectorResources as CoreDetectorResources,
};
use crate::receiver::{
    BasicReceiverResources as CoreBasicReceiverResources,
    BasicReceiverStorage as CoreBasicReceiverStorage, ReceiverResources as CoreReceiverResources,
    ReceiverStorage as CoreReceiverStorage,
};
use crate::source::{SourceResources as CoreSourceResources, SourceStorage as CoreSourceStorage};
use crate::storage::{MapLike, VecLike, coherence_check};
use crate::types::Universe;

/// Storage types for [`Source`](crate::embassy::source::Source).
///
/// Use [`embassy_static_storage!`](crate::embassy_static_storage!) to produce
/// a type that implements this trait for statically-allocated storage, or use
/// [`HeapStorage`](crate::HeapStorage) for heap-based storage.
pub trait SourceStorage: CoreSourceStorage {
    /// The per-universe destination tables (unicast endpoints and the universe's
    /// synchronization address), keyed by universe.
    type Destinations: MapLike<Universe, Destinations<Self>>;

    /// The unicast endpoints configured for a single universe.
    type Unicast: VecLike<IpEndpoint>;

    /// A scratch buffer sized to hold the largest set of concrete endpoints a
    /// single packet fans out to: its multicast groups (IPv4 and IPv6) plus its
    /// unicast destinations. A synchronization packet is the worst case, since it
    /// unions the unicast destinations of every universe in its group.
    type SendTargets: VecLike<IpEndpoint>;

    /// A buffer holding every endpoint whose last send is currently failing.
    type FailingTargets: VecLike<IpEndpoint>;

    /// Packet-metadata storage for the socket's transmit ring.
    type TxMeta: AsMut<[PacketMetadata]>;
    /// Payload storage for the socket's transmit ring.
    type TxBuffer: AsMut<[u8]>;
}

coherence_check! {
    /// Capacity coherence assertions for the embassy [`Source`](super::Source).
    AssertEmbassyCoherent<S: SourceStorage> = {
        let universes = <S::Destinations as MapLike<Universe, Destinations<S>>>::CAPACITY;
        let unicast = <S::Unicast as VecLike<IpEndpoint>>::CAPACITY;

        assert!(
            universes
                >= <S::TxUniverses as MapLike<Universe, crate::source::TxUniverseState>>::CAPACITY,
            "embassy SourceStorage::Destinations capacity must be >= core TxUniverses capacity",
        );

        // A single packet reaches its own group (both families) plus, for a sync
        // packet, the union of its members' unicast. With no universes nothing is
        // ever sent, so no multicast groups are needed.
        let single_multicast = if universes == 0 { 0 } else { 2 };
        assert!(
            <S::SendTargets as VecLike<IpEndpoint>>::CAPACITY
                >= unicast.saturating_mul(universes).saturating_add(single_multicast),
            "embassy SourceStorage::SendTargets capacity must hold one packet's fan-out",
        );

        // Across all routes: data groups (2 * universes) + sync groups
        // (2 * universes) + the discovery group (2), each on both families, plus
        // every unicast destination.
        let all_multicast = if universes == 0 {
            0
        } else {
            universes.saturating_mul(4).saturating_add(2)
        };
        assert!(
            <S::FailingTargets as VecLike<IpEndpoint>>::CAPACITY
                >= unicast.saturating_mul(universes).saturating_add(all_multicast),
            "embassy SourceStorage::FailingTargets capacity must hold every endpoint at once",
        );
    }
}

/// The destination state the embassy source adapter tracks for one universe: its
/// unicast endpoints, whether it transmits to multicast, plus the
/// synchronization address it currently advertises (`0` when unsynchronized).
///
/// A universe's multicast groups are derived from the network stack's configured
/// address families when a packet is sent, unless [`multicast`](Self::multicast)
/// is `false` (a unicast-only universe).
#[doc(hidden)]
pub struct Destinations<S: SourceStorage> {
    /// The unicast endpoints this universe is sent to, in addition to any
    /// multicast.
    pub(super) unicast: S::Unicast,
    /// Whether this universe transmits to its multicast group(s).
    pub(super) multicast: bool,
    /// The universe's synchronization address, or `0` if it is not synchronized.
    /// Mirrors the core's per-universe sync address so a [`Route::Sync`] can be
    /// expanded to the unicast destinations of the group's members.
    ///
    /// [`Route::Sync`]: crate::source::Route::Sync
    pub(super) sync_universe: u16,
}

impl<S: SourceStorage> Destinations<S> {
    pub(super) fn new() -> Self {
        Self {
            unicast: S::Unicast::default(),
            multicast: true,
            sync_universe: 0,
        }
    }
}

// Hand-written so the impl does not require `S: Debug`
impl<S: SourceStorage> core::fmt::Debug for Destinations<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self {
            unicast,
            multicast,
            sync_universe,
        } = self;
        f.debug_struct("Destinations")
            .field("unicast", unicast)
            .field("multicast", multicast)
            .field("sync_universe", sync_universe)
            .finish()
    }
}

/// A packet's in-progress delivery to its destinations, plus the cursor that
/// makes resuming an interrupted send cancel safe.
pub(super) struct Fanout<S: SourceStorage> {
    /// The resolved destinations, captured once when the fan-out begins.
    pub(super) targets: S::SendTargets,
    /// The index of the next destination still to send to. Advanced only after
    /// a send completes.
    pub(super) next: usize,
}

impl<S: SourceStorage> core::fmt::Debug for Fanout<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self { targets, next } = self;
        f.debug_struct("Fanout")
            .field("targets", targets)
            .field("next", next)
            .finish()
    }
}

/// The mutable working memory an embassy [`Source`](super::Source) operates on.
///
/// This struct holds everything about a source that scales with parameters
/// like the number of universes, so it is the potentially large allocation.
///
/// To construct:
///
/// - **Fixed-capacity:** use the
///   [`embassy_static_storage!`](crate::embassy_static_storage!) macro, which
///   emits a `const fn` `embassy_source_resources()` returning an empty
///   `SourceResources`, suitable for static allocation in a const context.
/// - **Heap:** construct with [`SourceResources::default`].
pub struct SourceResources<S: SourceStorage> {
    /// The core protocol working memory.
    pub(super) source: CoreSourceResources<S>,
    /// The adapter-owned per-universe destination tables.
    pub(super) destinations: S::Destinations,
    /// The current in-progress packet delivery fanout.
    pub(super) in_flight: Option<Fanout<S>>,
    /// The socket's transmit-ring metadata and payload storage.
    pub(super) tx_meta: S::TxMeta,
    pub(super) tx_buffer: S::TxBuffer,
}

impl<S: SourceStorage> SourceResources<S> {
    /// Assembles the resources from already-constructed (empty) parts.
    ///
    /// Not used directly; used only from
    /// [`embassy_static_storage!`](crate::embassy_static_storage!) or
    /// [`Default::default()`].
    #[doc(hidden)]
    pub const fn from_parts(
        source: CoreSourceResources<S>,
        destinations: S::Destinations,
        tx_meta: S::TxMeta,
        tx_buffer: S::TxBuffer,
    ) -> Self {
        Self {
            source,
            destinations,
            in_flight: None,
            tx_meta,
            tx_buffer,
        }
    }
}

impl<S: SourceStorage> core::fmt::Debug for SourceResources<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The core state and socket buffers are large and would force a
        // spurious `S: Debug` bound.
        f.debug_struct("SourceResources")
            .field("destinations", &self.destinations)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "alloc")]
impl SourceStorage for crate::HeapStorage {
    type Destinations = alloc::collections::BTreeMap<Universe, Destinations<Self>>;
    type Unicast = alloc::vec::Vec<IpEndpoint>;
    type SendTargets = alloc::vec::Vec<IpEndpoint>;
    type FailingTargets = alloc::vec::Vec<IpEndpoint>;
    type TxMeta = [PacketMetadata; 4];
    type TxBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
}

#[cfg(not(feature = "alloc"))]
impl SourceStorage for crate::HeapStorage {
    type Destinations = crate::SortedVecMap<Universe, Destinations<Self>, 0>;
    type Unicast = heapless::Vec<IpEndpoint, 0>;
    type SendTargets = heapless::Vec<IpEndpoint, 0>;
    type FailingTargets = heapless::Vec<IpEndpoint, 0>;
    type TxMeta = [PacketMetadata; 4];
    type TxBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
}

#[cfg(feature = "alloc")]
impl Default for SourceResources<crate::HeapStorage> {
    fn default() -> Self {
        Self::from_parts(
            CoreSourceResources::default(),
            alloc::collections::BTreeMap::new(),
            [PacketMetadata::EMPTY; 4],
            [0u8; crate::packet::MAX_PACKET_SIZE],
        )
    }
}

// --- Receiver storage --------------------------------------------------------

/// Storage types for [`BasicReceiver`](crate::embassy::BasicReceiver).
///
/// Use [`embassy_static_storage!`](crate::embassy_static_storage!) to produce
/// a type that implements this trait for statically-allocated storage, or use
/// [`HeapStorage`](crate::HeapStorage) for heap-based storage.
pub trait BasicReceiverStorage: CoreBasicReceiverStorage {
    /// Packet-metadata storage for the socket's receive ring.
    type RxMeta: AsMut<[PacketMetadata]>;
    /// Payload storage for the socket's receive ring.
    type RxBuffer: AsMut<[u8]>;
    /// The persistent datagram buffer that a received packet is copied into and
    /// parsed from. It must hold the largest possible sACN packet, and it
    /// outlives one `next_event` call so a deferred data event can be rebuilt
    /// from it.
    type RecvBuffer: AsMut<[u8]>;
    /// The per-universe multicast-join and sampling records, keyed by universe.
    type Joined: MapLike<Universe, JoinState>;
}

coherence_check! {
    /// Capacity coherence assertion for the embassy
    /// [`BasicReceiver`](super::BasicReceiver).
    AssertEmbassyBasicReceiverCoherent<S: BasicReceiverStorage> = {
        let universes = <<S as CoreBasicReceiverStorage>::BasicUniverses as MapLike<
            Universe,
            crate::receiver::BasicUniverseState<S>,
        >>::CAPACITY;

        // The adapter's multicast-join map must have room for every universe
        // the core can list, since `listen` records one entry per listened
        // universe.
        assert!(
            <S::Joined as MapLike<Universe, JoinState>>::CAPACITY >= universes,
            "embassy BasicReceiverStorage::Joined capacity must be >= core BasicUniverses capacity",
        );
    }
}

/// Storage types for the embassy merging [`Receiver`](crate::embassy::Receiver).
///
/// Extends the embassy [`BasicReceiverStorage`] and the core
/// [`ReceiverStorage`](crate::receiver::ReceiverStorage) with a join map for the
/// synchronization multicast groups.
pub trait ReceiverStorage: CoreReceiverStorage + BasicReceiverStorage {
    /// The per-synchronization-group multicast-join records, keyed by sync
    /// universe.
    type SyncJoined: MapLike<Universe, JoinState>;
}

coherence_check! {
    /// Capacity coherence assertion for the embassy [`Receiver`](super::Receiver):
    /// the sync-group join map must have room for every synchronization address
    /// the core can track, since `reconcile_sync_groups` records one entry per
    /// joined group with `upsert_expect`. (The `Joined` map is covered by
    /// [`AssertEmbassyBasicReceiverCoherent`], also forced by `Receiver::bind`.)
    AssertEmbassyReceiverCoherent<S: ReceiverStorage> = {
        let sync_addresses =
            <<S as CoreReceiverStorage>::SyncAddresses as MapLike<u16, crate::time::Instant>>::CAPACITY;
        assert!(
            <S::SyncJoined as MapLike<Universe, JoinState>>::CAPACITY >= sync_addresses,
            "embassy ReceiverStorage::SyncJoined capacity must be >= core SyncAddresses capacity",
        );
    }
}

/// The multicast-join and sampling state the embassy receiver tracks for one
/// listened universe (data universe) or one joined synchronization group.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct JoinState {
    /// Whether the IPv6 multicast group was joined.(the IPv4 group is always
    /// joined while the universe is in the map).
    pub(super) joined_v6: bool,
    /// Whether a `SamplingStarted` event is owed for this universe.
    pub(super) sampling_pending: bool,
}

/// The mutable working memory an embassy [`BasicReceiver`](crate::embassy::BasicReceiver)
/// operates on.
///
/// This struct holds everything about a basic receiver that scales with
/// parameters like the number of universes, so it is the potentially large
/// allocation.
///
/// To construct:
///
/// - **Fixed-capacity:** use the
///   [`embassy_static_storage!`](crate::embassy_static_storage!) macro, which
///   emits a `const fn` `embassy_basic_receiver_resources()` returning an empty
///   `BasicRecieverResources`, suitable for static allocation in a const context.
/// - **Heap:** construct with [`BasicReceiverResources::default`].
pub struct BasicReceiverResources<S: BasicReceiverStorage> {
    /// The core protocol working memory.
    pub(super) core: CoreBasicReceiverResources<S>,
    /// The per-universe multicast-join and sampling records.
    pub(super) joined: S::Joined,
    /// The socket's receive-ring metadata and payload storage.
    pub(super) rx_meta: S::RxMeta,
    pub(super) rx_buffer: S::RxBuffer,
    /// The persistent datagram buffer received packets are parsed from.
    pub(super) recv_buffer: S::RecvBuffer,
}

impl<S: BasicReceiverStorage> BasicReceiverResources<S> {
    /// Assembles the resources from already-constructed (empty) parts.
    ///
    /// Not used directly; used only from
    /// [`embassy_static_storage!`](crate::embassy_static_storage!) or
    /// [`Default::default()`].
    #[doc(hidden)]
    pub const fn from_parts(
        core: CoreBasicReceiverResources<S>,
        joined: S::Joined,
        rx_meta: S::RxMeta,
        rx_buffer: S::RxBuffer,
        recv_buffer: S::RecvBuffer,
    ) -> Self {
        Self {
            core,
            joined,
            rx_meta,
            rx_buffer,
            recv_buffer,
        }
    }
}

impl<S: BasicReceiverStorage> core::fmt::Debug for BasicReceiverResources<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BasicReceiverResources")
            .field("joined", &self.joined)
            .finish_non_exhaustive()
    }
}

/// The mutable working memory an embassy merging [`Receiver`](crate::embassy::Receiver)
/// operates on.
///
/// This struct holds everything about a receiver that scales with parameters
/// like the number of universes, so it is the potentially large allocation.
///
/// To construct:
///
/// - **Fixed-capacity:** use the
///   [`embassy_static_storage!`](crate::embassy_static_storage!) macro, which
///   emits a `const fn` `embassy_receiver_resources()` returning an empty
///   `RecieverResources`, suitable for static allocation in a const context.
/// - **Heap:** construct with [`ReceiverResources::default`].
pub struct ReceiverResources<S: ReceiverStorage> {
    /// The core merging working memory (which itself contains the basic
    /// receiver's working memory).
    pub(super) core: CoreReceiverResources<S>,
    /// The per-universe multicast-join and sampling records for data universes.
    pub(super) joined: S::Joined,
    /// The per-sync-group multicast-join records.
    pub(super) sync_joined: S::SyncJoined,
    /// The socket's receive-ring metadata and payload storage.
    pub(super) rx_meta: S::RxMeta,
    pub(super) rx_buffer: S::RxBuffer,
    /// The persistent datagram buffer received packets are parsed from.
    pub(super) recv_buffer: S::RecvBuffer,
}

impl<S: ReceiverStorage> ReceiverResources<S> {
    /// Assembles the resources from already-constructed (empty) parts.
    ///
    /// Not used directly; used only from
    /// [`embassy_static_storage!`](crate::embassy_static_storage!) or
    /// [`Default::default()`].
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub const fn from_parts(
        core: CoreReceiverResources<S>,
        joined: S::Joined,
        sync_joined: S::SyncJoined,
        rx_meta: S::RxMeta,
        rx_buffer: S::RxBuffer,
        recv_buffer: S::RecvBuffer,
    ) -> Self {
        Self {
            core,
            joined,
            sync_joined,
            rx_meta,
            rx_buffer,
            recv_buffer,
        }
    }
}

impl<S: ReceiverStorage> core::fmt::Debug for ReceiverResources<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReceiverResources")
            .field("joined", &self.joined)
            .field("sync_joined", &self.sync_joined)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "alloc")]
impl BasicReceiverStorage for crate::HeapStorage {
    type RxMeta = [PacketMetadata; 4];
    type RxBuffer = alloc::vec::Vec<u8>;
    type RecvBuffer = alloc::vec::Vec<u8>;
    type Joined = alloc::collections::BTreeMap<Universe, JoinState>;
}

#[cfg(feature = "alloc")]
impl ReceiverStorage for crate::HeapStorage {
    type SyncJoined = alloc::collections::BTreeMap<Universe, JoinState>;
}

#[cfg(not(feature = "alloc"))]
impl BasicReceiverStorage for crate::HeapStorage {
    type RxMeta = [PacketMetadata; 4];
    type RxBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
    type RecvBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
    type Joined = crate::SortedVecMap<Universe, JoinState, 0>;
}

#[cfg(not(feature = "alloc"))]
impl ReceiverStorage for crate::HeapStorage {
    type SyncJoined = crate::SortedVecMap<Universe, JoinState, 0>;
}

#[cfg(feature = "alloc")]
impl Default for BasicReceiverResources<crate::HeapStorage> {
    fn default() -> Self {
        Self::from_parts(
            CoreBasicReceiverResources::default(),
            alloc::collections::BTreeMap::new(),
            [PacketMetadata::EMPTY; 4],
            alloc::vec![0u8; crate::packet::MAX_PACKET_SIZE],
            alloc::vec![0u8; crate::packet::MAX_PACKET_SIZE],
        )
    }
}

#[cfg(feature = "alloc")]
impl Default for ReceiverResources<crate::HeapStorage> {
    fn default() -> Self {
        Self::from_parts(
            CoreReceiverResources::default(),
            alloc::collections::BTreeMap::new(),
            alloc::collections::BTreeMap::new(),
            [PacketMetadata::EMPTY; 4],
            alloc::vec![0u8; crate::packet::MAX_PACKET_SIZE],
            alloc::vec![0u8; crate::packet::MAX_PACKET_SIZE],
        )
    }
}

// --- Detector storage --------------------------------------------------------

/// Storage types for the embassy [`SourceDetector`](crate::embassy::SourceDetector).
///
/// Use [`embassy_static_storage!`](crate::embassy_static_storage!) to produce
/// a type that implements this trait for statically-allocated storage, or use
/// [`HeapStorage`](crate::HeapStorage) for heap-based storage.
pub trait DetectorStorage: CoreDetectorStorage {
    /// Packet-metadata storage for the socket's receive ring.
    type RxMeta: AsMut<[PacketMetadata]>;
    /// Payload storage for the socket's receive ring.
    type RxBuffer: AsMut<[u8]>;
    /// The datagram buffer a received packet is copied into and parsed from. It
    /// must hold the largest possible sACN packet.
    type RecvBuffer: AsMut<[u8]>;
}

/// The mutable working memory an embassy
/// [`SourceDetector`](crate::embassy::SourceDetector) operates on.
///
/// This struct holds everything about a detector that scales with the number of
/// tracked sources and their universe lists, so it is the potentially large
/// allocation.
///
/// To construct:
///
/// - **Fixed-capacity:** use the
///   [`embassy_static_storage!`](crate::embassy_static_storage!) macro, which
///   emits a `const fn` `embassy_detector_resources()` returning an empty
///   `DetectorResources`, suitable for static allocation in a const context.
/// - **Heap:** construct with [`DetectorResources::default`].
pub struct DetectorResources<S: DetectorStorage> {
    /// The core discovery-tracking working memory.
    pub(super) detector: CoreDetectorResources<S>,
    /// The socket's receive-ring metadata and payload storage.
    pub(super) rx_meta: S::RxMeta,
    pub(super) rx_buffer: S::RxBuffer,
    /// The datagram buffer received packets are parsed from.
    pub(super) recv_buffer: S::RecvBuffer,
}

impl<S: DetectorStorage> DetectorResources<S> {
    /// Assembles the resources from already-constructed (empty) parts.
    ///
    /// Not used directly; used only from
    /// [`embassy_static_storage!`](crate::embassy_static_storage!) or
    /// [`Default::default()`].
    #[doc(hidden)]
    pub const fn from_parts(
        detector: CoreDetectorResources<S>,
        rx_meta: S::RxMeta,
        rx_buffer: S::RxBuffer,
        recv_buffer: S::RecvBuffer,
    ) -> Self {
        Self {
            detector,
            rx_meta,
            rx_buffer,
            recv_buffer,
        }
    }
}

impl<S: DetectorStorage> core::fmt::Debug for DetectorResources<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The core state and socket buffers are large and would force a
        // spurious `S: Debug` bound.
        f.debug_struct("DetectorResources").finish_non_exhaustive()
    }
}

#[cfg(feature = "alloc")]
impl DetectorStorage for crate::HeapStorage {
    type RxMeta = [PacketMetadata; 4];
    type RxBuffer = alloc::vec::Vec<u8>;
    type RecvBuffer = alloc::vec::Vec<u8>;
}

#[cfg(not(feature = "alloc"))]
impl DetectorStorage for crate::HeapStorage {
    type RxMeta = [PacketMetadata; 4];
    type RxBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
    type RecvBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
}

#[cfg(feature = "alloc")]
impl Default for DetectorResources<crate::HeapStorage> {
    fn default() -> Self {
        Self::from_parts(
            CoreDetectorResources::default(),
            [PacketMetadata::EMPTY; 4],
            alloc::vec![0u8; crate::packet::MAX_PACKET_SIZE],
            alloc::vec![0u8; crate::packet::MAX_PACKET_SIZE],
        )
    }
}

/// Builds a fixed-capacity, allocation-free storage policy for the embassy
/// modules.
///
/// Invoke this macro with a set of set user-defined capacities. Using those,
/// the macro defines a zero-sized type and implements the necessary traits for
/// all of the embassy types to use it.
///
/// The resulting marker (e.g. `Caps`) is used as the storage parameter of every
/// embassy type: `Source<'_, Caps>`, `BasicReceiver<'_, Caps>` and
/// `Receiver<'_, Caps>`. The macro also defines associated `const fn`s such as
/// `Caps::embassy_source_resources()`, `Caps::embassy_basic_receiver_resources()`
/// and `Caps::embassy_receiver_resources()`, which return empty resource
/// structures for the embassy types. This is helpful if you want to place memory
/// resources in a static context like a `ConstStaticCell`.
///
/// # User-defined capacities
///
/// The following user-defined capacities are required, in order. Note that
/// capacities can be zero if you are not using the corresponding module (e.g.
/// the `rx_*` capacities can be set to 0 if you use no receiver types, and the
/// `tx_*` capacities can be set to 0 if you use no `Source` types).
///
/// | Capacity                   | Bounds                                            |
/// | -------------------------- | ------------------------------------------------- |
/// | `rx_universes`             | universes a receiver listens to                   |
/// | `rx_sources_per_universe`  | sources tracked on one universe                   |
/// | `rx_sync_addresses`        | synchronization addresses tracked by a receiver   |
/// | `tx_universes`             | universes the source transmits on                 |
/// | `tx_unicast_per_universe`  | unicast destinations configured on one universe   |
/// | `det_sources`              | sources a detector tracks                         |
/// | `det_universes_per_source` | universes one detected source may advertise       |
///
/// Every other capacity used internally is derived from these.
///
/// There is also a set of optional capacities for the sizes of socket buffers
/// used with [`embassy_net::udp::UdpSocket`]. When omitted, these default to
/// sizes large enough for a single packet generated and/or received by this
/// library. The 'metadata' buffer sizes limit the number of UDP packets that
/// can be present in the buffer at once, and the byte buffer sizes limit the
/// total length of all packets that can be in the buffer at once.
///
/// Each socket uses only one direction: a [`Source`](crate::embassy::Source)
/// only transmits and the receiver types only receive. The unused direction
/// (a source's receive ring, a receiver's transmit ring) is always zero-sized,
/// so `tx_*` sizes only the source's transmit ring and `rx_*` sizes only the
/// receivers' receive ring.
///
/// | Capacity    | Bounds                                        |
/// | ----------- | --------------------------------------------- |
/// | `tx_buffer` | The source's transmit byte buffer             |
/// | `tx_meta`   | The source's transmit metadata buffer         |
/// | `rx_buffer` | The receivers' receive byte buffer            |
/// | `rx_meta`   | The receivers' receive metadata buffer        |
///
/// # Example
///
/// ```
/// sacn::embassy_static_storage! {
///     pub struct Caps {
///         rx_universes: 4,
///         rx_sources_per_universe: 8,
///         rx_sync_addresses: 4,
///         tx_universes: 4,
///         tx_unicast_per_universe: 4,
///         det_sources: 0,
///         det_universes_per_source: 0,
///     }
/// }
///
/// // `embassy_source_resources()` is a `const fn`, so the resources are placed
/// // directly in static memory with no stack copy.
/// static RESOURCES: static_cell::ConstStaticCell<
///     sacn::embassy::SourceResources<Caps>,
/// > = static_cell::ConstStaticCell::new(Caps::embassy_source_resources());
/// let resources = RESOURCES.take();
/// // let source: sacn::embassy::Source<'_, Caps> = Source::new(stack, resources, config)?;
/// ```
#[cfg(feature = "embassy")]
#[macro_export]
macro_rules! embassy_static_storage {
    // Full form: all socket-buffer sizes given explicitly.
    (
        $(#[$attr:meta])*
        $vis:vis struct $name:ident {
            rx_universes: $rx_universes:expr,
            rx_sources_per_universe: $rx_sources:expr,
            rx_sync_addresses: $rx_sync:expr,
            tx_universes: $tx_universes:expr,
            tx_unicast_per_universe: $unicast:expr,
            det_sources: $det_sources:expr,
            det_universes_per_source: $det_universes:expr,
            rx_meta: $rx_meta:expr,
            rx_buffer: $rx_buffer:expr,
            tx_meta: $tx_meta:expr,
            tx_buffer: $tx_buffer:expr $(,)?
        }
    ) => {
        $(#[$attr])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        $vis struct $name;

        $crate::__impl_source_storage!($name, $tx_universes);
        $crate::__impl_receiver_storage!(
            $vis $name,
            $rx_universes,
            $rx_sources,
            $rx_sync,
        );

        impl $crate::embassy::SourceStorage for $name {
            type Destinations = $crate::SortedVecMap<
                $crate::Universe,
                $crate::embassy::Destinations<$name>,
                { $tx_universes },
            >;
            type Unicast = $crate::heapless::Vec<$crate::embassy::IpEndpoint, { $unicast }>;
            type SendTargets = $crate::heapless::Vec<
                $crate::embassy::IpEndpoint,
                { 2 + $unicast * $tx_universes },
            >;
            type FailingTargets = $crate::heapless::Vec<
                $crate::embassy::IpEndpoint,
                { ($unicast + 4) * $tx_universes + 2 },
            >;
            type TxMeta = [$crate::embassy::PacketMetadata; { $tx_meta }];
            type TxBuffer = [u8; { $tx_buffer }];
        }

        impl $crate::embassy::BasicReceiverStorage for $name {
            type RxMeta = [$crate::embassy::PacketMetadata; { $rx_meta }];
            type RxBuffer = [u8; { $rx_buffer }];
            type RecvBuffer = [u8; $crate::packet::MAX_PACKET_SIZE];
            type Joined = $crate::SortedVecMap<
                $crate::Universe,
                $crate::embassy::JoinState,
                { $rx_universes },
            >;
        }

        impl $crate::embassy::ReceiverStorage for $name {
            type SyncJoined = $crate::SortedVecMap<
                $crate::Universe,
                $crate::embassy::JoinState,
                { $rx_sync },
            >;
        }

        impl $crate::detector::DetectorStorage for $name {
            type Sources = $crate::SortedVecMap<
                $crate::Cid,
                $crate::detector::DetectedSource<$name>,
                { $det_sources },
            >;
            type Universes = $crate::heapless::Vec<u16, { $det_universes }>;
            type EventBuffer = $crate::heapless::Vec<
                $crate::detector::SourceDetectorPollEvent,
                { $det_sources },
            >;
        }

        impl $crate::embassy::DetectorStorage for $name {
            type RxMeta = [$crate::embassy::PacketMetadata; { $rx_meta }];
            type RxBuffer = [u8; { $rx_buffer }];
            type RecvBuffer = [u8; $crate::packet::MAX_PACKET_SIZE];
        }

        impl $name {
            /// Construct an empty
            /// [`SourceResources`](crate::embassy::SourceResources)
            /// in a const context.
            ///
            /// The returned value is large, so it's recommended to place it
            /// directly in `const`/`static` storage - e.g. a `ConstStaticCell` -
            /// rather than building it on the stack.
            #[allow(dead_code)]
            #[allow(clippy::large_stack_frames)]
            $vis const fn embassy_source_resources()
            -> $crate::embassy::SourceResources<$name> {
                $crate::embassy::SourceResources::from_parts(
                    $crate::source::SourceResources::from_parts(
                        $crate::SortedVecMap::new(),
                        $crate::SortedVecMap::new(),
                        $crate::heapless::Vec::new(),
                        $crate::heapless::Vec::new(),
                    ),
                    $crate::SortedVecMap::new(),
                    [$crate::embassy::PacketMetadata::EMPTY; { $tx_meta }],
                    [0u8; { $tx_buffer }],
                )
            }

            /// Construct an empty
            /// [`BasicReceiverResources`](crate::embassy::BasicReceiverResources)
            /// in a const context.
            ///
            /// The returned value is large, so it's recommended to place it
            /// directly in `const`/`static` storage - e.g. a `ConstStaticCell` -
            /// rather than building it on the stack.
            #[allow(dead_code)]
            #[allow(clippy::large_stack_frames)]
            $vis const fn embassy_basic_receiver_resources()
            -> $crate::embassy::BasicReceiverResources<$name> {
                $crate::embassy::BasicReceiverResources::from_parts(
                    $name::basic_receiver_resources(),
                    $crate::SortedVecMap::new(),
                    [$crate::embassy::PacketMetadata::EMPTY; { $rx_meta }],
                    [0u8; { $rx_buffer }],
                    [0u8; $crate::packet::MAX_PACKET_SIZE],
                )
            }

            /// Construct an empty
            /// [`ReceiverResources`](crate::embassy::ReceiverResources)
            /// in a const context.
            ///
            /// The returned value is large, so it's recommended to place it
            /// directly in `const`/`static` storage - e.g. a `ConstStaticCell` -
            /// rather than building it on the stack.
            #[allow(dead_code)]
            #[allow(clippy::large_stack_frames)]
            $vis const fn embassy_receiver_resources()
            -> $crate::embassy::ReceiverResources<$name> {
                $crate::embassy::ReceiverResources::from_parts(
                    $name::receiver_resources(),
                    $crate::SortedVecMap::new(),
                    $crate::SortedVecMap::new(),
                    [$crate::embassy::PacketMetadata::EMPTY; { $rx_meta }],
                    [0u8; { $rx_buffer }],
                    [0u8; $crate::packet::MAX_PACKET_SIZE],
                )
            }

            /// Construct an empty
            /// [`DetectorResources`](crate::embassy::DetectorResources)
            /// in a const context.
            ///
            /// The returned value is large, so it's recommended to place it
            /// directly in `const`/`static` storage - e.g. a `ConstStaticCell` -
            /// rather than building it on the stack.
            #[allow(dead_code)]
            #[allow(clippy::large_stack_frames)]
            $vis const fn embassy_detector_resources()
            -> $crate::embassy::DetectorResources<$name> {
                $crate::embassy::DetectorResources::from_parts(
                    $crate::detector::SourceDetectorResources::from_parts(
                        $crate::SortedVecMap::new(),
                        $crate::heapless::Vec::new(),
                    ),
                    [$crate::embassy::PacketMetadata::EMPTY; { $rx_meta }],
                    [0u8; { $rx_buffer }],
                    [0u8; $crate::packet::MAX_PACKET_SIZE],
                )
            }
        }
    };

    // Short form: default the socket-buffer sizes to single-packet storage.
    (
        $(#[$attr:meta])*
        $vis:vis struct $name:ident {
            rx_universes: $rx_universes:expr,
            rx_sources_per_universe: $rx_sources:expr,
            rx_sync_addresses: $rx_sync:expr,
            tx_universes: $tx_universes:expr,
            tx_unicast_per_universe: $unicast:expr,
            det_sources: $det_sources:expr,
            det_universes_per_source: $det_universes:expr $(,)?
        }
    ) => {
        $crate::embassy_static_storage! {
            $(#[$attr])*
            $vis struct $name {
                rx_universes: $rx_universes,
                rx_sources_per_universe: $rx_sources,
                rx_sync_addresses: $rx_sync,
                tx_universes: $tx_universes,
                tx_unicast_per_universe: $unicast,
                det_sources: $det_sources,
                det_universes_per_source: $det_universes,
                rx_meta: 4,
                rx_buffer: $crate::packet::MAX_PACKET_SIZE,
                tx_meta: 4,
                tx_buffer: $crate::packet::MAX_PACKET_SIZE,
            }
        }
    };
}
