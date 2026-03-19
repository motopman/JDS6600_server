/// Protocol Scanner – automatic port discovery + full sweep.
///
/// ## Watchdog coordination
/// The scanner needs exclusive port access.  Before opening any port it
/// signals the watchdog via a `tokio::sync::watch` channel by setting
/// `WatchdogCmd::ScannerActive`.  The watchdog pauses its own polling.
/// When the scan finishes (or on error) the channel is reset to `Idle`.
///
/// ## PermissionDenied handling
/// If `serialport::new(...).open()` returns `ErrorKind::PermissionDenied`
/// (Windows: "Access is denied") we log it and skip – the port is already
/// in use by something else.  We do NOT retry that port.
///
/// ## Protocol correctness
/// Read frame: `:rXX=0.\r\n`  — the `=0.` data field is required by the
/// JDS6600 firmware.  A bare `:rXX.\r\n` is silently discarded.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::models::WatchdogCmd;

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
pub enum LineKind { Info, Data, Found, Skip, Done, Error }

impl ScanLine {
    fn info(s: impl Into<String>)  -> Self { Self { kind: LineKind::Info,  message: s.into() } }
    fn data(s: impl Into<String>)  -> Self { Self { kind: LineKind::Data,  message: s.into() } }
    fn found(s: impl Into<String>) -> Self { Self { kind: LineKind::Found, message: s.into() } }
    fn skip(s: impl Into<String>)  -> Self { Self { kind: LineKind::Skip,  message: s.into() } }
    fn done(s: impl Into<String>)  -> Self { Self { kind: LineKind::Done,  message: s.into() } }
    fn err(s: impl Into<String>)   -> Self { Self { kind: LineKind::Error, message: s.into() } }
}

// ── Entry point ───────────────────────────────────────────────────────────

/// Start a scan.  The watchdog `cmd_tx` is used to signal port ownership.
pub fn start_scan(cmd_tx: watch::Sender<WatchdogCmd>) -> ScanHandle {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("jds-scanner".into())
        .spawn(move || run_scan(tx, cmd_tx))
        .expect("Failed to spawn scanner thread");
    ScanHandle { rx }
}

// ── Constants ─────────────────────────────────────────────────────────────

const BAUD:          u32 = 115_200;
const PROBE_TIMEOUT: u64 = 600;
const SWEEP_TIMEOUT: u64 = 500;
const INTER_CMD_MS:  u64 = 50;

fn log(tx: &mpsc::Sender<ScanLine>, line: ScanLine) { let _ = tx.send(line); }

// ── Main scan ─────────────────────────────────────────────────────────────

fn run_scan(tx: mpsc::Sender<ScanLine>, cmd_tx: watch::Sender<WatchdogCmd>) {
    // Signal watchdog to pause its port access.
    let _ = cmd_tx.send(WatchdogCmd::ScannerActive);
    log(&tx, ScanLine::info("Scanner taking port ownership — watchdog paused."));

    // Small delay so the watchdog's current probe (if any) can finish and
    // release its port handle before we try to open.
    thread::sleep(Duration::from_millis(300));

    let ports = match serialport::available_ports() {
        Ok(p) => p,
        Err(e) => {
            log(&tx, ScanLine::err(format!("Port enumeration error: {}", e)));
            let _ = cmd_tx.send(WatchdogCmd::Idle);
            log(&tx, ScanLine::done("Scan aborted."));
            return;
        }
    };

    if ports.is_empty() {
        log(&tx, ScanLine::err("No serial ports found on this system."));
        log(&tx, ScanLine::err("→ Check USB cable and CH340/CP2102 driver."));
        let _ = cmd_tx.send(WatchdogCmd::Idle);
        log(&tx, ScanLine::done("Scan finished — no ports."));
        return;
    }

    log(&tx, ScanLine::info(format!(
        "Found {} port(s) — probing each for JDS6600 (115200 8N1):", ports.len()
    )));
    for p in &ports {
        log(&tx, ScanLine::info(format!("  • {}", p.port_name)));
    }
    log(&tx, ScanLine::info("Probe command: :r00=0.\\r\\n".to_string()));

    let mut found_any = false;

    for port_info in &ports {
        let port_name = &port_info.port_name;
        log(&tx, ScanLine::info(format!("─── {} ──────────────────────────", port_name)));

        match open_port(port_name, PROBE_TIMEOUT) {
            Err(e) => {
                let detail = if is_permission_denied(&e) {
                    format!("  ✗ Access denied — port is busy (watchdog or another app)\n  \
                             Hint: another process may hold {}. Try again.", port_name)
                } else {
                    format!("  ✗ Cannot open: {:?} — {}", e.kind(), e)
                };
                log(&tx, ScanLine::skip(detail));
                continue;
            }
            Ok(mut port) => {
                match probe_cmd(port.as_mut(), ":r00=0.\r\n", PROBE_TIMEOUT) {
                    Some((elapsed, _raw, ascii)) if !ascii.is_empty() => {
                        log(&tx, ScanLine::found(format!(
                            "  ✓ JDS6600 found! ({} ms)  →  {:?}", elapsed.as_millis(), ascii
                        )));
                        found_any = true;
                        // Drop port cleanly before re-opening for sweep.
                        drop(port);
                        thread::sleep(Duration::from_millis(50));

                        match open_port(port_name, SWEEP_TIMEOUT) {
                            Ok(mut sweep_port) => {
                                run_read_sweep(&tx, port_name, sweep_port.as_mut());
                                run_write_probes(&tx, port_name, sweep_port.as_mut());
                            }
                            Err(e) => {
                                log(&tx, ScanLine::err(format!("  Re-open for sweep failed: {}", e)));
                            }
                        }
                    }
                    Some((elapsed, raw, ascii)) => {
                        log(&tx, ScanLine::skip(format!(
                            "  ✗ No JDS6600 response ({} ms)  hex:[{}]  str:{:?}",
                            elapsed.as_millis(),
                            if raw.is_empty() { "—".into() } else { raw },
                            ascii
                        )));
                    }
                    None => {
                        log(&tx, ScanLine::skip("  ✗ Timeout / IO error".to_string()));
                    }
                }
            }
        }
    }

    if !found_any {
        log(&tx, ScanLine::err("No JDS6600 found on any accessible port."));
        log(&tx, ScanLine::err("→ Check USB cable, power, and CH340G/CP2102 driver."));
    }

    // Always release watchdog, even on error.
    let _ = cmd_tx.send(WatchdogCmd::Idle);
    log(&tx, ScanLine::info("Watchdog port access restored.".to_string()));
    log(&tx, ScanLine::done("══════════ Scan complete ══════════".to_string()));
}

