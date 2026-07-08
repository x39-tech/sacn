//! The adapter-layer error type.
//!
//! Unlike the core [`Error`](crate::Error), this type may reference
//! [`std::io::Error`]: std-only failure sources live in the adapter layer.

use std::io;

/// An error produced by a runtime adapter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdapterError {
    /// A protocol-level error surfaced by the protocol core.
    #[error(transparent)]
    Protocol(#[from] crate::Error),

    /// A socket or other OS I/O operation failed.
    #[error("I/O error while {operation}")]
    Io {
        /// A short description of the operation that failed (e.g.
        /// `"joining multicast group"`).
        operation: &'static str,
        /// The underlying OS error.
        #[source]
        source: io::Error,
    },

    /// No suitable network interface could be found.
    #[error("no suitable network interface was found")]
    NoNetwork,
}
