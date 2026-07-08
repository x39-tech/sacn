//! Stateful property tests for the source send path.
//!
//! The source is a small state machine with several intertwined behaviors:
//! per-universe sequence numbering, transmission suppression, per-address
//! priority, termination, universe discovery, and - crucially - a drain that can
//! be abandoned partway through (a cancelled send) and resumed on the next poll.
//! Those behaviors interact, so beyond the hand-written scenarios in `tests.rs`
//! this suite drives the source with a random configuration and a random sequence
//! of operations, cancelling polls at random points, and after every step asserts
//! a handful of invariants that any correct run must satisfy.
//!
//! The harness is a model: it knows what it fed the source and observes the
//! packets it emits, so it can derive everything it checks without peeking at
//! the source's private state. The invariants:
//!
//! - **Sequence discipline.** Per universe, the sequence numbers of emitted data
//!   packets advance by exactly 0 or 1 (mod 256) in emission order. A step of 0
//!   is a re-send of a packet that was committed but not confirmed before a cancel;
//!   a step greater than 1 would mean a committed packet was dropped. The counter
//!   is only ever reset by a fresh `add_universe`, so the harness resets its
//!   expectation there.
//! - **Termination.** A stream-terminated packet is emitted only while a removal
//!   has the universe mid-termination, always carries the universe's last levels
//!   under the NULL start code, and a termination sequence emits at most
//!   [`TERMINATION_PACKETS`] distinct terminated sequence numbers.
//! - **Data legality.** A non-terminated level or per-address-priority packet is
//!   emitted only for a universe that is present, not terminating, and actually
//!   has that data; its values and header fields (priority, preview, sync
//!   address, source name, CID) match what the harness last set.
//! - **Discovery.** Every announced universe is currently present with levels;
//!   pages list universes in strictly ascending order with consistent page
//!   numbering.
//! - **Liveness.** On an uncancelled poll, if any universe is a live keep-alive
//!   universe (present, has levels, not terminating) the poll returns a
//!   strictly-future deadline - the source never goes to sleep while it still owes
//!   a stream.
//! - **Removal.** A universe reported physically removed was mid-termination.
//! - **Time-translation invariance.** Running the same operations against a
//!   shifted clock epoch yields identical packet bytes and identical
//!   epoch-relative deadlines - the source depends only on differences between
//!   instants, never their absolute value.

use super::*;
use crate::packet::{Packet, Payload};
use crate::static_storage;
use crate::time::{Duration, Instant};
use crate::types::{Cid, Priority, Universe};

use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

// These tests should compile under the `no_std` + `alloc` configuration.
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// Small alphabets so collisions (same universe) and full termination sequences
// actually happen within a short op sequence.
const N_UNIVERSES: u16 = 3;
const NAMES: [&str; 2] = ["src-a", "src-b"];

// --- test storage policy -----------------------------------------------------

static_storage! {
    struct PropCaps {
        rx_universes: 0,
        rx_sources_per_universe: 0,
        rx_sync_addresses: 0,
        tx_universes: 3,
        det_sources: 0,
        det_universes_per_source: 0,
    }
}

type Source = super::Source<PropCaps>;

impl Source {
    fn new(config: SourceConfig) -> Self {
        Self::with_config(config)
    }
}

fn univ(n: u16) -> Universe {
    Universe::new(n).unwrap()
}

/// A single operation applied to the source. Time advances only via
/// [`Op::Poll`], so the monotonic-clock contract holds by construction.
#[derive(Clone, Debug)]
enum Op {
    Add {
        universe: u16,
        priority: u8,
        preview: bool,
        sync: u16,
    },
    Remove {
        universe: u16,
    },
    UpdateLevels {
        universe: u16,
        levels: Vec<u8>,
    },
    UpdatePap {
        universe: u16,
        levels: Vec<u8>,
        pap: Vec<u8>,
    },
    RemovePap {
        universe: u16,
    },
    SetPriority {
        universe: u16,
        priority: u8,
    },
    SetPreview {
        universe: u16,
        preview: bool,
    },
    Resend {
        universe: u16,
    },
    SetName {
        name: &'static str,
    },
    /// Advance the clock by `advance_ms` and poll. `cut = Some(k)` abandons the
    /// drain after `k` transmissions (modelling a cancelled send) and then polls
    /// again at the same instant to resume it; `None` drains fully.
    Poll {
        advance_ms: u64,
        cut: Option<usize>,
    },
}

