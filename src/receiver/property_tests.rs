//! Stateful property tests for the basic receiver.
//!
//! The basic receiver has many configuration options that affect its behavior.
//! These configuration options can interact with each other in unexpected ways
//! and together they have the potential to create a combinatorial explosion of
//! behaviors. We try to capture anticipated interactions in the unit tests,
//! but to fill in any gaps, this suite hunts for interactions we did not think
//! of: it drives the receiver with a random configuration and random sequences
//! of operations and, after every step, asserts a handful of invariants that
//! any correct run must satisfy.
//!
//! Crucially, none of these invariants peek inside the receiver. The harness is
//! itself a model: it knows what it injected and observes the emitted events, so
//! it can derive everything it needs to check. The invariants:
//!
//! - **Local liveness.** Right after a packet that we *know* created or
//!   refreshed a source (an in-sequence, non-terminated packet with a processed
//!   START code on a listened universe), `poll(now)` must return a
//!   strictly-future deadline: the state machine must never go to sleep while a
//!   source it just accepted is tracked. Asserted at the instant we are certain
//!   a source exists; its particular value is covering the *birth* of an
//!   as-yet unreported source (one still in its initial PAP wait), which the
//!   global check below cannot see.
//! - **Global liveness.** A source that has emitted any `UniverseData` is
//!   "reported", and every later removal of a reported source is observable (a
//!   `SourcesLost`, or a universe `Stop`) - it can never be the silent drop that
//!   ends an unreported source's PAP wait. So the harness tracks the reported
//!   set exactly, and asserts that whenever it is non-empty `poll` returns a
//!   strictly-future deadline. This is the check with teeth: it holds across a
//!   reported source's whole lifetime, including while it sits in a termination
//!   set during source-loss settling.
//! - **Sampling discipline.** `SamplingStarted`/`SamplingEnded` strictly
//!   alternate per universe, and `UniverseData::is_sampling` agrees with the
//!   period we are observably inside.
//! - **PAP gating.** `SourcePapLost` is only ever emitted when per-address
//!   priority handling is enabled.
//! - **Time-translation invariance.** Running the same operations against a
//!   shifted clock epoch yields an identical stream of events (and identical
//!   deadlines, relative to the epoch) - the core depends only on differences
//!   between instants, never their absolute value.

use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

// These tests should compile under the `no_std` + `alloc` configuration.
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use super::*;
use crate::packet::{DataPacket, Packet, Payload};
use crate::receiver::BasicReceiverEvent;
use crate::time::Duration;
use crate::types::{Cid, NetintId, SequenceNumber};

// Small alphabets so collisions (same universe/CID) actually happen.
const N_UNIVERSES: u16 = 3;
const N_CIDS: u8 = 3;

/// A single operation applied to the receiver. Time advances only via
/// [`Op::Advance`], so the monotonic-clock contract holds by construction.
#[derive(Clone, Debug)]
enum Op {
    Listen {
        universe: u16,
    },
    Stop {
        universe: u16,
    },
    Packet {
        universe: u16,
        cid: u8,
        start_code: StartCode,
        terminated: bool,
    },
    Advance {
        ms: u64,
    },
}

/// The START code of a generated data packet.
#[derive(Clone, Copy, Debug)]
enum StartCode {
    /// NULL START code (DMX levels).
    Null,
    /// Per-address priority (`0xDD`).
    Pap,
    /// Some other alternate START code.
    Other,
}

/// The byte used for the generated "other" alternate START code.
const OTHER_START_CODE: u8 = 0x17;

impl StartCode {
    fn byte(self) -> u8 {
        match self {
            StartCode::Null => DMX_NULL_START_CODE,
            StartCode::Pap => PAP_START_CODE,
            StartCode::Other => OTHER_START_CODE,
        }
    }
}

/// One observed output, recorded so two runs can be compared bit-for-bit.
#[derive(Clone, Debug, PartialEq)]
enum Trace {
    Event(BasicReceiverEvent),
    /// A `poll` deadline, expressed relative to the clock epoch so it is
    /// comparable across epoch shifts.
    Deadline(Option<Duration>),
}

fn cid_of(id: u8) -> Cid {
    Cid::from_bytes([id; 16])
}

fn cid_id(cid: Cid) -> u8 {
    cid.as_bytes()[0]
}

