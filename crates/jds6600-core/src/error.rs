use thiserror::Error;

#[derive(Debug, Error)]
pub enum JdsError {
    #[error("Hardware not connected")]
    HardwareNotConnected,

    #[error("Serial port error: {0}")]
    SerialPort(#[from] serialport::Error),

    #[error("UART write error: {0}")]
    UartWrite(std::io::Error),

    #[error("UART read timeout after {timeout_ms}ms on port {port}")]
    UartReadTimeout { port: String, timeout_ms: u64 },

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Unknown waveform type: {0}")]
    UnknownWaveform(String),

    #[error("Value out of bounds: {field} = {value} (allowed {min}..{max})")]
    OutOfBounds { field: String, value: f64, min: f64, max: f64 },

    #[error("Sequencer event rejected: {reason}")]
    InvalidTransition { reason: String },

    #[error("Empty sequence – nothing to execute")]
    EmptySequence,

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type JdsResult<T> = Result<T, JdsError>;
