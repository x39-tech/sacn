//! The embassy adapter error type.

/// An error produced by the embassy runtime adapter.
#[derive(Debug, thiserror::Error)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum EmbassyError {
    /// A protocol-level error surfaced by the protocol core.
    #[error(transparent)]
    Protocol(#[from] crate::Error),

    /// Binding the UDP socket failed.
    #[error("binding the UDP socket failed: {0:?}")]
    Bind(embassy_net::udp::BindError),

    /// Joining or leaving a multicast group failed. The most common cause is the
    /// network stack's multicast group table being full: size the stack's
    /// [`StackResources`](embassy_net::StackResources) for at least one group per
    /// listened universe (plus one per active synchronization group).
    #[error("multicast group join/leave failed: {0:?}")]
    Multicast(embassy_net::MulticastError),
}
