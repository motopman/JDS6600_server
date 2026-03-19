//! egui application window.
//!
//! Layout:
//!  ┌─────────────────────────────────────────────┐
//!  │  QR code + ws:// URL + status dot + Minimize│
//!  ├─────────────────────────────────────────────┤
//!  │  CHANNEL  [● CH 1]  [  CH 2  ]              │
//!  │  OUTPUT   [● ON  ]  [  OFF   ]              │
//!  │  WAVEFORM [● Sine]  [  Sq.   ]              │
//!  │  FREQUENCY                                  │
//!  │  [583.680] [811.008] [854.016]              │
//!  │  [654.336] [758.784] [872.448]              │
//!  │            [1081.344]                       │
//!  ├─────────────────────────────────────────────┤
//!  │  ── Sent Commands ──────────────────────    │
//!  │  14:23:01  CH1  Sine   583.680 kHz  ▶ ON   │
//!  │  14:23:05  CH2  Square  ─              ─   │
//!  └─────────────────────────────────────────────┘
//!
//! Highlighting rules
//! ─────────────────
//! A button is highlighted (filled) when the last command sent for that
//! channel matches that value.  Channel and Output buttons reflect global
//! state.  State is never read from hardware – only what WE sent is tracked.

use std::time::Duration;

use eframe::egui::{self, ColorImage, FontId, RichText, TextureHandle, TextureOptions};
use qrcode::QrCode;

use jds6600_core::{
    dispatcher::QuickCmdSender,
    models::{MobileEvent, Waveform},
    protocol::commands::GeneratorCommand,
};

use crate::tray::{TrayController, TrayEvent};

// ── Constants ─────────────────────────────────────────────────────────────

const AMPLITUDE_V: f64 = 20.0;   // always sent with waveform commands

/// The seven selectable frequencies (Hz) in exact display order.
const FREQUENCIES: &[f64] = &[
    583_680.0,   // idx 0  red
    811_008.0,   // idx 1  orange
    854_016.0,   // idx 2  yellow
    1_081_344.0, // idx 3  green
    654_336.0,   // idx 4  cyan
    758_784.0,   // idx 5  blue
    872_448.0,   // idx 6  violet
];

/// Rainbow fill colour for each frequency button (same index as FREQUENCIES).
const FREQ_COLORS: &[egui::Color32] = &[
    egui::Color32::from_rgb(200,  40,  40),  // red
    egui::Color32::from_rgb(220, 120,  20),  // orange
    egui::Color32::from_rgb(190, 180,  10),  // yellow
    egui::Color32::from_rgb( 30, 160,  50),  // green
    egui::Color32::from_rgb( 20, 170, 180),  // cyan
    egui::Color32::from_rgb( 40,  80, 200),  // blue
    egui::Color32::from_rgb(130,  30, 200),  // violet
];

// ── Per-channel state ─────────────────────────────────────────────────────

/// What the user last *sent* to a channel.  None = nothing sent yet.
#[derive(Default, Clone)]
struct ChannelSent {
    waveform:  Option<Waveform>,
    frequency: Option<f64>,
}

// ── App state ─────────────────────────────────────────────────────────────

pub struct ServerApp {
    ws_url:      String,
    qr_texture:  Option<TextureHandle>,
    /// Current window visibility state — source of truth for eframe.
    visible:     bool,
    tray:        TrayController,
    quick_tx:    QuickCmdSender,

    // Control state (what we last sent)
    selected_channel: u8,       // 1 or 2 — which channel the waveform/freq buttons act on
    sent: [ChannelSent; 2],     // index 0 = CH1, index 1 = CH2
    output: [Option<bool>; 2],  // per-channel; index 0 = CH1, index 1 = CH2

    // Mobile client state
    mobile_rx:      std::sync::mpsc::Receiver<MobileEvent>,
    client_count:   u32,
    mobile_log:     Vec<String>,   // events from mobile
    // Hardware connection status (driven by watchdog)
    hw_connected:   bool,
    hw_detail:      String,        // "COM4" or "scanning 3 ports…"
    // Command log
    log: Vec<String>,
    /// Tracks the previous visibility so we only send Focus/label on transitions.
    prev_visible: bool,
}

