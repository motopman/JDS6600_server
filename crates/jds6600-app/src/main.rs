// Hide the Windows console window — the tray icon + egui window are enough.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod tray;
mod ui;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch, RwLock};

use jds6600_core::{
    dispatcher::{quick_cmd_channel, spawn_dispatcher},
    hal::mock::MockTransport,
    models::{LiveDeviceState, SharedSnapshot, WatchdogCmd},
    protocol::device_state::DeviceState,
    sequencer::{SequencerEngine, SequencerEvent},
    server::{start_server, ServerState},
    watchdog::run_watchdog,
};

const WS_PORT:       u16   = 8080;
const BROADCAST_CAP: usize = 32;
const SEQ_QUEUE:     usize = 64;

fn main() -> eframe::Result<()> {
    // On Windows (release) the console is hidden; logs go to stderr only
    // in debug builds.  In release the tracing calls are compiled out.
    #[cfg(debug_assertions)]
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("jds6600=debug".parse().unwrap()),
            )
            .init();
    }

    tracing::info!("JDS6600 App starting");

    let local_ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());
    let ws_url = format!("ws://{}:{}/ws", local_ip, WS_PORT);
    tracing::info!("WebSocket endpoint: {}", ws_url);

    // ── Shared state ──────────────────────────────────────────────────────
    let snapshot: Arc<RwLock<SharedSnapshot>> = Arc::new(RwLock::new(SharedSnapshot {
        hardware_connected: false,
        sequencer_state:    "DISCONNECTED".to_string(),
        ..Default::default()
    }));
    let live_state: Arc<RwLock<LiveDeviceState>> = Arc::new(RwLock::new(LiveDeviceState::default()));

    let (status_tx, _)              = broadcast::channel::<String>(BROADCAST_CAP);
    let (seq_tx, seq_rx)            = mpsc::channel::<SequencerEvent>(SEQ_QUEUE);
    let (watchdog_cmd_tx, watchdog_cmd_rx) = watch::channel(WatchdogCmd::Idle);
    // Direct UI→dispatcher bridge (sync sender lives in egui thread).
    let (quick_tx, quick_rx)        = quick_cmd_channel();

    // ── Background Tokio thread ───────────────────────────────────────────
    {
        let snapshot         = Arc::clone(&snapshot);
        let live_state       = Arc::clone(&live_state);
        let status_tx        = status_tx.clone();
        let seq_tx2          = seq_tx.clone();
        let watchdog_cmd_tx2 = watchdog_cmd_tx.clone();

        thread::Builder::new()
            .name("jds-backend".into())
            .spawn(move || {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .build()
                    .expect("Tokio runtime failed")
                    .block_on(async_main(
                        snapshot, live_state, status_tx,
                        seq_tx2, seq_rx,
                        watchdog_cmd_tx2, watchdog_cmd_rx,
                        quick_rx,
                    ));
            })
            .expect("Backend thread failed");
    }

    let _tray = tray::create();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("JDS6600 Remote Server")
            .with_inner_size([460.0, 660.0])
            .with_resizable(true)
            .with_maximize_button(true),
        ..Default::default()
    };

    eframe::run_native(
        "JDS6600 Server",
        options,
        Box::new(move |cc| Box::new(ui::ServerApp::new(
            cc, ws_url, quick_tx,
        ))),
    )
}

// ── Async backend ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn async_main(
    snapshot:        Arc<RwLock<SharedSnapshot>>,
    live_state:      Arc<RwLock<LiveDeviceState>>,
    status_tx:       broadcast::Sender<String>,
    seq_tx:          mpsc::Sender<SequencerEvent>,
    seq_rx:          mpsc::Receiver<SequencerEvent>,
    watchdog_cmd_tx: watch::Sender<WatchdogCmd>,
    watchdog_cmd_rx: watch::Receiver<WatchdogCmd>,
    quick_rx:        jds6600_core::dispatcher::QuickCmdReceiver,
) {
    let transport: Arc<Mutex<Box<dyn jds6600_core::hal::Transport>>> =
        Arc::new(Mutex::new(Box::new(MockTransport::new())));
    let device_state = Arc::new(Mutex::new(DeviceState::new()));

    let seq_tx_err = seq_tx.clone();
    let dispatcher = spawn_dispatcher(
        Arc::clone(&transport),
        Arc::clone(&device_state),
        move || {
            let tx = seq_tx_err.clone();
            tokio::spawn(async move { let _ = tx.send(SequencerEvent::HardwareLost).await; });
        },
    );

    // Bridge: poll the std::sync::mpsc receiver and forward to the async dispatcher.
    // The UI calls quick_tx.try_send() which is instantaneous; this task drains it.
    {
        let disp = dispatcher.clone();
        tokio::spawn(async move {
            loop {
                // Non-blocking drain — try_recv never sleeps.
                while let Ok(cmd) = quick_rx.rx.try_recv() {
                    if let Err(e) = disp.send(cmd).await {
                        tracing::error!("[QUICK] Dispatch failed: {}", e);
                    }
                }
                // Yield to the scheduler between drains.
                tokio::time::sleep(Duration::from_millis(16)).await;
            }
        });
    }

    let engine = SequencerEngine::new(
        seq_rx, dispatcher.clone(), Arc::clone(&snapshot), status_tx.clone(),
    );
    tokio::spawn(engine.run());

    tokio::spawn(run_watchdog(
        Arc::clone(&transport),
        Arc::clone(&device_state),
        seq_tx.clone(),
        watchdog_cmd_rx,
        Arc::clone(&live_state),
    ));

    sleep_inhibit::enable();

    start_server(WS_PORT, ServerState {
        status_tx,
        sequencer_tx: seq_tx,
        snapshot,
        dispatcher,
        live_state,
        watchdog_cmd: watchdog_cmd_tx,
    }).await;
}

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
        { let _ = std::process::Command::new("caffeinate").arg("-i").spawn(); }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        tracing::warn!("[SLEEP] Sleep inhibition not implemented for this OS");
    }
}
