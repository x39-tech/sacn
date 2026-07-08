//! An in-process UDP proxy that models timing skew on a network which can be
//! ameliorated by synchronization. See the doc comment in `main.rs` for the
//! overall picture.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use sacn::Packet;
use sacn::packet::Payload;
use tokio::net::UdpSocket;

use crate::UNIVERSES;

/// Extra delay the relay adds to a synchronization packet on top of the skew
/// spread, so the sync always trails the last (most delayed) universe of its
/// frame.
const SYNC_MARGIN_MS: u64 = 4;

/// Packets on each universe are held by that universe's share of the current
/// spread - universe 1 by none, universe N by the whole spread - and the
/// sync packet by the whole spread plus a margin, so it always trails the last
/// universe of its frame.
pub async fn run_relay(
    relay: Arc<UdpSocket>,
    off_addr: SocketAddr,
    on_addr: SocketAddr,
    cut: Arc<AtomicBool>,
    skew_ms: Arc<AtomicU32>,
) {
    let mut buf = vec![0u8; sacn::packet::MAX_PACKET_SIZE];
    loop {
        let Ok((len, _from)) = relay.recv_from(&mut buf).await else {
            continue;
        };
        let bytes = buf[..len].to_vec();
        let spread = skew_ms.load(Ordering::Relaxed) as u64;
        let (is_sync, delay_ms) = match Packet::parse(&bytes).map(|p| p.payload) {
            Ok(Payload::Sync(_)) => (true, spread + SYNC_MARGIN_MS),
            Ok(Payload::Data(data)) => {
                // Universe u (1..=N) waits for its fraction of the spread.
                let index = u64::from(data.universe.saturating_sub(1)).min(UNIVERSES as u64 - 1);
                (false, spread * index / (UNIVERSES as u64 - 1))
            }
            _ => (false, 0),
        };

        let relay = relay.clone();
        let cut = cut.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            // Cutting the stream simply stops the sync packets from arriving.
            if is_sync && cut.load(Ordering::Relaxed) {
                return;
            }
            let _ = relay.send_to(&bytes, off_addr).await;
            let _ = relay.send_to(&bytes, on_addr).await;
        });
    }
}