impl ServerApp {
    pub fn new(
        _cc:      &eframe::CreationContext<'_>,
        ws_url:   String,
        tray:     TrayController,
        quick_tx: QuickCmdSender,
        mobile_rx: std::sync::mpsc::Receiver<MobileEvent>,
    ) -> Self {
        Self {
            ws_url,
            qr_texture:       None,
            visible:          true,
            tray,
            quick_tx,
            selected_channel: 1,
            sent:             [ChannelSent::default(), ChannelSent::default()],
            output:           [None, None],
            mobile_rx,
            client_count:     0,
            mobile_log:       Vec::new(),
            hw_connected:     false,
            hw_detail:        "searching for generator…".into(),
            log:              Vec::new(),
            prev_visible:     true,
        }
    }

    // ── Command helpers ───────────────────────────────────────────────────

    /// Index into `self.sent` for the currently selected channel.
    fn ch_idx(&self) -> usize { (self.selected_channel - 1) as usize }

    /// Send one or more commands and append a log line.
    fn send(&mut self, cmds: &[GeneratorCommand], description: impl Into<String>) {
        for cmd in cmds {
            self.quick_tx.try_send(cmd.clone());
        }
        let ts = local_time();
        self.log.push(format!("{ts}  {}", description.into()));
    }

    fn send_waveform(&mut self, wf: Waveform) {
        let ch = self.selected_channel;
        let cmds = vec![
            GeneratorCommand::SetWaveform  { channel: ch, waveform: wf.clone() },
            GeneratorCommand::SetAmplitude { channel: ch, volts: AMPLITUDE_V },
        ];
        let desc = format!("CH{}  {}  20.0 V", ch, wf_label(&wf));
        self.send(&cmds, desc);
        self.sent[self.ch_idx()].waveform = Some(wf);
    }

    fn send_frequency(&mut self, hz: f64) {
        let ch = self.selected_channel;
        let cmds = vec![GeneratorCommand::SetFrequency { channel: ch, hz }];
        let desc = format!("CH{}  {}", ch, fmt_freq(hz));
        self.send(&cmds, desc);
        self.sent[self.ch_idx()].frequency = Some(hz);
    }

    fn send_output(&mut self, on: bool) {
        let idx = self.ch_idx();
        self.output[idx] = Some(on);
        // Build bitmask from both channels' known state (None treated as off).
        let ch1_on = self.output[0].unwrap_or(false);
        let ch2_on = self.output[1].unwrap_or(false);
        let cmds = vec![GeneratorCommand::SetOutputEnable { ch1: ch1_on, ch2: ch2_on }];
        let desc = format!("CH{}  {}", self.selected_channel,
            if on { "▶ ON" } else { "■ OFF" });
        self.send(&cmds, desc);
    }

