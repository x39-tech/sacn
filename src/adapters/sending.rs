//! Helpers for adapters that send sACN to multiple destinations.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;

use crate::adapters::MulticastInterface;
use crate::log::{info, warning};
use crate::proto::DISCOVERY_UNIVERSE;
use crate::source::Route;
use crate::types::Universe;

/// Encapsulates both the per-universe destination table for a source and
/// state tracking of which destinations are currently failing. The latter is
/// used for log suppression.
#[derive(Debug)]
pub(super) struct Routes {
    destinations: HashMap<Universe, Destinations>,
    failing: FailingRoutes,
}

impl Routes {
    pub(super) fn new() -> Self {
        Self {
            destinations: HashMap::new(),
            failing: FailingRoutes::default(),
        }
    }

    /// Adds a universe along with its set of outgoing multicast interfaces and
    /// its synchronization universe (`0` if not synchronized).
    pub(super) fn add_universe(
        &mut self,
        universe: Universe,
        interfaces: Vec<MulticastInterface>,
        sync_universe: u16,
    ) {
        self.destinations.insert(
            universe,
            Destinations {
                interfaces,
                unicast: Vec::new(),
                sync_universe,
            },
        );
    }

    /// Retires a universe and, with it, any failure state for its endpoints.
    pub(super) fn remove_universe(&mut self, universe: Universe) {
        let Some(dest) = self.destinations.remove(&universe) else {
            return;
        };
        for interface in dest.interfaces {
            self.failing.forget(&SendTarget::Multicast {
                universe: universe.get(),
                interface,
            });
        }
        for addr in dest.unicast {
            self.failing.forget(&SendTarget::Unicast(addr));
        }
    }

    /// Records a universe's synchronization universe (`0` to clear it), so a
    /// [`Route::Sync`] expands to the right group members. A no-op if the
    /// universe is not present.
    pub(super) fn set_sync(&mut self, universe: Universe, sync_universe: u16) {
        if let Some(dest) = self.destinations.get_mut(&universe) {
            dest.sync_universe = sync_universe;
        }
    }

    /// Adds a unicast destination for a universe. Returns `false` if the
    /// universe is not present or the address was already a destination.
    pub(super) fn add_unicast(&mut self, universe: Universe, addr: SocketAddr) -> bool {
        let Some(dest) = self.destinations.get_mut(&universe) else {
            return false;
        };
        if dest.unicast.contains(&addr) {
            return false;
        }
        dest.unicast.push(addr);
        true
    }

    /// Removes a unicast destination from a universe, along with any failure
    /// state for it. Returns `false` if the universe or address was not present.
    pub(super) fn remove_unicast(&mut self, universe: Universe, addr: SocketAddr) -> bool {
        let Some(dest) = self.destinations.get_mut(&universe) else {
            return false;
        };
        let Some(pos) = dest.unicast.iter().position(|&a| a == addr) else {
            return false;
        };
        dest.unicast.remove(pos);
        self.failing.forget(&SendTarget::Unicast(addr));
        true
    }

    /// Records the outcome of a send to `target`, logging only on the transition
    /// into and out of failure. Reports the outcome of a send whose concrete
    /// targets were already resolved by [`targets_for`](Self::targets_for).
    pub(super) fn report(&mut self, target: SendTarget, result: std::io::Result<usize>) {
        self.failing.report(target, result);
    }

    /// Expands a [`Route`] into the ordered list of concrete endpoints a single
    /// packet must reach: each universe multicast group on its interfaces then
    /// its unicast addresses, or (for discovery) the reserved group on the union
    /// of every universe's interfaces, each interface once.
    pub(super) fn targets_for(&self, route: Route) -> Vec<SendTarget> {
        let mut targets = Vec::new();
        match route {
            Route::Universe(universe) => {
                let Some(dest) = self.destinations.get(&universe) else {
                    return targets;
                };
                for &interface in &dest.interfaces {
                    targets.push(SendTarget::Multicast {
                        universe: universe.get(),
                        interface,
                    });
                }
                for &addr in &dest.unicast {
                    targets.push(SendTarget::Unicast(addr));
                }
            }
            Route::Discovery => {
                for dest in self.destinations.values() {
                    for &interface in &dest.interfaces {
                        let target = SendTarget::Multicast {
                            universe: DISCOVERY_UNIVERSE,
                            interface,
                        };
                        if !targets.contains(&target) {
                            targets.push(target);
                        }
                    }
                }
            }
            Route::Sync(sync_universe) => {
                // The sync packet goes to the sync universe's own multicast group
                // on the union of its members' interfaces, plus the union of the
                // members' unicast destinations, deduplicated.
                let sync = sync_universe.get();
                for dest in self.destinations.values() {
                    if dest.sync_universe != sync {
                        continue;
                    }
                    for &interface in &dest.interfaces {
                        let target = SendTarget::Multicast {
                            universe: sync,
                            interface,
                        };
                        if !targets.contains(&target) {
                            targets.push(target);
                        }
                    }
                    for &addr in &dest.unicast {
                        let target = SendTarget::Unicast(addr);
                        if !targets.contains(&target) {
                            targets.push(target);
                        }
                    }
                }
            }
        }
        targets
    }
}

/// Holds a set of configured destinations, generally tracked per-universe of
/// sACN.
#[derive(Debug)]
pub(super) struct Destinations {
    pub(super) interfaces: Vec<MulticastInterface>,
    pub(super) unicast: Vec<SocketAddr>,
    pub(super) sync_universe: u16,
}

/// Describes a destination to which a sACN packet could be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum SendTarget {
    /// A universe's multicast group sent out of a specific interface.
    Multicast {
        universe: u16,
        interface: MulticastInterface,
    },
    /// A unicast address with port.
    Unicast(SocketAddr),
}

impl fmt::Display for SendTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Multicast {
                universe,
                interface,
            } if *universe == DISCOVERY_UNIVERSE => {
                write!(f, "discovery multicast via {interface}")
            }
            Self::Multicast {
                universe,
                interface,
            } => {
                write!(f, "universe {universe} multicast via {interface}")
            }
            Self::Unicast(addr) => write!(f, "unicast {addr}"),
        }
    }
}

/// The set of endpoints whose last send failed. Used to ensure that a
/// persistent error is logged once on the way into failure and once on
/// recovery.
#[derive(Debug, Default)]
struct FailingRoutes(HashSet<SendTarget>);

impl FailingRoutes {
    /// Records the outcome of a send to `target`, logging only on the
    /// transition into a failed state and again on recovery.
    fn report(&mut self, target: SendTarget, result: std::io::Result<usize>) {
        match result {
            Err(error) => {
                if self.0.insert(target) {
                    warning!("sACN source send to {} failed: {}", target, error);
                }
            }
            Ok(_) => {
                if self.0.remove(&target) {
                    info!("sACN source send to {} recovered", target);
                }
            }
        }
    }

    /// Drops any failure state for `target`, called when its endpoint goes away.
    fn forget(&mut self, target: &SendTarget) {
        self.0.remove(target);
    }
}
