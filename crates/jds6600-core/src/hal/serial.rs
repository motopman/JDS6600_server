use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::error::{JdsError, JdsResult};
use crate::hal::Transport;

/// Real serial transport for the physical JDS6600.
/// Settings: 115 200 baud, 8N1 — confirmed by device documentation.
pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    port_name: String,
}

impl SerialTransport {
    pub fn open(port_name: &str, read_timeout_ms: u64) -> JdsResult<Self> {
        let port = serialport::new(port_name, 115_200)
            .timeout(Duration::from_millis(read_timeout_ms))
            .data_bits(serialport::DataBits::Eight)
            .stop_bits(serialport::StopBits::One)
            .parity(serialport::Parity::None)
            .open()?;

        tracing::info!("SerialTransport opened: {}", port_name);
        Ok(Self { port, port_name: port_name.to_string() })
    }
}

impl Transport for SerialTransport {
    fn write_raw(&mut self, data: &[u8]) -> JdsResult<()> {
        let _ = self.port.clear(serialport::ClearBuffer::Input);
        self.port.write_all(data).map_err(JdsError::UartWrite)?;
        tracing::debug!("[SERIAL TX] {}", String::from_utf8_lossy(data).trim_end_matches(['\r', '\n']));
        Ok(())
    }

    fn read_line(&mut self, timeout_ms: u64) -> JdsResult<Option<String>> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut buf: Vec<u8> = Vec::with_capacity(64);
        let mut byte = [0u8; 1];

        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.port.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    if byte[0] == b'\n' {
                        let s = String::from_utf8_lossy(&buf).into_owned();
                        tracing::debug!("[SERIAL RX] {}", s.trim_end_matches(['\r', '\n']));
                        return Ok(Some(s));
                    }
                    if buf.len() > 256 {
                        return Err(JdsError::Protocol("Response too long".into()));
                    }
                }
                Ok(_) => continue,
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok(None),
                Err(e) => return Err(JdsError::Io(e)),
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.port.bytes_to_read().is_ok()
    }

    fn name(&self) -> &str { &self.port_name }
}
