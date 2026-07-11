//! Helpers for the embassy Source involving send fanout to multiple
//! destinations.

use core::fmt;

use embassy_net::IpEndpoint;
use embassy_net::udp::SendError;

use crate::log::{info, warning};
use crate::source::Route;
use crate::storage::{MapLike, VecLike};
use crate::types::Universe;

use super::storage::{Destinations, SourceStorage};
use super::{DISCOVERY_UNIVERSE, v4_group, v6_group};

/// The per-universe destination tables together with the send-failure state
/// that must be kept in lockstep with them. The latter is used for log
/// suppression.
pub(super) struct Routes<'d, S: SourceStorage> {
    destinations: &'d mut S::Destinations,
    failing: FailingTargets<S>,
}

impl<'d, S: SourceStorage> Routes<'d, S> {
    pub(super) fn new(destinations: &'d mut S::Destinations) -> Self {
        Self {
            destinations,
            failing: FailingTargets::default(),
        }
    }

    /// Adds a destination table for a freshly added universe, recording whether
    /// it transmits multicast and its synchronization address (`0` if
    /// unsynchronized).
    pub(super) fn add_universe(&mut self, universe: Universe, multicast: bool, sync_universe: u16) {
        let mut dest = Destinations::new();
        dest.multicast = multicast;
        dest.sync_universe = sync_universe;
        self.destinations.upsert_expect(universe, dest);
    }

    /// Retires a universe together with all failure state for its multicast
    /// groups and unicast endpoints.
    pub(super) fn remove_universe(&mut self, universe: Universe) {
        if let Some(dest) = self.destinations.get(&universe) {
            self.failing.forget(&v4_group(universe.get()));
            self.failing.forget(&v6_group(universe.get()));
            for endpoint in dest.unicast.iter() {
                self.failing.forget(endpoint);
            }
        }
        self.destinations.remove(&universe);
    }

    /// Records a universe's synchronization address (`0` to clear it) so a
    /// [`Route::Sync`] expands to the right group members. A no-op if the
    /// universe is not present.
    pub(super) fn set_sync(&mut self, universe: Universe, sync_universe: u16) {
        if let Some(dest) = self.destinations.get_mut(&universe) {
            dest.sync_universe = sync_universe;
        }
    }

    /// Adds a unicast destination to a universe. Returns `false` if the universe
    /// is not present, the endpoint was already a destination, or the table is
    /// full.
    pub(super) fn add_unicast(&mut self, universe: Universe, endpoint: IpEndpoint) -> bool {
        let Some(dest) = self.destinations.get_mut(&universe) else {
            return false;
        };
        if dest.unicast.as_slice().contains(&endpoint) {
            return false;
        }
        dest.unicast.push(endpoint).is_ok()
    }

    /// Removes a unicast destination from a universe, along with any failure
    /// state for it. Returns `false` if the universe or endpoint was not present.
    pub(super) fn remove_unicast(&mut self, universe: Universe, endpoint: IpEndpoint) -> bool {
        let Some(dest) = self.destinations.get_mut(&universe) else {
            return false;
        };
        let Some(pos) = dest.unicast.as_slice().iter().position(|e| *e == endpoint) else {
            return false;
        };
        dest.unicast.remove(pos);
        self.failing.forget(&endpoint);
        true
    }

    /// Records the outcome of a send to `target`, logging only on the transition
    /// into and out of failure. `target` must be one of the endpoints
    /// [`targets_for`](Self::targets_for) resolved.
    pub(super) fn report(&mut self, target: IpEndpoint, result: Result<(), SendError>) {
        self.failing.report(target, result);
    }

    /// Expands a [`Route`] into the concrete endpoints one packet must reach on
    /// the address families the stack has configured. See
    /// [`resolve_targets_for_families`].
    pub(super) fn targets_for(&self, route: Route, v4: bool, v6: bool) -> S::SendTargets {
        resolve_targets_for_families::<S>(route, v4, v6, self.destinations)
    }
}

impl<S: SourceStorage> fmt::Debug for Routes<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Routes")
            .field("destinations", &self.destinations)
            .field("failing", &self.failing)
            .finish()
    }
}

