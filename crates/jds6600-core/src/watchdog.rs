/// Hardware watchdog – lifecycle owner for the serial transport.
///
/// ## Startup
/// 1. Scans all ports immediately; on JDS6600 found → reads current state
///    → publishes to `live_state` → sends `HardwareFound`.
/// 2. If no device after `MOCK_FALLBACK_SECS` → sends `HardwareFound` in
///    mock mode (UI still transitions out of DISCONNECTED).
///
/// ## Port coordination with the UI scanner
/// The UI scanner (triggered by the "Scan All Ports" button) needs to open
/// the same port the watchdog is monitoring.  On Windows, two opens of the
/// same COM port from one process = "Access is denied".
///
/// Solution: a `tokio::sync::watch` channel carries `WatchdogCmd`.
/// - UI sets it to `ScannerActive` before starting its scan thread.
/// - Watchdog detects this and pauses its own polling loop until the
///   command returns to `Idle`.
/// - Scanner is now the sole owner of the port for its duration.
/// - On Idle, watchdog re-probes and re-opens the port itself.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch, RwLock};

use crate::hal::{self, Transport};
use crate::hal::mock::MockTransport;
use crate::models::{LiveDeviceState, WatchdogCmd};
use crate::protocol::device_state::DeviceState;
use crate::sequencer::SequencerEvent;

const LIVENESS_POLL_SECS:    u64 = 2;
const RECONNECT_INTERVAL_SECS: u64 = 5;
const MOCK_FALLBACK_SECS:    u64 = 8;
/// How often to check the cmd channel while paused.
const PAUSE_POLL_MS:         u64 = 200;

pub async fn run_watchdog(
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    seq_tx:       mpsc::Sender<SequencerEvent>,
    // rx half of the UI→watchdog coordination channel
    mut cmd_rx:   watch::Receiver<WatchdogCmd>,
    // shared live state that the UI displays
    live_state:   Arc<RwLock<LiveDeviceState>>,
) {
    tracing::info!("[WATCHDOG] Started");

    // ── Phase 1: initial scan ─────────────────────────────────────────────
    let found = initial_scan(
        Arc::clone(&transport),
        Arc::clone(&device_state),
        Arc::clone(&live_state),
        &mut cmd_rx,
    ).await;

    if found {
        tracing::info!("[WATCHDOG] Real hardware ready → HardwareFound");
    } else {
        tracing::info!("[WATCHDOG] No JDS6600 in {}s → mock mode → HardwareFound",
            MOCK_FALLBACK_SECS);
    }
    let _ = seq_tx.send(SequencerEvent::HardwareFound).await;

    // ── Phase 2: steady-state loop ────────────────────────────────────────
    loop {
        // Yield to UI scanner if it has taken over.
        pause_if_scanner_active(&mut cmd_rx).await;

        let connected = transport.lock().unwrap().is_connected();

        if connected {
            tokio::time::sleep(Duration::from_secs(LIVENESS_POLL_SECS)).await;

            if !transport.lock().unwrap().is_connected() {
                tracing::warn!("[WATCHDOG] Transport lost");
                let _ = seq_tx.send(SequencerEvent::HardwareLost).await;
                *transport.lock().unwrap() = Box::new(MockTransport::new());
            }
        } else {
            let ports = hal::list_available_ports();
            tracing::debug!("[WATCHDOG] Reconnect scan: {:?}", ports);

            let mut recovered = false;
            for port_name in &ports {
                // Only try if scanner isn't active.
                if *cmd_rx.borrow() == WatchdogCmd::ScannerActive {
                    break;
                }
                if try_open_jds(
                    port_name,
                    Arc::clone(&transport),
                    Arc::clone(&device_state),
                    Arc::clone(&live_state),
                ).await {
                    let _ = seq_tx.send(SequencerEvent::HardwareRecovered).await;
                    recovered = true;
                    break;
                }
            }

            if !recovered {
                tracing::debug!("[WATCHDOG] No JDS6600, retry in {}s", RECONNECT_INTERVAL_SECS);
                tokio::time::sleep(Duration::from_secs(RECONNECT_INTERVAL_SECS)).await;
            }
        }
    }
}

// ── Port coordination ─────────────────────────────────────────────────────

/// Block until WatchdogCmd returns to Idle, polling every PAUSE_POLL_MS.
/// When ScannerActive: close our port so the scanner can open it.
async fn pause_if_scanner_active(cmd_rx: &mut watch::Receiver<WatchdogCmd>) {
    if *cmd_rx.borrow() != WatchdogCmd::ScannerActive {
        return;
    }
    tracing::info!("[WATCHDOG] Scanner active – pausing port access");
    loop {
        tokio::time::sleep(Duration::from_millis(PAUSE_POLL_MS)).await;
        if *cmd_rx.borrow() == WatchdogCmd::Idle {
            tracing::info!("[WATCHDOG] Scanner done – resuming");
            return;
        }
    }
}

// ── Initial scan ──────────────────────────────────────────────────────────

async fn initial_scan(
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    live_state:   Arc<RwLock<LiveDeviceState>>,
    cmd_rx:       &mut watch::Receiver<WatchdogCmd>,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(MOCK_FALLBACK_SECS);

    loop {
        pause_if_scanner_active(cmd_rx).await;

        let ports = hal::list_available_ports();
        tracing::info!("[WATCHDOG] Initial scan: {:?}", ports);

        for port_name in &ports {
            if try_open_jds(
                port_name,
                Arc::clone(&transport),
                Arc::clone(&device_state),
                Arc::clone(&live_state),
            ).await {
                return true;
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ── Open + read state ─────────────────────────────────────────────────────

async fn try_open_jds(
    port_name:    &str,
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    live_state:   Arc<RwLock<LiveDeviceState>>,
) -> bool {
    let pn = port_name.to_string();
    let is_jds = tokio::task::spawn_blocking(move || hal::probe_jds6600(&pn))
        .await.unwrap_or(false);

    if !is_jds { return false; }

    let pn2 = port_name.to_string();
    let result = tokio::task::spawn_blocking(move || {
        hal::serial::SerialTransport::open(&pn2, 500)
    }).await;

    match result {
        Ok(Ok(new_transport)) => {
            tracing::info!("[WATCHDOG] Opened {} — reading device state", port_name);
            *transport.lock().unwrap() = Box::new(new_transport);
            device_state.lock().unwrap().invalidate();

            // Read current state using a separate short-lived port handle,
            // since the transport trait doesn't expose the raw SerialPort.
            let pn3 = port_name.to_string();
            let read_result = tokio::task::spawn_blocking(move || {
                match serialport::new(&pn3, 115_200)
                    .timeout(std::time::Duration::from_millis(600))
                    .data_bits(serialport::DataBits::Eight)
                    .stop_bits(serialport::StopBits::One)
                    .parity(serialport::Parity::None)
                    .open()
                {
                    Ok(mut p) => hal::reader::read_device_state(p.as_mut()),
                    Err(e) => {
                        tracing::warn!("[WATCHDOG] State-read port open failed: {}", e);
                        None
                    }
                }
            }).await;
            if let Ok(Some(state)) = read_result {
                *live_state.write().await = state;
                tracing::info!("[WATCHDOG] Device state read successfully");
            } else {
                tracing::warn!("[WATCHDOG] Could not read initial device state");
            }

            true
        }
        Ok(Err(e)) => {
            tracing::warn!("[WATCHDOG] Open {}: {}", port_name, e);
            false
        }
        Err(e) => {
            tracing::error!("[WATCHDOG] spawn_blocking panic: {}", e);
            false
        }
    }
}
