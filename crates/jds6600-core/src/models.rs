use serde::{Deserialize, Serialize};

// ── Waveform ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Waveform {
    Sine,
    Square,
    Triangle,
    Pulse,
    #[serde(other)]
    Unknown,
}

impl Waveform {
    /// Maps to the JDS6600 waveform register value.
    /// Indexes confirmed by manual + Protocol Scanner; update after sweep.
    pub fn to_device_index(&self) -> u8 {
        match self {
            Waveform::Sine     => 0,
            Waveform::Square   => 1,
            Waveform::Triangle => 2,
            Waveform::Pulse    => 3,
            Waveform::Unknown  => 0,
        }
    }
}

impl std::fmt::Display for Waveform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Waveform::Sine     => write!(f, "sine"),
            Waveform::Square   => write!(f, "square"),
            Waveform::Triangle => write!(f, "triangle"),
            Waveform::Pulse    => write!(f, "pulse"),
            Waveform::Unknown  => write!(f, "unknown"),
        }
    }
}

// ── SequenceBlock ─────────────────────────────────────────────────────────

/// One step in an execution sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceBlock {
    /// Client-assigned opaque ID.  Echoed in `current_block_id` telemetry.
    pub id: String,
    /// Output channel: 1 or 2.
    pub channel: u8,
    /// Frequency in Hz.  Provisional range: 0.01 .. 60_000_000.
    pub frequency: f64,
    /// Peak amplitude in Volts (0.0 .. 20.0).
    pub amplitude: f64,
    /// DC offset in Volts (−10.0 .. +10.0).
    pub offset: f64,
    /// Duty cycle in percent (0.0 .. 99.9).  Used for square/pulse.
    pub duty: f64,
    pub waveform: Waveform,
    /// Duration to hold this step, in milliseconds.
    pub duration_ms: u64,
}

// ── Sequence ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sequence {
    pub sequence_name: String,
    pub blocks: Vec<SequenceBlock>,
}

// ── Incoming (Mobile → Server) ────────────────────────────────────────────

/// Tagged-union over all messages the mobile client may send.
/// JSON shape: `{"type": "...", "payload": {...}}`
#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum IncomingMessage {
    #[serde(rename = "sequence_upload")]
    SequenceUpload(Sequence),

    #[serde(rename = "control_command")]
    ControlCommand(ControlPayload),

    /// One-shot device command (e.g. set triangle wave) sent from the UI.
    /// Does not go through the Sequencer — routed directly to the Dispatcher.
    #[serde(rename = "quick_command")]
    QuickCommand(QuickCommandPayload),
}

#[derive(Debug, Deserialize)]
pub struct QuickCommandPayload {
    /// e.g. "set_waveform_triangle_ch1"
    pub action: String,
}

#[derive(Debug, Deserialize)]
pub struct ControlPayload {
    pub command: ControlCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlCommand {
    Start,
    Stop,
    Pause,
    Resume,
}

// ── Outgoing (Server → Mobile) ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum OutgoingMessage {
    #[serde(rename = "server_status")]
    ServerStatus(StatusPayload),
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct StatusPayload {
    pub hardware_connected: bool,
    pub sequencer_state: String,
    pub active_sequence: Option<String>,
    pub current_block_id: Option<String>,
    pub time_left_ms: u64,
    pub error_message: Option<String>,
}

// ── SharedSnapshot ────────────────────────────────────────────────────────

/// Shared read-only state view written by the Sequencer, read by the
/// WebSocket handler to greet newly-connecting clients.
#[derive(Debug, Clone, Default)]
pub struct SharedSnapshot {
    pub hardware_connected: bool,
    pub sequencer_state: String,
    pub active_sequence: Option<String>,
    pub current_block_id: Option<String>,
    pub time_left_ms: u64,
    pub error_message: Option<String>,
}

impl SharedSnapshot {
    pub fn to_status_payload(&self) -> StatusPayload {
        StatusPayload {
            hardware_connected: self.hardware_connected,
            sequencer_state: self.sequencer_state.clone(),
            active_sequence: self.active_sequence.clone(),
            current_block_id: self.current_block_id.clone(),
            time_left_ms: self.time_left_ms,
            error_message: self.error_message.clone(),
        }
    }
}

// ── Live device state (read from hardware on connect) ─────────────────────

/// Current parameter values read directly from the JDS6600.
/// Populated once after hardware connects and after each Quick Command.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LiveChannelState {
    pub waveform:     Waveform,
    pub frequency_hz: f64,
    pub amplitude_v:  f64,
    pub offset_v:     f64,
    pub duty_percent: f64,
}

impl Default for Waveform {
    fn default() -> Self { Waveform::Sine }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LiveDeviceState {
    pub ch1: LiveChannelState,
    pub ch2: LiveChannelState,
}

// ── Commands sent from the UI to the Watchdog ─────────────────────────────

/// Signals sent from the UI thread (via a `watch` channel) to the Watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogCmd {
    /// Normal steady-state – watchdog manages port on its own schedule.
    Idle,
    /// UI scanner wants exclusive port access for its sweep.
    /// Watchdog must release the port and notify when done.
    ScannerActive,
}
