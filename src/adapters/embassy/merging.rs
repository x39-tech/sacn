//! The embassy merging-receiver adapter.

use core::fmt;
use core::net::SocketAddr;

use embassy_futures::select::{Either, select};
use embassy_net::udp::UdpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_time::Instant as EmbassyInstant;

use crate::packet::Packet;
use crate::proto::{SACN_PORT, ipv4_multicast, ipv6_multicast};
use crate::receiver::{
    MergedDataRef, MergedPacketOutcome, MergedPollOutcome, ReceiverConfig, ReceiverCore,
    ReceiverEventRef, ReceiverPollEvent, ReceiverResources as CoreResources,
};
use crate::storage::MapLike;
use crate::time::Instant;
use crate::types::{NetintId, Universe};

use super::basic::source_addr;
use super::error::EmbassyError;
use super::storage::{JoinState, ReceiverResources, ReceiverStorage};
use super::{from_embassy_duration, to_embassy_duration};

#[cfg(test)]
#[path = "merging_tests.rs"]
mod tests;

/// An asynchronous, `no_std` sACN merging receiver driven by the embassy runtime.
///
/// Construct one with [`bind`](Self::bind), register universes with
/// [`listen`](Self::listen), and consume merged notifications with
/// [`next_event`](Self::next_event).
///
/// This wraps the [`crate::receiver::Receiver`] core, which documents the merge
/// and synchronization semantics. Its working memory, including the embassy
/// stack's socket buffers, lives in a separate [`ReceiverResources`] borrowed
/// for the receiver's whole lifetime. This type can be given fixed, heapless
/// limits and const-constructed in static memory using the macro
/// [`embassy_static_storage!`](crate::embassy_static_storage!).
///
/// ```no_run
/// use sacn::embassy::{Receiver, ReceiverResources};
/// use sacn::{ReceiverConfig, Universe, ReceiverEventRef};
/// use static_cell::ConstStaticCell;
///
/// sacn::embassy_static_storage! {
///     pub struct Caps {
///         rx_universes: 4,
///         rx_sources_per_universe: 8,
///         rx_sync_addresses: 4,
///         tx_universes: 0,
///         tx_unicast_per_universe: 0,
///     }
/// }
///
/// static RESOURCES: ConstStaticCell<ReceiverResources<Caps>> =
///     ConstStaticCell::new(Caps::embassy_receiver_resources());
///
/// # async fn demo(stack: embassy_net::Stack<'static>) -> Result<(), sacn::embassy::EmbassyError> {
/// let resources = RESOURCES.take();
/// // `stack` is your already-initialized `embassy_net::Stack`.
/// let mut rx = Receiver::bind(stack, resources, ReceiverConfig::new())?;
/// rx.listen(Universe::new(1).unwrap())?;
/// while let Some(event) = rx.next_event().await {
///     match event {
///         ReceiverEventRef::MergedData(merged) => {
///             // The post-merge value for every slot on `merged.universe`.
///             let _ = merged.levels();
///         }
///         ReceiverEventRef::SourcesLost { universe, .. } => { let _ = universe; }
///         _ => {}
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Note on stack resources
///
/// Your [`embassy_net::Stack`], which is based on `smoltcp`, must be
/// configured with storage for enough multicast addresses for the universe
/// count that you want to listen on simultaneously, plus the maximum number
/// of sync addresses you want to support. This is a smoltcp-level
/// configuration which is done through its [config environment
/// variable](https://github.com/smoltcp-rs/smoltcp#iface_max_multicast_group_count)
/// or the corresponding Cargo feature.
pub struct Receiver<'d, S: ReceiverStorage> {
    socket: UdpSocket<'d>,
    stack: Stack<'d>,
    core: ReceiverCore<S>,
    store: &'d mut CoreResources<S>,
    /// The multicast-join and sampling records, per listened data universe.
    joined: &'d mut S::Joined,
    /// The multicast-join records, per joined synchronization group.
    sync_joined: &'d mut S::SyncJoined,
    /// The persistent datagram buffer received packets are parsed from.
    recv_buf: &'d mut [u8],
    /// The instant treated as the core's monotonic epoch.
    epoch: EmbassyInstant,
    /// A merged result withheld behind a packet's first event (a passthrough or
    /// a PAP-lost), delivered on the next `next_event` call. It stays present in
    /// the store until then, so only the universe needs remembering.
    deferred_merged: Option<Universe>,
    /// The deadline reported by the most recent poll.
    last_deadline: Option<Instant>,
}

/// A poll event committed inside [`poll_first`](Receiver::poll_first), carried
/// out as a non-borrowing marker so the poll's borrow of the store ends before
/// the borrowed event is built.
enum PollMarker {
    SamplingEnded(Universe),
    /// Resolve the borrowed merged result via `merged(universe)`.
    MergedChanged(Universe),
    SourcesLost(Universe),
}

