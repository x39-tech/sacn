//! The embassy runtime adapter.
//!
//! This is the default entrypoint to sACN for embedded targets using the
//! [embassy](https://embassy.dev) async runtime and its `embassy-net` stack.
//! It is `no_std` and allocation-free.
//!
//! - [`Source`] wraps [`crate::source::Source`] and transmits sACN.
//!
//! On a target with no allocator, size the source's fixed-capacity storage
//! (including its unicast destination tables and socket buffers) with
//! [`embassy_static_storage!`](crate::embassy_static_storage!).

mod error;
mod sending;
mod source;
mod storage;

pub use error::EmbassyError;
pub use source::Source;
pub use storage::{Destinations, SourceResources, SourceStorage};

// Re-exported so the `embassy_static_storage!` macro can name these through
// `$crate::embassy::...` without the user's crate depending on `embassy-net`
// under those exact paths.
#[doc(hidden)]
pub use embassy_net::IpEndpoint;
#[doc(hidden)]
pub use embassy_net::udp::PacketMetadata;

use embassy_net::IpAddress;
use embassy_time::Duration as EmbassyDuration;

use crate::proto::{DISCOVERY_UNIVERSE, SACN_PORT, ipv4_multicast, ipv6_multicast};
use crate::time::Duration;

fn v4_group(universe: u16) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv4(ipv4_multicast(universe)), SACN_PORT)
}

fn v6_group(universe: u16) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv6(ipv6_multicast(universe)), SACN_PORT)
}

/// Maps a core [`Duration`] onto an `embassy_time` [`Duration`].
///
/// The core reports timer deadlines in [`core::time::Duration`]; the adapter
/// needs them as `embassy_time::Duration` to arm an `embassy_time::Timer`.
fn to_embassy_duration(duration: Duration) -> EmbassyDuration {
    // u64 in microsecs -> 584k years, no truncation risk
    EmbassyDuration::from_micros(duration.as_micros() as u64)
}

/// Maps an `embassy_time` [`Duration`](EmbassyDuration) onto a core [`Duration`].
///
/// Used to translate the runtime's elapsed time since the adapter's epoch into
/// the [`core::time::Duration`] the core's `now` is expressed against.
fn from_embassy_duration(duration: EmbassyDuration) -> Duration {
    duration.into()
}
