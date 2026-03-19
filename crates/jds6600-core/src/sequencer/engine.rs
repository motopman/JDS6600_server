/// Sequencer Engine – async state machine for sequence execution.
///
/// ## State graph
/// ```text
/// DISCONNECTED ──HardwareFound──► IDLE ◄──────────────────────────────────┐
///      ▲                           │                                       │
///      │                      Start│(ptr=0)                   last block / Stop
///      │                           ▼                                       │
///      │                       RUNNING ──Pause──► PAUSED ──Resume──► RUNNING
///      │                           │                  │
///      │                    HardwareLost           HardwareLost
///      │                           ▼                  ▼
///      │                         ERROR ──────────────────────────────────►─┘
///      │                           │
///      └───────────HardwareRecovered (→ IDLE, sequence reset for safety)
/// ```
///
/// ## Key design rules
/// 1. The engine owns **no** I/O – it only pushes `GeneratorCommand`s into
///    the `DispatcherHandle`.
/// 2. If the mobile client disconnects while RUNNING, the engine continues
///    uninterrupted – there is no "client heartbeat" state.
/// 3. On hardware error the engine immediately calls `emergency_stop()` on
///    the handle before transitioning to ERROR.
/// 4. After `HardwareRecovered` the sequence is **reset** (pointer = 0,
///    context cleared) because the device state is unknown after a reconnect.

use std::time::{Duration, Instant};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, RwLock};

use crate::dispatcher::DispatcherHandle;
use crate::models::{MobileEvent, OutgoingMessage, Sequence, SequenceBlock, SharedSnapshot, StatusPayload};
use crate::protocol::commands::GeneratorCommand;

// ── Public types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequencerState {
    Disconnected,
    Idle,
    Running,
    Paused,
    /// Carries a human-readable description of the fault.
    Error(String),
}

impl SequencerState {
    pub fn label(&self) -> &'static str {
        match self {
            SequencerState::Disconnected => "DISCONNECTED",
            SequencerState::Idle         => "IDLE",
            SequencerState::Running      => "RUNNING",
            SequencerState::Paused       => "PAUSED",
            SequencerState::Error(_)     => "ERROR",
        }
    }
}

#[derive(Debug)]
pub enum SequencerEvent {
    HardwareFound,
    HardwareLost,
    HardwareRecovered,
    SequenceLoaded(Sequence),
    Start,
    /// Internal: fired when the current block's timer expires.
    Tick,
    Pause,
    Resume,
    Stop,
}

// ── Internal execution context ────────────────────────────────────────────

struct Context {
    sequence: Option<Sequence>,
    pointer: usize,
    /// Wall-clock instant when the current block was activated.
    block_start: Option<Instant>,
    /// Time saved on Pause so Resume continues from the right offset.
    paused_remaining: Option<Duration>,
}

impl Context {
    fn new() -> Self {
        Self { sequence: None, pointer: 0, block_start: None, paused_remaining: None }
    }

    fn current_block(&self) -> Option<&SequenceBlock> {
        self.sequence.as_ref()?.blocks.get(self.pointer)
    }

    fn has_next(&self) -> bool {
        self.sequence.as_ref()
            .map(|s| self.pointer + 1 < s.blocks.len())
            .unwrap_or(false)
    }

    fn time_remaining(&self) -> Duration {
        // If paused, return the saved remainder.
        if let Some(rem) = self.paused_remaining {
            return rem;
        }

        let block = match self.current_block() {
            Some(b) => b,
            None    => return Duration::ZERO,
        };
        let total = Duration::from_millis(block.duration_ms);
        match self.block_start {
            Some(t) => total.saturating_sub(t.elapsed()),
            None    => total,
        }
    }

    fn time_remaining_ms(&self) -> u64 {
        self.time_remaining().as_millis() as u64
    }
}

// ── Engine ────────────────────────────────────────────────────────────────

