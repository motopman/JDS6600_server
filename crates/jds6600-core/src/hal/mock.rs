use std::thread;
use std::time::Duration;

use crate::error::{JdsError, JdsResult};
use crate::hal::Transport;

/// Software emulation of the JDS6600 serial interface.
/// Used during development before the USB cable arrives, and in unit tests.
pub struct MockTransport {
    name: String,
    latency: Duration,
    connected: bool,
}

impl MockTransport {
    pub fn new() -> Self {
        Self { name: "[MOCK]".into(), latency: Duration::from_millis(25), connected: true }
    }

    pub fn disconnected() -> Self {
        Self { connected: false, ..Self::new() }
    }

    pub fn reconnect(&mut self) {
        self.connected = true;
        tracing::info!("[MOCK] Hardware reconnected");
    }

    pub fn disconnect(&mut self) {
        self.connected = false;
        tracing::warn!("[MOCK] Hardware disconnected");
    }
}

impl Default for MockTransport {
    fn default() -> Self { Self::new() }
}

impl Transport for MockTransport {
    fn write_raw(&mut self, data: &[u8]) -> JdsResult<()> {
        if !self.connected { return Err(JdsError::HardwareNotConnected); }
        tracing::info!("[MOCK TX] {}", String::from_utf8_lossy(data).trim_end_matches(['\r', '\n']));
        Ok(())
    }

    fn read_line(&mut self, timeout_ms: u64) -> JdsResult<Option<String>> {
        if !self.connected { return Err(JdsError::HardwareNotConnected); }
        thread::sleep(self.latency.min(Duration::from_millis(timeout_ms)));
        tracing::debug!("[MOCK RX] OK");
        Ok(Some("OK\r\n".into()))
    }

    fn is_connected(&self) -> bool { self.connected }
    fn name(&self) -> &str { &self.name }
}