/// Expands a [`Route`] into the concrete endpoints one packet must reach: its
/// multicast groups on the given address families, plus its unicast
/// destinations.
///
/// A [`Route::Universe`] adds the universe's own multicast group and unicast
/// destinations; a [`Route::Sync`] adds the sync group and the unicast
/// destinations of every universe synchronized on that address (deduplicated);
/// [`Route::Discovery`] is multicast only.
pub(super) fn resolve_targets_for_families<S: SourceStorage>(
    route: Route,
    v4: bool,
    v6: bool,
    destinations: &S::Destinations,
) -> S::SendTargets {
    let mut targets = S::SendTargets::default();

    match route {
        Route::Universe(universe) => {
            let Some(dest) = destinations.get(&universe) else {
                return targets;
            };
            // The universe's own multicast group, then its unicast destinations.
            if dest.multicast {
                add_multicast(&mut targets, universe.get(), v4, v6);
            }
            for endpoint in dest.unicast.iter() {
                targets.push_expect(*endpoint);
            }
        }
        Route::Discovery => {
            // Discovery advertises the source by multicast only, so it is sent
            // only while at least one universe still transmits multicast.
            if destinations.iter().any(|(_, dest)| dest.multicast) {
                add_multicast(&mut targets, DISCOVERY_UNIVERSE, v4, v6);
            }
        }
        Route::Sync(sync_universe) => {
            let sync = sync_universe.get();
            // The sync group's own multicast, sent while any member of the group
            // still transmits multicast.
            let any_multicast = destinations
                .iter()
                .any(|(_, dest)| dest.sync_universe == sync && dest.multicast);
            if any_multicast {
                add_multicast(&mut targets, sync, v4, v6);
            }
            // Then the union of the members' unicast destinations, deduplicating
            // endpoints shared across members.
            for (_, dest) in destinations.iter() {
                if dest.sync_universe == sync {
                    for endpoint in dest.unicast.iter() {
                        if !targets.as_slice().contains(endpoint) {
                            targets.push_expect(*endpoint);
                        }
                    }
                }
            }
        }
    }
    targets
}

/// Appends the IPv4 and/or IPv6 multicast group of `universe` for whichever
/// address families are configured.
fn add_multicast<V: VecLike<IpEndpoint>>(targets: &mut V, universe: u16, v4: bool, v6: bool) {
    if v4 {
        targets.push_expect(v4_group(universe));
    }
    if v6 {
        targets.push_expect(v6_group(universe));
    }
}

/// The set of destinations whose last send failed. Used to log a persistent
/// error only once on the way into failure and again on recovery.
///
/// Backed by [`S::FailingTargets`](SourceStorage::FailingTargets), which is
/// sized to hold every distinct endpoint the source can send to, so recording a
/// new failure with [`push_expect`](VecLike::push_expect) never overflows.
struct FailingTargets<S: SourceStorage>(S::FailingTargets);

impl<S: SourceStorage> Default for FailingTargets<S> {
    fn default() -> Self {
        Self(S::FailingTargets::default())
    }
}

impl<S: SourceStorage> fmt::Debug for FailingTargets<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FailingTargets").field(&self.0).finish()
    }
}

impl<S: SourceStorage> FailingTargets<S> {
    /// Records the outcome of a send to `target`, logging only on the transition
    /// into a failed state and again on recovery.
    fn report(&mut self, target: IpEndpoint, result: Result<(), SendError>) {
        match result {
            Err(error) => {
                if !self.0.as_slice().contains(&target) {
                    self.0.push_expect(target);
                    warning!("sACN source send to {} failed: {:?}", target, error);
                }
            }
            Ok(()) => {
                if let Some(pos) = self.0.as_slice().iter().position(|t| *t == target) {
                    self.0.remove(pos);
                    info!("sACN source send to {} recovered", target);
                }
            }
        }
    }

    /// Drops any failure state for `target`, called when its endpoint goes away.
    fn forget(&mut self, target: &IpEndpoint) {
        if let Some(pos) = self.0.as_slice().iter().position(|t| t == target) {
            self.0.remove(pos);
        }
    }
}

#[cfg(test)]
#[path = "sending_tests.rs"]
mod tests;