pub struct SequencerEngine {
    state:      SequencerState,
    ctx:        Context,
    event_rx:   mpsc::Receiver<SequencerEvent>,
    dispatcher: DispatcherHandle,
    snapshot:   Arc<RwLock<SharedSnapshot>>,
    status_tx:  broadcast::Sender<String>,
    /// Push execution events to the egui UI (optional — None in tests).
    mobile_tx:  Option<std::sync::mpsc::SyncSender<MobileEvent>>,
}

impl SequencerEngine {
    pub fn new(
        event_rx:   mpsc::Receiver<SequencerEvent>,
        dispatcher: DispatcherHandle,
        snapshot:   Arc<RwLock<SharedSnapshot>>,
        status_tx:  broadcast::Sender<String>,
    ) -> Self {
        Self::with_mobile_tx(event_rx, dispatcher, snapshot, status_tx, None)
    }

    pub fn with_mobile_tx(
        event_rx:   mpsc::Receiver<SequencerEvent>,
        dispatcher: DispatcherHandle,
        snapshot:   Arc<RwLock<SharedSnapshot>>,
        status_tx:  broadcast::Sender<String>,
        mobile_tx:  Option<std::sync::mpsc::SyncSender<MobileEvent>>,
    ) -> Self {
        Self {
            state: SequencerState::Disconnected,
            ctx: Context::new(),
            event_rx,
            dispatcher,
            snapshot,
            status_tx,
            mobile_tx,
        }
    }

    /// Helper: send a MobileEvent to the UI (ignores errors if channel full or not connected).
    fn notify(&self, ev: MobileEvent) {
        if let Some(tx) = &self.mobile_tx {
            let _ = tx.try_send(ev);
        }
    }

    /// Consume the engine and run until the event channel closes.
    pub async fn run(mut self) {
        tracing::info!("[SEQUENCER] Started ({})", self.state.label());
        self.publish_status().await;

        loop {
            if self.state == SequencerState::Running {
                let remaining = self.ctx.time_remaining();

                tokio::select! {
                    biased; // external events checked first – enables immediate Stop/Pause

                    Some(ev) = self.event_rx.recv() => {
                        self.transition(ev).await;
                    }
                    _ = tokio::time::sleep(remaining) => {
                        self.transition(SequencerEvent::Tick).await;
                    }
                }
            } else {
                match self.event_rx.recv().await {
                    Some(ev) => self.transition(ev).await,
                    None     => { tracing::info!("[SEQUENCER] Channel closed"); break; }
                }
            }
        }
    }

    // ── Transition table ──────────────────────────────────────────────────

