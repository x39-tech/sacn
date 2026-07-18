# sACN

[![Crates.io](https://img.shields.io/crates/v/x39-sacn.svg)](https://crates.io/crates/x39-sacn)
[![Docs.rs](https://docs.rs/x39-sacn/badge.svg)](https://docs.rs/x39-sacn)
[![License](https://img.shields.io/crates/l/x39-sacn.svg)](https://github.com/x39-tech/sacn/blob/main/LICENSE)

A full-featured, production-ready, embedded-friendly Rust implementation of the "Streaming ACN" (sACN, ANSI E1.31) protocol for sending and receiving DMX data over IP networks. The sACN protocol is ubiquitous in live entertainment and some building automation applications, usually used to control lighting equipment.

## Quick start

```toml
[dependencies]
x39-sacn = "0.2"
```

In a [tokio](https://tokio.rs/) application, to receive merged DMX for a universe:

```rust,no_run
use sacn::tokio::Receiver;
use sacn::{ReceiverConfig, ReceiverEvent, Universe};

#[tokio::main]
async fn main() -> Result<(), sacn::AdapterError> {
    let mut rx = Receiver::bind(ReceiverConfig::new()).await?;
    rx.listen(Universe::new(1).unwrap()).await?;
    while let Some(event) = rx.next_event().await {
        match event {
            ReceiverEvent::MergedData(data) => {
                println!("universe 1: {:?}", &data.levels()[..8]);
            }
            // Source loss, synchronized releases, and more arrive as other
            // variants; see `ReceiverEvent` for the full set.
            _ => {}
        }
    }
    Ok(())
}
```

To transmit DMX on a universe:

```rust,no_run
use sacn::tokio::Source;
use sacn::{Cid, SourceConfig, UniverseConfig, Universe};

#[tokio::main]
async fn main() -> Result<(), sacn::AdapterError> {
    let config = SourceConfig::new(Cid::from_bytes([1; 16]), "My Source");
    let mut source = Source::bind(config).await?;
    let universe = Universe::new(1).unwrap();
    source.add_universe(UniverseConfig::new(universe))?;
    source.update_levels(universe, &[255, 128, 0]);

    // A source transmits on its own schedule, sending keep-alives even when the
    // data is unchanged. Drive it by sending what is due, then waiting.
    loop {
        match source.process().await? {
            Some(at) => tokio::time::sleep_until(at).await,
            None => break,
        }
    }
    Ok(())
}
```

See the [API documentation on docs.rs](https://docs.rs/x39-sacn) for the full tutorial, including universe synchronization, the per-source `BasicReceiver`, the `SourceDetector` to discover transmitting sources on a network, and the `no_std` protocol core; the `examples/` directory has complete terminal programs.

## Structure

The crate is organized in layers: an I/O-free protocol **core** (which is `no_std`) and **runtime adapters** that drive it. Most users only touch an adapter. The core (the receivers, source, merger and source detector) is a pure `(packets, time) -> events` state machine, exposed for embedded and advanced use or for driving from a runtime that has no adapter yet.

## Design Approach

Let's revisit the claims from the first paragraph that this library is _full-featured_, _production-ready_, and _embedded-friendly_. That's a lot of claims. What do we mean by them?

### Full-featured

**Full-featured** means this library strives to support any standard-compliant use case of sACN, including lesser-used features like alternate START codes, and features added in its recent revisions such as synchronization, universe discovery, IPv6 support, etc. It exposes a simple interface with sensible defaults, but there is a deep set of tunable parameters for those who want to get maximum performance from sACN. If it doesn't support a use case that you need, please don't hesitate to open an issue.

### Production-ready

**Production-ready** means that this implementation has logic to handle situations that occur in the largest and most complex sACN installations. sACN is a deceptively simple protocol with much hidden complexity, especially when reconciling data from multiple sources on the same universe. Features like universe synchronization add even more state-handling complexity. We strive to meet that complexity head-on, while maintaining easy-to-use, idiomatic APIs and our other design goals such as embedded-friendliness.

This also means that this crate is obsessively tested, with a full suite of unit and integration tests, property tests, and continuous fuzz testing of its protocol parsers. Please do not hesitate to submit any issues you find.

### Embedded-friendly

**Embedded-friendly** means that this library intends to provide first-class support for embedded applications. The core logic is `no_std`, and `alloc`-free configurations are also supported. Binary compilation size and memory usage are routinely checked against sane limits. A runtime adapter for the popular [Embassy](https://embassy.dev/) embedded framework is provided.

Embedded users should check out the [Embassy example](/examples/embassy/README.md) for a starting point.

## License

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

## Acknowledgments

This sACN implementation takes some algorithmic inspiration from the [implementation by ETC](https://github.com/ETCLabs/sACN), which is as good a reference implementation of sACN as exists in the open-source world. **Nick Ballhorn-Wagner** created most of the original algorithms in that implementation, and **Christian Reese** has been its steward and maintainer.
