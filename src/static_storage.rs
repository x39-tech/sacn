//! Use the [`static_storage!`](crate::static_storage!) to build a
//! fixed-capacity storage policy for the types in this library.

/// Invoke this macro with a set of set user-defined capacities. Using those,
/// the macro defines a zero-sized type and implements the necessary traits for
/// all of the types in this library to use it.
///
/// With the `alloc` feature disabled, this macro and the trait implementations
/// it generates are used by the library's types to build heapless backing
/// containers for the memory they need. The user-defined capacities are
/// expanded into derived capacities as necessary.
///
/// The resulting marker (e.g. `Caps`) is usable as the storage parameter of
/// every core type: `Receiver<Caps>`, `BasicReceiver<Caps>`, `DmxMerger<Caps>`,
/// `Source<Caps>`, and `SourceDetector<Caps>`.
///
/// # User-defined capacities
///
/// All six knobs are required, in this order:
///
/// | Knob                       | Bounds                                             |
/// | -------------------------- | -------------------------------------------------- |
/// | `rx_universes`             | universes a receiver listens to                    |
/// | `rx_sources_per_universe`  | sources tracked on one universe                    |
/// | `rx_sync_addresses`        | synchronization addresses tracked by a receiver    |
/// | `tx_universes`             | universes a source transmits on                    |
/// | `det_sources`              | sources a detector tracks                          |
/// | `det_universes_per_source` | universes one detected source may advertise        |
///
/// Every other capacity used internally is derived from these.
///
/// # Example
///
/// ```
/// sacn::static_storage! {
///     pub struct Caps {
///         rx_universes: 4,
///         rx_sources_per_universe: 8,
///         rx_sync_addresses: 8,
///         tx_universes: 4,
///         det_sources: 5,
///         det_universes_per_source: 5,
///     }
/// }
///
/// let mut rx: sacn::receiver::Receiver<Caps> =
///     sacn::receiver::Receiver::with_config(sacn::receiver::ReceiverConfig::default());
/// let _ = &mut rx;
/// ```
#[macro_export]
macro_rules! static_storage {
    (
        $(#[$attr:meta])*
        $vis:vis struct $name:ident {
            rx_universes: $rx_universes:expr,
            rx_sources_per_universe: $rx_sources_per_universe:expr,
            rx_sync_addresses: $rx_sync_addresses:expr,
            tx_universes: $tx_universes:expr,
            det_sources: $det_sources:expr,
            det_universes_per_source: $det_universes_per_source:expr $(,)?
        }
    ) => {
        $(#[$attr])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        $vis struct $name;

        impl $crate::merger::MergerStorage for $name {
            type MergeSources = $crate::heapless::Vec<
                $crate::merger::MergeSourceEntry,
                { $rx_sources_per_universe },
            >;
            type FreeList =
                $crate::heapless::Vec<$crate::merger::SourceIndex, { $rx_sources_per_universe }>;
        }

        impl $crate::receiver::BasicReceiverStorage for $name {
            type BasicUniverses = $crate::SortedVecMap<
                $crate::Universe,
                $crate::receiver::BasicUniverseState<$name>,
                { $rx_universes },
            >;
            type BasicSources = $crate::SortedVecMap<
                $crate::Cid,
                $crate::receiver::TrackedSource,
                { $rx_sources_per_universe },
            >;
            type TermSets = $crate::heapless::Vec<
                $crate::receiver::TerminationSet<$name>,
                { $rx_sources_per_universe },
            >;
            type TermSetSources = $crate::SortedVecMap<
                $crate::Cid,
                $crate::receiver::TerminationSetSource,
                { $rx_sources_per_universe },
            >;
            type PollKeys = $crate::heapless::Vec<$crate::Universe, { $rx_universes }>;
            type LossList =
                $crate::heapless::Vec<$crate::receiver::LostSource, { $rx_sources_per_universe }>;
            type OfflineScratch =
                $crate::heapless::Vec<($crate::Cid, bool), { $rx_sources_per_universe }>;
            type CidScratch =
                $crate::heapless::Vec<$crate::Cid, { $rx_sources_per_universe }>;
        }

        impl $crate::receiver::ReceiverStorage for $name {
            type Universes = $crate::SortedVecMap<
                $crate::Universe,
                $crate::receiver::UniverseMerge<$name>,
                { $rx_universes },
            >;
            type Sources = $crate::SortedVecMap<
                $crate::Cid,
                $crate::receiver::MergeSource,
                { $rx_sources_per_universe },
            >;
            type SyncAddresses =
                $crate::SortedVecMap<u16, $crate::time::Instant, { $rx_sync_addresses }>;
            type MergeLossList = $crate::heapless::Vec<
                $crate::receiver::MergedLostSource,
                { $rx_sources_per_universe },
            >;
            type SyncReleases = $crate::heapless::Vec<$crate::Universe, { $rx_universes }>;
        }

        impl $crate::source::SourceStorage for $name {
            type TxUniverses = $crate::SortedVecMap<
                $crate::Universe,
                $crate::source::TxUniverseState,
                { $tx_universes },
            >;
            type SyncGroups = $crate::SortedVecMap<
                $crate::Universe,
                $crate::source::SyncGroupState,
                { $tx_universes },
            >;
            type Pending = $crate::heapless::Vec<
                $crate::source::Pending,
                { $tx_universes * 3 + $tx_universes / 512 + 1 },
            >;
            type Removed = $crate::heapless::Vec<$crate::Universe, { $tx_universes }>;
        }

        impl $crate::detector::DetectorStorage for $name {
            type Sources = $crate::SortedVecMap<
                $crate::Cid,
                $crate::detector::DetectedSource<$name>,
                { $det_sources },
            >;
            type Universes = $crate::heapless::Vec<u16, { $det_universes_per_source }>;
            type EventBuffer = $crate::heapless::Vec<
                $crate::detector::SourceDetectorPollEvent,
                { $det_sources },
            >;
        }
    };
}
