//! The tokio merging-receiver adapter.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::UdpSocket;
use tokio::time::Instant as TokioInstant;

use crate::adapters::net::ToMulticastInterfaces;
use crate::adapters::{AdapterError, MulticastInterface};
use crate::packet::{MAX_PACKET_SIZE, Packet};
use crate::proto::SACN_PORT;
use crate::receiver::{Receiver as Core, ReceiverConfig, ReceiverEvent, ReceiverPollEvent};
use crate::time::Instant;
use crate::types::{NetintId, Universe};

use super::{JoinPolicy, bind_socket, execute_listen, io_error, system_multicast_interfaces};

#[cfg(test)]
#[path = "merging_tests.rs"]
mod tests;

/// An asynchronous sACN merging receiver driven by the tokio runtime.
///
/// Construct one with [`bind`](Self::bind), register universes with
/// [`listen`](Self::listen) / [`listen_on`](Self::listen_on), and consume merged
/// notifications with [`next_event`](Self::next_event). See the
/// [module docs](super) for an example.
///
/// This wraps the [`crate::receiver::Receiver`] core, which documents the merge
/// and synchronization semantics.
#[derive(Debug)]
pub struct Receiver {
    socket: UdpSocket,
    core: Core,
    /// The tokio-clock instant treated as the core's monotonic epoch.
    epoch: TokioInstant,
    /// Reusable receive buffer, sized for the largest possible sACN packet.
    recv_buf: Vec<u8>,
    /// Events produced by [`listen`](Self::listen) (e.g. the opening
    /// `SamplingStarted`) that have not yet been handed to the caller.
    pending: VecDeque<ReceiverEvent>,
    /// The multicast interfaces currently joined per universe.
    interfaces: HashMap<Universe, Vec<MulticastInterface>>,
    /// The multicast interfaces currently joined per synchronization universe.
    sync_joined: HashMap<Universe, Vec<MulticastInterface>>,
    /// The Pathway Secure sACN validator.
    #[cfg(feature = "pathway-secure")]
    validator: Option<crate::secure::SecureValidator>,
}

impl Receiver {
    /// Binds a receiver to the sACN port on all local addresses.
    ///
    /// The socket is created with address/port reuse so multiple sACN receivers
    /// can coexist on the same host. No universes are listened to until
    /// [`listen`](Self::listen) or [`listen_on`](Self::listen_on) is called.
    ///
    /// # Errors
    ///
    /// Returns an [`AdapterError::Io`] if the socket cannot be created or bound.
    pub async fn bind(config: ReceiverConfig) -> Result<Self, AdapterError> {
        Self::bind_to(SocketAddr::from((Ipv4Addr::UNSPECIFIED, SACN_PORT)), config).await
    }

    /// Binds a receiver to an explicit local address and port.
    ///
    /// Advanced usage: the caller can pick a specific local interface address
    /// and port to bind to. This will generally result in not receiving
    /// typical sACN streams (especially if a nonstandard port is used), but
    /// it can support advanced unicast scenarios. The socket still uses
    /// address/port reuse. No universes are listened to until
    /// [`listen`](Self::listen) or [`listen_on`](Self::listen_on) is called.
    ///
    /// # Errors
    ///
    /// Returns an [`AdapterError::Io`] if the socket cannot be created or bound.
    pub async fn bind_to(addr: SocketAddr, config: ReceiverConfig) -> Result<Self, AdapterError> {
        Ok(Self {
            socket: bind_socket(addr)?,
            core: Core::new(config),
            epoch: TokioInstant::now(),
            recv_buf: vec![0u8; MAX_PACKET_SIZE],
            pending: VecDeque::new(),
            interfaces: HashMap::new(),
            sync_joined: HashMap::new(),
            #[cfg(feature = "pathway-secure")]
            validator: None,
        })
    }

    /// Enables Pathway Secure sACN validation with the given candidate keys.
    ///
    /// With secure mode enabled, every received datagram must be a Pathway Secure
    /// packet whose keyed digest verifies against one of `keys` and whose
    /// sequence has not been replayed; all other datagrams (unauthenticated,
    /// forged, or replayed) are silently dropped. A source's replay state is
    /// reset when it is lost, so a rebooted transmitter is accepted again.
    ///
    /// This is a proof-of-concept extension and is off by default. See
    /// [`crate::secure`].
    #[cfg(feature = "pathway-secure")]
    #[must_use]
    pub fn with_pathway_secure_keys(
        mut self,
        keys: impl IntoIterator<Item = crate::secure::SecureKey>,
    ) -> Self {
        self.validator = Some(crate::secure::SecureValidator::new(keys));
        self
    }