fn addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 5568)
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let start_code = prop_oneof![
        Just(StartCode::Null),
        Just(StartCode::Pap),
        Just(StartCode::Other),
    ];
    prop_oneof![
        (1..=N_UNIVERSES).prop_map(|universe| Op::Listen { universe }),
        (1..=N_UNIVERSES).prop_map(|universe| Op::Stop { universe }),
        (1..=N_UNIVERSES, 0..N_CIDS, start_code, any::<bool>()).prop_map(
            |(universe, cid, start_code, terminated)| Op::Packet {
                universe,
                cid,
                start_code,
                terminated,
            }
        ),
        (0u64..=4000).prop_map(|ms| Op::Advance { ms }),
    ]
}

fn config_strategy() -> impl Strategy<Value = ReceiverConfig> {
    (
        0u64..=4000,
        0u64..=2000,
        any::<bool>(),
        0u64..=4000,
        prop::option::of(1usize..=3),
        // Which START codes are allow-listed (NULL, PAP, other). Each is varied
        // independently so the suite exercises excluding NULL or PAP too.
        (any::<bool>(), any::<bool>(), any::<bool>()),
    )
        .prop_map(
            |(sample_period, extra_hold, pap_handling, pap_wait, source_limit, allowed)| {
                let mut config = ReceiverConfig::new()
                    .with_sample_period(Duration::from_millis(sample_period))
                    .with_extra_hold_time(Duration::from_millis(extra_hold))
                    .with_per_address_priority_handling(pap_handling)
                    .with_per_address_priority_wait_time(Duration::from_millis(pap_wait));
                if let Some(limit) = source_limit {
                    config = config.with_source_limit(limit);
                }
                let (allow_null, allow_pap, allow_other) = allowed;
                let mut codes = Vec::new();
                if allow_null {
                    codes.push(DMX_NULL_START_CODE);
                }
                if allow_pap {
                    codes.push(PAP_START_CODE);
                }
                if allow_other {
                    codes.push(OTHER_START_CODE);
                }
                config.with_allowed_start_codes(&codes)
            },
        )
}

/// The harness: drives a receiver and maintains the observational shadow state
/// needed to check the invariants.
struct Model {
    rx: BasicReceiver,
    epoch_ms: u64,
    now_ms: u64,
    /// Whether a source limit is configured (liveness is only asserted when not,
    /// since at the limit a new source may legitimately be rejected).
    limited: bool,
    pap_handling: bool,
    /// Which generated START codes the configured allow-list processes, as
    /// `(NULL, PAP, other)`.
    allowed_start_codes: (bool, bool, bool),
    /// Universes we have asked to listen to (and not since stopped).
    listening: BTreeSet<u16>,
    /// Whether each universe is currently inside a sampling period, per the
    /// observed `SamplingStarted`/`SamplingEnded` events.
    in_sampling: BTreeMap<u16, bool>,
    /// Sources that have emitted at least one `UniverseData`, as
    /// `(universe, cid)`, and not since been observed lost (or had their
    /// universe stopped). While this set is non-empty, the receiver must report
    /// a future deadline (the global liveness invariant).
    reported: BTreeSet<(u16, u8)>,
    /// Sources we have terminated and not yet seen reported lost. Their level
    /// packets are dropped, so they are excluded from the local liveness check.
    terminated: BTreeSet<(u16, u8)>,
    /// Next sequence number to use per source, kept monotonic so every injected
    /// packet is accepted (sequence handling is covered by the scenario tests).
    next_seq: BTreeMap<(u16, u8), u8>,
    /// Owned events accumulated from the core's operation outcomes, drained (and
    /// checked) by [`drain`](Self::drain).
    events: alloc::collections::VecDeque<BasicReceiverEvent>,
    trace: Vec<Trace>,
}

impl Model {
    fn new(config: ReceiverConfig, epoch_ms: u64) -> Self {
        Self {
            // Reading these private fields is fine: this module is a child of
            // `receiver`, and the harness built the config itself.
            limited: config.source_limit.is_some(),
            pap_handling: config.pap_handling,
            allowed_start_codes: (
                config.processes(DMX_NULL_START_CODE),
                config.processes(PAP_START_CODE),
                config.processes(OTHER_START_CODE),
            ),
            rx: BasicReceiver::new(config),
            epoch_ms,
            now_ms: 0,
            listening: BTreeSet::new(),
            in_sampling: BTreeMap::new(),
            reported: BTreeSet::new(),
            terminated: BTreeSet::new(),
            next_seq: BTreeMap::new(),
            events: alloc::collections::VecDeque::new(),
            trace: Vec::new(),
        }
    }