fn levels_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..=5)
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let universe = 1..=N_UNIVERSES;
    let priority = 0u8..=Priority::MAX;
    let sync = prop_oneof![Just(0u16), 1..=N_UNIVERSES];
    prop_oneof![
        // Poll dominates so the clock advances and streams make progress.
        6 => (
            prop_oneof![4 => 0u64..=1200, 1 => 900u64..=12_000],
            prop_oneof![3 => Just(None), 1 => (0usize..=6).prop_map(Some)],
        )
            .prop_map(|(advance_ms, cut)| Op::Poll { advance_ms, cut }),
        2 => (universe.clone(), priority.clone(), any::<bool>(), sync).prop_map(
            |(universe, priority, preview, sync)| Op::Add {
                universe,
                priority,
                preview,
                sync,
            }
        ),
        2 => (universe.clone(), levels_strategy())
            .prop_map(|(universe, levels)| Op::UpdateLevels { universe, levels }),
        1 => (universe.clone(), levels_strategy(), levels_strategy())
            .prop_map(|(universe, levels, pap)| Op::UpdatePap {
                universe,
                levels,
                pap,
            }),
        1 => universe.clone().prop_map(|universe| Op::Remove { universe }),
        1 => universe.clone().prop_map(|universe| Op::RemovePap { universe }),
        1 => (universe.clone(), priority)
            .prop_map(|(universe, priority)| Op::SetPriority { universe, priority }),
        1 => (universe.clone(), any::<bool>())
            .prop_map(|(universe, preview)| Op::SetPreview { universe, preview }),
        1 => universe.prop_map(|universe| Op::Resend { universe }),
        1 => (0usize..NAMES.len()).prop_map(|i| Op::SetName { name: NAMES[i] }),
    ]
}

fn source_config() -> impl Strategy<Value = SourceConfig> {
    (700u64..=1100, 700u64..=1100, 0usize..NAMES.len()).prop_map(|(keep_alive, pap, name)| {
        SourceConfig::new(Cid::from_bytes([1; 16]), NAMES[name])
            .with_keep_alive(Duration::from_millis(keep_alive))
            .with_pap_keep_alive(Duration::from_millis(pap))
    })
}

/// One drained transmission captured in owned form (the source reuses one buffer
/// across a drain, so the bytes must be copied out before pulling the next).
struct Tx {
    route: Route,
    bytes: Vec<u8>,
}

/// What the harness believes about one universe. Mirrors exactly the source state
/// the observable packets depend on, so emissions can be checked against it.
#[derive(Debug)]
struct Uni {
    /// Present from a successful `add_universe` until the source reports the
    /// universe physically removed. Deliberately spans the brief
    /// finished-awaiting-removal window, during which the source still holds the
    /// universe and can even revive it, so this stays in step with the source.
    present: bool,
    levels: Option<Vec<u8>>,
    pap: Option<Vec<u8>>,
    priority: u8,
    preview: bool,
    sync: u16,
    /// A removal started a termination sequence that has not been cancelled (by
    /// new data) or completed (physical removal). A terminated packet may only be
    /// emitted while this holds.
    terminating: bool,
    /// Distinct sequence numbers seen carrying the terminated flag in the current
    /// termination sequence; must never exceed [`TERMINATION_PACKETS`].
    term_seqs: BTreeSet<u8>,
    /// Last sequence number emitted for this universe, for the gap check. Reset
    /// on a fresh add (the only thing that resets the source's counter).
    last_seq: Option<u8>,
}

/// One poll's observable outcome, recorded so two epoch-shifted runs can be
/// compared. Deadlines are epoch-relative for that comparison.
#[derive(Clone, Debug, PartialEq)]
struct PollTrace {
    packets: Vec<Vec<u8>>,
    deadline: Option<Duration>,
}

struct Model {
    source: Source,
    cid: Cid,
    name: String,
    epoch_ms: u64,
    now_ms: u64,
    unis: BTreeMap<u16, Uni>,
    trace: Vec<PollTrace>,
}

impl Model {
    fn new(config: SourceConfig, epoch_ms: u64) -> Self {
        let cid = config.cid();
        let name = config.name().to_string();
        Self {
            source: Source::new(config),
            cid,
            name,
            epoch_ms,
            now_ms: 0,
            unis: BTreeMap::new(),
            trace: Vec::new(),
        }
    }

