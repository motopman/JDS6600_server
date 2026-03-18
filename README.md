# JDS6600 Remote Control Server

Cross-platform Rust server for wireless remote control of the **JDS6600** signal generator from an Android/iOS mobile application.

---

## Architecture overview

```
┌──────────────────────────────────────────────────────────────┐
│  Mobile App (Android / iOS)                                  │
│  WebSocket client  ←──JSON API──►  ws://<LAN-IP>:8080/ws    │
└──────────────────────────────────────────────────────────────┘
                              │
                    ┌─────────▼─────────┐
                    │  jds6600-app      │  GUI binary (Rust ≥ 1.80)
                    │  ┌─────────────┐  │
                    │  │  Tray icon  │  │  Notification-tray icon
                    │  │  QR window  │  │  Right-click → Show QR
                    │  └─────────────┘  │
                    └────────┬──────────┘
                             │ (background OS thread)
                    ┌────────▼──────────────────────────────────┐
                    │  jds6600-core  (Rust ≥ 1.75)              │
                    │                                           │
                    │  WebSocket server (Axum)                  │
                    │       │                                   │
                    │  Sequencer engine  ← state machine        │
                    │       │                                   │
                    │  Dispatcher  ← FIFO + 30ms rate-limit     │
                    │       │                                   │
                    │  HAL Transport                            │
                    │    MockTransport  (dev / no cable)        │
                    │    SerialTransport  (real hardware)       │
                    │       │                                   │
                    │  Watchdog  ← auto-reconnect loop          │
                    └───────┬───────────────────────────────────┘
                            │ USB-CDC  115200 8N1
                    ┌───────▼──────┐
                    │  JDS6600     │
                    └──────────────┘
```

---

## Workspace layout

```
jds6600_server/
├── Cargo.toml                 workspace root
└── crates/
    ├── jds6600-core/          core library  (Rust ≥ 1.75, no GUI)
    │   └── src/
    │       ├── lib.rs
    │       ├── error.rs
    │       ├── models.rs      domain types + JSON API contract
    │       ├── hal/           Hardware Abstraction Layer
    │       │   ├── mod.rs     Transport trait + port discovery
    │       │   ├── mock.rs    software emulator (dev)
    │       │   └── serial.rs  real USB-CDC transport
    │       ├── protocol/
    │       │   ├── commands.rs  ASCII frame encoder (fully tested)
    │       │   └── device_state.rs  write-back cache
    │       ├── dispatcher.rs  rate-limited UART queue
    │       ├── sequencer/
    │       │   └── engine.rs  async state machine
    │       ├── server/
    │       │   ├── mod.rs     Axum server + routes
    │       │   └── ws_handler.rs  WebSocket handler
    │       └── watchdog.rs    auto-reconnect loop
    │
    ├── jds6600-app/           GUI wrapper  (Rust ≥ 1.80)
    │   └── src/
    │       ├── main.rs        thread split + sleep inhibition
    │       ├── ui.rs          egui QR-code window
    │       └── tray.rs        system-tray icon + context menu
    │
    └── jdsctl/                CLI protocol scanner  (Rust ≥ 1.75)
        └── src/main.rs
```

---

## Building

### Prerequisites

```bash
# Rust toolchain (stable)
rustup update stable

# Linux system library required by serialport
sudo apt install libudev-dev pkg-config

# Linux tray icon (for jds6600-app only)
sudo apt install libayatana-appindicator3-dev
# or: sudo apt install libappindicator3-dev
```

### Build the core library + jdsctl (Rust ≥ 1.75)

```bash
cargo build -p jds6600-core
cargo build -p jdsctl --release
```

### Build the full GUI app (Rust ≥ 1.80)

```bash
rustup update stable   # ensure >= 1.80
cargo build -p jds6600-app --release
```

The binary is at `target/release/jds6600-app`.

### Run unit tests

```bash
cargo test -p jds6600-core
```

---

## Usage

### GUI app (`jds6600-app`)

1. Run `./jds6600-app` (or the `.exe` on Windows).
2. A window appears showing the connection **QR code** — the encoded URL is `ws://<your-LAN-IP>:8080/ws`.
3. Scan with the mobile app to connect.
4. Click **Minimize to Tray** — the app continues running as a notification-tray icon.
5. Right-click the tray icon → **Show / Hide** to bring the QR window back.

