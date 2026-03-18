pub mod ws_handler;

use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::models::SharedSnapshot;
use crate::sequencer::SequencerEvent;

#[derive(Clone)]
pub struct ServerState {
    pub status_tx:    broadcast::Sender<String>,
    pub sequencer_tx: mpsc::Sender<SequencerEvent>,
    pub snapshot:     Arc<RwLock<SharedSnapshot>>,
}

pub async fn start_server(port: u16, state: ServerState) {
    use axum::{routing::get, Router};
    use tower_http::cors::{Any, CorsLayer};

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/ws",     get(ws_handler::ws_upgrade))
        .route("/health", get(health))
        .with_state(state)
        .layer(cors);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("[SERVER] WebSocket on ws://0.0.0.0:{}/ws", port);

    let listener = tokio::net::TcpListener::bind(addr).await
        .expect("Failed to bind TCP listener");
    axum::serve(listener, app).await
        .expect("Server error");
}

async fn health() -> &'static str { "JDS6600 Server OK" }