    /// The local address the receiver is bound to.
    ///
    /// # Errors
    ///
    /// Returns an [`AdapterError::Io`] if the socket's address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr, AdapterError> {
        self.socket
            .local_addr()
            .map_err(io_error("reading local address"))
    }

    /// Begins listening for a universe on all usable system interfaces.
    ///
    /// Interfaces are enumerated automatically; the listen succeeds as long as
    /// at least one of them can join the universe's multicast group. Opens a
    /// sampling period (a [`SamplingStarted`](ReceiverEvent::SamplingStarted)
    /// event delivered via [`next_event`](Self::next_event)).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::NoNetwork`] if no usable interface is found or
    /// none can join the group, or [`AdapterError::Protocol`] if listen failed
    /// on the core state machine.
    pub async fn listen(&mut self, universe: Universe) -> Result<(), AdapterError> {
        let interfaces = system_multicast_interfaces();
        if interfaces.is_empty() {
            return Err(AdapterError::NoNetwork);
        }
        self.listen_internal(universe, &interfaces, JoinPolicy::Continue)
    }

    /// Begins (or updates) listening for a universe on an explicit set of
    /// interfaces.
    ///
    /// Unlike [`listen`](Self::listen), every named interface must join
    /// successfully: if any fails the whole operation is rolled back and an
    /// error is returned. Calling it again for the same universe replaces the
    /// interface set.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Io`] if an interface cannot be resolved or a join
    /// fails, [`AdapterError::NoNetwork`] if the interface set is empty, or
    /// [`AdapterError::Protocol`] if listen failed on the core state machine.
    pub async fn listen_on(
        &mut self,
        universe: Universe,
        interfaces: impl ToMulticastInterfaces,
    ) -> Result<(), AdapterError> {
        let interfaces: Vec<_> = interfaces
            .to_multicast_interfaces()
            .map_err(io_error("resolving interfaces"))?
            .collect();
        self.listen_internal(universe, &interfaces, JoinPolicy::Rollback)
    }

    /// Registers the universe with the core, then joins the multicast group on
    /// `interfaces`, applying the rollback `policy` on failure and recording the
    /// joined set for later leaves.
    fn listen_internal(
        &mut self,
        universe: Universe,
        interfaces: &[MulticastInterface],
        policy: JoinPolicy,
    ) -> Result<(), AdapterError> {
        let now = self.now();
        let sampling_started = self.core.listen(now, universe)?.sampling_started;

        let old = self.interfaces.get(&universe).cloned().unwrap_or_default();
        match execute_listen(&self.socket, universe, &old, interfaces, policy) {
            Ok(joined) => {
                self.interfaces.insert(universe, joined);
                if sampling_started {
                    self.pending
                        .push_back(ReceiverEvent::SamplingStarted { universe });
                }
                Ok(())
            }
            Err(error) => {
                // The joins failed (and any partial joins were rolled back);
                // undo the core registration so it matches the socket state.
                let _ = self.core.stop_listening(universe);
                self.interfaces.remove(&universe);
                Err(error)
            }
        }
    }

    /// Stops listening for a universe, leaving every multicast group it was
    /// joined on. Returns whether the universe was being listened to.
    ///
    /// The merge state for the universe is discarded without a notification.
    ///
    /// # Errors
    ///
    /// Currently infallible (group leaves are best-effort), but returns a
    /// `Result` so future failures can be surfaced without a breaking change.
    pub async fn stop_listening(&mut self, universe: Universe) -> Result<bool, AdapterError> {
        let was_listening = self.core.stop_listening(universe).was_listening;
        if let Some(interfaces) = self.interfaces.remove(&universe) {
            for interface in interfaces {
                let _ = super::leave(&self.socket, universe.get(), interface);
            }
        }
        // Dropping the universe discards its sources, which may retire a
        // synchronization group.
        self.reconcile_sync_groups();
        Ok(was_listening)
    }

    /// Waits for and returns the next [`ReceiverEvent`].
    ///
    /// This is the receiver's engine: it advances the core's timers, waits for
    /// the next packet or timer deadline, feeds received packets into the core,
    /// and returns merged events as they are produced. Malformed packets are
    /// dropped silently.
    ///
    /// The receiver runs for as long as it is held, so in normal operation this
    /// never returns `None`; the `Option` leaves room for a future shutdown
    /// signal. It is cancel-safe.
    pub async fn next_event(&mut self) -> Option<ReceiverEvent> {
        loop {
            if let Some(event) = self.next_pending() {
                return Some(event);
            }

            let now = self.now();
            let mut outcome = self.core.poll(now);
            let deadline = outcome.deadline;
            // Drain the borrowed poll events into owned `ReceiverEvent`s,
            // resolving each `MergedDataChanged` signal through the outcome (the
            // merge view borrows the core, so it is read here, not re-looked-up).
            // Copying the Copy `universe` out ends the event borrow, so the
            // `merged` lookup on the next line can borrow the outcome again.
            while let Some(event) = outcome.next_event() {
                match event {
                    ReceiverPollEvent::SamplingEnded { universe } => {
                        self.pending
                            .push_back(ReceiverEvent::SamplingEnded { universe });
                    }
                    ReceiverPollEvent::MergedDataChanged { universe } => {
                        if let Some(merged) = outcome.merged(universe) {
                            self.pending
                                .push_back(ReceiverEvent::MergedData(merged.to_owned()));
                        }
                    }
                    ReceiverPollEvent::SourcesLost { universe, sources } => {
                        self.pending.push_back(ReceiverEvent::SourcesLost {
                            universe,
                            sources: sources.to_vec(),
                        });
                    }
                }
            }

            self.reconcile_sync_groups();

            if let Some(event) = self.next_pending() {
                return Some(event);
            }

            let timer = deadline.map(|d| self.epoch + d.since_epoch());
            let sleep = async move {
                match timer {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                result = self.socket.recv_from(&mut self.recv_buf) => {
                    if let Ok((len, from)) = result {
                        let now = self.now();
                        // In secure mode, drop any datagram that is not an
                        // authentic, non-replayed Pathway Secure packet before it
                        // reaches the parser or the core.
                        #[cfg(feature = "pathway-secure")]
                        if let Some(validator) = self.validator.as_mut()
                            && validator.check(&self.recv_buf[..len])
                                != crate::secure::SecureOutcome::Accepted
                        {
                            continue;
                        }

                        if let Ok(packet) = Packet::parse(&self.recv_buf[..len]) {
                            // The borrowed outcome points into `recv_buf` and the
                            // core's merge buffers; convert to owned form straight
                            // into `pending`. The receiving interface is not yet
                            // attributed (would require per-packet pktinfo).
                            let outcome =
                                self.core.handle_packet(now, from, NetintId::UNKNOWN, &packet);
                            outcome.for_each_owned(|event| self.pending.push_back(event));
                        }
                    }
                }
                () = sleep => {}
            }
        }
    }

    /// Pops the next buffered event, resetting the secure replay state of any
    /// sources reported lost so a rebooted transmitter (whose sequence restarts)
    /// is accepted again.
    #[cfg(feature = "pathway-secure")]
    fn next_pending(&mut self) -> Option<ReceiverEvent> {
        let event = self.pending.pop_front()?;
        if let Some(validator) = self.validator.as_mut()
            && let ReceiverEvent::SourcesLost { sources, .. } = &event
        {
            for lost in sources {
                validator.forget_source(&lost.cid);
            }
        }
        Some(event)
    }

    /// Pops the next buffered event.
    #[cfg(not(feature = "pathway-secure"))]
    fn next_pending(&mut self) -> Option<ReceiverEvent> {
        self.pending.pop_front()
    }

    /// Automatically implements the core's current interest in synchronization
    /// groups. Leaves groups no longer referenced by any source and joins
    /// newly referenced ones on the union of the interfaces the receiver
    /// listens on.
    fn reconcile_sync_groups(&mut self) {
        let desired: BTreeSet<Universe> = self.core.sync_group_interest().collect();

        // Leave groups that are no longer of interest.
        let stale: Vec<Universe> = self
            .sync_joined
            .keys()
            .copied()
            .filter(|u| !desired.contains(u))
            .collect();
        for universe in stale {
            if let Some(interfaces) = self.sync_joined.remove(&universe) {
                for interface in interfaces {
                    let _ = super::leave(&self.socket, universe.get(), interface);
                }
            }
        }

        // Join newly interesting groups. A sync universe that is also a listened
        // data universe is already joined for its data, so skip it.
        let union = union_interfaces(&self.interfaces);
        for universe in desired {
            if self.sync_joined.contains_key(&universe) || self.interfaces.contains_key(&universe) {
                continue;
            }
            let mut joined = Vec::new();
            for &interface in &union {
                if super::join(&self.socket, universe.get(), interface).is_ok() {
                    joined.push(interface);
                }
            }
            if !joined.is_empty() {
                self.sync_joined.insert(universe, joined);
            }
        }
    }

    /// The current time as a core [`Instant`], measured from this receiver's
    /// epoch.
    fn now(&self) -> Instant {
        Instant::from_epoch(self.epoch.elapsed())
    }
}

/// The deduplicated union of every listened universe's multicast interfaces.
fn union_interfaces(
    interfaces: &HashMap<Universe, Vec<MulticastInterface>>,
) -> Vec<MulticastInterface> {
    let mut union: Vec<MulticastInterface> = Vec::new();
    for list in interfaces.values() {
        for &interface in list {
            if !union.contains(&interface) {
                union.push(interface);
            }
        }
    }
    union
}
