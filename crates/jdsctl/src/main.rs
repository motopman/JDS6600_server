/// jdsctl – JDS6600 Protocol Scanner & Command-Line Interface
///
/// ## Usage
/// ```text
/// jdsctl <PORT> sweep                  Brute-force read codes 0..120, dump to stdout
/// jdsctl <PORT> raw <CMD>              Send raw ASCII command and print response
/// jdsctl <PORT> set-freq <CH> <HZ>     Set channel frequency (Hz)
/// jdsctl <PORT> set-amp  <CH> <V>      Set channel amplitude (Volts)
/// jdsctl <PORT> set-wave <CH> <IDX>    Set waveform index (0=sine 1=sq 2=tri 3=pulse)
/// jdsctl <PORT> set-duty <CH> <%>      Set duty cycle (0..99.9)
/// jdsctl <PORT> output   on|off        Enable or disable both outputs
/// jdsctl <PORT> read     <CODE>        Read single function code
/// jdsctl ports                         List available serial ports
/// ```
///
/// ## Protocol recap
/// Read:  `:rXX.\r\n`          → device replies `:rXX=VALUE.\r\n`
/// Write: `:wXX=VALUE.\r\n`    → device replies `OK\r\n` (unconfirmed)

use std::io;
use std::thread;
use std::time::{Duration, Instant};

const BAUD: u32       = 115_200;
const TIMEOUT_MS: u64 = 500;
const SWEEP_DELAY: u64 = 50;   // ms between commands in sweep mode

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        usage(&args[0]);
        std::process::exit(1);
    }

    // Special sub-command that needs no port.
    if args[1] == "ports" {
        list_ports();
        return;
    }

    if args.len() < 3 {
        usage(&args[0]);
        std::process::exit(1);
    }

    let port_name = &args[1];
    let mode      = &args[2];

    let mut port = open_port(port_name);

    match mode.as_str() {
        "sweep" => sweep(port.as_mut()),

        "raw" => {
            need(&args, 4, "raw <CMD>");
            let mut cmd = args[3].clone();
            if !cmd.ends_with('\n') { cmd.push_str("\r\n"); }
            pretty_probe(port.as_mut(), &cmd);
        }

        "set-freq" => {
            need(&args, 5, "set-freq <CH> <HZ>");
            let ch:  u8  = parse_u8(&args[3],  "channel");
            let hz:  f64 = parse_f64(&args[4], "hz");
            let code = freq_code(ch);
            let raw  = (hz * 100.0).round() as u64;
            let cmd  = format!(":w{:02}={},0.\r\n", code, raw);
            eprintln!("TX: {}", cmd.trim_end());
            pretty_probe(port.as_mut(), &cmd);
        }

        "set-amp" => {
            need(&args, 5, "set-amp <CH> <V>");
            let ch: u8   = parse_u8(&args[3],  "channel");
            let v:  f64  = parse_f64(&args[4], "volts");
            let code = amp_code(ch);
            let raw  = (v * 1000.0).round() as u32;
            let cmd  = format!(":w{:02}={}.\r\n", code, raw);
            eprintln!("TX: {}", cmd.trim_end());
            pretty_probe(port.as_mut(), &cmd);
        }

        "set-wave" => {
            need(&args, 5, "set-wave <CH> <IDX>");
            let ch:  u8 = parse_u8(&args[3], "channel");
            let idx: u8 = parse_u8(&args[4], "index");
            let code = wave_code(ch);
            let cmd  = format!(":w{:02}={}.\r\n", code, idx);
            eprintln!("TX: {}", cmd.trim_end());
            pretty_probe(port.as_mut(), &cmd);
        }

        "set-duty" => {
            need(&args, 5, "set-duty <CH> <%>");
            let ch:  u8  = parse_u8(&args[3],  "channel");
            let pct: f64 = parse_f64(&args[4], "percent");
            let code = duty_code(ch);
            let raw  = (pct * 10.0).round() as u32;
            let cmd  = format!(":w{:02}={}.\r\n", code, raw);
            eprintln!("TX: {}", cmd.trim_end());
            pretty_probe(port.as_mut(), &cmd);
        }

        "output" => {
            need(&args, 4, "output on|off");
            let bits: u8 = match args[3].as_str() {
                "on"  => 3,
                "off" => 0,
                other => { eprintln!("ERROR: expected on|off, got '{}'", other); std::process::exit(1); }
            };
            let cmd = format!(":w20={}.\r\n", bits);
            eprintln!("TX: {}", cmd.trim_end());
            pretty_probe(port.as_mut(), &cmd);
        }

        "read" => {
            need(&args, 4, "read <CODE>");
            let code: u8 = parse_u8(&args[3], "code");
            let cmd  = format!(":r{:02}.\r\n", code);
            eprintln!("TX: {}", cmd.trim_end());
            pretty_probe(port.as_mut(), &cmd);
        }

        unknown => {
            eprintln!("ERROR: unknown sub-command '{}'", unknown);
            usage(&args[0]);
            std::process::exit(1);
        }
    }
}

// ── Sweep ─────────────────────────────────────────────────────────────────

