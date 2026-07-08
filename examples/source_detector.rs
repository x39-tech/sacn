//! A sACN source detector that logs the sources it discovers on the network.
//!
//! It listens on the reserved universe discovery universe and prints each source
//! as it appears, changes the universes it transmits, or goes away. Run it with
//! `cargo run --example source_detector` and start one or more `source` examples
//! (or any other sACN source, such as sACNView) to watch them appear.

use sacn::tokio::SourceDetector;
use sacn::{SourceDetectorConfig, SourceDetectorEvent};
use tracing::{Level, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_target(false)
        .init();

    let mut detector = SourceDetector::bind(SourceDetectorConfig::new()).await?;
    info!("listening for sources; press Ctrl-C to stop");

    loop {
        tokio::select! {
            event = detector.next_event() => {
                let Some(event) = event else { break };
                match event {
                    SourceDetectorEvent::SourceUpdated { cid, name, universes } => {
                        info!("source {cid:?} \"{name}\" transmitting {universes:?}");
                    }
                    SourceDetectorEvent::SourceExpired { cid, name } => {
                        info!("source {cid:?} \"{name}\" expired");
                    }
                    SourceDetectorEvent::SourceLimitExceeded => {
                        info!("source limit exceeded; some sources are not tracked");
                    }
                    SourceDetectorEvent::UniverseLimitExceeded { cid } => {
                        info!("universe limit exceeded for source {cid:?}; its list was truncated");
                    }
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("stopping");
                break;
            }
        }
    }

    Ok(())
}
