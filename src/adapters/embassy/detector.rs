//! The embassy source-detector adapter.

use core::fmt;

use embassy_futures::select::{Either, select};
use embassy_net::udp::UdpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_time::Instant as EmbassyInstant;

use crate::detector::{
    LimitExceeded, SourceDetectorConfig, SourceDetectorCore, SourceDetectorEventRef,
    SourceDetectorPollEvent, SourceDetectorResources as CoreResources,
};
use crate::packet::Packet;
use crate::proto::{DISCOVERY_UNIVERSE, SACN_PORT, ipv4_multicast, ipv6_multicast};
use crate::time::Instant;

use super::error::EmbassyError;
use super::storage::{DetectorResources, DetectorStorage};
use super::{from_embassy_duration, to_embassy_duration};

#[cfg(test)]
#[path = "detector_tests.rs"]
mod tests;

/// An asynchronous, `no_std` sACN source detector driven by the embassy runtime.
///
/// A source detector listens on the multicast group reserved for universe
/// discovery and reports the sources present on the network and the universes
/// each of them transmits. Construct one with [`bind`](Self::bind) and consume
/// notifications with [`next_event`](Self::next_event).
///
/// This wraps the [`crate::detector::SourceDetector`] core, which documents the
/// discovery-page reassembly and source-expiry behavior. Its working memory,
/// including the embassy stack's socket buffers, lives in a separate
/// [`DetectorResources`] borrowed for the detector's whole lifetime. This type
/// can be given fixed, heapless limits and const-constructed in static memory
/// using the macro [`embassy_static_storage!`](crate::embassy_static_storage!).
///
/// ```no_run
/// use sacn::embassy::{SourceDetector, DetectorResources};
/// use sacn::{SourceDetectorConfig, SourceDetectorEventRef};
/// use static_cell::ConstStaticCell;
///
/// sacn::embassy_static_storage! {
///     pub struct Caps {
///         rx_universes: 0,
///         rx_sources_per_universe: 0,
///         rx_sync_addresses: 0,
///         tx_universes: 0,
///         tx_unicast_per_universe: 0,
///         det_sources: 16,
///         det_universes_per_source: 16,
///     }
/// }
///
/// static RESOURCES: ConstStaticCell<DetectorResources<Caps>> =
///     ConstStaticCell::new(Caps::embassy_detector_resources());
///
/// # async fn demo(stack: embassy_net::Stack<'static>) -> Result<(), sacn::embassy::EmbassyError> {
/// let resources = RESOURCES.take();
/// // `stack` is your already-initialized `embassy_net::Stack`.
/// let mut detector = SourceDetector::bind(stack, resources, SourceDetectorConfig::new())?;
/// while let Some(event) = detector.next_event().await {
///     match event {
///         SourceDetectorEventRef::SourceUpdated { name, universes, .. } => {
///             // `name` is transmitting `universes`.
///             let _ = (name, universes);
///         }
///         SourceDetectorEventRef::SourceExpired { name, .. } => { let _ = name; }
///         _ => {}
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Note on stack resources and coexisting sockets
///
/// The detector joins the discovery multicast group on the stack. As with the
/// receivers, your [`embassy_net::Stack`] must be configured with storage for at
/// least one multicast address (see the note on
/// [`BasicReceiver`](super::BasicReceiver)).
///
/// A detector binds the standard sACN port, and `embassy-net`'s underlying
/// `smoltcp` delivers each received datagram to only the first socket bound to
/// that port. A detector therefore cannot share a stack with a
/// [`BasicReceiver`](super::BasicReceiver) or [`Receiver`](super::Receiver):
/// whichever socket is registered first would swallow the other's traffic. Run a
/// detector on its own stack, or in a firmware image that has no receiver.
///
/// See for reference:
///
/// - [1](https://github.com/smoltcp-rs/smoltcp/issues/644)
/// - [2](http://github.com/smoltcp-rs/smoltcp/issues/925)
pub struct SourceDetector<'d, S: DetectorStorage> {
    socket: UdpSocket<'d>,
    core: SourceDetectorCore<S>,
    store: &'d mut CoreResources<S>,
    /// The datagram buffer received packets are parsed from.
    recv_buf: &'d mut [u8],
    /// The instant treated as the core's monotonic epoch.
    epoch: EmbassyInstant,
    /// The deadline reported by the most recent poll.
    last_deadline: Option<Instant>,
    /// The index of the next undrained expiry event from the most recent poll,
    /// and the number of expiries that poll produced. The events themselves stay
    /// buffered in `store` until the next poll.
    poll_cursor: usize,
    poll_len: usize,
    /// A limit-exceeded event withheld behind a just-delivered source update, so
    /// both surface when one packet produces both.
    deferred_limit: Option<LimitExceeded>,
}

