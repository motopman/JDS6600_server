/// JDS6600 App – GUI entry point.
///
/// # Thread model
/// ```text
/// Main Thread
/// ├── tray::create()        must be main thread (macOS / Windows requirement)
/// └── eframe::run_native()  blocks; owns the GUI event loop
///
/// Background OS Thread  ("jds-backend")
/// └── Tokio runtime
///     ├── Axum WebSocket server
///     ├── Sequencer engine  (state machine)
///     ├── Dispatcher        (rate-limited UART queue)
///     └── Watchdog          (initial scan + reconnect loop)
///                           ↑ sends HardwareFound once real device is confirmed
///                             (or after MOCK_FALLBACK_SECS if none found)
/// ```
///
/// # Startup sequence  (no more race condition)
/// 1. Transport starts as MockTransport (safe no-op).
/// 2. Watchdog scans all ports immediately.
/// 3a. JDS6600 found → swap to SerialTransport → send HardwareFound.
/// 3b. Timeout (8 s)  → keep Mock → send HardwareFound (mock mode).
/// Either way, HardwareFound is sent exactly ONCE by the watchdog,
/// after the actual hardware situation is known.

mod tray;
mod ui;

use std::sync::{Arc, Mutex};
use std::thread;

use tokio::sync::{broadcast, mpsc, RwLock};

use jds6600_core::{
    dispatcher::spawn_dispatcher,
    hal::mock::MockTransport,
    models::SharedSnapshot,
    protocol::device_state::DeviceState,
    sequencer::{SequencerEngine, SequencerEvent},
    server::{start_server, ServerState},
    watchdog::run_watchdog,
};

const WS_PORT:        u16   = 8080;
const BROADCAST_CAP:  usize = 32;
const SEQ_QUEUE:      usize = 64;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("jds6600=debug".parse().unwrap()),
        )
        .init();

    tracing::info!("JDS6600 App starting");

    let local_ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| {
            tracing::warn!("Could not determine local IP — using 127.0.0.1");
            "127.0.0.1".to_string()
        });
    let ws_url = format!("ws://{}:{}/ws", local_ip, WS_PORT);
    tracing::info!("WebSocket endpoint: {}", ws_url);

    // ── Shared inter-layer state ──────────────────────────────────────────
    let snapshot: Arc<RwLock<SharedSnapshot>> = Arc::new(RwLock::new(SharedSnapshot {
        hardware_connected: false,
        sequencer_state:    "DISCONNECTED".to_string(),
        ..Default::default()
    }));

    let (status_tx, _)   = broadcast::channel::<String>(BROADCAST_CAP);
    let (seq_tx, seq_rx) = mpsc::channel::<SequencerEvent>(SEQ_QUEUE);

    // ── Background Tokio thread ───────────────────────────────────────────
    {
        let snapshot  = Arc::clone(&snapshot);
        let status_tx = status_tx.clone();
        let seq_tx2   = seq_tx.clone();

        thread::Builder::new()
            .name("jds-backend".into())
            .spawn(move || {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("jds-tokio")
                    .build()
                    .expect("Tokio runtime failed")
                    .block_on(async_main(snapshot, status_tx, seq_tx2, seq_rx));
            })
            .expect("Failed to spawn backend thread");
    }

    // ── System tray (must be created on the main thread) ─────────────────
    let _tray = tray::create();

    // ── GUI event loop (blocks until the user quits) ──────────────────────
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("JDS6600 Remote Server")
            .with_inner_size([440.0, 580.0])
            .with_resizable(true)
            .with_maximize_button(true),
        ..Default::default()
    };

    eframe::run_native(
        "JDS6600 Server",
        options,
        Box::new(move |cc| Box::new(ui::ServerApp::new(cc, ws_url))),
    )
}

// ── Async backend ─────────────────────────────────────────────────────────

async fn async_main(
    snapshot:  Arc<RwLock<SharedSnapshot>>,
    status_tx: broadcast::Sender<String>,
    seq_tx:    mpsc::Sender<SequencerEvent>,
    seq_rx:    mpsc::Receiver<SequencerEvent>,
) {
    // Start with mock transport — watchdog will swap to real hardware if found.
    let transport: Arc<Mutex<Box<dyn jds6600_core::hal::Transport>>> =
        Arc::new(Mutex::new(Box::new(MockTransport::new())));

    let device_state = Arc::new(Mutex::new(DeviceState::new()));

    // Dispatcher — calls on_hardware_error → HardwareLost if a write fails.
    let seq_tx_err = seq_tx.clone();
    let dispatcher = spawn_dispatcher(
        Arc::clone(&transport),
        Arc::clone(&device_state),
        move || {
            let tx = seq_tx_err.clone();
            tokio::spawn(async move {
                let _ = tx.send(SequencerEvent::HardwareLost).await;
            });
        },
    );

    // Sequencer engine.
    let engine = SequencerEngine::new(
        seq_rx,
        dispatcher,
        Arc::clone(&snapshot),
        status_tx.clone(),
    );
    tokio::spawn(engine.run());

    // Watchdog — does the initial scan and sends HardwareFound when ready.
    // No more fake 300 ms timer — the watchdog owns the hardware state lifecycle.
    tokio::spawn(run_watchdog(
        Arc::clone(&transport),
        Arc::clone(&device_state),
        seq_tx.clone(),
    ));

    // OS sleep inhibition (prevents laptop from sleeping during sequences).
    sleep_inhibit::enable();

    // WebSocket server — blocks until process exits.
    start_server(WS_PORT, ServerState {
        status_tx,
        sequencer_tx: seq_tx,
        snapshot,
    })
    .await;
}

// ── OS sleep inhibition ───────────────────────────────────────────────────

mod sleep_inhibit {
    pub fn enable() {
        #[cfg(target_os = "windows")]
        unsafe {
            use winapi::um::winbase::SetThreadExecutionState;
            use winapi::um::winnt::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
            tracing::info!("[SLEEP] SetThreadExecutionState applied");
        }

        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("caffeinate").arg("-i").spawn();
            tracing::info!("[SLEEP] caffeinate launched");
        }

        #[cfg(target_os = "linux")]
        tracing::info!("[SLEEP] Linux: integrate systemd inhibit lock via zbus for production");

        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        tracing::warn!("[SLEEP] Sleep inhibition not implemented for this OS");
    }
}