    /// Drain all pending mobile events and update client_count / mobile_log.
    fn drain_mobile(&mut self) {
        while let Ok(ev) = self.mobile_rx.try_recv() {
            let ts = local_time();
            match ev {
                MobileEvent::ClientConnected => {
                    self.client_count = self.client_count.saturating_add(1);
                    self.mobile_log.push(format!("{ts}  📱 Mobile client connected  (active: {})", self.client_count));
                }
                MobileEvent::ClientDisconnected => {
                    self.client_count = self.client_count.saturating_sub(1);
                    self.mobile_log.push(format!("{ts}  📴 Mobile client disconnected  (active: {})", self.client_count));
                }
                MobileEvent::SequenceReceived { name, blocks, total_duration_secs } => {
                    let h = total_duration_secs / 3600;
                    let m = (total_duration_secs % 3600) / 60;
                    let s = total_duration_secs % 60;
                    let dur = if h > 0 { format!("{}h {:02}m {:02}s", h, m, s) }
                              else if m > 0 { format!("{}m {:02}s", m, s) }
                              else { format!("{}s", s) };
                    self.mobile_log.push(format!("{ts}  📥 Sequence [{name}]  {blocks} blocks  total: {dur}  ▶ starting"));
                    self.log.push(format!("{ts}  ▶▶ SEQUENCE START: [{name}]  ({blocks} blocks, {dur})"));
                }
                MobileEvent::BlockStarted { block_index, block_total, channel, frequency, waveform, amplitude, duration_ms } => {
                    let freq_label = if frequency >= 1_000_000.0 {
                        format!("{:.3} MHz", frequency / 1_000_000.0)
                    } else {
                        format!("{:.3} kHz", frequency / 1_000.0)
                    };
                    let dur_s = duration_ms / 1000;

                    // ── Update mobile progress log ─────────────────────────
                    self.mobile_log.push(format!(
                        "{ts}  ⚡ Block {}/{} — CH{} {} {} {:.1}V  ({}s)",
                        block_index + 1, block_total, channel,
                        freq_label, waveform, amplitude, dur_s
                    ));

                    // ── Mirror into the Sent Commands log ──────────────────
                    // Each entry matches exactly what a manual button press
                    // would produce, so the bottom panel shows the full picture.
                    let ch_idx = (channel as usize).saturating_sub(1).min(1);

                    // Parse waveform string back to enum for button highlight
                    let wf = match waveform.to_uppercase().as_str() {
                        "SINE"     => Some(jds6600_core::models::Waveform::Sine),
                        "SQUARE"   => Some(jds6600_core::models::Waveform::Square),
                        "TRIANGLE" => Some(jds6600_core::models::Waveform::Triangle),
                        "PULSE"    => Some(jds6600_core::models::Waveform::Pulse),
                        _          => None,
                    };

                    // Update button highlight state so the UI reflects
                    // what the sequencer is actively playing.
                    self.selected_channel = channel;
                    if let Some(ref w) = wf {
                        self.sent[ch_idx].waveform = Some(w.clone());
                    }
                    self.sent[ch_idx].frequency  = Some(frequency);
                    self.output[ch_idx]          = Some(true);

                    // Sent Commands log entries (same format as manual buttons)
                    if let Some(ref w) = wf {
                        self.log.push(format!("{ts}  [SEQ {}/{}] CH{}  {}  {:.1} V",
                            block_index + 1, block_total, channel,
                            wf_label(w), amplitude));
                    }
                    self.log.push(format!("{ts}  [SEQ {}/{}] CH{}  {}",
                        block_index + 1, block_total, channel, freq_label));
                    self.log.push(format!("{ts}  [SEQ {}/{}] CH{}  ▶ ON  ({}s)",
                        block_index + 1, block_total, channel, dur_s));

                    if self.log.len() > 500 { self.log.remove(0); }
                }
                MobileEvent::SequenceFinished { name } => {
                    self.mobile_log.push(format!("{ts}  ✅ Sequence [{name}] complete"));
                    self.log.push(format!("{ts}  ■■ SEQUENCE DONE: [{name}]"));
                    // Clear output highlights — generator outputs disabled
                    self.output = [Some(false), Some(false)];
                }
                MobileEvent::SequenceStopped => {
                    self.mobile_log.push(format!("{ts}  ⏹ Sequence stopped"));
                    self.log.push(format!("{ts}  ■■ SEQUENCE STOPPED"));
                    self.output = [Some(false), Some(false)];
                }
                MobileEvent::ControlReceived { command } => {
                    self.mobile_log.push(format!("{ts}  🎮 Control: {command}"));
                }
                MobileEvent::HardwareStatus { connected, detail } => {
                    self.hw_connected = connected;
                    self.hw_detail    = detail.clone();
                    if connected {
                        self.mobile_log.push(format!("{ts}  ✅ Generator connected on {detail}"));
                    } else if detail.contains("lost") {
                        // Loss is alarming — write to both logs
                        self.mobile_log.push(format!("{ts}  ⚠️  Generator LOST: {detail}"));
                        self.log.push(format!("{ts}  ⚠️  GENERATOR DISCONNECTED — commands halted"));
                    }
                    // "scanning…" messages are silent — no log entry
                }
                MobileEvent::DeviceResponse { sent, reply } => {
                    // Show in the Sent Commands log alongside the outgoing command.
                    // Format:  TX: :w21=0.    RX: :ok
                    self.log.push(format!("{ts}  TX: {sent}"));
                    self.log.push(format!("{ts}  RX: {reply}"));
                    if self.log.len() > 500 { self.log.drain(0..50); }
                }
            }
            // Cap log at 300 lines.
            if self.mobile_log.len() > 300 {
                self.mobile_log.remove(0);
            }
        }
    }
}

// ── App trait ─────────────────────────────────────────────────────────────

impl eframe::App for ServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── 1. Drain mobile events ───────────────────────────────────────
        self.drain_mobile();

        // ── 2. Tray events ────────────────────────────────────────────────
        while let Some(ev) = self.tray.try_recv() {
            match ev {
                TrayEvent::Toggle => { self.visible = !self.visible; }
                TrayEvent::Quit   => { std::process::exit(0); }
            }
        }

        // ── 3. Window ✕ → hide to tray ───────────────────────────────────
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.visible = false;
        }

        // ── 4. Sync viewport visibility ───────────────────────────────────
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(self.visible));

        // ── 5. Update tray label on state change ──────────────────────────
        if self.visible != self.prev_visible {
            self.tray.update_label(self.visible);
            self.prev_visible = self.visible;
        }

        // ── 6. When hidden: keep the event loop alive, skip rendering ─────
        if !self.visible {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }

        // ── 7. Render ─────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    render_header(ui, ctx, self);
                    sep(ui);
                    render_mobile_status(ui, self);
                    sep(ui);
                    render_controls(ui, self);
                    sep(ui);
                    render_log(ui, self);
                });
        });
    }
}

