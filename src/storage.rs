//! Storage policies: the abstraction that lets the protocol core run either on
//! heap-backed collections or on fixed-capacity, allocation-free ones.
//!
//! **You generally do not need to understand the types in this module to
//! use the library. If you are running with `alloc`, this module is completely
//! irrelevant. If you are running without `alloc`, use
//! [`static_storage!`](crate::static_storage!) to abstract away the types in
//! this module.**
//!
//! The core state machines ([`Receiver`](crate::receiver::Receiver),
//! [`DmxMerger`](crate::merger::DmxMerger), [`Source`](crate::source::Source),
//! [`SourceDetector`](crate::detector::SourceDetector)) are generic over a
//! storage policy `S`. A policy names, for each population-dependent collection
//! the core keeps, a concrete backing type that satisfies one of the two traits
//! [`MapLike`] and [`VecLike`].
//!
//! Two families of backing are provided:
//!
//! - [`HeapStorage`] (with the `alloc` feature) binds every collection to a
//!   growable heap type ([`BTreeMap`](alloc::collections::BTreeMap) /
//!   [`Vec`]); inserts never fail. This is the default policy
//!   for every public core type.
//! - A fixed-capacity policy, produced by the
//!   [`static_storage!`](crate::static_storage!) macro, binds every collection to
//!   a [`SortedVecMap`] or a [`heapless::Vec`] with a compile-time capacity, so
//!   the core runs with no allocator at all.
//!
//! The two modes are behaviorally identical and share the same tests.

/// Used to conveniently define a compile-time coherence assertion over a storage
/// policy `S`. This is used to assert that derived capacities are sane.
macro_rules! coherence_check {
    (
        $(#[$meta:meta])*
        $name:ident<$S:ident: $bound:path> = $body:block
    ) => {
        $(#[$meta])*
        pub(crate) struct $name<$S>(core::marker::PhantomData<$S>);

        impl<$S: $bound> $name<$S> {
            pub(crate) const CHECK: () = $body;
        }
    };
}
pub(crate) use coherence_check;

/// A sorted key-value collection.
///
/// Both backings ([`BTreeMap`](alloc::collections::BTreeMap) and
/// [`SortedVecMap`]) keep entries sorted ascending by key, so [`iter`](Self::iter)
/// and [`iter_mut`](Self::iter_mut) always visit keys in ascending order.
pub trait MapLike<K, V>: Default + core::fmt::Debug {
    /// The maximum number of entries this backing can hold, or [`usize::MAX`] for
    /// an unbounded (heap) backing.
    const CAPACITY: usize;

    /// The number of entries.
    fn len(&self) -> usize;

    /// Whether the map is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether an entry exists for `key`.
    fn contains_key(&self, key: &K) -> bool;

    /// A shared reference to the value for `key`, if present.
    fn get(&self, key: &K) -> Option<&V>;

    /// A mutable reference to the value for `key`, if present.
    fn get_mut(&mut self, key: &K) -> Option<&mut V>;

    /// Inserts or overwrites the value for `key`.
    ///
    /// Returns `Err(value)` when a fixed-capacity backing is full and `key` is
    /// not already present. Overwriting an existing key never fails.
    fn upsert(&mut self, key: K, value: V) -> Result<(), V>;

    /// Inserts or overwrites the value for `key`, panicking if the capacity is
    /// full and `key` is not already present.
    #[track_caller]
    fn upsert_expect(&mut self, key: K, value: V) {
        if self.upsert(key, value).is_err() {
            panic!("derived map overflowed; a storage coherence invariant guarantees room");
        }
    }

    /// Removes the entry for `key`, returning whether one was present.
    fn remove(&mut self, key: &K) -> bool;

    /// Retains only the entries for which `f` returns `true`, visiting keys in
    /// ascending order.
    fn retain(&mut self, f: impl FnMut(&K, &V) -> bool);

    /// Iterates over `(key, value)` pairs in ascending key order.
    fn iter<'a>(&'a self) -> impl Iterator<Item = (&'a K, &'a V)>
    where
        K: 'a,
        V: 'a;

    /// Iterates over `(key, &mut value)` pairs in ascending key order.
    fn iter_mut<'a>(&'a mut self) -> impl Iterator<Item = (&'a K, &'a mut V)>
    where
        K: 'a,
        V: 'a;

    /// Iterates over the values in ascending key order.
    fn values<'a>(&'a self) -> impl Iterator<Item = &'a V>
    where
        K: 'a,
        V: 'a,
    {
        self.iter().map(|(_, v)| v)
    }

    /// Iterates over mutable references to the values in ascending key order.
    fn values_mut<'a>(&'a mut self) -> impl Iterator<Item = &'a mut V>
    where
        K: 'a,
        V: 'a,
    {
        self.iter_mut().map(|(_, v)| v)
    }
}

