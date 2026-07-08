//! Shared sACN protocol utilities.

use core::net::{Ipv4Addr, Ipv6Addr};

/// The port all sACN traffic uses.
pub const SACN_PORT: u16 = 5568;

/// The reserved universe sources announce their universe lists on.
pub const DISCOVERY_UNIVERSE: u16 = 64214;

/// The IPv4 multicast group a universe is transmitted on:
/// `239.255.<high byte>.<low byte>`.
pub fn ipv4_multicast(universe: u16) -> Ipv4Addr {
    Ipv4Addr::new(239, 255, (universe >> 8) as u8, universe as u8)
}

/// The IPv6 multicast group a universe is transmitted on:
/// `ff18::8300:<universe>`.
pub fn ipv6_multicast(universe: u16) -> Ipv6Addr {
    Ipv6Addr::new(0xff18, 0, 0, 0, 0, 0, 0x8300, universe)
}