impl<'d, S: DetectorStorage> SourceDetector<'d, S> {
    /// Binds a detector to the standard sACN port and joins the universe
    /// discovery multicast group, using the working memory and socket buffers in
    /// `resources`.
    ///
    /// The IPv4 group is always joined; the IPv6 group is joined too when the
    /// stack has an IPv6 configuration. The resources must outlive the detector.
    ///
    /// # Errors
    ///
    /// Returns [`EmbassyError::Bind`] if the socket cannot be bound, or
    /// [`EmbassyError::Multicast`] if the discovery group cannot be joined (most
    /// often the stack's multicast group table is full).
    pub fn bind(
        stack: Stack<'d>,
        resources: &'d mut DetectorResources<S>,
        config: SourceDetectorConfig,
    ) -> Result<Self, EmbassyError> {
        let DetectorResources {
            detector,
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

        // Join the IPv4 discovery group always, and the IPv6 group when the stack
        // is configured for IPv6.
        stack
            .join_multicast_group(IpAddress::Ipv4(ipv4_multicast(DISCOVERY_UNIVERSE)))
            .map_err(EmbassyError::Multicast)?;
        if stack.config_v6().is_some()
            && let Err(error) =
                stack.join_multicast_group(IpAddress::Ipv6(ipv6_multicast(DISCOVERY_UNIVERSE)))
        {
            let _ =
                stack.leave_multicast_group(IpAddress::Ipv4(ipv4_multicast(DISCOVERY_UNIVERSE)));
            return Err(EmbassyError::Multicast(error));
        }

        Ok(Self {
            socket,
            core: SourceDetectorCore::with_config(config),
            store: detector,
            recv_buf: recv_buffer.as_mut(),
            epoch: EmbassyInstant::now(),
            last_deadline: None,
            poll_cursor: 0,
            poll_len: 0,
            deferred_limit: None,
        })
    }

    /// The configuration this detector was created with.
    pub fn config(&self) -> &SourceDetectorConfig {
        self.core.config()
    }

    /// Waits for and returns the next [`SourceDetectorEventRef`].
    ///
    /// This is the detector's engine: it advances the core's expiry timers, waits
    /// for the next discovery packet or timer deadline, feeds received packets
    /// into the core, and returns events as they are produced. The returned event
    /// borrows the detector and is valid only until the next call.
    ///
    /// Due to the constraints of this implementation, it might sometimes yield
    /// [`NoEvent`](SourceDetectorEventRef::NoEvent) when the wakeup produced
    /// nothing to report. Simply call the function again.
    ///
    /// In normal operation this never returns `None`; `None` is reserved for a
    /// future shutdown signal. It is cancel-safe.
    pub async fn next_event(&mut self) -> Option<SourceDetectorEventRef<'_>> {
        // 1: A limit-exceeded event withheld behind a prior source update.
        if let Some(limit) = self.deferred_limit.take() {
            return Some(limit_ref(limit));
        }

        // 2: An undrained expiry from the most recent poll.
        if self.poll_cursor < self.poll_len {
            let index = self.poll_cursor;
            self.poll_cursor += 1;
            return Some(expiry_ref(&self.core.poll_events(self.store)[index]));
        }

        // 3: Poll for newly-expired sources, delivering the first if any.
        let now = self.now();
        let (deadline, count) = {
            let outcome = self.core.poll(self.store, now);
            (outcome.deadline, outcome.events().len())
        };
        self.last_deadline = deadline;
        self.poll_len = count;
        self.poll_cursor = 0;
        if count > 0 {
            self.poll_cursor = 1;
            return Some(expiry_ref(&self.core.poll_events(self.store)[0]));
        }

        // 4: Await the next packet or the poll deadline, then deliver one event.
        //    A wakeup that yields nothing deliverable resolves to `NoEvent`.
        let recv = self.socket.recv_from(self.recv_buf);
        let timer = wait_deadline(self.epoch, self.last_deadline);
        match select(recv, timer).await {
            Either::First(Ok((len, _meta))) => Some(self.handle_packet_event(len)),
            Either::First(Err(_)) => Some(SourceDetectorEventRef::NoEvent),
            Either::Second(()) => Some(SourceDetectorEventRef::NoEvent),
        }
    }

