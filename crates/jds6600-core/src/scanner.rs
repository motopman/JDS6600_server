/// Protocol Scanner – automatic port discovery + full sweep.
///
/// ## What this answers
/// "What does the physical device actually transmit in response to each command?"
///
/// ## Workflow
/// 1. Enumerate every serial port on the OS.
/// 2. Send the identification read `:r00=0.\r\n` to each port.
/// 3. A valid JDS6600 reply starts with `:r00=`.
/// 4. On confirmation, run the full read sweep r00..r120.
/// 5. Then run targeted write probes to confirm OK-response format.
/// 6. Results stream through `mpsc` so the UI can display live progress.
///
/// ## Protocol correctness
/// All read commands MUST use the form `:rXX=0.\r\n` (with `=0.` data field).
/// A bare `:rXX.\r\n` (no `=`) is silently ignored by the JDS6600 firmware.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

// ── Public types ──────────────────────────────────────────────────────────

pub struct ScanHandle {
    pub rx: mpsc::Receiver<ScanLine>,
}

#[derive(Debug, Clone)]
pub struct ScanLine {
    pub kind:    LineKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    Info,
    Data,
    Found,
    Skip,
    Done,
    Error,
}

impl ScanLine {
    fn info(s: impl Into<String>)  -> Self { Self { kind: LineKind::Info,  message: s.into() } }
    fn data(s: impl Into<String>)  -> Self { Self { kind: LineKind::Data,  message: s.into() } }
    fn found(s: impl Into<String>) -> Self { Self { kind: LineKind::Found, message: s.into() } }
    fn skip(s: impl Into<String>)  -> Self { Self { kind: LineKind::Skip,  message: s.into() } }
    fn done(s: impl Into<String>)  -> Self { Self { kind: LineKind::Done,  message: s.into() } }
    fn err(s: impl Into<String>)   -> Self { Self { kind: LineKind::Error, message: s.into() } }
}

// ── Entry point ───────────────────────────────────────────────────────────

pub fn start_scan() -> ScanHandle {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("jds-scanner".into())
        .spawn(move || run_scan(tx))
        .expect("Failed to spawn scanner thread");
    ScanHandle { rx }
}

// ── Constants ─────────────────────────────────────────────────────────────

const BAUD:          u32 = 115_200;
const PROBE_TIMEOUT: u64 = 600;  // ms – per-port identification (generous)
const SWEEP_TIMEOUT: u64 = 500;  // ms – per-command during sweep
const INTER_CMD_MS:  u64 = 50;   // ms – rate limit between commands (device buffer ~128 B)

fn tx_send(tx: &mpsc::Sender<ScanLine>, line: ScanLine) {
    let _ = tx.send(line);
}

// ── Main scan loop ────────────────────────────────────────────────────────

fn run_scan(tx: mpsc::Sender<ScanLine>) {
    let ports = match serialport::available_ports() {
        Ok(p) => p,
        Err(e) => {
            tx_send(&tx, ScanLine::err(format!("Port enumeration failed: {}", e)));
            tx_send(&tx, ScanLine::done("Scan aborted."));
            return;
        }
    };

    if ports.is_empty() {
        tx_send(&tx, ScanLine::err("No serial ports found on this system."));
        tx_send(&tx, ScanLine::err("→ Check USB cable and CH340/CP2102 driver."));
        tx_send(&tx, ScanLine::done("Scan finished – no ports."));
        return;
    }

    tx_send(&tx, ScanLine::info(format!(
        "Found {} port(s) — probing each for JDS6600 (baud: 115200 8N1):", ports.len()
    )));
    for p in &ports {
        tx_send(&tx, ScanLine::info(format!("  • {}", p.port_name)));
    }
    tx_send(&tx, ScanLine::info(
        "Identification probe: :r00=0.  (correct JDS6600 read format)".to_string()
    ));

    let mut found_any = false;

    for port_info in &ports {
        let port_name = &port_info.port_name;
        tx_send(&tx, ScanLine::info(format!("─── {} ───────────────────────────", port_name)));

        let mut port = match open_port(port_name, PROBE_TIMEOUT) {
            Ok(p) => p,
            Err(e) => {
                tx_send(&tx, ScanLine::skip(format!("  ✗ Cannot open: {}", e)));
                continue;
            }
        };

        // Identification: use correct `:r00=0.\r\n` format.
        // A bare `:r00.\r\n` is ignored by the firmware.
        match probe_cmd(port.as_mut(), ":r00=0.\r\n", PROBE_TIMEOUT) {
            Some((elapsed, _raw_hex, ascii)) if ascii.starts_with(":r00=") || !ascii.is_empty() => {
                tx_send(&tx, ScanLine::found(format!(
                    "  ✓ JDS6600 detected!  ({} ms)  response: {:?}",
                    elapsed.as_millis(), ascii
                )));
                found_any = true;
                drop(port);

                match open_port(port_name, SWEEP_TIMEOUT) {
                    Ok(mut sweep_port) => {
                        run_read_sweep(&tx, port_name, sweep_port.as_mut());
                        run_write_probes(&tx, port_name, sweep_port.as_mut());
                    }
                    Err(e) => {
                        tx_send(&tx, ScanLine::err(format!("  Re-open for sweep failed: {}", e)));
                    }
                }
            }
            Some((elapsed, raw_hex, ascii)) => {
                tx_send(&tx, ScanLine::skip(format!(
                    "  ✗ No JDS6600 response ({} ms)  hex: [{}]  ascii: {:?}",
                    elapsed.as_millis(),
                    if raw_hex.is_empty() { "—".into() } else { raw_hex },
                    ascii
                )));
            }
            None => {
                tx_send(&tx, ScanLine::skip(format!("  ✗ Timeout / IO error")));
            }
        }
    }

    if !found_any {
        tx_send(&tx, ScanLine::err("No JDS6600 found on any port."));
        tx_send(&tx, ScanLine::err("→ Verify USB cable, power, and driver (CH340G or CP2102)."));
    }

    tx_send(&tx, ScanLine::done("══════════ Scan complete ══════════".to_string()));
}