    fn now(&self) -> Instant {
        Instant::from_epoch(Duration::from_millis(self.epoch_ms + self.now_ms))
    }

    /// Whether the receiver processes (tracks/forwards) this START code, per the
    /// configured allow-list.
    fn processes(&self, start_code: StartCode) -> bool {
        let (null, pap, other) = self.allowed_start_codes;
        match start_code {
            StartCode::Null => null,
            StartCode::Pap => pap,
            StartCode::Other => other,
        }
    }

    fn seq_for(&mut self, universe: u16, cid: u8) -> u8 {
        let slot = self.next_seq.entry((universe, cid)).or_insert(0);
        let seq = *slot;
        *slot = slot.wrapping_add(1);
        seq
    }

    fn feed(&mut self, universe: u16, cid: u8, start_code: u8, terminated: bool) {
        let seq = self.seq_for(universe, cid);
        let values: &[u8] = if start_code == PAP_START_CODE {
            &[200, 200]
        } else if terminated {
            &[]
        } else {
            &[1, 2, 3]
        };
        let packet = Packet {
            cid: cid_of(cid),
            payload: Payload::Data(DataPacket {
                source_name: "prop",
                priority: 100,
                sync_address: 0,
                sequence_number: SequenceNumber::new(seq),
                preview: false,
                stream_terminated: terminated,
                force_sync: false,
                universe,
                start_code,
                values,
            }),
        };
        let now = self.now();
        self.rx
            .handle_packet(now, addr(), NetintId::UNKNOWN, &packet)
            .for_each_owned(|event| self.events.push_back(event));
    }

    fn step(&mut self, op: &Op) {
        match *op {
            Op::Listen { universe } => {
                let now = self.now();
                let outcome = self
                    .rx
                    .listen(now, Universe::new(universe).unwrap())
                    .expect("a heap-backed receiver never exhausts its universe capacity");
                if outcome.sampling_started {
                    self.events.push_back(BasicReceiverEvent::SamplingStarted {
                        universe: Universe::new(universe).unwrap(),
                    });
                }
                self.listening.insert(universe);
                self.drain();
            }
            Op::Stop { universe } => {
                let _ = self.rx.stop_listening(Universe::new(universe).unwrap());
                self.listening.remove(&universe);
                // The universe's state is gone, including any sampling period and
                // all its sources (removed without a SourcesLost notification).
                self.in_sampling.insert(universe, false);
                self.reported.retain(|&(u, _)| u != universe);
                self.terminated.retain(|&(u, _)| u != universe);
                self.drain();
            }
            Op::Packet {
                universe,
                cid,
                start_code,
                terminated,
            } => {
                // A non-allow-listed START code is ignored entirely, so it can
                // neither terminate nor refresh a source.
                let processed = self.processes(start_code);
                self.feed(universe, cid, start_code.byte(), terminated);
                if terminated && processed {
                    self.terminated.insert((universe, cid));
                }
                self.drain();
                // Any processed, non-terminated packet leaves a source tracked
                // with a future loss timer (it creates or refreshes one), so the
                // local liveness check applies.
                if processed && !terminated {
                    self.check_liveness(universe, cid);
                }
            }
            Op::Advance { ms } => {
                self.now_ms += ms;
                let now = self.now();
                let mut outcome = self.rx.poll(now);
                let deadline = outcome.deadline;
                while let Some(event) = outcome.next_event() {
                    self.events.push_back(event.into());
                }
                self.trace.push(Trace::Deadline(self.relative(deadline)));
                // Drain first, so `reported` reflects any source lost in this
                // very poll; then check global liveness against the post-poll
                // set. A reported source's every removal is observable, so if
                // any remain the receiver must still have a future deadline.
                self.drain();
                if !self.reported.is_empty() {
                    assert!(
                        matches!(deadline, Some(d) if d > now),
                        "reported source(s) still tracked but poll returned {deadline:?} at {now:?}",
                    );
                }
            }
        }
    }

