/// Hardware watchdog – auto-reconnect loop.
///
/// Polls the transport every `LIVENESS_POLL_SECS` seconds while connected.
/// When disconnected, scans all system ports every `RECONNECT_INTERVAL_SECS`
/// seconds and probes each one for the JDS6600 identification response.
/// On success it swaps in a fresh `SerialTransport`, invalidates the device
/// state cache, and sends `HardwareRecovered` to the sequencer.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::hal::{self, Transport};
use crate::protocol::device_state::DeviceState;
use crate::sequencer::SequencerEvent;

const LIVENESS_POLL_SECS: u64   = 2;
const RECONNECT_INTERVAL_SECS: u64 = 5;

pub async fn run_watchdog(
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    seq_tx:       mpsc::Sender<SequencerEvent>,
) {
    tracing::info!("[WATCHDOG] Started");
    loop {
        let connected = transport.lock().unwrap().is_connected();

        if connected {
            tokio::time::sleep(Duration::from_secs(LIVENESS_POLL_SECS)).await;

            if !transport.lock().unwrap().is_connected() {
                tracing::warn!("[WATCHDOG] Connection lost");
                let _ = seq_tx.send(SequencerEvent::HardwareLost).await;
            }
        } else {
            // Scan for hardware.
            let ports = hal::list_available_ports();
            tracing::debug!("[WATCHDOG] Scanning {} port(s): {:?}", ports.len(), ports);

            let mut found = false;
            for port_name in &ports {
                let pn = port_name.clone();
                let is_jds = tokio::task::spawn_blocking(move || hal::probe_jds6600(&pn))
                    .await.unwrap_or(false);

                if is_jds {
                    let pn2 = port_name.clone();
                    match tokio::task::spawn_blocking(move || {
                        hal::serial::SerialTransport::open(&pn2, 500)
                    }).await {
                        Ok(Ok(new_transport)) => {
                            tracing::info!("[WATCHDOG] Reconnected on {}", port_name);
                            *transport.lock().unwrap() = Box::new(new_transport);
                            device_state.lock().unwrap().invalidate();
                            let _ = seq_tx.send(SequencerEvent::HardwareRecovered).await;
                            found = true;
                            break;
                        }
                        Ok(Err(e)) => tracing::warn!("[WATCHDOG] Open {} failed: {}", port_name, e),
                        Err(e)     => tracing::error!("[WATCHDOG] spawn_blocking panic: {}", e),
                    }
                }
            }

            if !found {
                tracing::debug!("[WATCHDOG] No JDS6600 found, retry in {}s", RECONNECT_INTERVAL_SECS);
                tokio::time::sleep(Duration::from_secs(RECONNECT_INTERVAL_SECS)).await;
            }
        }
    }
}
