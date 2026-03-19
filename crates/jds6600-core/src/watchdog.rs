/// Hardware watchdog — scans for and monitors the JDS6600.
///
/// ## Design
///
/// The watchdog is the SOLE authority on transport lifetime.
/// There is NO mock fallback — if the device is not connected the sequencer
/// stays in DISCONNECTED state and the UI shows "Searching…".
///
/// ## Lifecycle
///
/// Phase 1 — Initial scan (runs once at startup):
///   Scans all COM/tty ports every 2 s until a JDS6600 answers `:r\r\n`.
///   No timeout.  Notifies the UI with `HardwareStatus` on each attempt.
///   On success → opens SerialTransport → sends `HardwareFound` to sequencer.
///
/// Phase 2 — Silent liveness (steady-state):
///   Sends a silent `:r\r\n` probe every KEEPALIVE_SECS.
///   Does NOT log to the UI unless the probe FAILS.
///   On failure → `HardwareLost` to sequencer + `HardwareStatus(false)` to UI
///              → fall back to Phase 1 scan loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch, RwLock};

use crate::hal::{self, Transport};
use crate::models::{LiveDeviceState, MobileEvent, WatchdogCmd};
use crate::protocol::device_state::DeviceState;
use crate::sequencer::SequencerEvent;

/// How often to send a silent `:r\r\n` keepalive while device is connected.
const KEEPALIVE_SECS:       u64 = 5;
/// How long between port re-scan attempts if no device found.
const RESCAN_INTERVAL_SECS: u64 = 2;

pub async fn run_watchdog(
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    seq_tx:       mpsc::Sender<SequencerEvent>,
    mut cmd_rx:   watch::Receiver<WatchdogCmd>,
    live_state:   Arc<RwLock<LiveDeviceState>>,
    // Push HardwareStatus events to the egui UI
    mobile_tx:    Option<std::sync::mpsc::SyncSender<MobileEvent>>,
) {
    tracing::info!("[WATCHDOG] Started — scanning for JDS6600");

    loop {
        // ── Phase 1: scan until device found ─────────────────────────────
        let port = scan_until_found(
            Arc::clone(&transport),
            Arc::clone(&device_state),
            Arc::clone(&live_state),
            &mut cmd_rx,
            &mobile_tx,
        ).await;

        tracing::info!("[WATCHDOG] JDS6600 found on {} → HardwareFound", port);
        notify(&mobile_tx, MobileEvent::HardwareStatus {
            connected: true,
            detail:    port.clone(),
        });
        let _ = seq_tx.send(SequencerEvent::HardwareFound).await;

        // ── Phase 2: silent keepalive ─────────────────────────────────────
        loop {
            tokio::time::sleep(Duration::from_secs(KEEPALIVE_SECS)).await;

            // Yield if scanner is active (it has taken ownership of the port)
            if *cmd_rx.borrow() == WatchdogCmd::ScannerActive {
                continue;
            }

            let alive = silent_probe(Arc::clone(&transport)).await;
            if !alive {
                tracing::warn!("[WATCHDOG] Keepalive probe failed — device lost on {}", port);
                notify(&mobile_tx, MobileEvent::HardwareStatus {
                    connected: false,
                    detail:    format!("lost on {}", port),
                });
                let _ = seq_tx.send(SequencerEvent::HardwareLost).await;
                // Reset transport so dispatcher stops trying to send
                {
                    let mut lock = transport.lock().unwrap();
                    // Replace with a stub that reports disconnected
                    *lock = Box::new(crate::hal::mock::MockTransport::disconnected());
                }
                break; // → back to Phase 1
            }
            // Still alive — say nothing to the UI
            tracing::debug!("[WATCHDOG] Keepalive OK on {}", port);
        }
    }
}

// ── Phase 1: scan ─────────────────────────────────────────────────────────

/// Scan all ports until a JDS6600 responds.  Returns the port name.
/// This function never times out.  Notifies the UI on each scan round.
async fn scan_until_found(
    transport:    Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    live_state:   Arc<RwLock<LiveDeviceState>>,
    cmd_rx:       &mut watch::Receiver<WatchdogCmd>,
    mobile_tx:    &Option<std::sync::mpsc::SyncSender<MobileEvent>>,
) -> String {
    let mut attempt: u32 = 0;
    loop {
        // Pause if the UI scanner is using the port
        if *cmd_rx.borrow() == WatchdogCmd::ScannerActive {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        let ports = hal::list_available_ports();
        attempt += 1;

        if ports.is_empty() {
            tracing::debug!("[WATCHDOG] Scan #{}: no ports found", attempt);
            notify(mobile_tx, MobileEvent::HardwareStatus {
                connected: false,
                detail:    "no serial ports found — check USB cable".into(),
            });
        } else {
            tracing::debug!("[WATCHDOG] Scan #{}: checking {:?}", attempt, ports);
            notify(mobile_tx, MobileEvent::HardwareStatus {
                connected: false,
                detail:    format!("scanning {} port(s)…", ports.len()),
            });
        }

        for port_name in &ports {
            if *cmd_rx.borrow() == WatchdogCmd::ScannerActive { break; }
            if try_open_jds(port_name, Arc::clone(&transport), Arc::clone(&device_state), Arc::clone(&live_state)).await {
                return port_name.clone();
            }
        }

        tokio::time::sleep(Duration::from_secs(RESCAN_INTERVAL_SECS)).await;
    }
}

// ── Silent liveness probe ─────────────────────────────────────────────────

/// Send `:r\r\n` and return `true` if the device replies within 400 ms.
/// Never logs to the UI — silent by design.
async fn silent_probe(transport: Arc<Mutex<Box<dyn Transport>>>) -> bool {
    tokio::task::spawn_blocking(move || {
        let mut lock = match transport.lock() {
            Ok(l)  => l,
            Err(_) => return false,
        };
        if !lock.is_connected() { return false; }
        if lock.write_raw(b":r\r\n").is_err() { return false; }
        matches!(lock.read_line(400), Ok(Some(r)) if !r.trim().is_empty())
    }).await.unwrap_or(false)
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
    match tokio::task::spawn_blocking(move || hal::serial::SerialTransport::open(&pn2, 500)).await {
        Ok(Ok(new_transport)) => {
            tracing::info!("[WATCHDOG] Opened {}", port_name);
            *transport.lock().unwrap() = Box::new(new_transport);
            device_state.lock().unwrap().invalidate();

            // Best-effort state read (separate short-lived handle)
            let pn3 = port_name.to_string();
            if let Ok(Some(state)) = tokio::task::spawn_blocking(move || {
                serialport::new(&pn3, 115_200)
                    .timeout(Duration::from_millis(600))
                    .data_bits(serialport::DataBits::Eight)
                    .stop_bits(serialport::StopBits::One)
                    .parity(serialport::Parity::None)
                    .open()
                    .ok()
                    .and_then(|mut p| hal::reader::read_device_state(p.as_mut()))
            }).await {
                *live_state.write().await = state;
            }
            true
        }
        Ok(Err(e)) => { tracing::warn!("[WATCHDOG] Open {}: {}", port_name, e); false }
        Err(e)    => { tracing::error!("[WATCHDOG] Panic: {}", e); false }
    }
}

fn notify(tx: &Option<std::sync::mpsc::SyncSender<MobileEvent>>, ev: MobileEvent) {
    if let Some(t) = tx { let _ = t.try_send(ev); }
}