// ── Section 1: Header ─────────────────────────────────────────────────────

fn render_header(ui: &mut egui::Ui, ctx: &egui::Context, app: &mut ServerApp) {
    ui.vertical_centered(|ui| {
        ui.add_space(10.0);
        ui.label(RichText::new("JDS6600 Remote Server").size(18.0).strong());
        ui.add_space(2.0);
        ui.label(RichText::new("Scan to connect the mobile app:")
            .color(egui::Color32::GRAY).size(12.0));
        ui.add_space(8.0);

        let tex = app.qr_texture.get_or_insert_with(|| build_qr(ctx, &app.ws_url));
        ui.add(egui::Image::new((tex.id(), egui::vec2(200.0, 200.0))));
        ui.add_space(6.0);

        ui.monospace(&app.ws_url);
        ui.add_space(8.0);

        // ── Hardware connection status ────────────────────────────────────
        let (dot, color, hw_msg) = if app.hw_connected {
            ("●", egui::Color32::from_rgb(55, 210, 55),
             format!("Generator connected — {}", app.hw_detail))
        } else if app.hw_detail.contains("lost") {
            ("✗", egui::Color32::from_rgb(220, 60, 60),
             format!("Generator DISCONNECTED — {}", app.hw_detail))
        } else {
            ("◌", egui::Color32::from_rgb(220, 160, 30),
             format!("Searching… {}", app.hw_detail))
        };
        ui.label(RichText::new(format!("{dot}  {hw_msg}")).strong().color(color));
        ui.add_space(6.0);
        if ui.button("  Minimize to Tray  ").clicked() { app.visible = false; }
        ui.add_space(4.0);
    });
}


// ── Mobile connection status + event log ──────────────────────────────────

fn render_mobile_status(ui: &mut egui::Ui, app: &ServerApp) {
    // ── Connection badge ──────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        let (dot, color, label) = if app.client_count > 0 {
            ("●", egui::Color32::from_rgb(55, 210, 55),
             format!("  {} mobile client{} connected",
                app.client_count,
                if app.client_count == 1 { "" } else { "s" }))
        } else {
            ("○", egui::Color32::GRAY, "  No mobile clients".to_string())
        };
        ui.label(RichText::new(dot).color(color).size(14.0).strong());
        ui.label(RichText::new(label).size(13.0));
    });

    if app.mobile_log.is_empty() { return; }

    ui.add_space(4.0);

    // ── Event log (last events from mobile) ───────────────────────────────
    egui::ScrollArea::vertical()
        .id_source("mobile_log")
        .max_height(100.0)
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.add_space(2.0);
            for line in &app.mobile_log {
                ui.label(
                    RichText::new(line.as_str())
                        .font(FontId::monospace(11.0))
                        .color(egui::Color32::from_rgb(180, 220, 180)),
                );
            }
            ui.add_space(2.0);
        });
}

// ── Section 2: Controls ────────────────────────────────────────────────────

fn render_controls(ui: &mut egui::Ui, app: &mut ServerApp) {
    ui.add_space(6.0);

    // ── Channel ───────────────────────────────────────────────────────────
    control_row(ui, "Channel", |ui| {
        for ch in [1u8, 2] {
            let active = app.selected_channel == ch;
            if toggle_btn(ui, &format!("  CH {}  ", ch), active, BLUE).clicked() {
                app.selected_channel = ch;
            }
        }
    });

    ui.add_space(4.0);

    // ── Output ────────────────────────────────────────────────────────────
    control_row(ui, "Output  ", |ui| {
        let ch_output  = app.output[app.ch_idx()];
        let on_active  = ch_output == Some(true);
        let off_active = ch_output == Some(false);
        if toggle_btn(ui, "  ▶ ON   ", on_active,  GREEN).clicked() {
            app.send_output(true);
        }
        if toggle_btn(ui, "  ■ OFF  ", off_active, RED).clicked() {
            app.send_output(false);
        }
    });

    ui.add_space(4.0);

    // ── Waveform ──────────────────────────────────────────────────────────
    control_row(ui, "Waveform", |ui| {
        let sent_wf = app.sent[app.ch_idx()].waveform.clone();
        for (wf, label) in [
            (Waveform::Sine,   "  ∿ Sine   "),
            (Waveform::Square, "  ⊓ Square "),
        ] {
            let active = sent_wf.as_ref() == Some(&wf);
            if toggle_btn(ui, label, active, BLUE).clicked() {
                app.send_waveform(wf);
            }
        }
    });

    ui.add_space(8.0);

    // ── Frequencies ───────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(RichText::new("Frequency").size(13.0).strong());
    });

    ui.add_space(4.0);

    let sent_freq = app.sent[app.ch_idx()].frequency;

    // 4 in first row, 3 in second row  (matches the specified display order)
    for (row_start, row_len) in [(0usize, 4usize), (4, 3)] {
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            for i in row_start..row_start + row_len {
                let hz    = FREQUENCIES[i];
                let color = FREQ_COLORS[i];
                let active = sent_freq == Some(hz);
                let label  = fmt_freq_btn(hz);
                if toggle_btn(ui, &label, active, color).clicked() {
                    app.send_frequency(hz);
                }
            }
        });
        ui.add_space(3.0);
    }

    ui.add_space(6.0);
}

