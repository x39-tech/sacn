//! Host tests for the embassy adapter.
//!
//! Not currently exercised on a live `embassy-net` stack; only pure helpers
//! are tested.

use embassy_net::{IpAddress, IpEndpoint};
use embassy_time::Duration as EmbassyDuration;

use crate::proto::SACN_PORT;
use crate::source::Route;
use crate::storage::MapLike;
use crate::time::Duration;
use crate::types::Universe;

use super::super::sending::resolve_targets_for_families;
use super::super::{
    Destinations, SourceResources, SourceStorage, from_embassy_duration, to_embassy_duration,
    v4_group, v6_group,
};

crate::embassy_static_storage! {
    struct Caps {
        tx_universes: 4,
        tx_unicast_per_universe: 4,
    }
}

fn uni(n: u16) -> Universe {
    Universe::new(n).unwrap()
}

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpEndpoint {
    IpEndpoint::new(IpAddress::v4(a, b, c, d), SACN_PORT)
}

/// Builds a `Caps` destination table from `(universe, sync_universe, multicast,
/// unicast)` tuples.
fn destinations(
    entries: &[(u16, u16, bool, &[IpEndpoint])],
) -> <Caps as SourceStorage>::Destinations {
    let mut map = <Caps as SourceStorage>::Destinations::default();
    for &(universe, sync_universe, multicast, unicast) in entries {
        let mut dest = Destinations::<Caps>::new();
        dest.multicast = multicast;
        dest.sync_universe = sync_universe;
        for &endpoint in unicast {
            dest.unicast.push(endpoint).unwrap();
        }
        map.upsert(uni(universe), dest).unwrap();
    }
    map
}

fn resolve(
    route: Route,
    v4_up: bool,
    v6_up: bool,
    dests: &[(u16, u16, bool, &[IpEndpoint])],
) -> Vec<IpEndpoint> {
    let map = destinations(dests);
    resolve_targets_for_families::<Caps>(route, v4_up, v6_up, &map)
        .iter()
        .copied()
        .collect()
}

#[test]
fn v4_group_is_the_universe_multicast_address() {
    // 239.255.<high>.<low> of the universe, on the sACN port.
    let expected = IpEndpoint::new(
        IpAddress::Ipv4(core::net::Ipv4Addr::new(239, 255, 0x12, 0x34)),
        SACN_PORT,
    );
    assert_eq!(v4_group(0x1234), expected);
}

#[test]
fn v6_group_is_the_universe_multicast_address() {
    // ff18::8300:<universe>, on the sACN port.
    let expected = IpEndpoint::new(
        IpAddress::Ipv6(core::net::Ipv6Addr::new(
            0xff18, 0, 0, 0, 0, 0, 0x8300, 0x1234,
        )),
        SACN_PORT,
    );
    assert_eq!(v6_group(0x1234), expected);
}

#[test]
fn duration_round_trips_through_microseconds() {
    for micros in [0u64, 22_000, 900_000, 1_000_000, 10_000_000] {
        let core = Duration::from_micros(micros);
        assert_eq!(
            to_embassy_duration(core),
            EmbassyDuration::from_micros(micros)
        );
        assert_eq!(
            from_embassy_duration(EmbassyDuration::from_micros(micros)),
            core
        );
    }
}

#[test]
fn universe_route_resolves_multicast_then_unicast() {
    let dest = v4(10, 0, 0, 5);
    let targets = resolve(
        Route::Universe(uni(1)),
        true,
        true,
        &[(1, 0, true, &[dest])],
    );
    assert_eq!(targets, vec![v4_group(1), v6_group(1), dest]);
}

#[test]
fn only_configured_families_contribute_multicast() {
    let uni1 = &[(1u16, 0u16, true, &[][..])][..];
    assert_eq!(
        resolve(Route::Universe(uni(1)), true, false, uni1),
        vec![v4_group(1)]
    );
    assert_eq!(
        resolve(Route::Universe(uni(1)), false, true, uni1),
        vec![v6_group(1)]
    );
    assert!(resolve(Route::Universe(uni(1)), false, false, uni1).is_empty());
}

