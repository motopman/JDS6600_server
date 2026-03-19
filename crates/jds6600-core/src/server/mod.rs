pub mod ws_handler;

use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch, RwLock};

use crate::dispatcher::DispatcherHandle;
use crate::models::{LiveDeviceState, SharedSnapshot, WatchdogCmd};
use crate::sequencer::SequencerEvent;

#[derive(Clone)]
pub struct ServerState {
    pub status_tx:    broadcast::Sender<String>,
    pub sequencer_tx: mpsc::Sender<SequencerEvent>,
    pub snapshot:     Arc<RwLock<SharedSnapshot>>,
    /// Handle for sending quick one-off commands to the device.
    pub dispatcher:   DispatcherHandle,
    /// Current device register values (updated on connect + quick command).
    pub live_state:   Arc<RwLock<LiveDeviceState>>,
    /// Send `ScannerActive` / `Idle` to coordinate port ownership.
    pub watchdog_cmd: watch::Sender<WatchdogCmd>,
}

pub async fn start_server(port: u16, state: ServerState) {
    use axum::{routing::get, Router};
    use tower_http::cors::{Any, CorsLayer};

    let app = Router::new()
        .route("/ws",     get(ws_handler::ws_upgrade))
        .route("/health", get(health))
        .with_state(state)
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("[SERVER] WebSocket on ws://0.0.0.0:{}/ws", port);
    let listener = tokio::net::TcpListener::bind(addr).await
        .expect("Failed to bind TCP listener");
    axum::serve(listener, app).await.expect("Server error");
}

async fn health() -> &'static str { "JDS6600 Server OK" }