// ── Sweep helpers ─────────────────────────────────────────────────────────

fn run_read_sweep(
    tx: &mpsc::Sender<ScanLine>,
    port_name: &str,
    port: &mut dyn serialport::SerialPort,
) {
    log(tx, ScanLine::info(format!("  Read sweep r00..r120 on {}...", port_name)));
    log(tx, ScanLine::data(
        "  CODE │ ms   │ HEX response                    │ ASCII".to_string()
    ));
    log(tx, ScanLine::data(
        "  ─────┼──────┼─────────────────────────────────┼────────────────────────".to_string()
    ));

    for code in 0u8..=120 {
        let cmd = format!(":r{:02}=0.\r\n", code);
        match probe_cmd(port, &cmd, SWEEP_TIMEOUT) {
            Some((elapsed, raw, ascii)) => {
                log(tx, ScanLine::data(format!(
                    "  {:>4} │ {:>4} │ {:<31} │ {}",
                    code, elapsed.as_millis(),
                    if raw.is_empty() { "(no response)".into() } else { raw },
                    ascii
                )));
            }
            None => {
                log(tx, ScanLine::data(format!("  {:>4} │  ERR │ (IO error)", code)));
            }
        }
        thread::sleep(Duration::from_millis(INTER_CMD_MS));
    }
    log(tx, ScanLine::info(format!("  ✓ Read sweep complete on {}.", port_name)));
}

fn run_write_probes(
    tx: &mpsc::Sender<ScanLine>,
    port_name: &str,
    port: &mut dyn serialport::SerialPort,
) {
    log(tx, ScanLine::info(format!("  Write probes on {}...", port_name)));
    // Safe: sine wave, 1 kHz, 1 Vpp, 0 V bias, outputs off.
    let probes: &[(&str, &str)] = &[
        (":w21=0.\r\n",        "CH1 waveform  = sine    (w21=0)"),
        (":w23=100000,0.\r\n", "CH1 frequency = 1000 Hz (w23)"),
        (":w25=1000.\r\n",     "CH1 amplitude = 1.0 V   (w25, mV)"),
        (":w27=1000.\r\n",     "CH1 bias      = 0 V     (w27=1000)"),
        (":w20=0.\r\n",        "Outputs       = OFF     (w20=0)"),
    ];
    for (cmd, desc) in probes {
        match probe_cmd(port, cmd, SWEEP_TIMEOUT) {
            Some((elapsed, raw, ascii)) => {
                log(tx, ScanLine::data(format!(
                    "  WRITE {:>4}ms │ {:<40} → [{}] {:?}",
                    elapsed.as_millis(), desc, raw, ascii
                )));
            }
            None => {
                log(tx, ScanLine::data(format!("  WRITE  ERR │ {} → timeout", desc)));
            }
        }
        thread::sleep(Duration::from_millis(INTER_CMD_MS));
    }
    log(tx, ScanLine::info(format!("  ✓ Write probes complete on {}.", port_name)));
}

// ── Low-level I/O ─────────────────────────────────────────────────────────

fn open_port(
    name: &str,
    timeout_ms: u64,
) -> Result<Box<dyn serialport::SerialPort>, serialport::Error> {
    tracing::debug!("[SCANNER] Opening port {} ...", name);
    let result = serialport::new(name, BAUD)
        .timeout(Duration::from_millis(timeout_ms))
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        .flow_control(serialport::FlowControl::None)
        .open();

    match &result {
        Ok(_)  => tracing::debug!("[SCANNER] Opened {}", name),
        Err(e) => tracing::debug!("[SCANNER] Failed to open {}: {:?} — {}", name, e.kind(), e),
    }
    result
}

fn is_permission_denied(e: &serialport::Error) -> bool {
    matches!(e.kind(), serialport::ErrorKind::Io(k)
        if k == std::io::ErrorKind::PermissionDenied
            || k == std::io::ErrorKind::WouldBlock)
}

fn probe_cmd(
    port: &mut dyn serialport::SerialPort,
    cmd: &str,
    timeout_ms: u64,
) -> Option<(Duration, String, String)> {
    let _ = port.clear(serialport::ClearBuffer::All);
    let t0 = Instant::now();

    if port.write_all(cmd.as_bytes()).is_err() { return None; }

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

    let hex = response.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
    let ascii = String::from_utf8_lossy(&response)
        .trim_end_matches(['\r', '\n'])
        .trim().to_string();
    Some((t0.elapsed(), hex, ascii))
}
