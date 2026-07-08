//! Host tests for the embassy adapter's pure translation helpers.

use embassy_net::{IpAddress, IpEndpoint};
use embassy_time::Duration as EmbassyDuration;

use crate::proto::SACN_PORT;
use crate::source::Route;
use crate::time::Duration;
use crate::types::Universe;

use super::super::{
    from_embassy_duration, route_universe, to_embassy_duration, v4_group, v6_group,
};

#[test]
fn every_route_carries_its_universe() {
    let universe = Universe::new(0x1234).unwrap();
    assert_eq!(route_universe(Route::Universe(universe)), 0x1234);
    // The reserved discovery universe, 64214.
    assert_eq!(route_universe(Route::Discovery), 64214);
    // A sync packet targets its sync universe's own multicast group; a
    // multicast-only source has no member interface/unicast sets to union.
    let sync = Universe::new(7000).unwrap();
    assert_eq!(route_universe(Route::Sync(sync)), 7000);
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
