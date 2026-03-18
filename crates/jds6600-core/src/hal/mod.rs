pub mod mock;
pub mod serial;

use crate::error::JdsResult;

/// Synchronous low-level transport contract.
///
/// Implementations are always wrapped in `Arc<Mutex<Box<dyn Transport>>>` and
/// called from `tokio::task::spawn_blocking` to avoid blocking the async
/// executor.
pub trait Transport: Send + 'static {
    fn write_raw(&mut self, data: &[u8]) -> JdsResult<()>;
    /// Block until `\n`-terminated line or `timeout_ms` elapses.
    fn read_line(&mut self, timeout_ms: u64) -> JdsResult<Option<String>>;
    fn is_connected(&self) -> bool;
    fn name(&self) -> &str;
}

/// Enumerate COM / ttyUSB* ports available on this host.
pub fn list_available_ports() -> Vec<String> {
    match serialport::available_ports() {
        Ok(p) => p.into_iter().map(|p| p.port_name).collect(),
        Err(e) => {
            tracing::warn!("Port enumeration failed: {}", e);
            vec![]
        }
    }
}

/// Probe a single port: open it and send the JDS6600 identification ping.
/// Returns `true` when the device responds.
pub fn probe_jds6600(port_name: &str) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;

    let Ok(mut port) = serialport::new(port_name, 115_200)
        .timeout(Duration::from_millis(400))
        .open()
    else {
        return false;
    };

    let _ = port.clear(serialport::ClearBuffer::All);

    if port.write_all(b":r00.\r\n").is_err() {
        return false;
    }

    let mut buf = [0u8; 64];
    match port.read(&mut buf) {
        Ok(n) if n > 0 => {
            tracing::info!(
                "JDS6600 probe on {}: {:?}",
                port_name,
                String::from_utf8_lossy(&buf[..n]).trim()
            );
            true
        }
        _ => false,
    }
}
