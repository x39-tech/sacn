//! A sACN source that transmits an animated "chase" pattern, optionally
//! synchronized across several universes.
//!
//! Sends a single moving high slot that sweeps across one or more universes and
//! terminates the streams cleanly on Ctrl-C. Run it with
//! `cargo run --example source`, and watch it with the `basic_receiver` or
//! `merge_receiver` example.
//!
//! Flags (all optional):
//!
//! - `--universes 1,2,3` - the universes to transmit on (default `1`). The chase
//!   sweeps across them in order, as if they were bands of one tall fixture.
//! - `--sync <universe>` - synchronize the universes on this sync universe, so a
//!   receiver latches the whole sweep atomically instead of per-universe.
//! - `--on-loss hold|revert` - receiver behavior if the sync stream dies: `hold`
//!   freezes on the last synced frame ([`OnSyncLoss::HoldLastLook`]), `revert`
//!   falls back to live output ([`OnSyncLoss::RevertToLive`]). Default `hold`.
//!
//! For example, `cargo run --example source -- --universes 1,2,3 --sync 100`
//! transmits a synchronized three-universe sweep, releasing all three with one
//! sync packet on universe 100.

use std::time::Duration;

use sacn::tokio::Source;
use sacn::{Cid, OnSyncLoss, SourceConfig, Universe, UniverseConfig};
use tracing::{Level, info, warn};
use uuid::Uuid;

/// The number of DMX slots in a full universe.
const SLOTS: usize = 512;

/// How often the chase pattern advances.
const FRAME: Duration = Duration::from_millis(30);

/// Command-line configuration parsed from the process arguments.
struct Args {
    universes: Vec<Universe>,
    sync: Option<Universe>,
    on_loss: OnSyncLoss,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut universes = Vec::new();
    let mut sync = None;
    let mut on_loss = OnSyncLoss::HoldLastLook;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--universes" => {
                let list = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--universes needs a value"))?;
                for part in list.split(',') {
                    universes.push(Universe::new(part.trim().parse()?)?);
                }
            }
            "--sync" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--sync needs a value"))?;
                sync = Some(Universe::new(value.trim().parse()?)?);
            }
            "--on-loss" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--on-loss needs a value"))?;
                on_loss = match value.as_str() {
                    "hold" => OnSyncLoss::HoldLastLook,
                    "revert" => OnSyncLoss::RevertToLive,
                    other => anyhow::bail!("--on-loss must be `hold` or `revert`, got `{other}`"),
                };
            }
            other => anyhow::bail!(
                "unrecognized argument `{other}`; use --universes, --sync, or --on-loss"
            ),
        }
    }

    if universes.is_empty() {
        universes.push(Universe::new(1)?);
    }
    Ok(Args {
        universes,
        sync,
        on_loss,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_target(false)
        .init();

    let args = parse_args()?;
    let cid: Cid = Uuid::new_v4().into();
    let mut source = Source::bind(SourceConfig::new(cid, "x39 sACN")).await?;

    for &universe in &args.universes {
        let mut config = UniverseConfig::new(universe);
        if let Some(sync) = args.sync {
            config = config.synchronized_on(sync, args.on_loss);
        }
        match source.add_universe(config) {
            Ok(_) => info!("transmitting on universe {universe}"),
            Err(error) => {
                warn!("could not add universe {universe}: {error}");
                return Ok(());
            }
        }
    }
    if let Some(sync) = args.sync {
        info!(
            "synchronized on universe {sync} ({})",
            match args.on_loss {
                OnSyncLoss::HoldLastLook => "hold last look on sync loss",
                OnSyncLoss::RevertToLive => "revert to live on sync loss",
            }
        );
    }

    let mut position = 0usize;
    let mut bands: Vec<[u8; SLOTS]> = args.universes.iter().map(|_| [0u8; SLOTS]).collect();
    for (i, &universe) in args.universes.iter().enumerate() {
        source.update_levels(universe, &bands[i]);
    }

    let mut frame = tokio::time::interval(FRAME);
    frame.tick().await;

    info!("press Ctrl-C to stop");
    loop {
        let deadline = source.process().await?;
        let wait = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            () = wait => {}
            _ = frame.tick() => {
                advance(&mut bands, &mut position);
                for (i, &universe) in args.universes.iter().enumerate() {
                    source.update_levels(universe, &bands[i]);
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("terminating streams");
                break;
            }
        }
    }

    // Graceful shutdown: send the stream-terminated sequence on every universe,
    // then drain it.
    for &universe in &args.universes {
        source.remove_universe(universe);
    }
    while args.universes.iter().any(|&u| source.has_universe(u)) {
        if let Some(at) = source.process().await? {
            tokio::time::sleep_until(at).await;
        }
    }
    info!("streams terminated");
    Ok(())
}

/// Advances the chase across each universe.
fn advance(bands: &mut [[u8; SLOTS]], position: &mut usize) {
    *position = (*position + 1) % SLOTS;

    for band in bands.iter_mut() {
        for level in band.iter_mut() {
            *level = level.saturating_sub(40);
        }
        let slot = *position % SLOTS;
        band[slot] = 255;
    }
}