impl<'d, S: ReceiverStorage> Receiver<'d, S> {
    /// Binds a receiver to the standard sACN port on all local addresses, using
    /// the working memory and socket buffers in `resources`.
    ///
    /// The resources must outlive the receiver. No universes are listened to
    /// until [`listen`](Self::listen) is called.
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Bind`] if the socket cannot be bound.
    pub fn bind(
        stack: Stack<'d>,
        resources: &'d mut ReceiverResources<S>,
        config: ReceiverConfig,
    ) -> Result<Self, EmbassyError> {
        // Force the storage capacity coherence assertions at monomorphization.
        // The basic check covers `Joined`; the merging check covers `SyncJoined`.
        let () = super::storage::AssertEmbassyBasicReceiverCoherent::<S>::CHECK;
        let () = super::storage::AssertEmbassyReceiverCoherent::<S>::CHECK;

        let ReceiverResources {
            core,
            joined,
            sync_joined,
            rx_meta,
            rx_buffer,
            recv_buffer,
        } = resources;

        let mut socket = UdpSocket::new(
            stack,
            rx_meta.as_mut(),
            rx_buffer.as_mut(),
            &mut [],
            &mut [],
        );
        socket.bind(SACN_PORT).map_err(EmbassyError::Bind)?;
        Ok(Self {
            socket,
            stack,
            core: ReceiverCore::with_config(config),
            store: core,
            joined,
            sync_joined,
            recv_buf: recv_buffer.as_mut(),
            epoch: EmbassyInstant::now(),
            deferred_merged: None,
            last_deadline: None,
        })
    }

    /// The configuration this receiver was created with.
    pub fn config(&self) -> &ReceiverConfig {
        self.core.config()
    }

    /// Begins listening for a universe, joining its multicast group(s) on the
    /// network stack.
    ///
    /// The IPv4 group is always joined; the IPv6 group is joined too when the
    /// stack has an IPv6 configuration. Opens a sampling period (a
    /// [`SamplingStarted`](ReceiverEventRef::SamplingStarted) event delivered via
    /// [`next_event`](Self::next_event)).
    ///
    /// Calling it again for a universe already being listened to is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Protocol`] wrapping
    /// [`Error::NoCapacity`](crate::Error::NoCapacity) if a fixed-capacity
    /// receiver's universe table is full, or [`EmbassyError::Multicast`] if a
    /// group cannot be joined. On a multicast failure the core registration is
    /// rolled back so the receiver's state matches the stack's.
    pub fn listen(&mut self, universe: Universe) -> Result<(), EmbassyError> {
        let now = self.now();
        let outcome = self.core.listen(self.store, now, universe)?;
        if !outcome.sampling_started {
            // Already listening: groups already joined, nothing to do.
            return Ok(());
        }

        if let Err(error) = self
            .stack
            .join_multicast_group(IpAddress::Ipv4(ipv4_multicast(universe.get())))
        {
            self.core.stop_listening(self.store, universe);
            return Err(EmbassyError::Multicast(error));
        }
        let joined_v6 = if self.stack.config_v6().is_some() {
            if let Err(error) = self
                .stack
                .join_multicast_group(IpAddress::Ipv6(ipv6_multicast(universe.get())))
            {
                let _ = self
                    .stack
                    .leave_multicast_group(IpAddress::Ipv4(ipv4_multicast(universe.get())));
                self.core.stop_listening(self.store, universe);
                return Err(EmbassyError::Multicast(error));
            }
            true
        } else {
            false
        };

        self.joined.upsert_expect(
            universe,
            JoinState {
                joined_v6,
                sampling_pending: true,
            },
        );
        Ok(())
    }

    /// Stops listening for a universe, leaving every multicast group it was
    /// joined on and reconciling the synchronization groups. Returns whether the
    /// universe was being listened to.
    ///
    /// All merge state for the universe is discarded without a notification.
    /// Group leaves are best-effort; failures are ignored.
    pub fn stop_listening(&mut self, universe: Universe) -> bool {
        let was_listening = self.core.stop_listening(self.store, universe).was_listening;
        if let Some(state) = self.joined.get(&universe).copied() {
            let _ = self
                .stack
                .leave_multicast_group(IpAddress::Ipv4(ipv4_multicast(universe.get())));
            if state.joined_v6 {
                let _ = self
                    .stack
                    .leave_multicast_group(IpAddress::Ipv6(ipv6_multicast(universe.get())));
            }
            self.joined.remove(&universe);
        }
        // Dropping the universe discards its sources, which may retire a
        // synchronization group.
        self.reconcile_sync_groups();
        was_listening
    }

    /// The current merged result for a universe, or `None` if it is not listened
    /// to or is still in its sampling period.
    #[must_use]
    pub fn merged(&self, universe: Universe) -> Option<MergedDataRef<'_, S>> {
        self.core.merged(self.store, universe)
    }

    /// Waits for and returns the next [`ReceiverEventRef`].
    ///
    /// This is the receiver's engine: it advances the core's timers, waits for
    /// the next packet or timer deadline, feeds received packets into the core's
    /// merge, and returns merged events as they are produced. The returned event
    /// borrows the receiver and is valid only until the next call.
    ///
    /// Due to the constraints of this implementation, it might sometimes yield
    /// [`NoEvent`](ReceiverEventRef::NoEvent) when the wakeup produced nothing
    /// to report. Simply call the function again.
    ///
    /// In normal operation this never returns `None`; `None` is reserved for a
    /// future shutdown signal. It is cancel-safe.
    pub async fn next_event(&mut self) -> Option<ReceiverEventRef<'_, S>> {
        // (a) A merged result withheld behind a previous packet's first event.
        if let Some(universe) = self.deferred_merged.take()
            && self.core.merged(self.store, universe).is_some()
        {
            // Strange is_some/expect construction due to borrow checker
            // shenanigans
            return Some(ReceiverEventRef::MergedData(
                self.core
                    .merged(self.store, universe)
                    .expect("merged result just observed present"),
            ));
        }

        // (b) A pending sampling-started notification, delivered before polling
        //     so it precedes the universe's data and its sampling end.
        if let Some(universe) = self.take_sampling_pending() {
            return Some(ReceiverEventRef::SamplingStarted { universe });
        }

        // (c) One poll event.
        let now = self.now();
        if let Some(marker) = self.poll_first(now) {
            return Some(self.build_poll_event(marker));
        }

        // (d) The poll is drained; reconcile synchronization groups against the
        //     settled source membership, then await a packet or the timer.
        self.reconcile_sync_groups();

        let recv = self.socket.recv_from(self.recv_buf);
        let timer = wait_deadline(self.epoch, self.last_deadline);
        match select(recv, timer).await {
            Either::First(Ok((len, meta))) => {
                Some(self.handle_packet_event(len, source_addr(meta)))
            }
            Either::First(Err(_)) => Some(ReceiverEventRef::NoEvent),
            Either::Second(()) => Some(ReceiverEventRef::NoEvent),
        }
    }

    /// The current time as a core [`Instant`], measured from this receiver's
    /// epoch.
    fn now(&self) -> Instant {
        Instant::from_epoch(from_embassy_duration(self.epoch.elapsed()))
    }

    /// Finds a universe owing a `SamplingStarted` notification, clearing its flag.
    fn take_sampling_pending(&mut self) -> Option<Universe> {
        self.joined.iter_mut().find_map(|(universe, state)| {
            core::mem::take(&mut state.sampling_pending).then_some(*universe)
        })
    }

    /// Runs a poll and commits its first event, returning a non-borrowing marker
    /// for it (or `None` if the poll produced nothing). Re-running poll on the
    /// next call resumes with the next undrained event.
    fn poll_first(&mut self, now: Instant) -> Option<PollMarker> {
        let mut poll: MergedPollOutcome<'_, S> = self.core.poll(self.store, now);
        self.last_deadline = poll.deadline;
        let marker = match poll.next_event()? {
            ReceiverPollEvent::SamplingEnded { universe } => PollMarker::SamplingEnded(universe),
            ReceiverPollEvent::MergedDataChanged { universe } => {
                PollMarker::MergedChanged(universe)
            }
            ReceiverPollEvent::SourcesLost { universe, .. } => PollMarker::SourcesLost(universe),
        };
        Some(marker)
    }

    /// Builds the borrowed event for a committed poll marker from the settled
    /// store.
    fn build_poll_event(&self, marker: PollMarker) -> ReceiverEventRef<'_, S> {
        match marker {
            PollMarker::SamplingEnded(universe) => ReceiverEventRef::SamplingEnded { universe },
            PollMarker::MergedChanged(universe) => match self.core.merged(self.store, universe) {
                Some(merged) => ReceiverEventRef::MergedData(merged),
                // Should be unreachable
                None => ReceiverEventRef::NoEvent,
            },
            PollMarker::SourcesLost(universe) => ReceiverEventRef::SourcesLost {
                universe,
                sources: self.core.merge_loss_sources(self.store),
            },
        }
    }

    /// Feeds a freshly received datagram into the core's merge and returns its
    /// first event directly, recording any second event as a deferral to
    /// deliver next call. Returns [`NoEvent`](ReceiverEventRef::NoEvent) when
    /// the packet was malformed or produced nothing deliverable.
    fn handle_packet_event(&mut self, len: usize, from: SocketAddr) -> ReceiverEventRef<'_, S> {
        let now = self.now();
        let Ok(packet) = Packet::parse(&self.recv_buf[..len]) else {
            return ReceiverEventRef::NoEvent;
        };
        match self
            .core
            .handle_packet(self.store, now, from, NetintId::UNKNOWN, &packet)
        {
            MergedPacketOutcome::Ignored => ReceiverEventRef::NoEvent,
            MergedPacketOutcome::LimitExceeded { universe } => {
                ReceiverEventRef::SourceLimitExceeded { universe }
            }
            MergedPacketOutcome::SyncLimitExceeded { sync_address } => {
                ReceiverEventRef::SyncLimitExceeded { sync_address }
            }
            MergedPacketOutcome::Data {
                universe,
                merged,
                pap_lost,
            } => {
                if let Some(source) = pap_lost {
                    // Deliver PAP-lost now (borrowing the source table); withhold
                    // the merged result, which stays re-queryable from the store.
                    self.deferred_merged = merged.as_ref().map(|_| universe);
                    ReceiverEventRef::SourcePapLost { universe, source }
                } else {
                    match merged {
                        Some(merged) => ReceiverEventRef::MergedData(merged),
                        None => ReceiverEventRef::NoEvent,
                    }
                }
            }
            MergedPacketOutcome::Passthrough { data, merged } => {
                // Deliver the passthrough now (borrowing the packet); withhold the
                // merged result behind it.
                self.deferred_merged = merged.as_ref().map(|_| data.universe);
                ReceiverEventRef::UniverseData(data)
            }
            MergedPacketOutcome::Sync(release) => {
                if release.merged_frames().next().is_some() {
                    ReceiverEventRef::SyncMergedData(release)
                } else {
                    ReceiverEventRef::NoEvent
                }
            }
        }
    }

    /// Reconciles the joined synchronization multicast groups against the core's
    /// current interest: leaves groups no longer referenced by any source and
    /// joins newly referenced ones. A sync universe that is also a listened data
    /// universe is already joined for its data, so it is skipped.
    fn reconcile_sync_groups(&mut self) {
        // Leave groups no longer of interest. Each pass removes one, so this
        // terminates even though `leave` is best-effort.
        loop {
            let stale = self.sync_joined.iter().find_map(|(universe, state)| {
                let still_wanted = self
                    .core
                    .sync_group_interest(self.store)
                    .any(|wanted| wanted == *universe);
                (!still_wanted).then_some((*universe, *state))
            });
            let Some((universe, state)) = stale else {
                break;
            };
            let _ = self
                .stack
                .leave_multicast_group(IpAddress::Ipv4(ipv4_multicast(universe.get())));
            if state.joined_v6 {
                let _ = self
                    .stack
                    .leave_multicast_group(IpAddress::Ipv6(ipv6_multicast(universe.get())));
            }
            self.sync_joined.remove(&universe);
        }

        // Join newly interesting groups. One pass per group not yet joined; a
        // failed join is simply not recorded and retried on a later reconcile.
        loop {
            let next = self.core.sync_group_interest(self.store).find(|universe| {
                !self.sync_joined.contains_key(universe) && !self.joined.contains_key(universe)
            });
            let Some(universe) = next else {
                break;
            };
            if self
                .stack
                .join_multicast_group(IpAddress::Ipv4(ipv4_multicast(universe.get())))
                .is_err()
            {
                // Record it as joined-but-empty so this pass makes progress; a
                // later reconcile will not retry (the interest persists, but the
                // group table is full, so retrying now would spin).
                self.sync_joined.upsert_expect(
                    universe,
                    JoinState {
                        joined_v6: false,
                        sampling_pending: false,
                    },
                );
                continue;
            }
            let joined_v6 = if self.stack.config_v6().is_some() {
                self.stack
                    .join_multicast_group(IpAddress::Ipv6(ipv6_multicast(universe.get())))
                    .is_ok()
            } else {
                false
            };
            self.sync_joined.upsert_expect(
                universe,
                JoinState {
                    joined_v6,
                    sampling_pending: false,
                },
            );
        }
    }
}

impl<S: ReceiverStorage> fmt::Debug for Receiver<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("epoch", &self.epoch)
            .field("last_deadline", &self.last_deadline)
            .finish_non_exhaustive()
    }
}

/// Awaits the given core deadline (or never, if there is none).
async fn wait_deadline(epoch: EmbassyInstant, deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => {
            let at = epoch + to_embassy_duration(deadline.since_epoch());
            embassy_time::Timer::at(at).await;
        }
        None => core::future::pending().await,
    }
}
