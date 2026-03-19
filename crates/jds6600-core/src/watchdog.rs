/// Hardware watchdog – startup discovery + steady-state reconnect loop.
///
/// ## Startup sequence (replaces the old fake HardwareFound)
///
/// ```text
/// start
///   └─ Phase 1: INITIAL SCAN  (runs once, immediately)
///       ├─ JDS6600 found  → swap transport, send HardwareFound, enter Phase 2
///       └─ not found after MOCK_FALLBACK_SECS
///           → keep MockTransport, send HardwareFound (mock mode), enter Phase 2
///
///   └─ Phase 2: STEADY-STATE LOOP
///       ├─ connected  → poll is_connected every LIVENESS_POLL_SECS
///       │               on loss → send HardwareLost, try to reconnect
///       └─ disconnected → scan all ports, on success → swap + HardwareRecovered
/// ```
///
/// ## Why the old code was wrong
/// The old `async_main` sent `HardwareFound` after 300 ms unconditionally.
/// When the watchdog later found real hardware it sent `HardwareRecovered`,
/// which the sequencer only handles in `ERROR` state — from `IDLE` it is a
/// no-op, so the transport was swapped but the sequencer never acknowledged it.
///
/// ## Sequencer event semantics
/// | Event              | Sequencer handles in |
/// |--------------------|----------------------|
/// | HardwareFound      | DISCONNECTED → IDLE  |
/// | HardwareLost       | RUNNING/PAUSED/IDLE  |
/// | HardwareRecovered  | ERROR → IDLE         |

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::hal::{self, Transport};
use crate::hal::mock::MockTransport;
use crate::protocol::device_state::DeviceState;
use crate::sequencer::SequencerEvent;

const LIVENESS_POLL_SECS:   u64 = 2;
const RECONNECT_INTERVAL_SECS: u64 = 5;
/// If no real hardware is found within this many seconds at startup,
/// declare mock mode ready so the UI transitions out of DISCONNECTED.
const MOCK_FALLBACK_SECS:   u64 = 8;

pub async fn run_watchdog(
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    seq_tx:       mpsc::Sender<SequencerEvent>,
) {
    tracing::info!("[WATCHDOG] Started");

    // ── Phase 1: initial scan ─────────────────────────────────────────────
    let found = initial_scan(
        Arc::clone(&transport),
        Arc::clone(&device_state),
    ).await;

    if found {
        tracing::info!("[WATCHDOG] Real hardware ready → HardwareFound");
        let _ = seq_tx.send(SequencerEvent::HardwareFound).await;
    } else {
        // No real device — run mock mode but still advance the sequencer.
        tracing::info!(
            "[WATCHDOG] No JDS6600 found in {}s — entering mock mode → HardwareFound",
            MOCK_FALLBACK_SECS
        );
        let _ = seq_tx.send(SequencerEvent::HardwareFound).await;
    }

    // ── Phase 2: steady-state liveness + reconnect loop ──────────────────
    loop {
        let connected = transport.lock().unwrap().is_connected();

        if connected {
            tokio::time::sleep(Duration::from_secs(LIVENESS_POLL_SECS)).await;

            if !transport.lock().unwrap().is_connected() {
                tracing::warn!("[WATCHDOG] Transport disconnected");
                let _ = seq_tx.send(SequencerEvent::HardwareLost).await;
                // Revert to mock so the dispatcher doesn't stall.
                *transport.lock().unwrap() = Box::new(MockTransport::new());
            }
        } else {
            // Try to reconnect real hardware.
            let ports = hal::list_available_ports();
            tracing::debug!("[WATCHDOG] Reconnect scan: {:?}", ports);

            let mut recovered = false;
            for port_name in &ports {
                if try_open_jds(port_name, Arc::clone(&transport), Arc::clone(&device_state)).await {
                    let _ = seq_tx.send(SequencerEvent::HardwareRecovered).await;
                    recovered = true;
                    break;
                }
            }

            if !recovered {
                tracing::debug!(
                    "[WATCHDOG] No JDS6600, retry in {}s",
                    RECONNECT_INTERVAL_SECS
                );
                tokio::time::sleep(Duration::from_secs(RECONNECT_INTERVAL_SECS)).await;
            }
        }
    }
}

// ── Initial scan (phase 1) ────────────────────────────────────────────────

/// Scan all available ports immediately at startup.
/// Returns `true` if a JDS6600 was found and the transport was swapped.
/// Waits up to `MOCK_FALLBACK_SECS` total across all retry attempts.
async fn initial_scan(
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,

) -> bool {
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(MOCK_FALLBACK_SECS);

    loop {
        let ports = hal::list_available_ports();
        tracing::info!("[WATCHDOG] Initial scan: {} port(s) found: {:?}", ports.len(), ports);

        for port_name in &ports {
            if try_open_jds(port_name, Arc::clone(&transport), Arc::clone(&device_state)).await {
                tracing::info!("[WATCHDOG] JDS6600 found on {} during initial scan", port_name);
                return true;
            }
        }

        if tokio::time::Instant::now() >= deadline {
            tracing::info!("[WATCHDOG] Initial scan timeout — no JDS6600 found");
            return false;
        }

        // Wait before retrying (short interval during startup).
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ── Shared: probe one port and swap transport on success ──────────────────

async fn try_open_jds(
    port_name:    &str,
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
) -> bool {
    let pn = port_name.to_string();

    // probe_jds6600 is synchronous serial I/O — run in blocking thread.
    let is_jds = tokio::task::spawn_blocking(move || hal::probe_jds6600(&pn))
        .await
        .unwrap_or(false);

    if !is_jds { return false; }

    let pn2 = port_name.to_string();
    match tokio::task::spawn_blocking(move || hal::serial::SerialTransport::open(&pn2, 500)).await {
        Ok(Ok(new_transport)) => {
            tracing::info!("[WATCHDOG] Opened SerialTransport on {}", port_name);
            *transport.lock().unwrap() = Box::new(new_transport);
            // Invalidate cache — device state unknown after (re)connect.
            device_state.lock().unwrap().invalidate();
            true
        }
        Ok(Err(e)) => {
            tracing::warn!("[WATCHDOG] Could not open {}: {}", port_name, e);
            false
        }
        Err(e) => {
            tracing::error!("[WATCHDOG] spawn_blocking panic on {}: {}", port_name, e);
            false
        }
    }
}
