use serde::{Deserialize, Deserializer, Serialize};

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
///
/// Accepts both the server's internal field names AND the Android client names:
///   frequency   OR frequencyHz
///   amplitude   OR amplitudeV
///   offset      OR offsetV
///   duty        OR dutyCycle
///   duration_ms OR durationSeconds (auto-converted ×1000)
///   waveform    accepts "SINE"/"sine"/"Sine" etc. (case-insensitive)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceBlock {
    /// Client-assigned opaque ID.  Echoed in `current_block_id` telemetry.
    pub id: String,
    /// Output channel: 1 or 2.
    pub channel: u8,

    /// Frequency in Hz.  Accepts `frequency` or `frequencyHz`.
    #[serde(alias = "frequencyHz")]
    pub frequency: f64,

    /// Peak amplitude in Volts.  Accepts `amplitude` or `amplitudeV`.
    #[serde(alias = "amplitudeV")]
    pub amplitude: f64,

    /// DC offset in Volts.  Accepts `offset` or `offsetV`.
    #[serde(alias = "offsetV", default)]
    pub offset: f64,

    /// Duty cycle %.  Accepts `duty` or `dutyCycle`.
    #[serde(alias = "dutyCycle", default)]
    pub duty: f64,

    /// Waveform shape.  Accepts uppercase (SINE), lowercase (sine), mixed.
    #[serde(deserialize_with = "waveform_case_insensitive")]
    pub waveform: Waveform,

    /// Duration in milliseconds.
    /// Accepts `duration_ms` (ms) or `durationSeconds` (auto ×1000).
    #[serde(alias = "durationSeconds", deserialize_with = "duration_ms_or_seconds")]
    pub duration_ms: u64,
}

// ── Sequence ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sequence {
    /// Sequence name.  Accepts `sequence_name` (internal) or `name` (Android client).
    #[serde(alias = "name")]
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



// ── Shared serde helpers ─────────────────────────────────────────────────

/// Deserialise `waveform` case-insensitively.
/// Accepts "SINE", "sine", "Sine", "SQUARE", etc.
fn waveform_case_insensitive<'de, D: Deserializer<'de>>(d: D) -> Result<Waveform, D::Error> {
    let s = String::deserialize(d)?;
    Ok(match s.to_uppercase().as_str() {
        "SINE"     => Waveform::Sine,
        "SQUARE"   => Waveform::Square,
        "TRIANGLE" => Waveform::Triangle,
        "PULSE"    => Waveform::Pulse,
        _          => Waveform::Unknown,
    })
}

/// Deserialise duration from either:
///   `duration_ms`     – integer milliseconds  (internal format)
///   `durationSeconds` – float seconds          (Android format, ×1000)
///
/// serde picks this function when `duration_ms` is the field name in the
/// struct; the `#[serde(alias)]` on the field makes it also fire for
/// `durationSeconds`.
fn duration_ms_or_seconds<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    // Try f64 first — covers both integer ms and fractional seconds.
    let v = f64::deserialize(d)?;
    // Heuristic: values ≥ 100_000 are already in milliseconds;
    // values < 100_000 are in seconds (max realistic sequence step = ~86 400 s).
    Ok(if v >= 100_000.0 {
        v.round() as u64
    } else {
        (v * 1_000.0).round() as u64
    })
}

// ── Mobile client wire format (direct sequence object, no envelope) ───────
//
// The Android/iOS client sends a raw Sequence object — NOT wrapped in the
// {"type":"sequence_upload","payload":{…}} envelope the server uses internally.
//
// Field-name differences from the internal `Sequence` type:
//   "name"            → sequence_name
//   "frequencyHz"     → frequency  (Hz, same unit)
//   "amplitudeV"      → amplitude  (V, same unit)
//   "offsetV"         → offset     (V, same unit)
//   "dutyCycle"       → duty       (%, same unit)
//   "durationSeconds" → duration_ms  (× 1000 for conversion)
//   "waveform"        → Waveform   (client sends "SINE" / "SQUARE" — uppercase)
//
// Extra client fields ("phase", "loop", "lastModifiedMs") are silently ignored.

/// Raw sequence block as sent by the mobile client.
#[derive(Debug, Deserialize)]
pub struct ClientBlock {
    pub id:              String,
    pub channel:         u8,
    #[serde(rename = "frequencyHz")]
    pub frequency_hz:    f64,
    #[serde(rename = "amplitudeV")]
    pub amplitude_v:     f64,
    #[serde(rename = "offsetV", default)]
    pub offset_v:        f64,
    #[serde(rename = "dutyCycle", default)]
    pub duty_cycle:      f64,
    #[serde(deserialize_with = "waveform_case_insensitive")]
    pub waveform:        Waveform,
    /// Duration in **seconds** — converted to milliseconds during From impl.
    #[serde(rename = "durationSeconds")]
    pub duration_secs:   f64,
}

/// Raw sequence as sent by the mobile client (no envelope wrapper).
#[derive(Debug, Deserialize)]
pub struct ClientSequence {
    pub name:   String,
    pub blocks: Vec<ClientBlock>,
}

/// Convert the mobile wire format into the server's internal `Sequence` type.
impl From<ClientSequence> for Sequence {
    fn from(cs: ClientSequence) -> Self {
        Sequence {
            sequence_name: cs.name,
            blocks: cs.blocks.into_iter().map(|b| SequenceBlock {
                id:          b.id,
                channel:     b.channel,
                frequency:   b.frequency_hz,
                amplitude:   b.amplitude_v,
                offset:      b.offset_v,
                duty:        b.duty_cycle,
                waveform:    b.waveform,
                duration_ms: (b.duration_secs * 1000.0).round() as u64,
            }).collect(),
        }
    }
}


