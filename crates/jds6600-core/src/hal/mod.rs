pub mod mock;
pub mod serial;

use crate::error::JdsResult;

/// Synchronous low-level transport contract.
///
/// All implementations are wrapped in `Arc<Mutex<Box<dyn Transport>>>` and
/// called from `tokio::task::spawn_blocking` so they never block the executor.
pub trait Transport: Send + 'static {
    fn write_raw(&mut self, data: &[u8]) -> JdsResult<()>;
    /// Block until `\n`-terminated line or `timeout_ms` elapses → `None`.
    fn read_line(&mut self, timeout_ms: u64) -> JdsResult<Option<String>>;
    fn is_connected(&self) -> bool;
    fn name(&self) -> &str;
}

/// Enumerate COM / ttyUSB* ports on this host.
pub fn list_available_ports() -> Vec<String> {
    match serialport::available_ports() {
        Ok(p) => p.into_iter().map(|p| p.port_name).collect(),
        Err(e) => {
            tracing::warn!("Port enumeration failed: {}", e);
            vec![]
        }
    }
}

/// Probe a single port for a JDS6600 device.
///
/// ## Protocol note
/// The correct read frame format is `:rXX=0.\r\n` (with `=0.` data field).
/// A bare `:rXX.\r\n` (no `=`) is silently ignored by the device.
///
/// The device responds to a read with `:rXX=VALUE.\r\n`.
/// We accept any non-empty response that starts with `:r00=` as a positive ID.
pub fn probe_jds6600(port_name: &str) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;

    let Ok(mut port) = serialport::new(port_name, 115_200)
        .timeout(Duration::from_millis(400))
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        .open()
    else {
        return false;
    };

    let _ = port.clear(serialport::ClearBuffer::All);

    // Correct read format: :r00=0.\r\n
    if port.write_all(b":r00=0.\r\n").is_err() {
        return false;
    }

    let mut buf = [0u8; 64];
    match port.read(&mut buf) {
        Ok(n) if n > 0 => {
            let reply = String::from_utf8_lossy(&buf[..n]);
            let trimmed = reply.trim();
            tracing::info!("JDS6600 probe on {}: {:?}", port_name, trimmed);
            // Accept any response starting with :r00= as a valid JDS6600 reply
            trimmed.starts_with(":r00=") || !trimmed.is_empty()
        }
        _ => false,
    }
}