    /// Local liveness: a processed (allow-listed), non-terminated packet on a
    /// listened universe creates or refreshes a source with a loss timer set to
    /// `now + 2.5s`, so a `poll` at the same instant must report a
    /// strictly-future deadline. (The caller only invokes this for processed
    /// START codes; an unlisted one is ignored and leaves no source.)
    ///
    /// The guards exclude the cases where we cannot be certain the packet
    /// actually created or refreshed a source, and so cannot assume a future
    /// deadline:
    ///
    /// - `limited`: at a source limit a new source may be rejected outright.
    /// - not listening: the packet is dropped (no such universe).
    /// - `terminated`: a terminated source's packets are dropped before any
    ///   timer update, so the source may be sitting timed-out and about to be
    ///   reaped - `poll` returning `None` would be correct.
    fn check_liveness(&mut self, universe: u16, cid: u8) {
        if self.limited
            || !self.listening.contains(&universe)
            || self.terminated.contains(&(universe, cid))
        {
            return;
        }
        let now = self.now();
        let mut outcome = self.rx.poll(now);
        let deadline = outcome.deadline;
        while let Some(event) = outcome.next_event() {
            self.events.push_back(event.into());
        }
        self.drain();
        assert!(
            matches!(deadline, Some(d) if d > now),
            "a source was just accepted on universe {universe} but poll returned {deadline:?} at {now:?}",
        );
    }

    fn relative(&self, deadline: Option<Instant>) -> Option<Duration> {
        deadline.map(|d| {
            d.since_epoch()
                .saturating_sub(Duration::from_millis(self.epoch_ms))
        })
    }

    /// Drains and records every pending event, checking the observational
    /// invariants as it goes.
    fn drain(&mut self) {
        while let Some(event) = self.events.pop_front() {
            match &event {
                BasicReceiverEvent::SamplingStarted { universe } => {
                    let was = self.in_sampling.insert(universe.get(), true);
                    assert_ne!(
                        was,
                        Some(true),
                        "sampling started on universe {universe} while already sampling",
                    );
                }
                BasicReceiverEvent::SamplingEnded { universe } => {
                    let was = self.in_sampling.insert(universe.get(), false);
                    assert_eq!(
                        was,
                        Some(true),
                        "sampling ended on universe {universe} without an active period",
                    );
                }
                BasicReceiverEvent::UniverseData(data) => {
                    let sampling = *self.in_sampling.get(&data.universe.get()).unwrap_or(&false);
                    assert_eq!(
                        data.is_sampling, sampling,
                        "is_sampling disagrees with the observed sampling period",
                    );
                    // The source has now been delivered (any START code), so the
                    // receiver marks it `ever_delivered` and its eventual loss is
                    // reported rather than dropped silently. That makes every
                    // removal of a reported source observable (a `SourcesLost` or
                    // a `Stop`), keeping this set exact.
                    self.reported
                        .insert((data.universe.get(), cid_id(data.source.cid)));
                }
                BasicReceiverEvent::SourcePapLost { .. } => {
                    assert!(
                        self.pap_handling,
                        "SourcePapLost emitted with per-address-priority handling disabled",
                    );
                }
                BasicReceiverEvent::SourcesLost { universe, sources } => {
                    for source in sources {
                        let key = (universe.get(), cid_id(source.cid));
                        assert!(self.reported.remove(&key));
                        self.terminated.remove(&key);
                    }
                }
                _ => {}
            }
            self.trace.push(Trace::Event(event));
        }
    }
}

fn run(config: ReceiverConfig, ops: &[Op], epoch_ms: u64) -> Vec<Trace> {
    let mut model = Model::new(config, epoch_ms);
    for op in ops {
        model.step(op);
    }
    model.trace
}

proptest! {
    #![proptest_config(ProptestConfig::with_failure_persistence(
        FileFailurePersistence::SourceParallel("tests/proptest-regressions")
    ))]

    /// Drives the receiver through a random op sequence, checking the
    /// observational invariants after every step.
    #[test]
    fn invariants_hold(config in config_strategy(), ops in prop::collection::vec(op_strategy(), 0..40)) {
        run(config, &ops, 0);
    }

    /// The same operations against a shifted clock epoch must yield an identical
    /// trace (events, commands, and epoch-relative deadlines).
    #[test]
    fn time_translation_invariance(
        config in config_strategy(),
        ops in prop::collection::vec(op_strategy(), 0..40),
    ) {
        let base = run(config, &ops, 0);
        let shifted = run(config, &ops, 7_000_000);
        prop_assert_eq!(base, shifted);
    }
}