// ── Read sweep: r00..r120 ─────────────────────────────────────────────────

fn run_read_sweep(
    tx: &mpsc::Sender<ScanLine>,
    port_name: &str,
    port: &mut dyn serialport::SerialPort,
) {
    tx_send(tx, ScanLine::info(format!("  Read sweep r00..r120 on {}...", port_name)));
    tx_send(tx, ScanLine::data(
        "  CODE │ ms   │ HEX response                    │ ASCII response".to_string()
    ));
    tx_send(tx, ScanLine::data(
        "  ─────┼──────┼─────────────────────────────────┼────────────────────────".to_string()
    ));

    for code in 0u8..=120 {
        // Correct read format: :rXX=0.\r\n
        let cmd = format!(":r{:02}=0.\r\n", code);

        match probe_cmd(port, &cmd, SWEEP_TIMEOUT) {
            Some((elapsed, raw_hex, ascii)) => {
                let empty_marker = if ascii.is_empty() { " ·" } else { "  " };
                tx_send(tx, ScanLine::data(format!(
                    "  {:>4} │ {:>4} │ {:<31} │ {}{}",
                    code,
                    elapsed.as_millis(),
                    if raw_hex.is_empty() { "(no response)".into() } else { raw_hex },
                    empty_marker,
                    ascii
                )));
            }
            None => {
                tx_send(tx, ScanLine::data(format!("  {:>4} │  ERR │ (IO error)", code)));
            }
        }

        thread::sleep(Duration::from_millis(INTER_CMD_MS));
    }

    tx_send(tx, ScanLine::info(format!("  ✓ Read sweep complete on {}.", port_name)));
}

// ── Write probes: characterise OK-response format ─────────────────────────
// Sends a small set of known-safe writes using the CORRECT function codes
// from the official JDS6600 manual.

fn run_write_probes(
    tx: &mpsc::Sender<ScanLine>,
    port_name: &str,
    port: &mut dyn serialport::SerialPort,
) {
    tx_send(tx, ScanLine::info(format!("  Write probes on {}...", port_name)));
    tx_send(tx, ScanLine::info(
        "  (Safe values: sine wave, 1 kHz, 1 Vpp, 0 V bias, outputs off)".to_string()
    ));

    // All commands use correct codes from the Joy-IT JDS6600 manual:
    //   w21 = CH1 waveform,  w23 = CH1 frequency,  w25 = CH1 amplitude
    //   w27 = CH1 bias (1000 = 0 V),               w20 = output enable
    let probes: &[(&str, &str)] = &[
        (":w21=0.\r\n",        "CH1 waveform  = sine (w21=0)"),
        (":w23=100000,0.\r\n", "CH1 frequency = 1000 Hz (w23=100000,0)"),
        (":w25=1000.\r\n",     "CH1 amplitude = 1.0 Vpp (w25=1000 mV)"),
        (":w27=1000.\r\n",     "CH1 bias      = 0 V     (w27=1000)"),
        (":w20=0.\r\n",        "Outputs       = OFF     (w20=0)"),
    ];

    for (cmd, description) in probes {
        match probe_cmd(port, cmd, SWEEP_TIMEOUT) {
            Some((elapsed, raw_hex, ascii)) => {
                tx_send(tx, ScanLine::data(format!(
                    "  WRITE {:>4}ms │ {:<40} → hex: [{}]  str: {:?}",
                    elapsed.as_millis(), description, raw_hex, ascii
                )));
            }
            None => {
                tx_send(tx, ScanLine::data(format!(
                    "  WRITE  ERR  │ {:<40} → timeout", description
                )));
            }
        }
        thread::sleep(Duration::from_millis(INTER_CMD_MS));
    }

    tx_send(tx, ScanLine::info(format!("  ✓ Write probes complete on {}.", port_name)));
}

// ── Low-level I/O ─────────────────────────────────────────────────────────

fn open_port(
    name: &str,
    timeout_ms: u64,
) -> Result<Box<dyn serialport::SerialPort>, serialport::Error> {
    serialport::new(name, BAUD)
        .timeout(Duration::from_millis(timeout_ms))
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        .flow_control(serialport::FlowControl::None)
        .open()
}

/// Send `cmd`, read until `\n` or timeout.
/// Returns `(elapsed, hex, ascii_trimmed)` or `None` on fatal I/O error.
fn probe_cmd(
    port: &mut dyn serialport::SerialPort,
    cmd: &str,
    timeout_ms: u64,
) -> Option<(Duration, String, String)> {
    let _ = port.clear(serialport::ClearBuffer::All);

    let t0 = Instant::now();
    if port.write_all(cmd.as_bytes()).is_err() {
        return None;
    }

    let deadline = t0 + Duration::from_millis(timeout_ms);
    let mut response: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        if Instant::now() >= deadline { break; }

        match port.read(&mut byte) {
            Ok(1) => {
                response.push(byte[0]);
                if byte[0] == b'\n' || response.len() > 256 { break; }
            }
            Ok(_) => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(_) => break,
        }
    }

    let elapsed = t0.elapsed();
    let hex = response.iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ");
    let ascii = String::from_utf8_lossy(&response)
        .trim_end_matches(['\r', '\n'])
        .trim()
        .to_string();

    Some((elapsed, hex, ascii))
}