// ── Section 3: Sent-command log ────────────────────────────────────────────

fn render_log(ui: &mut egui::Ui, app: &ServerApp) {
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(RichText::new("📋  Sent Commands").size(13.0).strong());
    });
    ui.add_space(4.0);

    if app.log.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(RichText::new("(nothing sent yet)").color(egui::Color32::GRAY).size(11.5));
        });
        return;
    }

    egui::ScrollArea::vertical()
        .id_source("cmd_log")
        .max_height(220.0)
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.add_space(2.0);
            for line in &app.log {
                ui.label(
                    RichText::new(line.as_str())
                        .font(FontId::monospace(11.5))
                        .color(egui::Color32::from_rgb(200, 220, 255)),
                );
            }
            ui.add_space(2.0);
        });
}

// ── Button helpers ─────────────────────────────────────────────────────────

const BLUE:  egui::Color32 = egui::Color32::from_rgb(30,  90,  180);
const GREEN: egui::Color32 = egui::Color32::from_rgb(30,  130, 60);
const RED:   egui::Color32 = egui::Color32::from_rgb(160, 40,  40);

/// A button that is filled with `color` when `active`, plain otherwise.
fn toggle_btn(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    color: egui::Color32,
) -> egui::Response {
    let btn = if active {
        egui::Button::new(RichText::new(label).size(13.0).strong()).fill(color)
    } else {
        egui::Button::new(RichText::new(label).size(13.0))
    };
    ui.add(btn)
}

/// A labelled row: "Label  [buttons…]"
fn control_row(ui: &mut egui::Ui, label: &str, buttons: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(RichText::new(label).size(13.0).strong());
        ui.add_space(8.0);
        buttons(ui);
    });
}

fn sep(ui: &mut egui::Ui) {
    ui.add_space(5.0);
    ui.separator();
    ui.add_space(4.0);
}

// ── Formatting helpers ─────────────────────────────────────────────────────

fn fmt_freq(hz: f64) -> String {
    if hz >= 1_000_000.0 {
        format!("{:.3} MHz", hz / 1_000_000.0)
    } else {
        format!("{:.3} kHz", hz / 1_000.0)
    }
}

/// Compact label for frequency buttons.
fn fmt_freq_btn(hz: f64) -> String {
    if hz >= 1_000_000.0 {
        format!(" {:.3} MHz ", hz / 1_000_000.0)
    } else {
        format!(" {:.3} kHz ", hz / 1_000.0)
    }
}

fn wf_label(wf: &Waveform) -> &'static str {
    match wf {
        Waveform::Sine     => "∿ Sine",
        Waveform::Square   => "⊓ Square",
        Waveform::Triangle => "∧ Triangle",
        Waveform::Pulse    => "⌐ Pulse",
        Waveform::Unknown  => "?",
    }
}

/// Current UTC wall-clock time as HH:MM:SS.
fn local_time() -> String {
    let now = chrono::Local::now();
    now.format("%H:%M:%S").to_string()
}

// ── QR code ────────────────────────────────────────────────────────────────

fn build_qr(ctx: &egui::Context, url: &str) -> TextureHandle {
    let code = QrCode::new(url.as_bytes()).expect("QrCode failed");
    let img  = code.render::<image::Luma<u8>>().quiet_zone(true).build();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for p in img.pixels() { let v = p[0]; rgba.extend_from_slice(&[v, v, v, 255]); }
    ctx.load_texture(
        "qr_code",
        ColorImage::from_rgba_unmultiplied([w, h], &rgba),
        TextureOptions::NEAREST,
    )
}