    fn now(&self) -> Instant {
        Instant::from_epoch(Duration::from_millis(self.epoch_ms + self.now_ms))
    }

    fn apply(&mut self, op: &Op) {
        match op.clone() {
            Op::Add {
                universe,
                priority,
                preview,
                sync,
            } => self.add(universe, priority, preview, sync),
            Op::Remove { universe } => self.remove(universe),
            Op::UpdateLevels { universe, levels } => self.update_levels(universe, &levels),
            Op::UpdatePap {
                universe,
                levels,
                pap,
            } => self.update_pap(universe, &levels, &pap),
            Op::RemovePap { universe } => {
                self.source.remove_pap(univ(universe));
                if let Some(state) = self.present_mut(universe) {
                    state.pap = None;
                }
            }
            Op::SetPriority { universe, priority } => {
                self.source
                    .set_priority(univ(universe), Priority::new(priority).unwrap());
                if let Some(state) = self.present_mut(universe) {
                    state.priority = priority;
                }
            }
            Op::SetPreview { universe, preview } => {
                self.source.set_preview(univ(universe), preview);
                if let Some(state) = self.present_mut(universe) {
                    state.preview = preview;
                }
            }
            Op::Resend { universe } => self.source.resend(univ(universe)),
            Op::SetName { name } => {
                self.source.set_name(name);
                self.name = name.to_string();
            }
            Op::Poll { advance_ms, cut } => self.poll(advance_ms, cut),
        }
    }

    /// The tracked state of `universe` if the harness considers it present, for
    /// mutating ops that the source likewise ignores on an absent universe.
    fn present_mut(&mut self, universe: u16) -> Option<&mut Uni> {
        self.unis.get_mut(&universe).filter(|s| s.present)
    }

    fn add(&mut self, universe: u16, priority: u8, preview: bool, sync: u16) {
        let mut config = UniverseConfig::new(univ(universe))
            .with_priority(Priority::new(priority).unwrap())
            .with_preview(preview);
        if sync != 0 {
            config = config.synchronized_on(univ(sync), OnSyncLoss::HoldLastLook);
        }
        // Trust the return value: a fresh insert (also replacing a
        // finished-awaiting-removal universe) resets the sequence counter, while
        // a rejected add leaves an already-active universe untouched.
        if let Ok(true) = self.source.add_universe(config) {
            self.unis.insert(
                universe,
                Uni {
                    present: true,
                    levels: None,
                    pap: None,
                    priority,
                    preview,
                    sync,
                    terminating: false,
                    term_seqs: BTreeSet::new(),
                    last_seq: None,
                },
            );
        }
    }

    fn remove(&mut self, universe: u16) {
        if !self.source.remove_universe(univ(universe)) {
            return;
        }
        let state = self
            .unis
            .get_mut(&universe)
            .expect("removed a tracked universe");
        if state.levels.is_some() {
            // A universe with data enters the three-packet termination sequence.
            state.terminating = true;
            state.term_seqs.clear();
        } else {
            // Nothing to terminate: the source drops it immediately.
            state.present = false;
        }
    }

    fn update_levels(&mut self, universe: u16, levels: &[u8]) {
        self.source.update_levels(univ(universe), levels);
        if let Some(state) = self.present_mut(universe) {
            state.levels = Some(slots(levels));
            // New data cancels any in-progress termination.
            state.terminating = false;
        }
    }

    fn update_pap(&mut self, universe: u16, levels: &[u8], pap: &[u8]) {
        self.source
            .update_levels_and_pap(univ(universe), levels, pap);
        if let Some(state) = self.present_mut(universe) {
            state.levels = Some(slots(levels));
            state.pap = Some(slots(pap));
            state.terminating = false;
        }
    }

