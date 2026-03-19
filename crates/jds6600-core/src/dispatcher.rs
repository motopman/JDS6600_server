/// Rate-limited UART command dispatcher with confirmed-write semantics.
///
/// ## Protocol
/// Every write command to the JDS6600 receives a `:ok\r\n` acknowledgement.
/// The dispatcher now enforces this: it writes a command, then reads back the
/// response in the same mutex lock.  Only if the response contains `:ok` is
/// the command considered successful and the cache updated.
///
/// ## Liveness check
/// Before the first command is sent, the dispatcher sends `:r\r\n` (a bare
/// read probe that the device acknowledges).  If the device does not respond,
/// the dispatcher enters a wait loop rather than spamming commands into the void.
///
/// ## Architecture
/// Write + read happen inside one `spawn_blocking` closure that holds the
/// transport mutex for the entire round-trip.  No other task can interleave
/// a write between our write and our read.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::hal::Transport;
use crate::models::MobileEvent;
use crate::protocol::commands::{build_ascii_command, GeneratorCommand};
use crate::protocol::device_state::DeviceState;

/// Inter-command gap — gives the CH340 UART bridge time to flush its TX buffer.
const RATE_LIMIT_MS:    u64 = 40;
/// How long to wait for the device to reply with `:ok`.
const ACK_TIMEOUT_MS:   u64 = 300;
/// How many consecutive missing `:ok` responses before declaring the device dead.
const MAX_NACK:         u32 = 3;
const QUEUE_DEPTH:     usize = 128;

// ── Handle ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DispatcherHandle {
    tx: mpsc::Sender<GeneratorCommand>,
}

impl DispatcherHandle {
    pub async fn send(&self, cmd: GeneratorCommand) -> Result<(), String> {
        self.tx.send(cmd).await.map_err(|e| format!("Dispatcher queue closed: {}", e))
    }

    pub fn emergency_stop(&self) {
        let cmd = GeneratorCommand::SetOutputEnable { ch1: false, ch2: false };
        match self.tx.try_send(cmd) {
            Ok(_)  => tracing::warn!("[DISPATCHER] Emergency stop enqueued"),
            Err(e) => tracing::error!("[DISPATCHER] Emergency stop lost: {}", e),
        }
    }
}

// ── Spawn ──────────────────────────────────────────────────────────────────

pub fn spawn_dispatcher(
    transport:         Arc<Mutex<Box<dyn Transport>>>,
    device_state:      Arc<Mutex<DeviceState>>,
    on_hardware_error: impl Fn() + Send + 'static,
) -> DispatcherHandle {
    spawn_dispatcher_with_log(transport, device_state, on_hardware_error, None)
}

pub fn spawn_dispatcher_with_log(
    transport:         Arc<Mutex<Box<dyn Transport>>>,
    device_state:      Arc<Mutex<DeviceState>>,
    on_hardware_error: impl Fn() + Send + 'static,
    response_tx:       Option<std::sync::mpsc::SyncSender<MobileEvent>>,
) -> DispatcherHandle {
    let (tx, rx) = mpsc::channel::<GeneratorCommand>(QUEUE_DEPTH);
    tokio::spawn(run(rx, transport, device_state, on_hardware_error, response_tx));
    DispatcherHandle { tx }
}

// ── Liveness probe ────────────────────────────────────────────────────────

/// Send `:r\r\n` and wait for any non-empty response.
/// Returns `true` if the device replies within the timeout.
fn probe_alive(transport: &Arc<Mutex<Box<dyn Transport>>>) -> bool {
    let t = Arc::clone(transport);
    match std::thread::spawn(move || {
        let mut lock = t.lock().unwrap();
        if lock.write_raw(b":r\r\n").is_err() {
            return false;
        }
        match lock.read_line(400) {
            Ok(Some(reply)) => {
                let r = reply.trim_end_matches(['\r', '\n']);
                tracing::info!("[DISPATCHER] Liveness probe reply: {:?}", r);
                !r.is_empty()
            }
            _ => false,
        }
    }).join() {
        Ok(v)  => v,
        Err(_) => false,
    }
}

// ── Consumer task ─────────────────────────────────────────────────────────

