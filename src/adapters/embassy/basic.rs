//! The embassy basic-receiver adapter.

use core::fmt;
use core::net::SocketAddr;

use embassy_futures::select::{Either, select};
use embassy_net::udp::{UdpMetadata, UdpSocket};
use embassy_net::{IpAddress, Stack};
use embassy_time::Instant as EmbassyInstant;

use crate::packet::{Packet, Payload};
use crate::proto::{SACN_PORT, ipv4_multicast, ipv6_multicast};
use crate::receiver::{
    BasicReceiverCore, BasicReceiverEventRef, BasicReceiverPollEvent,
    BasicReceiverResources as CoreResources, PacketOutcome, ReceiverConfig, SourceInfoRef,
    UniverseDataRef,
};
use crate::storage::MapLike;
use crate::time::Instant;
use crate::types::{NetintId, Universe};

use super::error::EmbassyError;
use super::storage::{BasicReceiverResources, BasicReceiverStorage, JoinState};
use super::{from_embassy_duration, to_embassy_duration};

#[cfg(test)]
#[path = "basic_tests.rs"]
mod tests;

/// An asynchronous, `no_std` sACN basic receiver driven by the embassy runtime.
///
/// Construct one with [`bind`](Self::bind), register universes with
/// [`listen`](Self::listen), and consume per-source notifications with
/// [`next_event`](Self::next_event). For merged universe data, use the merging
/// [`Receiver`](super::Receiver) instead.
///
/// This wraps the [`crate::receiver::BasicReceiver`] core, which implements the
/// receive-path behavior (sampling periods, sequence numbering, source loss and
/// per-address priority). Its working memory, including the embassy stack's
/// socket buffers, lives in a separate [`BasicReceiverResources`] borrowed for
/// the receiver's whole lifetime. This type can be given fixed, heapless limits
/// and const-constructed in static memory using the macro
/// [`embassy_static_storage!`](crate::embassy_static_storage!).
///
/// ```no_run
/// use sacn::embassy::{BasicReceiver, BasicReceiverResources};
/// use sacn::{ReceiverConfig, Universe, BasicReceiverEventRef};
/// use static_cell::ConstStaticCell;
///
/// sacn::embassy_static_storage! {
///     pub struct Caps {
///         rx_universes: 1,
///         rx_sources_per_universe: 4,
///         rx_sync_addresses: 0,
///         tx_universes: 0,
///         tx_unicast_per_universe: 0,
///     }
/// }
///
/// static RESOURCES: ConstStaticCell<BasicReceiverResources<Caps>> =
///     ConstStaticCell::new(Caps::embassy_basic_receiver_resources());
///
/// # async fn demo(stack: embassy_net::Stack<'static>) -> Result<(), sacn::embassy::EmbassyError> {
/// let resources = RESOURCES.take();
/// // `stack` is your already-initialized `embassy_net::Stack`.
/// let mut rx = BasicReceiver::bind(stack, resources, ReceiverConfig::new())?;
/// rx.listen(Universe::new(1).unwrap())?;
/// while let Some(event) = rx.next_event().await {
///     match event {
///         BasicReceiverEventRef::UniverseData(data) => {
///             // Per-source levels for `data.universe`. Because this is the
///             // basic receiver, reconciling data across sources on the same
///             // universe is up to you.
///             let _ = data.values;
///         }
///         BasicReceiverEventRef::SourcesLost { universe, .. } => { let _ = universe; }
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
pub struct BasicReceiver<'d, S: BasicReceiverStorage> {
    socket: UdpSocket<'d>,
    stack: Stack<'d>,
    core: BasicReceiverCore<S>,
    store: &'d mut CoreResources<S>,
    /// The multicast-join and sampling records, per listened universe.
    joined: &'d mut S::Joined,
    /// The persistent datagram buffer received packets are parsed from.
    recv_buf: &'d mut [u8],
    /// The instant treated as the core's monotonic epoch.
    epoch: EmbassyInstant,
    /// A data event withheld behind a just-delivered PAP-lost event.
    deferred_data: Option<DeferredData>,
    /// The deadline reported by the most recent poll.
    last_deadline: Option<Instant>,
}

/// A data event that was withheld so its per-address-priority-lost event could be
/// delivered first. Fields that cannot be trivially re-derived by re-parsing the
/// datagram still in the receive buffer are stored here.
#[derive(Clone, Copy)]
struct DeferredData {
    len: usize,
    from: SocketAddr,
    universe: Universe,
    sync_address: u16,
    is_sampling: bool,
}

/// A poll event committed inside [`poll_first`](BasicReceiver::poll_first).
enum PollMarker {
    SamplingEnded(Universe),
    SourcesLost(Universe),
}