// ── Mobile client events (WS → UI) ───────────────────────────────────────

/// Events produced by the WebSocket handler and consumed by the egui UI.
/// Delivered via a `std::sync::mpsc::SyncSender` (cloneable, Send, no async).
#[derive(Debug, Clone)]
pub enum MobileEvent {
    /// A new WebSocket client connected.
    ClientConnected,
    /// A WebSocket client disconnected.
    ClientDisconnected,
    /// The client uploaded a sequence; execution starts immediately.
    SequenceReceived { name: String, blocks: usize, total_duration_secs: u64 },
    /// The client sent a control command (start/stop/pause/resume).
    ControlReceived { command: String },
    /// Sequencer started executing a block.
    BlockStarted {
        block_index: usize,
        block_total: usize,
        channel:     u8,
        frequency:   f64,
        waveform:    String,
        amplitude:   f64,
        duration_ms: u64,
    },
    /// All blocks finished.
    SequenceFinished { name: String },
    /// Execution was stopped early.
    SequenceStopped,
}

#[cfg(test)]
mod client_format_tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "name": "My Sequence",
        "blocks": [
            {
                "id": "54dd9d64-d260-458d-bae8-598efefde1f3",
                "channel": 1,
                "frequencyHz": 811008.0,
                "amplitudeV": 20.0,
                "waveform": "SINE",
                "durationSeconds": 1800,
                "phase": 0,
                "offsetV": 0.0,
                "dutyCycle": 50
            },
            {
                "id": "af4cc8c8-500c-4f68-998e-87cadbd36ba5",
                "channel": 1,
                "frequencyHz": 1081344.0,
                "amplitudeV": 20.0,
                "waveform": "SINE",
                "durationSeconds": 1800,
                "phase": 0,
                "offsetV": 0.0,
                "dutyCycle": 50
            }
        ],
        "loop": false,
        "lastModifiedMs": 1773908956199
    }"#;

    #[test]
    fn parse_client_sequence() {
        let cs: ClientSequence = serde_json::from_str(SAMPLE)
            .expect("ClientSequence must parse");
        assert_eq!(cs.name, "My Sequence");
        assert_eq!(cs.blocks.len(), 2);
        assert_eq!(cs.blocks[0].frequency_hz, 811_008.0);
        assert_eq!(cs.blocks[0].amplitude_v, 20.0);
        assert_eq!(cs.blocks[0].duty_cycle, 50.0);
        assert_eq!(cs.blocks[0].waveform, Waveform::Sine);
    }

    #[test]
    fn convert_to_internal_sequence() {
        let cs: ClientSequence = serde_json::from_str(SAMPLE).unwrap();
        let seq: Sequence = cs.into();
        assert_eq!(seq.sequence_name, "My Sequence");
        assert_eq!(seq.blocks.len(), 2);
        // durationSeconds 1800 → duration_ms 1_800_000
        assert_eq!(seq.blocks[0].duration_ms, 1_800_000);
        assert_eq!(seq.blocks[1].frequency, 1_081_344.0);
        assert_eq!(seq.blocks[0].id, "54dd9d64-d260-458d-bae8-598efefde1f3");
    }

    #[test]
    fn waveform_uppercase_sine() {
        let cs: ClientSequence = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(cs.blocks[0].waveform, Waveform::Sine);
    }

    #[test]
    fn extra_fields_ignored() {
        // "loop", "lastModifiedMs", "phase" must not cause parse failure
        let cs: Result<ClientSequence, _> = serde_json::from_str(SAMPLE);
        assert!(cs.is_ok(), "extra fields must be silently ignored");
    }

    /// The client actually sends the full envelope with Android field names inside.
    /// This is the REAL format confirmed by VS Code debugger (string ends with "}}")
    #[test]
    fn parse_full_envelope_with_android_fields() {
        let envelope = r#"{
            "type": "sequence_upload",
            "payload": {
                "name": "My Sequence",
                "blocks": [
                    {
                        "id": "54dd9d64-d260-458d-bae8-598efefde1f3",
                        "channel": 1,
                        "frequencyHz": 811008.0,
                        "amplitudeV": 20.0,
                        "waveform": "SINE",
                        "durationSeconds": 1800,
                        "phase": 0,
                        "offsetV": 0.0,
                        "dutyCycle": 50
                    }
                ],
                "loop": false,
                "lastModifiedMs": 1773908956199
            }
        }"#;

        let msg: IncomingMessage = serde_json::from_str(envelope)
            .expect("full envelope with Android field names must parse");

        match msg {
            IncomingMessage::SequenceUpload(seq) => {
                assert_eq!(seq.sequence_name, "My Sequence");
                assert_eq!(seq.blocks.len(), 1);
                assert_eq!(seq.blocks[0].frequency, 811_008.0);
                assert_eq!(seq.blocks[0].amplitude, 20.0);
                assert_eq!(seq.blocks[0].waveform, Waveform::Sine);
                // durationSeconds: 1800 → duration_ms: 1_800_000
                assert_eq!(seq.blocks[0].duration_ms, 1_800_000);
            }
            _ => panic!("Expected SequenceUpload variant"),
        }
    }
}