/// A growable list.
pub trait VecLike<T>: Default + core::fmt::Debug {
    /// The maximum number of elements this backing can hold, or [`usize::MAX`] for
    /// an unbounded (heap) backing.
    const CAPACITY: usize;

    /// The number of elements.
    fn len(&self) -> usize;

    /// Whether the list is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `value`. Returns `Err(value)` when a fixed-capacity backing is
    /// full.
    fn push(&mut self, value: T) -> Result<(), T>;

    /// Appends `value`.
    ///
    /// # Panics
    ///
    /// Panics if a fixed-capacity backing is full.
    #[track_caller]
    fn push_expect(&mut self, value: T) {
        if self.push(value).is_err() {
            panic!("derived list overflowed; a storage coherence invariant guarantees room");
        }
    }

    /// Inserts `value` at `index`, shifting later elements right. Returns
    /// `Err(value)` when a fixed-capacity backing is full.
    ///
    /// # Panics
    ///
    /// Panics if `index > len`.
    fn insert(&mut self, index: usize, value: T) -> Result<(), T>;

    /// Removes and returns the element at `index`, shifting later elements left.
    ///
    /// # Panics
    ///
    /// Panics if `index >= len`.
    fn remove(&mut self, index: usize) -> T;

    /// Removes and returns the last element, if any.
    fn pop(&mut self) -> Option<T>;

    /// Empties the list.
    fn clear(&mut self);

    /// Retains only the elements for which `f` returns `true`, in order.
    fn retain(&mut self, f: impl FnMut(&T) -> bool);

    /// The elements as a slice.
    fn as_slice(&self) -> &[T];

    /// The elements as a mutable slice.
    fn as_mut_slice(&mut self) -> &mut [T];

    /// Iterates over shared references to the elements, in order.
    fn iter<'a>(&'a self) -> core::slice::Iter<'a, T>
    where
        T: 'a,
    {
        self.as_slice().iter()
    }

    /// Iterates over mutable references to the elements, in order.
    fn iter_mut<'a>(&'a mut self) -> core::slice::IterMut<'a, T>
    where
        T: 'a,
    {
        self.as_mut_slice().iter_mut()
    }
}

// --- Fixed-capacity map backing ---------------------------------------------

/// A fixed-capacity, allocation-free [`MapLike`] backing: a sorted
/// [`heapless::Vec`] of `(key, value)` pairs with binary-search insert.
///
/// Entries are kept sorted ascending by key, so iteration is ascending and
/// lookups are `O(log N)`.
#[derive(Clone)]
pub struct SortedVecMap<K, V, const N: usize> {
    entries: heapless::Vec<(K, V), N>,
}

impl<K, V, const N: usize> SortedVecMap<K, V, N> {
    /// Create a new map.
    pub const fn new() -> Self {
        Self {
            entries: heapless::Vec::new(),
        }
    }
}

impl<K, V, const N: usize> Default for SortedVecMap<K, V, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord, V, const N: usize> SortedVecMap<K, V, N> {
    /// Returns the index of `key`, or the index it would be inserted at.
    fn search(&self, key: &K) -> Result<usize, usize> {
        self.entries.binary_search_by(|(k, _)| k.cmp(key))
    }
}

impl<K: Ord, V, const N: usize> core::fmt::Debug for SortedVecMap<K, V, N>
where
    K: core::fmt::Debug,
    V: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_map()
            .entries(self.entries.iter().map(|(k, v)| (k, v)))
            .finish()
    }
}