impl<'d, S: BasicReceiverStorage> BasicReceiver<'d, S> {
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
        resources: &'d mut BasicReceiverResources<S>,
        config: ReceiverConfig,
    ) -> Result<Self, EmbassyError> {
        // Force the storage capacity coherence assertions at monomorphization.
        let () = super::storage::AssertEmbassyBasicReceiverCoherent::<S>::CHECK;

        let BasicReceiverResources {
            core,
            joined,
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
            core: BasicReceiverCore::with_config(config),
            store: core,
            joined,
            recv_buf: recv_buffer.as_mut(),
            epoch: EmbassyInstant::now(),
            deferred_data: None,
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
    /// The IPv4 group is always joined, per the requirement of E1.31 sect. 9.4.
    /// The IPv6 group is joined too when the stack has an IPv6 configuration.
    /// Opens a sampling period (a [`SamplingStarted`](BasicReceiverEventRef::SamplingStarted)
    /// event delivered via [`next_event`](Self::next_event)).
    ///
    /// Calling it again for a universe already being listened to is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Protocol`] wrapping
    /// [`Error::NoCapacity`](crate::Error::NoCapacity) if a fixed-capacity
    /// receiver's universe table is full, or [`EmbassyError::Multicast`] if a
    /// group cannot be joined (most often the stack's multicast group table is
    /// full). On a multicast failure the core registration is rolled back so the
    /// receiver's state matches the stack's.
    pub fn listen(&mut self, universe: Universe) -> Result<(), EmbassyError> {
        let now = self.now();
        let outcome = self.core.listen(self.store, now, universe)?;
        if !outcome.sampling_started {
            // Already listening: the core left it untouched, so the groups are
            // already joined. Nothing to do.
            return Ok(());
        }

        // Join the IPv4 group always, and the IPv6 group when the stack is
        // configured for IPv6.
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
                // Roll back the IPv4 join and the core registration.
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

        // Capacity asserted by compile-time coherence check
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
    /// joined on. Returns whether the universe was being listened to.
    ///
    /// Sources tracked on the universe are dropped without a loss notification.
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
        was_listening
    }

    /// Waits for and returns the next [`BasicReceiverEventRef`].
    ///
    /// This is the receiver's engine: it advances the core's timers, waits for
    /// the next packet or timer deadline, feeds received packets into the core,
    /// and returns events as they are produced. The returned event borrows the
    /// receiver and is valid only until the next call.
    ///
    /// Due to the constraints of this implementation, it might sometimes yield
    /// [`NoEvent`](BasicReceiverEventRef::NoEvent) when the wakeup produced
    /// nothing to report. Simply call the function again.
    ///
    /// In normal operation this never returns `None`; `None` is reserved for a
    /// future shutdown signal. It is cancel-safe.
    pub async fn next_event(&mut self) -> Option<BasicReceiverEventRef<'_>> {
        // 1: A data event withheld behind a prior PAP-lost event.
        if let Some(deferred) = self.deferred_data.take() {
            return Some(self.reconstruct_data(deferred));
        }

        // 2: A pending sampling-started notification, delivered before polling so
        //    it always precedes the universe's data and its sampling end.
        if let Some(universe) = self.take_sampling_pending() {
            return Some(BasicReceiverEventRef::SamplingStarted { universe });
        }

        // 3: One poll event, if the timers produced any. `poll_first` commits it
        //    and returns a non-borrowing marker (its borrow of the store ends on
        //    return), so the borrowed event is built here from the settled store.
        let now = self.now();
        if let Some(marker) = self.poll_first(now) {
            return Some(match marker {
                PollMarker::SamplingEnded(universe) => {
                    BasicReceiverEventRef::SamplingEnded { universe }
                }
                PollMarker::SourcesLost(universe) => BasicReceiverEventRef::SourcesLost {
                    universe,
                    sources: self.core.lost_sources(self.store),
                },
            });
        }

        // 4: Await the next packet or the timer deadline, then deliver one event.
        //    A wakeup that yields nothing deliverable resolves to `NoEvent`.
        let recv = self.socket.recv_from(self.recv_buf);
        let timer = wait_deadline(self.epoch, self.last_deadline);
        match select(recv, timer).await {
            Either::First(Ok((len, meta))) => {
                Some(self.handle_packet_event(len, source_addr(meta)))
            }
            Either::First(Err(_)) => Some(BasicReceiverEventRef::NoEvent),
            Either::Second(()) => Some(BasicReceiverEventRef::NoEvent),
        }
    }

    /// The current time as a core [`Instant`], measured from this receiver's
    /// epoch.
    fn now(&self) -> Instant {
        Instant::from_epoch(from_embassy_duration(self.epoch.elapsed()))
    }

    /// Finds a universe owing a `SamplingStarted` notification, clearing its flag
    /// so the next call finds the next one.
    fn take_sampling_pending(&mut self) -> Option<Universe> {
        self.joined.iter_mut().find_map(|(universe, state)| {
            core::mem::take(&mut state.sampling_pending).then_some(*universe)
        })
    }

    /// Runs a poll and commits its first event, returning a non-borrowing marker
    /// for it (or `None` if the poll produced nothing). Re-running poll on the
    /// next call resumes with the next undrained event, per the core receiver's
    /// contract.
    fn poll_first(&mut self, now: Instant) -> Option<PollMarker> {
        let mut poll = self.core.poll(self.store, now);
        self.last_deadline = poll.deadline;
        let marker = match poll.next_event()? {
            BasicReceiverPollEvent::SamplingEnded { universe } => {
                PollMarker::SamplingEnded(universe)
            }
            BasicReceiverPollEvent::SourcesLost { universe, .. } => {
                PollMarker::SourcesLost(universe)
            }
        };
        Some(marker)
    }

    /// Feeds a freshly received datagram into the core and returns its first
    /// event, deferring the data event behind a PAP-lost event if both fired.
    /// Returns [`NoEvent`](BasicReceiverEventRef::NoEvent) when the packet was
    /// malformed or produced nothing deliverable (ignored, duplicated, or out of
    /// sequence).
    fn handle_packet_event(&mut self, len: usize, from: SocketAddr) -> BasicReceiverEventRef<'_> {
        let now = self.now();
        let Ok(packet) = Packet::parse(&self.recv_buf[..len]) else {
            return BasicReceiverEventRef::NoEvent;
        };
        match self
            .core
            .handle_packet(self.store, now, from, NetintId::UNKNOWN, &packet)
        {
            PacketOutcome::Ignored => BasicReceiverEventRef::NoEvent,
            PacketOutcome::LimitExceeded { universe } => {
                BasicReceiverEventRef::SourceLimitExceeded { universe }
            }
            PacketOutcome::Sync { sync_address, cid } => {
                BasicReceiverEventRef::SyncReceived { sync_address, cid }
            }
            PacketOutcome::Data {
                universe,
                data,
                pap_lost,
            } => {
                if let Some(source) = pap_lost {
                    // Deliver PAP-lost now; withhold the data event for the next
                    // call, capturing relevant info so it can be rebuilt from
                    // the datagram still in `recv_buf`.
                    if let Some(d) = data {
                        self.deferred_data = Some(DeferredData {
                            len,
                            from,
                            universe: d.universe,
                            sync_address: d.sync_address,
                            is_sampling: d.is_sampling,
                        });
                    }
                    BasicReceiverEventRef::SourcePapLost { universe, source }
                } else {
                    match data {
                        Some(data) => BasicReceiverEventRef::UniverseData(data),
                        None => BasicReceiverEventRef::NoEvent,
                    }
                }
            }
        }
    }

    /// Rebuilds the withheld data event from the datagram still in `recv_buf`,
    /// combining the previously captured stateful info with fields re-parsed
    /// out of the buffer.
    fn reconstruct_data(&self, deferred: DeferredData) -> BasicReceiverEventRef<'_> {
        let Ok(packet) = Packet::parse(&self.recv_buf[..deferred.len]) else {
            // Should be unreachable
            return BasicReceiverEventRef::NoEvent;
        };
        let Payload::Data(data) = &packet.payload else {
            return BasicReceiverEventRef::NoEvent;
        };
        BasicReceiverEventRef::UniverseData(UniverseDataRef {
            universe: deferred.universe,
            source: SourceInfoRef {
                cid: packet.cid,
                name: data.source_name,
            },
            addr: deferred.from,
            priority: data.priority,
            start_code: data.start_code,
            values: data.values,
            preview: data.preview,
            sync_address: deferred.sync_address,
            is_sampling: deferred.is_sampling,
        })
    }
}

impl<S: BasicReceiverStorage> fmt::Debug for BasicReceiver<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The socket, stack and core hold no publicly meaningful debug state (and
        // the socket is not `Debug`); surface the adapter's own fields.
        f.debug_struct("BasicReceiver")
            .field("epoch", &self.epoch)
            .field("last_deadline", &self.last_deadline)
            .finish_non_exhaustive()
    }
}

/// The source address of a received datagram, from its [`UdpMetadata`].
pub(super) fn source_addr(meta: UdpMetadata) -> SocketAddr {
    meta.endpoint.into()
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
