/// Rate-limited UART command dispatcher.
///
/// The JDS6600 uses a cheap CH340/CP2102 USB-CDC bridge with a 32–128 byte
/// RX hardware buffer.  If multiple commands are written back-to-back, the
/// buffer overflows and commands are silently lost.
///
/// Architecture (Producer–Consumer):
/// * **Producers** — Sequencer + WebSocket handler — push commands into a
///   bounded `mpsc` channel (non-blocking, constant time).
/// * **Consumer** — this task — drains the channel one command at a time,
///   sleeping `RATE_LIMIT_MS` between each write.
/// * The `DeviceState` cache is consulted here to skip no-op writes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::hal::Transport;
use crate::protocol::commands::{build_ascii_command, GeneratorCommand};
use crate::protocol::device_state::DeviceState;

const RATE_LIMIT_MS: u64 = 30;
const QUEUE_DEPTH: usize = 128;

// ── Handle (given to producers) ───────────────────────────────────────────

#[derive(Clone)]
pub struct DispatcherHandle {
    tx: mpsc::Sender<GeneratorCommand>,
}

impl DispatcherHandle {
    /// Enqueue a command.  Returns `Err` only if the Dispatcher task died.
    pub async fn send(&self, cmd: GeneratorCommand) -> Result<(), String> {
        self.tx.send(cmd).await.map_err(|e| format!("Dispatcher queue closed: {}", e))
    }

    /// Non-blocking emergency output-disable.  Safe to call from any context.
    pub fn emergency_stop(&self) {
        let cmd = GeneratorCommand::SetOutputEnable { ch1: false, ch2: false };
        match self.tx.try_send(cmd) {
            Ok(_)  => tracing::warn!("[DISPATCHER] Emergency stop enqueued"),
            Err(e) => tracing::error!("[DISPATCHER] Emergency stop lost: {}", e),
        }
    }
}

// ── Spawn ──────────────────────────────────────────────────────────────────

/// Spawn the dispatcher task.  Returns a `DispatcherHandle` for producers.
/// `on_hardware_error` is called when the transport reports a write failure.
pub fn spawn_dispatcher(
    transport: Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    on_hardware_error: impl Fn() + Send + 'static,
) -> DispatcherHandle {
    let (tx, rx) = mpsc::channel::<GeneratorCommand>(QUEUE_DEPTH);
    tokio::spawn(run(rx, transport, device_state, on_hardware_error));
    DispatcherHandle { tx }
}

// ── Consumer task ─────────────────────────────────────────────────────────

async fn run(
    mut rx: mpsc::Receiver<GeneratorCommand>,
    transport: Arc<Mutex<Box<dyn Transport>>>,
    device_state: Arc<Mutex<DeviceState>>,
    on_hardware_error: impl Fn(),
) {
    tracing::info!("[DISPATCHER] Started (rate: {}ms/cmd)", RATE_LIMIT_MS);

    loop {
        let cmd = match rx.recv().await {
            Some(c) => c,
            None    => { tracing::info!("[DISPATCHER] Channel closed"); break; }
        };

        // Skip if cached state already matches.
        if !needs_write(&cmd, &device_state) {
            tracing::debug!("[DISPATCHER] Skip (no change): {:?}", cmd);
            continue;
        }

        // Encode to bytes.
        let frame = match build_ascii_command(&cmd) {
            Ok(f)  => f,
            Err(e) => { tracing::error!("[DISPATCHER] Encode error: {}", e); continue; }
        };

        // Write (blocking I/O in spawn_blocking to never block the executor).
        let t = Arc::clone(&transport);
        let f = frame.clone();
        let result = tokio::task::spawn_blocking(move || {
            t.lock().unwrap().write_raw(&f)
        }).await;

        match result {
            Ok(Ok(())) => {
                update_cache(&cmd, &device_state);
            }
            Ok(Err(e)) => {
                tracing::error!("[DISPATCHER] Write error: {}", e);
                // Drain queue – stale commands cannot be sent.
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

// ── Cache helpers ─────────────────────────────────────────────────────────

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
