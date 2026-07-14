# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0](https://github.com/x39-tech/sacn/compare/v0.1.0...v0.2.0) - 2026-07-14

### Added

- add embassy::SourceDetector ([#31](https://github.com/x39-tech/sacn/pull/31))
- add embassy::BasicReceiver and embassy::Receiver ([#29](https://github.com/x39-tech/sacn/pull/29))
- embassy: Add an example application ([#22](https://github.com/x39-tech/sacn/pull/22))
- embassy::Source: feature parity with tokio ([#20](https://github.com/x39-tech/sacn/pull/20))

### Changed

- Split storage and logic on core types to better accommodate embedded adapters:
  - SourceDetector ([#30](https://github.com/x39-tech/sacn/pull/30))
  - BasicReceiver and Receiver ([#27](https://github.com/x39-tech/sacn/pull/27))
  - Source ([#18](https://github.com/x39-tech/sacn/pull/18))

## [0.1.0] - 2026-07-08

### Added

- Initial release: a `no_std`-friendly implementation of the sACN (ANSI E1.31) protocol.

[0.1.0]: https://github.com/x39-tech/sacn/compare/f68ce68...v0.1.0