#[test]
fn universe_reaches_its_unicast_destinations_with_no_families() {
    let dest = v4(10, 0, 0, 7);
    let targets = resolve(
        Route::Universe(uni(1)),
        false,
        false,
        &[(1, 0, true, &[dest])],
    );
    assert_eq!(targets, vec![dest]);
}

#[test]
fn unicast_only_universe_skips_its_multicast_group() {
    let dest = v4(10, 0, 0, 7);
    let targets = resolve(
        Route::Universe(uni(1)),
        true,
        true,
        &[(1, 0, false, &[dest])],
    );
    assert_eq!(targets, vec![dest]);
}

#[test]
fn discovery_is_multicast_only() {
    let dest = v4(10, 0, 0, 5);
    let targets = resolve(Route::Discovery, true, true, &[(1, 0, true, &[dest])]);
    assert_eq!(
        targets,
        vec![
            v4_group(super::super::DISCOVERY_UNIVERSE),
            v6_group(super::super::DISCOVERY_UNIVERSE)
        ]
    );
}

#[test]
fn discovery_is_suppressed_when_every_universe_is_unicast_only() {
    let dest = v4(10, 0, 0, 5);
    let targets = resolve(
        Route::Discovery,
        true,
        true,
        &[(1, 0, false, &[dest]), (2, 0, false, &[])],
    );
    assert!(targets.is_empty());
}

#[test]
fn discovery_multicasts_when_any_universe_is_multicast() {
    let targets = resolve(
        Route::Discovery,
        true,
        true,
        &[(1, 0, false, &[]), (2, 0, true, &[])],
    );
    assert_eq!(
        targets,
        vec![
            v4_group(super::super::DISCOVERY_UNIVERSE),
            v6_group(super::super::DISCOVERY_UNIVERSE)
        ]
    );
}

#[test]
fn sync_route_unions_member_unicast_deduplicated() {
    let shared = v4(10, 0, 0, 1);
    let only_two = v4(10, 0, 0, 2);
    let other_group = v4(10, 0, 0, 9);
    // Universes 1 and 2 sync on 100 (1 shares `shared`, 2 has `shared` + its
    // own); universe 3 syncs on a different address and must be excluded.
    let targets = resolve(
        Route::Sync(uni(100)),
        true,
        false,
        &[
            (1, 100, true, &[shared]),
            (2, 100, true, &[shared, only_two]),
            (3, 200, true, &[other_group]),
        ],
    );
    // The sync group's own multicast, then the deduplicated member unicast.
    assert_eq!(targets, vec![v4_group(100), shared, only_two]);
    assert!(!targets.contains(&other_group));
}

#[test]
fn sync_multicast_is_suppressed_when_every_member_is_unicast_only() {
    let one = v4(10, 0, 0, 1);
    let two = v4(10, 0, 0, 2);
    let targets = resolve(
        Route::Sync(uni(100)),
        true,
        false,
        &[(1, 100, false, &[one]), (2, 100, false, &[two])],
    );
    assert_eq!(targets, vec![one, two]);
}

#[test]
fn sync_multicasts_when_any_member_is_multicast() {
    let one = v4(10, 0, 0, 1);
    let two = v4(10, 0, 0, 2);
    let targets = resolve(
        Route::Sync(uni(100)),
        true,
        false,
        &[(1, 100, false, &[one]), (2, 100, true, &[two])],
    );
    assert_eq!(targets, vec![v4_group(100), one, two]);
}

#[test]
fn resources_are_const_constructible_for_static_storage() {
    static RESOURCES: static_cell::ConstStaticCell<SourceResources<Caps>> =
        static_cell::ConstStaticCell::new(Caps::embassy_source_resources());
    let resources = RESOURCES.take();
    // The receive/transmit payload buffers default to a single max-size packet.
    assert_eq!(resources.rx_buffer.len(), crate::packet::MAX_PACKET_SIZE);
    assert_eq!(resources.tx_buffer.len(), crate::packet::MAX_PACKET_SIZE);
}