    fn poll(&mut self, advance_ms: u64, cut: Option<usize>) {
        self.now_ms += advance_ms;
        let now = self.now();

        // The scheduling poll, drained up to `cut` (fully if `None`).
        let (deadline, mut packets) = self.drive_poll(now, cut.unwrap_or(usize::MAX));
        // If it was cancelled, poll again at the same instant to drain the
        // leftovers.
        let deadline = if cut.is_some() {
            let (resume_deadline, resumed) = self.drive_poll(now, usize::MAX);
            packets.extend(resumed);
            resume_deadline
        } else {
            deadline
        };

        // Liveness is only meaningful on an uncancelled poll: a resumed poll
        // returns `Some(now)` just to flush leftovers, not a computed deadline.
        if cut.is_none() {
            let has_live = self
                .unis
                .values()
                .any(|s| s.present && s.levels.is_some() && !s.terminating);
            if has_live {
                assert!(
                    matches!(deadline, Some(d) if d > now),
                    "a live universe exists but poll returned deadline {deadline:?} at {now:?}",
                );
            }
        }

        // Everything the source still lists as a universe must be one we consider
        // present (our present set is a superset, including the finished window).
        let listed: Vec<u16> = self.source.universes().map(|u| u.get()).collect();
        for u in listed {
            assert!(
                self.unis.get(&u).is_some_and(|s| s.present),
                "source lists universe {u} the harness thinks absent",
            );
        }

        let relative = deadline.map(|d| {
            d.since_epoch()
                .saturating_sub(Duration::from_millis(self.epoch_ms))
        });
        self.trace.push(PollTrace {
            packets,
            deadline: relative,
        });
    }

    /// Performs one `source.poll(now)`, draining up to `limit` transmissions.
    /// Applies the poll's physical removals (which happen at its start, before
    /// any emission) and then validates the drained packets against the current
    /// tracked state, so a later poll's removal never retroactively invalidates
    /// an earlier poll's legitimate emission. Returns the poll deadline and the
    /// drained packet bytes.
    fn drive_poll(&mut self, now: Instant, limit: usize) -> (Option<Instant>, Vec<Vec<u8>>) {
        let mut poll = self.source.poll(now);
        let deadline = poll.deadline;
        let removed: Vec<u16> = poll.removed().iter().map(|u| u.get()).collect();
        let mut txs: Vec<Tx> = Vec::new();
        while txs.len() < limit {
            let Some(t) = poll.next_transmission() else {
                break;
            };
            txs.push(Tx {
                route: t.route,
                bytes: t.data.to_vec(),
            });
        }

        for u in removed {
            let state = self
                .unis
                .get_mut(&u)
                .unwrap_or_else(|| panic!("universe {u} reported removed but is untracked"));
            // A resumed poll re-reports the scheduling poll's removals, so ignore
            // any we have already accounted for.
            if state.present {
                assert!(
                    state.terminating,
                    "universe {u} reported removed but was not mid-termination",
                );
                state.present = false;
                state.terminating = false;
            }
        }

        self.validate(&txs);
        (deadline, txs.into_iter().map(|t| t.bytes).collect())
    }