impl<K, V, const N: usize> MapLike<K, V> for SortedVecMap<K, V, N>
where
    K: Ord + core::fmt::Debug,
    V: core::fmt::Debug,
{
    const CAPACITY: usize = N;

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn contains_key(&self, key: &K) -> bool {
        self.search(key).is_ok()
    }

    fn get(&self, key: &K) -> Option<&V> {
        match self.search(key) {
            Ok(i) => Some(&self.entries[i].1),
            Err(_) => None,
        }
    }

    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        match self.search(key) {
            Ok(i) => Some(&mut self.entries[i].1),
            Err(_) => None,
        }
    }

    fn upsert(&mut self, key: K, value: V) -> Result<(), V> {
        match self.search(&key) {
            Ok(i) => {
                self.entries[i].1 = value;
                Ok(())
            }
            Err(i) => match self.entries.insert(i, (key, value)) {
                Ok(()) => Ok(()),
                Err((_, value)) => Err(value),
            },
        }
    }

    fn remove(&mut self, key: &K) -> bool {
        match self.search(key) {
            Ok(i) => {
                self.entries.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    fn retain(&mut self, mut f: impl FnMut(&K, &V) -> bool) {
        self.entries.retain(|(k, v)| f(k, v));
    }

    fn iter<'a>(&'a self) -> impl Iterator<Item = (&'a K, &'a V)>
    where
        K: 'a,
        V: 'a,
    {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    fn iter_mut<'a>(&'a mut self) -> impl Iterator<Item = (&'a K, &'a mut V)>
    where
        K: 'a,
        V: 'a,
    {
        self.entries.iter_mut().map(|(k, v)| (&*k, v))
    }
}

// --- Fixed-capacity vec backing ---------------------------------------------

impl<T: core::fmt::Debug, const N: usize> VecLike<T> for heapless::Vec<T, N> {
    const CAPACITY: usize = N;

    fn len(&self) -> usize {
        let slice: &[T] = self;
        slice.len()
    }

    fn push(&mut self, value: T) -> Result<(), T> {
        self.push(value)
    }

    fn insert(&mut self, index: usize, value: T) -> Result<(), T> {
        self.insert(index, value)
    }

    fn remove(&mut self, index: usize) -> T {
        self.remove(index)
    }

    fn pop(&mut self) -> Option<T> {
        self.pop()
    }

    fn clear(&mut self) {
        self.clear();
    }

    fn retain(&mut self, f: impl FnMut(&T) -> bool) {
        self.retain(f);
    }

    fn as_slice(&self) -> &[T] {
        self
    }

    fn as_mut_slice(&mut self) -> &mut [T] {
        self
    }
}

// --- Heap backings ----------------------------------------------------------

#[cfg(feature = "alloc")]
impl<K, V> MapLike<K, V> for alloc::collections::BTreeMap<K, V>
where
    K: Ord + core::fmt::Debug,
    V: core::fmt::Debug,
{
    const CAPACITY: usize = usize::MAX;

    fn len(&self) -> usize {
        Self::len(self)
    }

    fn contains_key(&self, key: &K) -> bool {
        Self::contains_key(self, key)
    }

    fn get(&self, key: &K) -> Option<&V> {
        Self::get(self, key)
    }

    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        Self::get_mut(self, key)
    }

    fn upsert(&mut self, key: K, value: V) -> Result<(), V> {
        self.insert(key, value);
        Ok(())
    }

    fn remove(&mut self, key: &K) -> bool {
        Self::remove(self, key).is_some()
    }

    fn retain(&mut self, mut f: impl FnMut(&K, &V) -> bool) {
        Self::retain(self, |k, v| f(k, v));
    }

    fn iter<'a>(&'a self) -> impl Iterator<Item = (&'a K, &'a V)>
    where
        K: 'a,
        V: 'a,
    {
        Self::iter(self)
    }

    fn iter_mut<'a>(&'a mut self) -> impl Iterator<Item = (&'a K, &'a mut V)>
    where
        K: 'a,
        V: 'a,
    {
        Self::iter_mut(self)
    }
}

#[cfg(feature = "alloc")]
impl<T: core::fmt::Debug> VecLike<T> for alloc::vec::Vec<T> {
    const CAPACITY: usize = usize::MAX;

    fn len(&self) -> usize {
        Self::len(self)
    }

    fn push(&mut self, value: T) -> Result<(), T> {
        Self::push(self, value);
        Ok(())
    }

    fn insert(&mut self, index: usize, value: T) -> Result<(), T> {
        Self::insert(self, index, value);
        Ok(())
    }

    fn remove(&mut self, index: usize) -> T {
        Self::remove(self, index)
    }

    fn pop(&mut self) -> Option<T> {
        Self::pop(self)
    }

    fn clear(&mut self) {
        Self::clear(self);
    }

    fn retain(&mut self, f: impl FnMut(&T) -> bool) {
        Self::retain(self, f);
    }

    fn as_slice(&self) -> &[T] {
        self
    }

    fn as_mut_slice(&mut self) -> &mut [T] {
        self
    }
}

