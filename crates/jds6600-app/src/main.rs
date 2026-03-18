/// JDS6600 App – GUI entry point.
///
/// # Thread model
/// ```text
/// Main Thread (OS requirement on macOS)
/// ├── create_tray_icon()      must be main thread on macOS / Windows
/// └── eframe::run_native()    blocks; owns the GUI event loop
///
/// Background OS Thread
/// └── tokio::Runtime::block_on(async_main)
///     ├── WebSocket server  (Axum)
///     ├── Sequencer engine
///     ├── Dispatcher
///     └── Watchdog
/// ```

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

const WS_PORT: u16 = 8080;
const BROADCAST_CAP: usize = 32;
const SEQ_QUEUE: usize = 64;

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
            tracing::warn!("Could not determine local IP – using 127.0.0.1");
            "127.0.0.1".to_string()
        });
    let ws_url = format!("ws://{}:{}/ws", local_ip, WS_PORT);
    tracing::info!("WebSocket endpoint: {}", ws_url);

    // ── Shared state ──────────────────────────────────────────────────────
    let snapshot: Arc<RwLock<SharedSnapshot>> = Arc::new(RwLock::new(SharedSnapshot {
        hardware_connected: false,
        sequencer_state: "DISCONNECTED".to_string(),
        ..Default::default()
    }));

    let (status_tx, _) = broadcast::channel::<String>(BROADCAST_CAP);
    let (seq_tx, seq_rx) = mpsc::channel::<SequencerEvent>(SEQ_QUEUE);

    // ── Background thread ─────────────────────────────────────────────────
    {
        let snapshot  = Arc::clone(&snapshot);
        let status_tx = status_tx.clone();
        let seq_tx2   = seq_tx.clone();

        thread::Builder::new()
            .name("jds-backend".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("jds-tokio")
                    .build()
                    .expect("Tokio runtime failed");

                rt.block_on(async_main(snapshot, status_tx, seq_tx2, seq_rx));
            })
            .expect("Failed to spawn backend thread");
    }

    // ── System tray (must be main thread) ─────────────────────────────────
    let _tray = tray::create();

    // ── GUI event loop (blocks until exit) ────────────────────────────────
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("JDS6600 Remote Server")
            .with_inner_size([400.0, 480.0])
            .with_resizable(false)
            .with_maximize_button(false),
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
    let transport: Arc<Mutex<Box<dyn jds6600_core::hal::Transport>>> =
        Arc::new(Mutex::new(Box::new(MockTransport::new())));

    let device_state = Arc::new(Mutex::new(DeviceState::new()));

    // Dispatcher – notify sequencer on hardware fault.
    let seq_tx_hw = seq_tx.clone();
    let dispatcher = spawn_dispatcher(
        Arc::clone(&transport),
        Arc::clone(&device_state),
        move || {
            let tx = seq_tx_hw.clone();
            tokio::spawn(async move { let _ = tx.send(SequencerEvent::HardwareLost).await; });
        },
    );

    // Sequencer engine.
    let engine = SequencerEngine::new(seq_rx, dispatcher.clone(), Arc::clone(&snapshot), status_tx.clone());
    tokio::spawn(engine.run());

    // In Mock mode: pretend hardware is immediately present.
    {
        let tx = seq_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tracing::info!("[APP] Mock hardware ready → HardwareFound");
            let _ = tx.send(SequencerEvent::HardwareFound).await;
        });
    }

    // Watchdog (no-op for MockTransport; active when real serial is used).
    tokio::spawn(run_watchdog(Arc::clone(&transport), Arc::clone(&device_state), seq_tx.clone()));

    // Sleep inhibition.
    sleep_inhibit::enable();

    // WebSocket server – runs until process exits.
    start_server(WS_PORT, ServerState { status_tx, sequencer_tx: seq_tx, snapshot }).await;
}

// ── OS sleep inhibition ───────────────────────────────────────────────────

mod sleep_inhibit {
    pub fn enable() {
        #[cfg(target_os = "windows")]
        unsafe {
            use winapi::um::winbase::{
                SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, ES_DISPLAY_REQUIRED,
            };
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
            tracing::info!("[SLEEP] SetThreadExecutionState applied");
        }

        #[cfg(target_os = "macos")]
        {
            // `caffeinate -i` launched as child process is a common approach
            // without adding a heavy framework dependency.
            let _ = std::process::Command::new("caffeinate")
                .arg("-i")
                .spawn();
            tracing::info!("[SLEEP] caffeinate launched");
        }

        #[cfg(target_os = "linux")]
        {
            // Systemd inhibit lock via dbus is the correct path; here we
            // just log a reminder – full implementation requires zbus.
            tracing::info!("[SLEEP] Linux sleep inhibition: integrate systemd dbus inhibitor");
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        tracing::warn!("[SLEEP] Sleep inhibition not implemented for this OS");
    }
}