    fn validate(&mut self, txs: &[Tx]) {
        for tx in txs {
            let packet = Packet::parse(&tx.bytes).expect("emitted packet parses");
            assert_eq!(packet.cid, self.cid, "packet CID matches the source");
            match packet.payload {
                Payload::Data(d) => {
                    let u = d.universe;
                    assert_eq!(
                        tx.route,
                        Route::Universe(univ(u)),
                        "data packet route matches its universe",
                    );
                    assert_eq!(d.source_name, self.name.as_str(), "source name matches");
                    let state = self
                        .unis
                        .get_mut(&u)
                        .unwrap_or_else(|| panic!("data emitted for untracked universe {u}"));

                    assert_eq!(d.priority, state.priority, "priority matches for u{u}");
                    assert_eq!(d.preview, state.preview, "preview flag matches for u{u}");
                    let expected_sync = if d.stream_terminated { 0 } else { state.sync };
                    assert_eq!(
                        d.sync_address, expected_sync,
                        "sync address matches for u{u}"
                    );
                    // The model only ever configures HoldLastLook, so force_sync is
                    // always the 0 bit.
                    assert!(!d.force_sync, "force_sync is never set for u{u}");

                    // Sequence discipline: the core hands each packet out exactly
                    // once, in order, so every packet for a universe advances the
                    // sequence number by exactly one - no skip (a burned number or
                    // dropped packet) and no repeat (a duplicate).
                    let seq = d.sequence_number.get();
                    if let Some(last) = state.last_seq {
                        assert_eq!(
                            seq.wrapping_sub(last),
                            1,
                            "u{u} sequence went from {last} to {seq} (expected +1 exactly)",
                        );
                    }
                    state.last_seq = Some(seq);

                    if d.stream_terminated {
                        assert_eq!(
                            d.start_code, DMX_NULL_START_CODE,
                            "termination packets carry the NULL start code",
                        );
                        assert!(
                            state.terminating,
                            "terminated packet for u{u} that is not mid-termination",
                        );
                        let levels = state
                            .levels
                            .as_deref()
                            .expect("a terminating universe has levels");
                        assert_eq!(
                            d.values, levels,
                            "termination carries the last levels for u{u}"
                        );
                        state.term_seqs.insert(seq);
                        assert!(
                            state.term_seqs.len() <= TERMINATION_PACKETS as usize,
                            "u{u} emitted more than {TERMINATION_PACKETS} distinct termination packets",
                        );
                    } else {
                        assert!(
                            state.present && !state.terminating,
                            "non-terminated data for u{u} that is absent or terminating",
                        );
                        match d.start_code {
                            DMX_NULL_START_CODE => {
                                let levels = state
                                    .levels
                                    .as_deref()
                                    .expect("a level packet requires levels");
                                assert_eq!(d.values, levels, "levels match tracked data for u{u}");
                            }
                            PAP_START_CODE => {
                                let pap = state
                                    .pap
                                    .as_deref()
                                    .expect("a per-address-priority packet requires PAP data");
                                assert_eq!(d.values, pap, "PAP matches tracked data for u{u}");
                            }
                            other => panic!("unexpected start code {other:#x} for u{u}"),
                        }
                    }
                }
                Payload::UniverseDiscovery(disc) => {
                    assert_eq!(
                        tx.route,
                        Route::Discovery,
                        "discovery page routed to discovery"
                    );
                    assert!(disc.page <= disc.last_page, "page index within range");
                    let listed: Vec<u16> = disc.universes.iter().collect();
                    for pair in listed.windows(2) {
                        assert!(
                            pair[0] < pair[1],
                            "discovery universes must be strictly ascending",
                        );
                    }
                    for lu in listed {
                        // A listed universe is valid if it is an active data
                        // universe, or the sync universe of one.
                        let is_data = self
                            .unis
                            .get(&lu)
                            .is_some_and(|s| s.present && s.levels.is_some());
                        let is_sync = self
                            .unis
                            .values()
                            .any(|s| s.present && s.levels.is_some() && s.sync == lu);
                        assert!(
                            is_data || is_sync,
                            "discovery lists u{lu} which is neither an active data nor sync universe",
                        );
                    }
                }
                Payload::Sync(s) => {
                    // A sync packet is routed to its own sync universe and carries
                    // a nonzero sync address matching that route.
                    assert_ne!(s.sync_address, 0, "sync packet carries a nonzero address");
                    assert_eq!(
                        tx.route,
                        Route::Sync(univ(s.sync_address)),
                        "sync packet routed to its sync universe",
                    );
                }
            }
        }
    }
}

/// The slots the source would retain from `values`: at most [`MAX_SLOTS`].
fn slots(values: &[u8]) -> Vec<u8> {
    values[..values.len().min(MAX_SLOTS)].to_vec()
}

fn run(config: SourceConfig, ops: &[Op], epoch_ms: u64) -> Vec<PollTrace> {
    let mut model = Model::new(config, epoch_ms);
    for op in ops {
        model.apply(op);
    }
    model.trace
}

proptest! {
    #![proptest_config(ProptestConfig::with_failure_persistence(
        FileFailurePersistence::SourceParallel("tests/proptest-regressions")
    ))]

    /// Drives the source through a random op sequence, cancelling polls at random
    /// points, and checks the observational invariants after every step.
    #[test]
    fn invariants_hold(config in source_config(), ops in prop::collection::vec(op_strategy(), 0..48)) {
        run(config, &ops, 0);
    }

    /// The same operations against a shifted clock epoch must yield identical
    /// packet bytes and identical epoch-relative deadlines.
    #[test]
    fn time_translation_invariance(
        config in source_config(),
        ops in prop::collection::vec(op_strategy(), 0..48),
    ) {
        let base = run(config.clone(), &ops, 0);
        let shifted = run(config, &ops, 9_000_000);
        prop_assert_eq!(base, shifted);
    }
}
