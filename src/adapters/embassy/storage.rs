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

    /// Packet-metadata storage for the socket's receive ring.
    type RxMeta: AsMut<[PacketMetadata]>;
    /// Payload storage for the socket's receive ring.
    type RxBuffer: AsMut<[u8]>;
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
    /// The socket's receive-ring metadata and payload storage. A source only
    /// transmits, but `UdpSocket::new` requires receive buffers all the same.
    pub(super) rx_meta: S::RxMeta,
    pub(super) rx_buffer: S::RxBuffer,
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
        rx_meta: S::RxMeta,
        rx_buffer: S::RxBuffer,
        tx_meta: S::TxMeta,
        tx_buffer: S::TxBuffer,
    ) -> Self {
        Self {
            source,
            destinations,
            in_flight: None,
            rx_meta,
            rx_buffer,
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
    type RxMeta = [PacketMetadata; 4];
    type RxBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
    type TxMeta = [PacketMetadata; 4];
    type TxBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
}

#[cfg(not(feature = "alloc"))]
impl SourceStorage for crate::HeapStorage {
    type Destinations = crate::SortedVecMap<Universe, Destinations<Self>, 0>;
    type Unicast = heapless::Vec<IpEndpoint, 0>;
    type SendTargets = heapless::Vec<IpEndpoint, 0>;
    type FailingTargets = heapless::Vec<IpEndpoint, 0>;
    type RxMeta = [PacketMetadata; 4];
    type RxBuffer = [u8; crate::packet::MAX_PACKET_SIZE];
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
            [PacketMetadata::EMPTY; 4],
            [0u8; crate::packet::MAX_PACKET_SIZE],
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
/// The resulting marker (e.g. `Caps`) is used as the storage parameter of each
/// embassy type (currently just `Source<'_, Caps>`). The macro also defines
/// associated `const fn`s such as `Caps::embassy_source_resources()`, which
/// return empty resource structures for the embassy types. This is helpful if
/// you want to place memory resources in a static context like a
/// `ConstStaticCell`.
///
/// # User-defined capacities
///
/// The following user-defined capacities are required, in order. Note that
/// capacities can be zero if you are not using the corresponding module (e.g.
/// tx_* can be set to 0 if you do not use any `Source` types).
///
/// | Capacity                  | Bounds                                             |
/// | ------------------------- | -------------------------------------------------- |
/// | `tx_universes`            | universes the source transmits on                  |
/// | `tx_unicast_per_universe` | unicast destinations configured on one universe    |
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
/// | Capacity    | Bounds                                |
/// | ----------- | ------------------------------------- |
/// | `tx_buffer` | The socket's transmit byte buffer     |
/// | `tx_meta`   | The socket's transmit metadata buffer |
/// | `rx_buffer` | The socket's receive byte buffer      |
/// | `rx_meta`   | The socket's receive metadata buffer  |
///
/// # Example
///
/// ```
/// sacn::embassy_static_storage! {
///     pub struct Caps {
///         tx_universes: 4,
///         tx_unicast_per_universe: 4,
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
            tx_universes: $tx_universes:expr,
            tx_unicast_per_universe: $unicast:expr,
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
            type RxMeta = [$crate::embassy::PacketMetadata; { $rx_meta }];
            type RxBuffer = [u8; { $rx_buffer }];
            type TxMeta = [$crate::embassy::PacketMetadata; { $tx_meta }];
            type TxBuffer = [u8; { $tx_buffer }];
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
                    [$crate::embassy::PacketMetadata::EMPTY; { $rx_meta }],
                    [0u8; { $rx_buffer }],
                    [$crate::embassy::PacketMetadata::EMPTY; { $tx_meta }],
                    [0u8; { $tx_buffer }],
                )
            }
        }
    };

    // Short form: default the socket-buffer sizes to single-packet storage.
    (
        $(#[$attr:meta])*
        $vis:vis struct $name:ident {
            tx_universes: $tx_universes:expr,
            tx_unicast_per_universe: $unicast:expr $(,)?
        }
    ) => {
        $crate::embassy_static_storage! {
            $(#[$attr])*
            $vis struct $name {
                tx_universes: $tx_universes,
                tx_unicast_per_universe: $unicast,
                rx_meta: 4,
                rx_buffer: $crate::packet::MAX_PACKET_SIZE,
                tx_meta: 4,
                tx_buffer: $crate::packet::MAX_PACKET_SIZE,
            }
        }
    };
}
