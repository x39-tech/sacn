//! A side-by-side visual demonstration of E1.31 universe synchronization, run
//! entirely over real sockets with the tokio adapters.
//!
//! A single synchronized [`Source`] bounces a bar back and forth across a
//! stack of universes, each universe is represented in a different color, in
//! a crude simulation of a video wall. Two identical [`Receiver`]s consume the
//! same stream; the only difference is that the left one has synchronization
//! disabled ([`ReceiverConfig::with_synchronization`]`(false)`) and the right
//! one has it enabled. Occasional glitches/tears can be seen on the left side,
//! exaggerated when the skew is increased using the `+`/`-` keys.
//!
//! ```text
//!   Sync OFF            Sync ON
//!      ███                ███
//!      ███                ███
//!   ███      <- the       ███   <- the bar is one
//!   ███      bar splits   ███      unbroken vertical
//!   ███      at a seam    ███      stripe, always
//! ```
//!
//! The `skew` parameter controls how much delay is applied to each universe in
//! turn, to mimic ordered serialization and propagation delays from a real
//! network, albeit in a somewhat exaggerated fashion.
//!
//! A tiny in-process UDP proxy imposes the skew. It holds each universe's
//! datagram by its share of the current spread (universe 1 by none, universe N
//! by the whole spread) and the sync packet until just after the last of them.
//! Everything else is the real, public tokio API end to end: a real [`Source`],
//! two real [`Receiver`]s on their own ports, and real UDP sockets between.
//!
//! Run it with `cargo run --example synchronization` (optionally
//! `--example synchronization -- --on-loss revert`).

mod relay;
mod ui;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sacn::tokio::{Receiver, Source};
use sacn::{Cid, OnSyncLoss, ReceiverConfig, SourceConfig, Universe, UniverseConfig};
use tokio::net::UdpSocket;

/// How many universes are stacked into the canvas.
const UNIVERSES: u16 = 6;
/// The synchronization universe used by the stack.
const SYNC_UNIVERSE: u16 = 100;
/// How many columns (of 256) the bar's position moves each animation step.
const STEP: u8 = 24;
/// The animation interval, i.e. the gap between the bar's position changes.
const ANIM_INTERVAL: Duration = Duration::from_millis(180);

/// One receiver's view of the wall: the latest bar position (0 -> 255) it has
/// received for each universe, or `None` until that universe's first frame
/// arrives.
type Panel = Arc<Mutex<[Option<u8>; UNIVERSES as usize]>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let on_loss = parse_on_loss()?;

    let universes: Vec<Universe> = (1..=UNIVERSES).map(|u| Universe::new(u).unwrap()).collect();

    // Two receivers, distinguished only by the synchronization flag, each on its
    // own ephemeral port so the relay can address them independently.
    let base_config = ReceiverConfig::new()
        .with_sample_period(Duration::from_millis(80))
        // The source sends only levels; skip the per-address-priority wait so a
        // new source's first frame is not withheld.
        .with_per_address_priority_handling(false);
    let mut rx_no_sync = Receiver::bind_to(
        "0.0.0.0:0".parse()?,
        base_config.with_synchronization(false),
    )
    .await?;
    let mut rx_sync =
        Receiver::bind_to("0.0.0.0:0".parse()?, base_config.with_synchronization(true)).await?;
    for &universe in &universes {
        rx_no_sync.listen(universe).await?;
        rx_sync.listen(universe).await?;
    }
    let no_sync_addr = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        rx_no_sync.local_addr()?.port(),
    );
    let sync_addr = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        rx_sync.local_addr()?.port(),
    );

    // The jitter relay: an ephemeral UDP socket the source unicasts to, and that
    // forwards to both receivers after a random hold. Sync packets are held a
    // touch longer (so they trail their frame) and dropped while the stream is
    // cut.
    let relay = Arc::new(UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?);
    let relay_port = relay.local_addr()?.port();

    // The synchronized source, unicasting to the relay.
    let mut source =
        Source::bind(SourceConfig::new(Cid::from_bytes([0x51; 16]), "sync demo")).await?;
    for &universe in &universes {
        source.add_universe_on(
            UniverseConfig::new(universe).synchronized_on(Universe::new(SYNC_UNIVERSE)?, on_loss),
            &[][..],
        )?;
        source.add_unicast_to_port(universe, IpAddr::V4(Ipv4Addr::LOCALHOST), relay_port);
    }

    // Shared control + display state.
    let cut = Arc::new(AtomicBool::new(false));
    let skew_ms = Arc::new(AtomicU32::new(14));
    let no_sync_panel: Panel = Arc::new(Mutex::new([None; UNIVERSES as usize]));
    let sync_panel: Panel = Arc::new(Mutex::new([None; UNIVERSES as usize]));

    tokio::spawn(relay::run_relay(
        relay.clone(),
        no_sync_addr,
        sync_addr,
        cut.clone(),
        skew_ms.clone(),
    ));

    // The source: animate the sweep and drive transmission.
    tokio::spawn(run_source(source, universes.clone()));

    // Two receivers, each folding merged data into its panel.
    tokio::spawn(run_receiver(rx_no_sync, no_sync_panel.clone()));
    tokio::spawn(run_receiver(rx_sync, sync_panel.clone()));

    ui::run_ui(no_sync_panel, sync_panel, cut, skew_ms, on_loss)
}

