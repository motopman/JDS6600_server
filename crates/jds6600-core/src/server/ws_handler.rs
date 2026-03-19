use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State},
    response::Response,
};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::dispatcher::DispatcherHandle;
use crate::models::{
    ControlCommand, IncomingMessage, LiveDeviceState, QuickCommandPayload, Waveform,
};
use crate::protocol::commands::GeneratorCommand;
use crate::sequencer::SequencerEvent;
use crate::server::ServerState;

// ── Upgrade ───────────────────────────────────────────────────────────────

pub async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<ServerState>) -> Response {
    ws.on_upgrade(move |socket| handle(socket, state))
}

// ── Per-connection handler ────────────────────────────────────────────────

async fn handle(mut socket: WebSocket, state: ServerState) {
    tracing::info!("[WS] Client connected");

    // Greet with current snapshot + live device state.
    if let Some(json) = greeting(&state).await {
        if socket.send(Message::Text(json)).await.is_err() {
            return;
        }
    }

    let mut status_rx: broadcast::Receiver<String> = state.status_tx.subscribe();

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => dispatch(&text, &state).await,
                    Some(Ok(Message::Ping(p)))    => { let _ = socket.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("[WS] Client disconnected");
                        break;
                    }
                    Some(Ok(_))   => {}
                    Some(Err(e))  => { tracing::warn!("[WS] Recv error: {}", e); break; }
                }
            }
            result = status_rx.recv() => {
                match result {
                    Ok(json) => {
                        if socket.send(Message::Text(json)).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("[WS] Lagged by {} messages", n);
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

// ── Message routing ───────────────────────────────────────────────────────

async fn dispatch(text: &str, state: &ServerState) {
    tracing::debug!("[WS] RX: {}", text);
    match serde_json::from_str::<IncomingMessage>(text) {
        Ok(IncomingMessage::SequenceUpload(seq)) => {
            tracing::info!("[WS] SequenceUpload '{}' ({} blocks)", seq.sequence_name, seq.blocks.len());
            seq_send(state, SequencerEvent::SequenceLoaded(seq)).await;
        }
        Ok(IncomingMessage::ControlCommand(ctrl)) => {
            tracing::info!("[WS] Control: {:?}", ctrl.command);
            let ev = match ctrl.command {
                ControlCommand::Start  => SequencerEvent::Start,
                ControlCommand::Stop   => SequencerEvent::Stop,
                ControlCommand::Pause  => SequencerEvent::Pause,
                ControlCommand::Resume => SequencerEvent::Resume,
            };
            seq_send(state, ev).await;
        }
        Ok(IncomingMessage::QuickCommand(qc)) => {
            tracing::info!("[WS] QuickCommand: {}", qc.action);
            handle_quick_command(qc, &state.dispatcher, &state.live_state).await;
        }
        Err(e) => {
            tracing::warn!("[WS] JSON parse error: {} — raw: {}", e, text);
        }
    }
}

// ── Quick command executor ────────────────────────────────────────────────

async fn handle_quick_command(
    qc:         QuickCommandPayload,
    dispatcher: &DispatcherHandle,
    live_state: &tokio::sync::RwLock<LiveDeviceState>,
) {
    let cmd: Option<GeneratorCommand> = match qc.action.as_str() {
        "set_waveform_sine_ch1"     => Some(GeneratorCommand::SetWaveform { channel: 1, waveform: Waveform::Sine }),
        "set_waveform_square_ch1"   => Some(GeneratorCommand::SetWaveform { channel: 1, waveform: Waveform::Square }),
        "set_waveform_triangle_ch1" => Some(GeneratorCommand::SetWaveform { channel: 1, waveform: Waveform::Triangle }),
        "set_waveform_pulse_ch1"    => Some(GeneratorCommand::SetWaveform { channel: 1, waveform: Waveform::Pulse }),
        "set_waveform_sine_ch2"     => Some(GeneratorCommand::SetWaveform { channel: 2, waveform: Waveform::Sine }),
        "set_waveform_square_ch2"   => Some(GeneratorCommand::SetWaveform { channel: 2, waveform: Waveform::Square }),
        "set_waveform_triangle_ch2" => Some(GeneratorCommand::SetWaveform { channel: 2, waveform: Waveform::Triangle }),
        "set_waveform_pulse_ch2"    => Some(GeneratorCommand::SetWaveform { channel: 2, waveform: Waveform::Pulse }),
        "output_on"  => Some(GeneratorCommand::SetOutputEnable { ch1: true,  ch2: true  }),
        "output_off" => Some(GeneratorCommand::SetOutputEnable { ch1: false, ch2: false }),
        other => {
            tracing::warn!("[WS] Unknown quick command action: {}", other);
            None
        }
    };

    if let Some(cmd) = cmd {
        // Update live state optimistically.
        if let GeneratorCommand::SetWaveform { channel, ref waveform } = cmd {
            let mut ls = live_state.write().await;
            if channel == 1 { ls.ch1.waveform = waveform.clone(); }
            else             { ls.ch2.waveform = waveform.clone(); }
        }
        if let Err(e) = dispatcher.send(cmd).await {
            tracing::error!("[WS] Quick command dispatch failed: {}", e);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

async fn seq_send(state: &ServerState, ev: SequencerEvent) {
    if let Err(e) = state.sequencer_tx.send(ev).await {
        tracing::error!("[WS] Sequencer channel closed: {}", e);
    }
}

/// Greeting includes both the sequencer snapshot and the live device state.
async fn greeting(state: &ServerState) -> Option<String> {
    let snap  = state.snapshot.read().await;
    let live  = state.live_state.read().await;

    #[derive(Serialize)]
    struct Greeting<'a> {
        #[serde(rename = "type")]
        kind:        &'static str,
        payload:     &'a crate::models::StatusPayload,
        live_device: &'a LiveDeviceState,
    }

    let payload = snap.to_status_payload();
    serde_json::to_string(&Greeting {
        kind:        "server_status",
        payload:     &payload,
        live_device: &live,
    }).ok()
}