fn sweep(port: &mut dyn serialport::SerialPort) {
    eprintln!("[jdsctl] Sweeping read codes 0..120 – this takes ~{}s",
        (120 * SWEEP_DELAY) / 1000 + 1);

    println!("{:<6} {:<28} {:>8}  {:<36}  {}",
        "CODE", "TX", "TIME_ms", "RX_HEX", "RX_ASCII");
    println!("{}", "-".repeat(100));

    for code in 0u8..=120 {
        let cmd = format!(":r{:02}.\r\n", code);
        if let Some((elapsed, raw, parsed)) = probe_raw(port, &cmd) {
            let hex = hex(&raw);
            println!("{:<6} {:<28} {:>8}  {:<36}  {}",
                code, cmd.trim_end(), elapsed.as_millis(), hex, parsed);
        }
        thread::sleep(Duration::from_millis(SWEEP_DELAY));
    }

    eprintln!("[jdsctl] Sweep complete.");
}

// ── Single-command probe ──────────────────────────────────────────────────

fn pretty_probe(port: &mut dyn serialport::SerialPort, cmd: &str) {
    match probe_raw(port, cmd) {
        Some((elapsed, raw, parsed)) => {
            println!("Time: {}ms  HEX: [{}]  STR: '{}'",
                elapsed.as_millis(), hex(&raw), parsed);
        }
        None => println!("(no response / error)"),
    }
}

fn probe_raw(port: &mut dyn serialport::SerialPort, cmd: &str) -> Option<(Duration, Vec<u8>, String)> {
    let _ = port.clear(serialport::ClearBuffer::All);

    let t0 = Instant::now();
    if let Err(e) = port.write_all(cmd.as_bytes()) {
        eprintln!("TX error: {}", e);
        return None;
    }

    let mut response: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1];

    loop {
        match port.read(&mut buf) {
            Ok(n) if n > 0 => {
                response.push(buf[0]);
                if buf[0] == b'\n' || response.len() > 256 { break; }
            }
            Ok(_)  => continue,
            Err(ref e) if e.kind() == io::ErrorKind::TimedOut => break,
            Err(e) => { eprintln!("RX error: {}", e); break; }
        }
    }

    let elapsed = t0.elapsed();
    let parsed  = String::from_utf8_lossy(&response)
        .trim_end_matches(['\r', '\n'])
        .to_string();

    Some((elapsed, response, parsed))
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn open_port(name: &str) -> Box<dyn serialport::SerialPort> {
    serialport::new(name, BAUD)
        .timeout(Duration::from_millis(TIMEOUT_MS))
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        .open()
        .unwrap_or_else(|e| {
            eprintln!("ERROR: Cannot open '{}': {}", name, e);
            eprintln!("Available ports:");
            list_ports();
            std::process::exit(1);
        })
}

fn list_ports() {
    match serialport::available_ports() {
        Ok(ports) if ports.is_empty() => eprintln!("  (no ports found)"),
        Ok(ports) => {
            for p in ports {
                println!("  {}", p.port_name);
            }
        }
        Err(e) => eprintln!("  (enumeration error: {})", e),
    }
}

fn hex(raw: &[u8]) -> String {
    raw.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ")
}

fn freq_code(ch: u8) -> u8 { if ch == 2 { 24 } else { 23 } }
fn amp_code(ch: u8)  -> u8 { if ch == 2 { 28 } else { 27 } }
fn wave_code(ch: u8) -> u8 { if ch == 2 { 26 } else { 25 } }
fn duty_code(ch: u8) -> u8 { if ch == 2 { 32 } else { 31 } }

fn parse_u8(s: &str, name: &str) -> u8 {
    s.parse::<u8>().unwrap_or_else(|_| {
        eprintln!("ERROR: '{}' must be an integer (got '{}')", name, s);
        std::process::exit(1);
    })
}

fn parse_f64(s: &str, name: &str) -> f64 {
    s.parse::<f64>().unwrap_or_else(|_| {
        eprintln!("ERROR: '{}' must be a number (got '{}')", name, s);
        std::process::exit(1);
    })
}

fn need(args: &[String], n: usize, usage_hint: &str) {
    if args.len() < n {
        eprintln!("ERROR: not enough arguments. Usage: {} {} {}", args[0], args[2], usage_hint);
        std::process::exit(1);
    }
}

fn usage(bin: &str) {
    eprintln!(
r#"jdsctl – JDS6600 Protocol Scanner & CLI

Usage:
  {b} ports                         List available serial ports
  {b} <PORT> sweep                  Brute-force read codes 0..120 (pipe to file)
  {b} <PORT> raw <CMD>              Send raw command
  {b} <PORT> read <CODE>            Read single function code
  {b} <PORT> set-freq <CH> <HZ>     Set frequency (Hz, float)
  {b} <PORT> set-amp  <CH> <V>      Set amplitude (Volts, float)
  {b} <PORT> set-wave <CH> <IDX>    Set waveform (0=sine 1=square 2=tri 3=pulse)
  {b} <PORT> set-duty <CH> <%>      Set duty cycle (0.0..99.9)
  {b} <PORT> output   on|off        Enable/disable both outputs

Examples:
  {b} ports
  {b} COM5 sweep > dump.txt
  {b} /dev/ttyUSB0 raw ":r23."
  {b} COM5 set-freq 1 1000
  {b} COM5 set-amp  1 5.0
  {b} COM5 output on
"#, b = bin);
}
