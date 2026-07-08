//! Shared networking helpers for the adapter layer.

use std::{fmt, io, iter, net::Ipv4Addr, option, slice};

/// A network interface on which to send or receive multicast traffic.
///
/// IPv4 interfaces are identified by a local address; IPv6 interfaces by their
/// numeric interface index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MulticastInterface {
    /// An IPv4 interface, identified by one of its local addresses.
    V4(Ipv4Addr),
    /// An IPv6 interface, identified by its numeric interface index.
    V6(u32),
}

impl fmt::Display for MulticastInterface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V4(addr) => addr.fmt(f),
            Self::V6(index) => write!(f, "index {index} (IPv6)"),
        }
    }
}

/// Types that can be converted into a set of [`MulticastInterface`]s.
//
// Not sealed: this is intentionally open for callers to implement for their own
// interface-collection types.
pub trait ToMulticastInterfaces {
    /// The iterator produced by [`to_multicast_interfaces`].
    ///
    /// [`to_multicast_interfaces`]: ToMulticastInterfaces::to_multicast_interfaces
    type Iter: Iterator<Item = MulticastInterface>;

    /// Converts `self` into an iterator of [`MulticastInterface`]s.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the value cannot be interpreted as one or
    /// more interfaces (for example, a string that is not a valid IPv4 address).
    fn to_multicast_interfaces(&self) -> io::Result<Self::Iter>;
}

impl ToMulticastInterfaces for MulticastInterface {
    type Iter = option::IntoIter<MulticastInterface>;

    fn to_multicast_interfaces(&self) -> io::Result<Self::Iter> {
        Ok(Some(*self).into_iter())
    }
}

impl ToMulticastInterfaces for Ipv4Addr {
    type Iter = option::IntoIter<MulticastInterface>;

    fn to_multicast_interfaces(&self) -> io::Result<Self::Iter> {
        MulticastInterface::V4(*self).to_multicast_interfaces()
    }
}

// Strings are treated as IPv4 addresses implicitly.
impl ToMulticastInterfaces for str {
    type Iter = option::IntoIter<MulticastInterface>;

    fn to_multicast_interfaces(&self) -> io::Result<Self::Iter> {
        self.parse::<Ipv4Addr>()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?
            .to_multicast_interfaces()
    }
}

impl ToMulticastInterfaces for String {
    type Iter = option::IntoIter<MulticastInterface>;

    fn to_multicast_interfaces(&self) -> io::Result<Self::Iter> {
        (**self).to_multicast_interfaces()
    }
}

impl<'a> ToMulticastInterfaces for &'a [MulticastInterface] {
    type Iter = iter::Cloned<slice::Iter<'a, MulticastInterface>>;

    fn to_multicast_interfaces(&self) -> io::Result<Self::Iter> {
        Ok(self.iter().cloned())
    }
}

impl<T: ToMulticastInterfaces + ?Sized> ToMulticastInterfaces for &T {
    type Iter = T::Iter;

    fn to_multicast_interfaces(&self) -> io::Result<Self::Iter> {
        (**self).to_multicast_interfaces()
    }
}