async fn run(
    mut rx:            mpsc::Receiver<GeneratorCommand>,
    transport:         Arc<Mutex<Box<dyn Transport>>>,
    device_state:      Arc<Mutex<DeviceState>>,
    on_hardware_error: impl Fn(),
    response_tx:       Option<std::sync::mpsc::SyncSender<MobileEvent>>,
) {
    tracing::info!("[DISPATCHER] Started (rate: {}ms/cmd, ack_timeout: {}ms)",
        RATE_LIMIT_MS, ACK_TIMEOUT_MS);

    // ── Phase 1: liveness check before sending any command ────────────────
    // Retry until the device answers or the transport reports disconnected.
    loop {
        if !transport.lock().unwrap().is_connected() {
            tracing::info!("[DISPATCHER] Transport not connected — waiting");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let t = Arc::clone(&transport);
        let alive = tokio::task::spawn_blocking(move || probe_alive(&t))
            .await
            .unwrap_or(false);
        if alive {
            tracing::info!("[DISPATCHER] Device alive — starting command dispatch");
            notify(&response_tx, MobileEvent::DeviceResponse {
                sent:  ":r".into(),
                reply: "device alive ✓".into(),
            });
            break;
        }
        tracing::warn!("[DISPATCHER] No response to :r probe — retrying in 2s");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // ── Phase 2: command dispatch loop ────────────────────────────────────
    let mut nack_streak: u32 = 0;

    loop {
        let cmd = match rx.recv().await {
            Some(c) => c,
            None    => { tracing::info!("[DISPATCHER] Channel closed"); break; }
        };

        if !needs_write(&cmd, &device_state) {
            tracing::debug!("[DISPATCHER] Skip (no change): {:?}", cmd);
            continue;
        }

        let frame = match build_ascii_command(&cmd) {
            Ok(f)  => f,
            Err(e) => { tracing::error!("[DISPATCHER] Encode error: {}", e); continue; }
        };

        // ── Write + read in ONE spawn_blocking, ONE mutex lock ────────────
        // Holding the lock for both operations guarantees the reply we read
        // belongs to the command we just sent — no interleaving possible.
        let t       = Arc::clone(&transport);
        let f       = frame.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut lock = t.lock().unwrap();
            lock.write_raw(&f)?;
            let reply = lock.read_line(ACK_TIMEOUT_MS)
                .ok()
                .flatten()
                .unwrap_or_default();
            let reply = reply.trim_end_matches(['\r', '\n']).to_string();
            let ack   = reply.to_lowercase().contains("ok");
            Ok::<(bool, String), crate::error::JdsError>((ack, reply))
        }).await;

        let sent_str = String::from_utf8_lossy(&frame)
            .trim_end_matches(['\r', '\n'])
            .to_string();

        // spawn_blocking returns Result<inner_result, JoinError>
        match result {
            Ok(Ok((true, reply))) => {
                // ✓ Device confirmed the command.
                nack_streak = 0;
                update_cache(&cmd, &device_state);
                tracing::debug!("[DISPATCHER] ACK  TX:{:?}  RX:{:?}", sent_str, reply);
                notify(&response_tx, MobileEvent::DeviceResponse { sent: sent_str, reply });
            }
            Ok(Ok((false, reply))) => {
                // Device replied but not with :ok.
                nack_streak += 1;
                let msg = if reply.is_empty() {
                    format!("(no response after {}ms)", ACK_TIMEOUT_MS)
                } else {
                    format!("unexpected: {:?}", reply)
                };
                tracing::warn!("[DISPATCHER] NACK ({}/{}) TX:{:?}  RX:{}",
                    nack_streak, MAX_NACK, sent_str, msg);
                notify(&response_tx, MobileEvent::DeviceResponse {
                    sent:  sent_str,
                    reply: format!("⚠ {}", msg),
                });
                if nack_streak >= MAX_NACK {
                    tracing::error!("[DISPATCHER] consecutive NACKs limit reached — declaring hardware error");
                    while rx.try_recv().is_ok() {}
                    on_hardware_error();
                    break;
                }
            }
            Ok(Err(e)) => {
                tracing::error!("[DISPATCHER] Write/read error: {}", e);
                while rx.try_recv().is_ok() {}
                on_hardware_error();
                break;
            }
            Err(e) => {
                tracing::error!("[DISPATCHER] spawn_blocking panic: {}", e);
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(RATE_LIMIT_MS)).await;
    }

    tracing::info!("[DISPATCHER] Stopped");
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn notify(tx: &Option<std::sync::mpsc::SyncSender<MobileEvent>>, ev: MobileEvent) {
    if let Some(t) = tx { let _ = t.try_send(ev); }
}

fn needs_write(cmd: &GeneratorCommand, state: &Arc<Mutex<DeviceState>>) -> bool {
    let s = state.lock().unwrap();
    match cmd {
        GeneratorCommand::SetFrequency   { channel, hz }      => s.frequency_changed(*channel, *hz),
        GeneratorCommand::SetAmplitude   { channel, volts }   => s.amplitude_changed(*channel, *volts),
        GeneratorCommand::SetOffset      { channel, volts }   => s.offset_changed(*channel, *volts),
        GeneratorCommand::SetDuty        { channel, percent } => s.duty_changed(*channel, *percent),
        GeneratorCommand::SetWaveform    { channel, waveform} => s.waveform_changed(*channel, waveform),
        GeneratorCommand::SetOutputEnable{ ch1, ch2 }         => s.output_changed(*ch1, *ch2),
    }
}

fn update_cache(cmd: &GeneratorCommand, state: &Arc<Mutex<DeviceState>>) {
    let mut s = state.lock().unwrap();
    match cmd {
        GeneratorCommand::SetFrequency   { channel, hz }       => s.set_frequency(*channel, *hz),
        GeneratorCommand::SetAmplitude   { channel, volts }    => s.set_amplitude(*channel, *volts),
        GeneratorCommand::SetOffset      { channel, volts }    => s.set_offset(*channel, *volts),
        GeneratorCommand::SetDuty        { channel, percent }  => s.set_duty(*channel, *percent),
        GeneratorCommand::SetWaveform    { channel, waveform } => s.set_waveform(*channel, waveform.clone()),
        GeneratorCommand::SetOutputEnable{ ch1, ch2 }          => s.set_output(*ch1, *ch2),
    }
}

// ── Quick-command bridge (UI thread → async dispatcher) ───────────────────

#[derive(Clone)]
pub struct QuickCmdSender {
    tx: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::SyncSender<GeneratorCommand>>>,
}

pub struct QuickCmdReceiver {
    pub rx: std::sync::mpsc::Receiver<GeneratorCommand>,
}

pub fn quick_cmd_channel() -> (QuickCmdSender, QuickCmdReceiver) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<GeneratorCommand>(32);
    (
        QuickCmdSender { tx: std::sync::Arc::new(std::sync::Mutex::new(tx)) },
        QuickCmdReceiver { rx },
    )
}

impl QuickCmdSender {
    pub fn try_send(&self, cmd: GeneratorCommand) {
        let _ = self.tx.lock().unwrap().try_send(cmd);
    }
}
