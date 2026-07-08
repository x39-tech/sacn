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
}
