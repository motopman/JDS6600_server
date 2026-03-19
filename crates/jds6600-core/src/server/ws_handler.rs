use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State},
    response::Response,
};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::dispatcher::DispatcherHandle;
use crate::models::{
    ClientSequence, ControlCommand, IncomingMessage, LiveDeviceState,
    MobileEvent, QuickCommandPayload, Waveform,
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
    let _ = state.mobile_tx.try_send(MobileEvent::ClientConnected);

    // Greet with current snapshot + live device state.
    if let Some(json) = greeting(&state).await {
        if socket.send(Message::Text(json)).await.is_err() {
            let _ = state.mobile_tx.try_send(MobileEvent::ClientDisconnected);
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

    let _ = state.mobile_tx.try_send(MobileEvent::ClientDisconnected);
}

// ── Message routing ───────────────────────────────────────────────────────

async fn dispatch(text: &str, state: &ServerState) {
    tracing::debug!("[WS] RX: {}", text);

    // ── Path 1: tagged envelope {"type":"…","payload":{…}} ───────────────
    match serde_json::from_str::<IncomingMessage>(text) {
        Ok(IncomingMessage::SequenceUpload(seq)) => {
            handle_sequence(seq, state).await;
            return;
        }
        Ok(IncomingMessage::ControlCommand(ctrl)) => {
            handle_control(ctrl, state).await;
            return;
        }
        Ok(IncomingMessage::QuickCommand(qc)) => {
            tracing::info!("[WS] QuickCommand: {}", qc.action);
            handle_quick_command(qc, &state.dispatcher, &state.live_state).await;
            return;
        }
        Err(_) => {} // fall through to Path 2
    }

    // ── Path 2: raw client sequence (no envelope, different field names) ──
    // The mobile app sends the sequence object directly:
    //   { "name": "…", "blocks": [ { "frequencyHz":…, "durationSeconds":… } ] }
    match serde_json::from_str::<ClientSequence>(text) {
        Ok(client_seq) => {
            let seq = client_seq.into();  // ClientSequence → Sequence
            handle_sequence(seq, state).await;
        }
        Err(e) => {
            tracing::warn!("[WS] Could not parse message (tried both formats): {} — raw: {}", e, text);
        }
    }
}

async fn handle_sequence(seq: crate::models::Sequence, state: &ServerState) {
    tracing::info!("[WS] Sequence '{}' ({} blocks) — forwarding to sequencer (auto-start)",
        seq.sequence_name, seq.blocks.len());
    // SequenceReceived MobileEvent is fired by the SequencerEngine after auto-start,
    // along with the accurate total_duration_secs.  No duplicate event here.
    seq_send(state, SequencerEvent::SequenceLoaded(seq)).await;
}

async fn handle_control(ctrl: crate::models::ControlPayload, state: &ServerState) {
    tracing::info!("[WS] Control: {:?}", ctrl.command);
    let cmd_name = format!("{:?}", ctrl.command).to_lowercase();
    let _ = state.mobile_tx.try_send(MobileEvent::ControlReceived { command: cmd_name });
    let ev = match ctrl.command {
        ControlCommand::Start  => SequencerEvent::Start,
        ControlCommand::Stop   => SequencerEvent::Stop,
        ControlCommand::Pause  => SequencerEvent::Pause,
        ControlCommand::Resume => SequencerEvent::Resume,
    };
    seq_send(state, ev).await;
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