    /// The current time as a core [`Instant`], measured from this detector's
    /// epoch.
    fn now(&self) -> Instant {
        Instant::from_epoch(from_embassy_duration(self.epoch.elapsed()))
    }

    /// Feeds a freshly received datagram into the core and returns its first
    /// event. A packet can, at most, complete a changed universe list (an update)
    /// and report that a new source could not be tracked (a limit event); when it
    /// does both, the update is delivered now and the limit event is withheld for
    /// the next call. Returns [`NoEvent`](SourceDetectorEventRef::NoEvent) when
    /// the packet was malformed or produced nothing (ignored, or an unchanged
    /// list).
    fn handle_packet_event(&mut self, len: usize) -> SourceDetectorEventRef<'_> {
        let now = self.now();
        let Ok(packet) = Packet::parse(&self.recv_buf[..len]) else {
            return SourceDetectorEventRef::NoEvent;
        };
        let outcome = self.core.handle_packet(self.store, now, &packet);
        match outcome.updated {
            Some(update) => {
                // Withhold any co-occurring limit event behind the update; unlike
                // the borrowed update, it is cheap to carry to the next call.
                self.deferred_limit = outcome.limit_exceeded;
                SourceDetectorEventRef::SourceUpdated {
                    cid: update.cid,
                    name: update.name,
                    universes: update.universes,
                }
            }
            None => match outcome.limit_exceeded {
                Some(limit) => limit_ref(limit),
                None => SourceDetectorEventRef::NoEvent,
            },
        }
    }
}

impl<S: DetectorStorage> fmt::Debug for SourceDetector<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The socket and core hold no publicly meaningful debug state (and the
        // socket is not `Debug`); surface the adapter's own fields.
        f.debug_struct("SourceDetector")
            .field("epoch", &self.epoch)
            .field("last_deadline", &self.last_deadline)
            .finish_non_exhaustive()
    }
}

/// Builds the borrowed event for a limit the core reported. Both variants carry
/// no borrow, so the result is valid for any lifetime.
fn limit_ref(limit: LimitExceeded) -> SourceDetectorEventRef<'static> {
    match limit {
        LimitExceeded::Source => SourceDetectorEventRef::SourceLimitExceeded,
        LimitExceeded::Universe { cid } => SourceDetectorEventRef::UniverseLimitExceeded { cid },
    }
}

/// Builds the borrowed event for one buffered expiry, borrowing the expired
/// source's name from the detector's poll-event buffer.
fn expiry_ref(event: &SourceDetectorPollEvent) -> SourceDetectorEventRef<'_> {
    match event {
        SourceDetectorPollEvent::SourceExpired { cid, name } => {
            SourceDetectorEventRef::SourceExpired {
                cid: *cid,
                name: name.as_str(),
            }
        }
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
