//! Tests for [`Routes`].

use embassy_net::udp::SendError;
use embassy_net::{IpAddress, IpEndpoint};

use crate::proto::SACN_PORT;
use crate::storage::{MapLike, VecLike};
use crate::types::Universe;

use super::super::storage::SourceStorage;
use super::super::{v4_group, v6_group};
use super::Routes;

crate::embassy_static_storage! {
    struct Caps {
        tx_universes: 4,
        tx_unicast_per_universe: 4,
    }
}

type Dests = <Caps as SourceStorage>::Destinations;

fn uni(n: u16) -> Universe {
    Universe::new(n).unwrap()
}

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpEndpoint {
    IpEndpoint::new(IpAddress::v4(a, b, c, d), SACN_PORT)
}

/// Whether `endpoint` is currently tracked as failing.
fn is_failing(routes: &Routes<'_, Caps>, endpoint: &IpEndpoint) -> bool {
    routes.failing.0.as_slice().contains(endpoint)
}

#[test]
fn remove_unicast_drops_its_failure_state() {
    let mut dests = Dests::default();
    let mut routes = Routes::<Caps>::new(&mut dests);
    let dest = v4(10, 0, 0, 5);

    routes.add_universe(uni(1), true, 0);
    assert!(routes.add_unicast(uni(1), dest));
    // Mark the endpoint as failing, then retire it.
    routes.report(dest, Err(SendError::NoRoute));
    assert!(is_failing(&routes, &dest));

    assert!(routes.remove_unicast(uni(1), dest));
    assert!(!is_failing(&routes, &dest));
    // Removing again is a no-op now that the endpoint is gone.
    assert!(!routes.remove_unicast(uni(1), dest));
}

#[test]
fn remove_universe_drops_multicast_and_unicast_failure_state() {
    let mut dests = Dests::default();
    let mut routes = Routes::<Caps>::new(&mut dests);
    let dest = v4(10, 0, 0, 7);

    routes.add_universe(uni(1), true, 0);
    assert!(routes.add_unicast(uni(1), dest));
    // Fail both multicast groups and the unicast destination.
    routes.report(v4_group(1), Err(SendError::NoRoute));
    routes.report(v6_group(1), Err(SendError::NoRoute));
    routes.report(dest, Err(SendError::NoRoute));

    routes.remove_universe(uni(1));

    assert!(!is_failing(&routes, &v4_group(1)));
    assert!(!is_failing(&routes, &v6_group(1)));
    assert!(!is_failing(&routes, &dest));
    assert!(!routes.destinations.contains_key(&uni(1)));
}

#[test]
fn report_tracks_a_single_entry_across_the_failure_transition() {
    let mut dests = Dests::default();
    let mut routes = Routes::<Caps>::new(&mut dests);
    let target = v4_group(1);

    // Repeated failures keep exactly one entry (deduplicated).
    routes.report(target, Err(SendError::NoRoute));
    routes.report(target, Err(SendError::NoRoute));
    assert_eq!(routes.failing.0.len(), 1);

    // Recovery clears it; a further success is a no-op.
    routes.report(target, Ok(()));
    assert!(!is_failing(&routes, &target));
    routes.report(target, Ok(()));
    assert_eq!(routes.failing.0.len(), 0);
}

#[test]
fn add_unicast_rejects_duplicates_and_unknown_universes() {
    let mut dests = Dests::default();
    let mut routes = Routes::<Caps>::new(&mut dests);
    let dest = v4(10, 0, 0, 9);

    // Unknown universe.
    assert!(!routes.add_unicast(uni(1), dest));

    routes.add_universe(uni(1), true, 0);
    assert!(routes.add_unicast(uni(1), dest));
    // Duplicate.
    assert!(!routes.add_unicast(uni(1), dest));
}

const FAILING_CAPACITY: usize =
    <<Caps as SourceStorage>::FailingTargets as VecLike<IpEndpoint>>::CAPACITY;
const TX_UNIVERSES: u16 = <Caps as SourceStorage>::Destinations::CAPACITY as u16;
const TX_UNICAST_PER_UNIVERSE: usize = <Caps as SourceStorage>::Unicast::CAPACITY;

#[test]
fn failure_buffer_holds_every_endpoint_at_full_capacity() {
    assert_eq!(FAILING_CAPACITY, (4 + 4) * 4 + 2);

    let mut dests = Dests::default();
    let mut routes = Routes::<Caps>::new(&mut dests);

    for u in 1..=TX_UNIVERSES {
        routes.add_universe(uni(u), true, 100 + u);
        for k in 1..=TX_UNICAST_PER_UNIVERSE {
            assert!(routes.add_unicast(uni(u), v4(10, 0, u as u8, k as u8)));
        }
    }

    // Fail every distinct endpoint the source could ever emit to: each
    // universe's data group and its sync group on both families, plus its
    // unicast, plus the discovery group on both families.
    for u in 1..=TX_UNIVERSES {
        routes.report(v4_group(u), Err(SendError::NoRoute));
        routes.report(v6_group(u), Err(SendError::NoRoute));
        routes.report(v4_group(100 + u), Err(SendError::NoRoute));
        routes.report(v6_group(100 + u), Err(SendError::NoRoute));
        for k in 1..=4u8 {
            routes.report(v4(10, 0, u as u8, k), Err(SendError::NoRoute));
        }
    }
    routes.report(
        v4_group(super::super::DISCOVERY_UNIVERSE),
        Err(SendError::NoRoute),
    );
    routes.report(
        v6_group(super::super::DISCOVERY_UNIVERSE),
        Err(SendError::NoRoute),
    );

    assert_eq!(routes.failing.0.len(), FAILING_CAPACITY);
}
