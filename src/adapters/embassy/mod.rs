//! The embassy runtime adapter.
//!
//! This is the default entrypoint to sACN for embedded targets using the
//! [embassy](https://embassy.dev) async runtime and its `embassy-net` stack.
//! It is `no_std` and allocation-free.
//!
//! - [`Source`] wraps [`crate::source::Source`] and transmits sACN.
//!
//! # Setup
//!
//! This adapter depends on `embassy-net` but deliberately does not select a
//! link-layer *medium* for it, because the right one depends on your hardware.
//! `embassy-net` (via `smoltcp`) needs at least one `medium-*` feature enabled
//! to compile, so your own crate must turn one on. Without it the network stack
//! fails to build. For example, to use ethernet:
//!
//! ```toml
//! [dependencies]
//! sacn = { version = "0.1", default-features = false, features = ["embassy"] }
//! embassy-net = { version = "0.9", features = ["medium-ethernet"] }
//! ```

mod error;
mod source;

pub use error::EmbassyError;
pub use source::Source;

use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_time::Duration as EmbassyDuration;

use crate::proto::{DISCOVERY_UNIVERSE, SACN_PORT, ipv4_multicast, ipv6_multicast};
use crate::source::Route;
use crate::time::Duration;

// IPv4 and IPv6
const MAX_TARGETS: usize = 2;

type Targets = heapless::Vec<IpEndpoint, MAX_TARGETS>;

fn route_universe(route: Route) -> u16 {
    match route {
        Route::Universe(universe) => universe.get(),
        Route::Discovery => DISCOVERY_UNIVERSE,
        Route::Sync(sync_universe) => sync_universe.get(),
    }
}

fn v4_group(universe: u16) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv4(ipv4_multicast(universe)), SACN_PORT)
}

fn v6_group(universe: u16) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv6(ipv6_multicast(universe)), SACN_PORT)
}

/// Resolves the multicast destinations a route is delivered to, given the
/// address families the `stack` currently has configured.
fn targets_for(route: Route, stack: Stack<'_>) -> Targets {
    let mut targets = Targets::new();
    let universe = route_universe(route);
    if stack.config_v4().is_some() {
        let _ = targets.push(v4_group(universe));
    }
    if stack.config_v6().is_some() {
        let _ = targets.push(v6_group(universe));
    }
    targets
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