/// Reads `--on-loss hold|revert` from the arguments (default `hold`).
fn parse_on_loss() -> anyhow::Result<OnSyncLoss> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--on-loss" {
            let value = args.next().unwrap_or_default();
            return match value.as_str() {
                "hold" => Ok(OnSyncLoss::HoldLastLook),
                "revert" => Ok(OnSyncLoss::RevertToLive),
                other => anyhow::bail!("--on-loss must be `hold` or `revert`, got `{other}`"),
            };
        }
    }
    Ok(OnSyncLoss::HoldLastLook)
}

/// Drives the source: bounce the bar one [`STEP`] every [`ANIM_INTERVAL`] and
/// transmit.
async fn run_source(mut source: Source, universes: Vec<Universe>) -> anyhow::Result<()> {
    // The bar's position, `0..=255`, and its current direction. Bounces back
    // and forth across the range.
    let mut pos: u8 = 0;
    let mut rising = true;
    let mut anim = tokio::time::interval(ANIM_INTERVAL);
    // Skipping missed ticks rather than firing them together in a short burst
    // results in more consistent behavior.
    anim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // The bar position is encoded as a single value per universe (slot 0).
    let push = |source: &mut Source, pos: u8| {
        for &universe in &universes {
            source.update_levels(universe, &[pos]);
        }
    };
    push(&mut source, pos);

    loop {
        tokio::select! {
            res = source.run() => {
                // Run is infallible so this must be an error
                let Err(e) = res;
                return Err(e.into());
            }
            _ = anim.tick() => {
                let (next, dir) = advance(pos, rising);
                pos = next;
                rising = dir;
                push(&mut source, pos);
            }
        }
    }
}

/// Advances the bouncing bar one [`STEP`] from `pos`, reflecting off the `0` and
/// `255` ends. Returns the new position and direction.
fn advance(pos: u8, rising: bool) -> (u8, bool) {
    if rising {
        match pos.checked_add(STEP) {
            Some(next) => (next, true),
            None => (255 - STEP, false),
        }
    } else {
        match pos.checked_sub(STEP) {
            Some(next) => (next, false),
            None => (STEP, true),
        }
    }
}

/// Applies one merged frame to its band in the panel.
fn apply_frame(panel: &Panel, data: &sacn::MergedData) {
    let index = (data.universe.get() - 1) as usize;
    if let (Some(slot), Some(cell)) = (data.levels().first(), panel.lock().unwrap().get_mut(index))
    {
        *cell = Some(*slot);
    }
}

/// Runs the receiver - gets all data updates and updates the panel positions.
async fn run_receiver(mut receiver: Receiver, panel: Panel) {
    while let Some(event) = receiver.next_event().await {
        match event {
            sacn::ReceiverEvent::MergedData(data) => apply_frame(&panel, &data),
            sacn::ReceiverEvent::SyncMergedData(frames) => {
                for data in &frames {
                    apply_frame(&panel, data);
                }
            }
            _ => {}
        }
    }
}
