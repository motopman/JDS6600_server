/// Device state reader – queries the JDS6600 for its current register values.
///
/// Called once immediately after hardware is confirmed connected.
/// Runs synchronously (caller must use spawn_blocking).
///
/// ## Read protocol
/// Send `:rXX=0.\r\n`, device replies `:rXX=VALUE.\r\n`
/// Parse VALUE between `=` and `.` at end of response.

use std::time::{Duration, Instant};

use crate::models::{LiveDeviceState, LiveChannelState, Waveform};

const READ_TIMEOUT_MS: u64 = 400;
const INTER_READ_MS:   u64 = 40;

/// Blocking: read the current state of CH1 and CH2 from an open serial port.
/// Returns `None` if communication fails entirely.
pub fn read_device_state(
    port: &mut dyn serialport::SerialPort,
) -> Option<LiveDeviceState> {
    // Function codes from the JDS6600 manual:
    // 21=CH1 waveform, 22=CH2 waveform
    // 23=CH1 freq,     24=CH2 freq
    // 25=CH1 amp,      26=CH2 amp
    // 27=CH1 bias,     28=CH2 bias
    // 29=CH1 duty,     30=CH2 duty

    let waveform1 = read_register(port, 21)?;
    let waveform2 = read_register(port, 22)?;
    let freq1_raw = read_register(port, 23)?;
    let freq2_raw = read_register(port, 24)?;
    let amp1_raw  = read_register(port, 25)?;
    let amp2_raw  = read_register(port, 26)?;
    let bias1_raw = read_register(port, 27)?;
    let bias2_raw = read_register(port, 28)?;
    let duty1_raw = read_register(port, 29)?;
    let duty2_raw = read_register(port, 30)?;

    Some(LiveDeviceState {
        ch1: LiveChannelState {
            waveform:     waveform_from_index(waveform1),
            frequency_hz: freq1_raw as f64 / 100.0,
            amplitude_v:  amp1_raw  as f64 / 1000.0,
            offset_v:     (bias1_raw as f64 - 1000.0) / 100.0,
            duty_percent: duty1_raw as f64 / 10.0,
        },
        ch2: LiveChannelState {
            waveform:     waveform_from_index(waveform2),
            frequency_hz: freq2_raw as f64 / 100.0,
            amplitude_v:  amp2_raw  as f64 / 1000.0,
            offset_v:     (bias2_raw as f64 - 1000.0) / 100.0,
            duty_percent: duty2_raw as f64 / 10.0,
        },
    })
}

/// Send `:rXX=0.\r\n` and parse the integer VALUE from the `:rXX=VALUE.` response.
/// Returns `None` on timeout or parse failure.
fn read_register(port: &mut dyn serialport::SerialPort, code: u8) -> Option<u32> {
    let _ = port.clear(serialport::ClearBuffer::All);

    let cmd = format!(":r{:02}=0.\r\n", code);
    if port.write_all(cmd.as_bytes()).is_err() {
        tracing::warn!("[READER] write failed for code {}", code);
        return None;
    }

    let response = read_line(port, READ_TIMEOUT_MS)?;

    // Response format: `:rXX=VALUE.\r\n` or `:rXX=VALUE,UNIT.\r\n`
    // We want the first integer after the `=`.
    let parsed = parse_first_value(&response, code);
    if parsed.is_none() {
        tracing::debug!("[READER] code {:02}: unparseable response {:?}", code, response.trim());
    }

    // Rate-limit reads so we don't overwhelm the UART buffer.
    std::thread::sleep(Duration::from_millis(INTER_READ_MS));

    parsed
}

/// Read bytes until `\n` or deadline.
fn read_line(port: &mut dyn serialport::SerialPort, timeout_ms: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    let mut byte = [0u8; 1];

    loop {
        if Instant::now() >= deadline { break; }
        match port.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' || buf.len() > 64 { break; }
            }
            Ok(_) => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(_) => break,
        }
    }

    if buf.is_empty() { None } else { Some(String::from_utf8_lossy(&buf).into_owned()) }
}

/// Parse the first integer value from `:rXX=VALUE.` or `:rXX=VALUE,UNIT.`
fn parse_first_value(response: &str, code: u8) -> Option<u32> {
    // Expected prefix: `:rXX=`
    let prefix = format!(":r{:02}=", code);
    let trimmed = response.trim();

    let after_eq = if let Some(p) = trimmed.strip_prefix(&prefix) {
        p
    } else {
        // Some firmware versions return without zero-padding
        let prefix2 = format!(":r{}=", code);
        trimmed.strip_prefix(&prefix2)?
    };

    // Take digits up to `,` or `.`
    let value_str: String = after_eq.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();

    value_str.parse::<u32>().ok()
}

fn waveform_from_index(idx: u32) -> Waveform {
    match idx {
        0 => Waveform::Sine,
        1 => Waveform::Square,
        2 => Waveform::Triangle,
        3 => Waveform::Pulse,
        _ => Waveform::Unknown,
    }
}