/// The heap-backed storage policy: every collection grows dynamically and
/// inserts never fail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStorage;

/// Build a fixed-capacity, allocation-free storage policy for core modules
/// in this library.
///
/// Invoke this macro with a set of set user-defined capacities. Using those,
/// the macro defines a zero-sized type and implements the necessary traits for
/// all of the core types in this library to use it.
///
/// The resulting marker (e.g. `Caps`) is usable as the storage parameter of
/// every core type: `Receiver<Caps>`, `BasicReceiver<Caps>`, `DmxMerger<Caps>`,
/// `Source<Caps>`, and `SourceDetector<Caps>`. The macro also defines
/// associated `const fn`s such as `Caps::source_resources()`, which return
/// empty resource structures for core types. This is helpful if you want to
/// place memory resources in a static context like a `ConstStaticCell`.
///
/// # User-defined capacities
///
/// The following user-defined capacities are required, in order. Note that
/// capacities can be zero if you are not using the corresponding module (e.g.
/// `rx_*` can be set to 0 if you do not use any `Receiver` types).
///
/// | Capacity                   | Bounds                                             |
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

        $crate::__impl_source_storage!($name, $tx_universes);

        impl $name {
            /// Construct a [`SourceResources`](sacn::source::SourceResources)
            /// in a const context.
            ///
            /// The returned value is large, so it's recommended to place it
            /// directly in `const`/`static` storage - e.g. a `ConstStaticCell` -
            /// rather than building it on the stack.
            #[allow(dead_code)]
            #[allow(clippy::large_stack_frames)]
            $vis const fn source_resources() -> $crate::source::SourceResources<$name> {
                $crate::source::SourceResources::from_parts(
                    $crate::SortedVecMap::new(),
                    $crate::SortedVecMap::new(),
                    $crate::heapless::Vec::new(),
                    $crate::heapless::Vec::new(),
                )
            }
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

/// Implements [`SourceStorage`](crate::source::SourceStorage) for a marker type,
/// sizing every derived collection from the `tx_universes` capacity.
///
/// This is an implementation detail macro used by other macros and is not
/// intended to be called directly.
#[doc(hidden)]
#[macro_export]
macro_rules! __impl_source_storage {
    ($name:ident, $tx_universes:expr) => {
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
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    type Map = SortedVecMap<u16, u32, 4>;

    #[test]
    fn upsert_overwrites_and_reports_full() {
        let mut m = Map::default();
        assert!(m.upsert(3, 30).is_ok());
        assert!(m.upsert(1, 10).is_ok());
        assert!(m.upsert(2, 20).is_ok());
        // Overwrite existing key: allowed even when it does not grow.
        assert!(m.upsert(2, 22).is_ok());
        assert_eq!(m.get(&2), Some(&22));
        assert!(m.upsert(4, 40).is_ok());
        // Full: a new key fails, but overwriting a present key still works.
        assert_eq!(m.upsert(5, 50), Err(50));
        assert!(m.upsert(3, 33).is_ok());
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn iterates_ascending_after_random_insert() {
        let mut m = Map::default();
        for k in [4u16, 1, 3, 2] {
            m.upsert(k, u32::from(k) * 10).unwrap();
        }
        let keys: heapless::Vec<u16, 4> = m.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn remove_and_contains() {
        let mut m = Map::default();
        m.upsert(1, 10).unwrap();
        m.upsert(2, 20).unwrap();
        assert!(m.contains_key(&1));
        assert!(m.remove(&1));
        assert!(!m.remove(&1));
        assert!(!m.contains_key(&1));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn retain_keeps_ascending() {
        let mut m = Map::default();
        for k in [1u16, 2, 3, 4] {
            m.upsert(k, u32::from(k)).unwrap();
        }
        m.retain(|k, _| k % 2 == 0);
        let keys: heapless::Vec<u16, 4> = m.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys.as_slice(), &[2, 4]);
    }

    #[test]
    fn vec_like_insert_remove() {
        let mut v: heapless::Vec<u16, 4> = heapless::Vec::new();
        VecLike::push(&mut v, 1).unwrap();
        VecLike::push(&mut v, 3).unwrap();
        VecLike::insert(&mut v, 1, 2).unwrap();
        assert_eq!(v.as_slice(), &[1, 2, 3]);
        assert_eq!(VecLike::remove(&mut v, 0), 1);
        assert_eq!(v.as_slice(), &[2, 3]);
    }
}
