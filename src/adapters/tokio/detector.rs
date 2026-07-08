//! The tokio source-detector adapter.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::UdpSocket;
use tokio::time::Instant as TokioInstant;

use crate::adapters::net::ToMulticastInterfaces;
use crate::adapters::{AdapterError, MulticastInterface};
use crate::detector::{SourceDetector as Core, SourceDetectorConfig, SourceDetectorEvent};
use crate::packet::{Packet, MAX_PACKET_SIZE};
use crate::proto::{DISCOVERY_UNIVERSE, SACN_PORT};
use crate::time::Instant;

use super::{bind_socket, io_error, join, leave, system_multicast_interfaces, JoinPolicy};

#[cfg(test)]
#[path = "detector_tests.rs"]
mod tests;

/// An asynchronous sACN source detector driven by the tokio runtime.
///
/// A source detector listens on the multicast group reserved for universe
/// discovery and reports the sources present on the network and the universes
/// each of them transmits. Construct one with [`bind`](Self::bind) (or
/// [`bind_on`](Self::bind_on) for explicit interfaces) and consume notifications
/// with [`next_event`](Self::next_event).
///
/// This wraps the [`crate::detector::SourceDetector`] core, which documents the
/// discovery-page reassembly and source-expiry behavior.
///
/// ```no_run
/// use sacn::tokio::SourceDetector;
/// use sacn::{SourceDetectorConfig, SourceDetectorEvent};
///
/// # async fn demo() -> Result<(), sacn::AdapterError> {
/// let mut detector = SourceDetector::bind(SourceDetectorConfig::new()).await?;
/// while let Some(event) = detector.next_event().await {
///     match event {
///         SourceDetectorEvent::SourceUpdated { name, universes, .. } => {
///             println!("{name} is transmitting {universes:?}");
///         }
///         SourceDetectorEvent::SourceExpired { name, .. } => {
///             println!("{name} went away");
///         }
///         _ => {}
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct SourceDetector {
    socket: UdpSocket,
    core: Core,
    epoch: TokioInstant,
    recv_buf: Vec<u8>,
    pending: VecDeque<SourceDetectorEvent>,
}

impl SourceDetector {
    /// Binds a detector to the sACN port and joins the universe discovery group
    /// on all usable system interfaces.
    ///
    /// The socket is created with address/port reuse so the detector can coexist
    /// with other sACN receivers on the same host. Interfaces are enumerated
    /// automatically; binding succeeds as long as at least one of them can join
    /// the discovery group.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Io`] if the socket cannot be created or bound, or
    /// [`AdapterError::NoNetwork`] if no usable interface can join the discovery
    /// group.
    pub async fn bind(config: SourceDetectorConfig) -> Result<Self, AdapterError> {
        let interfaces = system_multicast_interfaces();
        if interfaces.is_empty() {
            return Err(AdapterError::NoNetwork);
        }
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, SACN_PORT));
        let detector = Self::bind_to(addr, config).await?;
        join_discovery(&detector.socket, &interfaces, JoinPolicy::Continue)?;
        Ok(detector)
    }

    /// Binds a detector to the sACN port and joins the universe discovery group
    /// on an explicit set of interfaces.
    ///
    /// Unlike [`bind`](Self::bind), every named interface must join successfully;
    /// if any fails the whole operation is rolled back and an error is returned.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Io`] if the socket cannot be bound, an interface
    /// cannot be resolved, or a join fails, or [`AdapterError::NoNetwork`] if the
    /// interface set is empty.
    pub async fn bind_on(
        config: SourceDetectorConfig,
        interfaces: impl ToMulticastInterfaces,
    ) -> Result<Self, AdapterError> {
        let interfaces: Vec<_> = interfaces
            .to_multicast_interfaces()
            .map_err(io_error("resolving interfaces"))?
            .collect();
        if interfaces.is_empty() {
            return Err(AdapterError::NoNetwork);
        }
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, SACN_PORT));
        let detector = Self::bind_to(addr, config).await?;
        join_discovery(&detector.socket, &interfaces, JoinPolicy::Rollback)?;
        Ok(detector)
    }

    /// Binds a detector to an explicit local address without joining any group.
    /// Shared by [`bind`]/[`bind_on`] and the tests, which bind an ephemeral port
    /// and deliver discovery packets by unicast to stay isolated from one another.
    ///
    /// [`bind`]: Self::bind
    /// [`bind_on`]: Self::bind_on
    async fn bind_to(addr: SocketAddr, config: SourceDetectorConfig) -> Result<Self, AdapterError> {
        Ok(Self {
            socket: bind_socket(addr)?,
            core: Core::new(config),
            epoch: TokioInstant::now(),
            recv_buf: vec![0u8; MAX_PACKET_SIZE],
            pending: VecDeque::new(),
        })
    }

    /// Waits for and returns the next [`SourceDetectorEvent`].
    ///
    /// The detector runs for as long as it is held, so in normal operation this
    /// never returns `None`; the `Option` leaves room for a future shutdown
    /// signal. It is cancel-safe.
    pub async fn next_event(&mut self) -> Option<SourceDetectorEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }

            let now = self.now();
            let outcome = self.core.poll(now);
            let deadline = outcome.deadline;
            self.pending.extend(outcome.events().iter().map(Into::into));
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }

            let timer = deadline.map(|d| self.epoch + d.since_epoch());
            let sleep = async move {
                match timer {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    // No pending timer: wait only on the socket.
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                result = self.socket.recv_from(&mut self.recv_buf) => {
                    if let Ok((len, _from)) = result {
                        let now = self.now();
                        if let Ok(packet) = Packet::parse(&self.recv_buf[..len]) {
                            // The borrowed outcome points into the core's storage;
                            // convert its events to owned form into `pending`.
                            let outcome = self.core.handle_packet(now, &packet);
                            outcome.for_each_owned(|event| self.pending.push_back(event));
                        }
                    }
                }
                () = sleep => {}
            }
        }
    }

    /// The current time as a core [`Instant`], measured from this detector's
    /// epoch.
    fn now(&self) -> Instant {
        Instant::from_epoch(self.epoch.elapsed())
    }
}

/// Joins the universe discovery multicast group on `interfaces`, applying the
/// rollback `policy` on a failed join.
///
/// On success the joins are retained for the life of the socket (they are
/// released when it is dropped). On failure under [`JoinPolicy::Rollback`], the
/// joins this call made are undone before the error is returned.
fn join_discovery(
    socket: &UdpSocket,
    interfaces: &[MulticastInterface],
    policy: JoinPolicy,
) -> Result<(), AdapterError> {
    let mut joined = Vec::new();
    let mut first_error = None;
    for &interface in interfaces {
        match join(socket, DISCOVERY_UNIVERSE, interface) {
            Ok(()) => joined.push(interface),
            Err(error) => match policy {
                JoinPolicy::Rollback => {
                    first_error = Some(error);
                    break;
                }
                JoinPolicy::Continue => {
                    crate::log::debug!(
                        "failed to join discovery group on an interface; continuing"
                    );
                }
            },
        }
    }

    if first_error.is_some() || joined.is_empty() {
        for interface in joined {
            let _ = leave(socket, DISCOVERY_UNIVERSE, interface);
        }
        return Err(first_error.unwrap_or(AdapterError::NoNetwork));
    }

    Ok(())
}
