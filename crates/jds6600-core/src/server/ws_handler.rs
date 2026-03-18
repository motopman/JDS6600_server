use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State},
    response::Response,
};
use tokio::sync::broadcast;

use crate::models::{ControlCommand, IncomingMessage, OutgoingMessage};
use crate::sequencer::SequencerEvent;
use crate::server::ServerState;

pub async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<ServerState>) -> Response {
    ws.on_upgrade(move |socket| handle(socket, state))
}

async fn handle(mut socket: WebSocket, state: ServerState) {
    tracing::info!("[WS] Client connected");

    // Greet the client with the current server snapshot.
    if let Some(json) = greeting(&state).await {
        if socket.send(Message::Text(json)).await.is_err() {
            tracing::warn!("[WS] Greeting failed – client gone immediately");
            return;
        }
    }

    let mut status_rx: broadcast::Receiver<String> = state.status_tx.subscribe();

    loop {
        tokio::select! {
            // ── Incoming from mobile ──────────────────────────────────
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => dispatch(&text, &state).await,
                    Some(Ok(Message::Ping(p)))    => { let _ = socket.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("[WS] Client disconnected");
                        break;
                    }
                    Some(Ok(_))   => {} // binary frames ignored
                    Some(Err(e))  => { tracing::warn!("[WS] Recv error: {}", e); break; }
                }
            }

            // ── Status push from sequencer ────────────────────────────
            result = status_rx.recv() => {
                match result {
                    Ok(json) => {
                        if socket.send(Message::Text(json)).await.is_err() {
                            tracing::warn!("[WS] Send failed – client gone");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("[WS] Broadcast lagged by {} messages", n);
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

async fn dispatch(text: &str, state: &ServerState) {
    tracing::debug!("[WS] RX: {}", text);
    match serde_json::from_str::<IncomingMessage>(text) {
        Ok(IncomingMessage::SequenceUpload(seq)) => {
            tracing::info!("[WS] SequenceUpload '{}' ({} blocks)",
                seq.sequence_name, seq.blocks.len());
            send(state, SequencerEvent::SequenceLoaded(seq)).await;
        }
        Ok(IncomingMessage::ControlCommand(ctrl)) => {
            tracing::info!("[WS] Control: {:?}", ctrl.command);
            let ev = match ctrl.command {
                ControlCommand::Start  => SequencerEvent::Start,
                ControlCommand::Stop   => SequencerEvent::Stop,
                ControlCommand::Pause  => SequencerEvent::Pause,
                ControlCommand::Resume => SequencerEvent::Resume,
            };
            send(state, ev).await;
        }
        Err(e) => {
            tracing::warn!("[WS] JSON parse error: {} — raw: {}", e, text);
        }
    }
}

async fn send(state: &ServerState, ev: SequencerEvent) {
    if let Err(e) = state.sequencer_tx.send(ev).await {
        tracing::error!("[WS] Sequencer channel closed: {}", e);
    }
}

async fn greeting(state: &ServerState) -> Option<String> {
    let snap = state.snapshot.read().await;
    serde_json::to_string(&OutgoingMessage::ServerStatus(snap.to_status_payload())).ok()
}