### Protocol Scanner (`jdsctl`)

Use this to reverse-engineer the JDS6600 before the cable arrives at production, or to verify hardware behaviour:

```bash
# List detected serial ports
jdsctl ports

# Full sweep of all 121 read commands → save to file
jdsctl COM5 sweep > jds6600_dump.txt        # Windows
jdsctl /dev/ttyUSB0 sweep > dump.txt        # Linux

# Send a single raw command
jdsctl COM5 raw ":r23."

# Read a specific function code
jdsctl COM5 read 23

# Set CH1 to 1000 Hz
jdsctl COM5 set-freq 1 1000

# Set CH1 amplitude to 5 V
jdsctl COM5 set-amp 1 5.0

# Enable both outputs
jdsctl COM5 output on
```

---

## WebSocket API contract

### Mobile → Server

#### Upload a sequence

```json
{
  "type": "sequence_upload",
  "payload": {
    "sequence_name": "Resonance_Sweep",
    "blocks": [
      {
        "id": "uuid-1",
        "channel": 1,
        "frequency": 1000.0,
        "amplitude": 5.0,
        "offset": 0.0,
        "duty": 50.0,
        "waveform": "sine",
        "duration_ms": 5000
      }
    ]
  }
}
```

`waveform` values: `"sine"` | `"square"` | `"triangle"` | `"pulse"`

#### Control command

```json
{
  "type": "control_command",
  "payload": { "command": "start" }
}
```

`command` values: `"start"` | `"stop"` | `"pause"` | `"resume"`

---

### Server → Mobile (telemetry push)

Sent on every state change and immediately when a new client connects:

```json
{
  "type": "server_status",
  "payload": {
    "hardware_connected": true,
    "sequencer_state": "RUNNING",
    "active_sequence": "Resonance_Sweep",
    "current_block_id": "uuid-1",
    "time_left_ms": 3450,
    "error_message": null
  }
}
```

`sequencer_state` values: `"DISCONNECTED"` | `"IDLE"` | `"RUNNING"` | `"PAUSED"` | `"ERROR"`

---

## Sequencer state machine

```
DISCONNECTED ──HardwareFound──► IDLE ◄───────────────────────────┐
     ▲                           │                    Stop/finish │
     │                      Start│                               │
     │                           ▼                               │
     │           ┌────────── RUNNING ──────────────┐             │
     │           │               │                 │             │
     │         Pause           Tick             Stop/HW         │
     │           │          (next block            │             │
     │           ▼           or finish)            │             │
     │        PAUSED                               ▼             │
     │           │                              IDLE/ERROR ──────┘
     │      Resume/Stop
     │
     └──── ERROR ──HardwareRecovered──► IDLE (sequence reset)
```

Key properties:
- Mobile client disconnecting does **not** interrupt a running sequence
- Hardware fault during RUNNING triggers an immediate output-disable before entering ERROR
- After hardware recovery the sequence is reset to a known-safe state

---

## Development phases

| Phase | Status | Description |
|-------|--------|-------------|
| 0 – Protocol Scanner | ✅ Ready | `jdsctl sweep` — run when USB cable arrives |
| 1 – HAL + Dispatcher | ✅ Complete | Mock + Serial transport, rate-limited queue |
| 2 – Protocol Layer | ✅ Complete | ASCII encoder with 13 unit tests |
| 3 – Sequencer Engine | ✅ Complete | Full async state machine |
| 4 – WebSocket API | ✅ Complete | Axum server, JSON contract |
| 5 – GUI + Tray | ✅ Complete | egui QR window, OS tray icon |
| 6 – Mobile Integration | 🔲 Next | Hand off JSON contract to mobile developer |
| 7 – Real Hardware | 🔲 Blocked | Awaiting USB cable; swap MockTransport → SerialTransport |

---

## Replacing Mock with real hardware

In `crates/jds6600-app/src/main.rs`, find the comment `// In Mock mode` and replace:

```rust
// FROM (Mock):
Arc::new(Mutex::new(Box::new(MockTransport::new())))

// TO (Real hardware on a known port):
Arc::new(Mutex::new(Box::new(
    jds6600_core::hal::serial::SerialTransport::open("COM5", 500)?
)))
```

Or leave the Mock in place and let the **Watchdog** discover and connect the real device automatically once the cable is plugged in.