    async fn transition(&mut self, event: SequencerEvent) {
        tracing::debug!("[SEQUENCER] {} + {:?}", self.state.label(), event);

        let next: Option<SequencerState> = match (&self.state, event) {

            // ── DISCONNECTED ──────────────────────────────────────────
            (SequencerState::Disconnected, SequencerEvent::HardwareFound) => {
                tracing::info!("[SEQUENCER] Hardware found → IDLE");
                Some(SequencerState::Idle)
            }

            // ── IDLE ──────────────────────────────────────────────────
            (SequencerState::Idle, SequencerEvent::SequenceLoaded(seq)) => {
                let total_ms: u64 = seq.blocks.iter().map(|b| b.duration_ms).sum();
                let total_secs = total_ms / 1000;
                tracing::info!("[SEQUENCER] Sequence '{}' loaded ({} blocks, {}s total) → auto-start",
                    seq.sequence_name, seq.blocks.len(), total_secs);
                self.notify(MobileEvent::SequenceReceived {
                    name:                seq.sequence_name.clone(),
                    blocks:              seq.blocks.len(),
                    total_duration_secs: total_secs,
                });
                self.ctx.sequence = Some(seq);
                self.ctx.pointer  = 0;
                // Auto-start: begin executing immediately without waiting for Start.
                match self.begin_execution().await {
                    Ok(())  => Some(SequencerState::Running),
                    Err(e)  => Some(SequencerState::Error(e)),
                }
            }
            // Still accept an explicit Start command (e.g. from UI) — no-op if already running.
            (SequencerState::Idle, SequencerEvent::Start) => {
                match self.begin_execution().await {
                    Ok(())  => Some(SequencerState::Running),
                    Err(e)  => Some(SequencerState::Error(e)),
                }
            }
            (SequencerState::Idle, SequencerEvent::HardwareLost) => {
                Some(SequencerState::Disconnected)
            }

            // ── RUNNING ───────────────────────────────────────────────
            (SequencerState::Running, SequencerEvent::Tick) => {
                if self.ctx.has_next() {
                    self.ctx.pointer += 1;
                    match self.apply_current_block().await {
                        Ok(())  => None,                                   // stay RUNNING
                        Err(e)  => {
                            self.dispatcher.emergency_stop();
                            Some(SequencerState::Error(e))
                        }
                    }
                } else {
                    let name = self.ctx.sequence.as_ref()
                        .map(|s| s.sequence_name.clone())
                        .unwrap_or_default();
                    tracing::info!("[SEQUENCER] Sequence '{}' finished → IDLE", name);
                    self.notify(MobileEvent::SequenceFinished { name });
                    self.disable_outputs().await;
                    self.ctx.pointer       = 0;
                    self.ctx.block_start   = None;
                    Some(SequencerState::Idle)
                }
            }
            (SequencerState::Running, SequencerEvent::Pause) => {
                let rem = self.ctx.time_remaining();
                self.ctx.paused_remaining = Some(rem);
                self.ctx.block_start      = None;
                tracing::info!("[SEQUENCER] Paused – {}ms remaining on block {}",
                    rem.as_millis(), self.ctx.pointer);
                Some(SequencerState::Paused)
            }
            (SequencerState::Running, SequencerEvent::Stop) => {
                tracing::info!("[SEQUENCER] Stop");
                self.notify(MobileEvent::SequenceStopped);
                self.disable_outputs().await;
                self.ctx.pointer           = 0;
                self.ctx.block_start       = None;
                self.ctx.paused_remaining  = None;
                Some(SequencerState::Idle)
            }
            (SequencerState::Running, SequencerEvent::HardwareLost) => {
                tracing::error!("[SEQUENCER] Hardware lost while RUNNING");
                self.dispatcher.emergency_stop();
                Some(SequencerState::Error("Hardware lost during execution".into()))
            }

            // ── PAUSED ────────────────────────────────────────────────
            (SequencerState::Paused, SequencerEvent::Resume) => {
                // Re-anchor block_start so time_remaining() is correct from here.
                let remaining = self.ctx.paused_remaining.unwrap_or(Duration::ZERO);
                let block_dur = self.ctx.current_block()
                    .map(|b| Duration::from_millis(b.duration_ms))
                    .unwrap_or(Duration::ZERO);

                // block_start = now - (block_dur - remaining)
                let elapsed_before_pause = block_dur.saturating_sub(remaining);
                self.ctx.block_start       = Some(Instant::now() - elapsed_before_pause);
                self.ctx.paused_remaining  = None;

                tracing::info!("[SEQUENCER] Resumed – {}ms remaining", remaining.as_millis());
                Some(SequencerState::Running)
            }
            (SequencerState::Paused, SequencerEvent::Stop) => {
                tracing::info!("[SEQUENCER] Stop from Paused");
                self.disable_outputs().await;
                self.ctx.pointer           = 0;
                self.ctx.block_start       = None;
                self.ctx.paused_remaining  = None;
                Some(SequencerState::Idle)
            }
            (SequencerState::Paused, SequencerEvent::HardwareLost) => {
                self.dispatcher.emergency_stop();
                Some(SequencerState::Error("Hardware lost while paused".into()))
            }

            // ── ERROR ─────────────────────────────────────────────────
            (SequencerState::Error(_), SequencerEvent::HardwareRecovered) => {
                tracing::info!("[SEQUENCER] Hardware recovered → IDLE (sequence reset)");
                self.ctx = Context::new();
                Some(SequencerState::Idle)
            }
            (SequencerState::Error(_), SequencerEvent::SequenceLoaded(seq)) => {
                // Accept new sequence while in ERROR so it's ready after recovery.
                self.ctx.sequence = Some(seq);
                None
            }

            // ── Catch-all: log and ignore ──────────────────────────────
            (state, event) => {
                tracing::warn!("[SEQUENCER] Ignored {:?} in {}", event, state.label());
                None
            }
        };

        if let Some(s) = next {
            self.state = s;
        }

        self.publish_status().await;
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    async fn begin_execution(&mut self) -> Result<(), String> {
        match &self.ctx.sequence {
            None                            => return Err("No sequence loaded".into()),
            Some(s) if s.blocks.is_empty()  => return Err("Sequence is empty".into()),
            _                               => {}
        }
        self.ctx.pointer = 0;
        self.apply_current_block().await
    }

    async fn apply_current_block(&mut self) -> Result<(), String> {
        let block = self.ctx.current_block()
            .ok_or_else(|| "Execution pointer past end of sequence".to_string())?
            .clone();

        let block_total = self.ctx.sequence.as_ref().map(|s| s.blocks.len()).unwrap_or(1);
        tracing::info!("[SEQUENCER] → Block {}/{} | {}Hz {} amp={}V dur={}ms",
            self.ctx.pointer + 1, block_total, block.frequency, block.waveform,
            block.amplitude, block.duration_ms);

        self.notify(MobileEvent::BlockStarted {
            block_index: self.ctx.pointer,
            block_total,
            channel:     block.channel,
            frequency:   block.frequency,
            waveform:    block.waveform.to_string(),
            amplitude:   block.amplitude,
            duration_ms: block.duration_ms,
        });

        // Waveform first – avoids transient wrong-waveform output.
        let cmds: &[GeneratorCommand] = &[
            GeneratorCommand::SetWaveform  { channel: block.channel, waveform: block.waveform.clone() },
            GeneratorCommand::SetFrequency { channel: block.channel, hz:       block.frequency },
            GeneratorCommand::SetAmplitude { channel: block.channel, volts:    block.amplitude },
            GeneratorCommand::SetOffset    { channel: block.channel, volts:    block.offset    },
            GeneratorCommand::SetDuty      { channel: block.channel, percent:  block.duty      },
            GeneratorCommand::SetOutputEnable {
                ch1: block.channel == 1,
                ch2: block.channel == 2,
            },
        ];

        for cmd in cmds {
            self.dispatcher.send(cmd.clone()).await
                .map_err(|e| format!("Dispatcher error: {}", e))?;
        }

        self.ctx.block_start      = Some(Instant::now());
        self.ctx.paused_remaining = None;
        Ok(())
    }

    async fn disable_outputs(&self) {
        let _ = self.dispatcher.send(
            GeneratorCommand::SetOutputEnable { ch1: false, ch2: false }
        ).await;
    }

    async fn publish_status(&self) {
        let hw_ok = !matches!(
            self.state,
            SequencerState::Disconnected | SequencerState::Error(_)
        );

        let payload = StatusPayload {
            hardware_connected: hw_ok,
            sequencer_state:    self.state.label().to_string(),
            active_sequence:    self.ctx.sequence.as_ref().map(|s| s.sequence_name.clone()),
            current_block_id:   self.ctx.current_block().map(|b| b.id.clone()),
            time_left_ms:       self.ctx.time_remaining_ms(),
            error_message:      if let SequencerState::Error(e) = &self.state { Some(e.clone()) } else { None },
        };

        // Update snapshot for newly-connecting WS clients.
        {
            let mut snap = self.snapshot.write().await;
            snap.hardware_connected = payload.hardware_connected;
            snap.sequencer_state    = payload.sequencer_state.clone();
            snap.active_sequence    = payload.active_sequence.clone();
            snap.current_block_id   = payload.current_block_id.clone();
            snap.time_left_ms       = payload.time_left_ms;
            snap.error_message      = payload.error_message.clone();
        }

        // Broadcast to all live WS clients.
        if let Ok(json) = serde_json::to_string(&OutgoingMessage::ServerStatus(payload)) {
            let _ = self.status_tx.send(json);
        }
    }
}
