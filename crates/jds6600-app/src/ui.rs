/// egui application window.
///
/// Layout (top-to-bottom, scrollable outer area):
///
///  ┌──────────────────────────────────────┐
///  │  JDS6600 Remote Server    [heading]  │
///  │  [QR code 240×240]                   │
///  │  ws://192.168.x.x:8080/ws  [mono]    │
///  │  ● Server running                    │
///  │  [Minimize to Tray]                  │
///  ├──────────────────────────────────────┤
///  │  ⚙ Protocol Scanner                  │
///  │  "Scans every port, probes JDS6600…" │
///  │  [▶ Scan All Ports & Sweep] [🗑] [💾] │
///  │  ┌────────────────────────────────┐  │
///  │  │  colour-coded scrollable log   │  │  ← appears once scan starts
///  │  └────────────────────────────────┘  │
///  └──────────────────────────────────────┘
///
/// Tray behaviour:
///  • [Minimize to Tray] → hides window, tray icon stays.
///  • ✕ (close button) → also hides, does NOT quit.
///  • Right-click tray → Show / Hide | Quit.
///  • Left-click  tray → toggles Show / Hide.

use std::sync::mpsc::TryRecvError;

use eframe::egui::{self, ColorImage, FontId, RichText, TextureHandle, TextureOptions};
use image::Luma;
use qrcode::QrCode;

use jds6600_core::scanner::{start_scan, LineKind, ScanHandle};

use crate::tray;

// ── Application state ─────────────────────────────────────────────────────

pub struct ServerApp {
    ws_url:        String,
    qr_texture:    Option<TextureHandle>,
    visible:       bool,
    scan_handle:   Option<ScanHandle>,
    scan_log:      Vec<(LineKind, String)>,
    scan_running:  bool,
}

impl ServerApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, ws_url: String) -> Self {
        Self {
            ws_url,
            qr_texture:   None,
            visible:      true,
            scan_handle:  None,
            scan_log:     Vec::new(),
            scan_running: false,
        }
    }

    /// Drain everything the scanner thread produced since the last frame.
    fn drain_scan_channel(&mut self) {
        let Some(handle) = &self.scan_handle else { return };
        loop {
            match handle.rx.try_recv() {
                Ok(line) => {
                    if line.kind == LineKind::Done {
                        self.scan_running = false;
                    }
                    self.scan_log.push((line.kind, line.message));
                }
                Err(TryRecvError::Empty)        => break,
                Err(TryRecvError::Disconnected) => { self.scan_running = false; break; }
            }
        }
    }
}

// ── App trait ─────────────────────────────────────────────────────────────

impl eframe::App for ServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Poll tray events every frame.
        //    tray::poll also calls ctx.request_repaint_after(50ms) which keeps
        //    the loop alive even when the window is hidden.
        tray::poll(ctx, &mut self.visible);

        // 2. Intercept the ✕ button: hide instead of quit.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.visible = false;
        }

        // 3. Sync window visibility with our flag.
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(self.visible));

        // 4. Skip rendering entirely when hidden.
        if !self.visible { return; }

        // 5. Drain scanner channel (no-op if not scanning).
        if self.scan_handle.is_some() {
            self.drain_scan_channel();
            if self.scan_running {
                // Request frequent repaints while scan is live.
                ctx.request_repaint_after(std::time::Duration::from_millis(60));
            }
        }

        // 6. Render the window.
        egui::CentralPanel::default().show(ctx, |ui| {
            // Outer scroll area so the full layout fits even on small screens.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    render_header(ui, ctx, self);
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);
                    render_scanner(ui, self);
                });
        });
    }
}

// ── Top section ───────────────────────────────────────────────────────────

fn render_header(ui: &mut egui::Ui, ctx: &egui::Context, app: &mut ServerApp) {
    ui.vertical_centered(|ui| {
        ui.add_space(14.0);

        ui.label(
            RichText::new("JDS6600 Remote Server")
                .size(19.0)
                .strong(),
        );
        ui.add_space(3.0);
        ui.label(
            RichText::new("Scan with the mobile app to connect:")
                .color(egui::Color32::GRAY),
        );
        ui.add_space(10.0);

        // QR code – lazily generated on first render.
        let texture = app.qr_texture.get_or_insert_with(|| build_qr(ctx, &app.ws_url));
        ui.add(egui::Image::new((texture.id(), egui::vec2(240.0, 240.0))));

        ui.add_space(8.0);

        // WebSocket URL – read-only, selectable for manual copy.
        let mut url = app.ws_url.clone();
        ui.add(
            egui::TextEdit::singleline(&mut url)
                .font(egui::TextStyle::Monospace)
                .desired_width(330.0)
                .interactive(false),
        );

        ui.add_space(10.0);

        ui.label(
            RichText::new("● Server running")
                .color(egui::Color32::from_rgb(60, 210, 60))
                .strong(),
        );

        ui.add_space(10.0);

        if ui.button("  Minimize to Tray  ").clicked() {
            app.visible = false;
        }

        ui.add_space(4.0);
    });
}

// ── Scanner section ───────────────────────────────────────────────────────

fn render_scanner(ui: &mut egui::Ui, app: &mut ServerApp) {
    // ── Title + description ───────────────────────────────────────────
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("⚙  Protocol Scanner").size(15.0).strong());
        ui.add_space(3.0);
        ui.label(
            RichText::new(
                "Probes every serial port, identifies the JDS6600,\n\
                 then runs a full r00..r120 command sweep and write\n\
                 probes to characterise the exact protocol responses.",
            )
            .size(11.5)
            .color(egui::Color32::GRAY),
        );
        ui.add_space(8.0);
    });

    // ── Button row ────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.add_space(8.0);

        let btn_label = if app.scan_running {
            "⏳  Scanning…"
        } else {
            "▶  Scan All Ports & Sweep"
        };

        if ui
            .add_enabled(
                !app.scan_running,
                egui::Button::new(RichText::new(btn_label).size(13.0)),
            )
            .clicked()
        {
            app.scan_log.clear();
            app.scan_handle  = Some(start_scan());
            app.scan_running = true;
        }

        if !app.scan_log.is_empty() {
            if ui.button("🗑  Clear").clicked() {
                app.scan_log.clear();
                app.scan_handle  = None;
                app.scan_running = false;
            }

            if ui.button("💾  Save log").clicked() {
                save_log(&app.scan_log);
            }
        }
    });

    ui.add_space(6.0);

    // ── Log panel ─────────────────────────────────────────────────────
    if app.scan_log.is_empty() {
        return;
    }

    // stick_to_bottom: egui's built-in auto-scroll-to-bottom.
    // It activates whenever the user is not manually scrolling up.
    egui::ScrollArea::vertical()
        .id_source("scan_log")
        .max_height(320.0)
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.add_space(2.0);
            for (kind, msg) in &app.scan_log {
                let color = match kind {
                    LineKind::Found => egui::Color32::from_rgb(80, 220, 80),
                    LineKind::Skip  => egui::Color32::GRAY,
                    LineKind::Error => egui::Color32::from_rgb(240, 80, 80),
                    LineKind::Done  => egui::Color32::from_rgb(100, 200, 255),
                    LineKind::Data  => egui::Color32::from_rgb(200, 200, 200),
                    LineKind::Info  => egui::Color32::WHITE,
                };
                ui.label(
                    RichText::new(msg.as_str())
                        .font(FontId::monospace(11.0))
                        .color(color),
                );
            }
            ui.add_space(2.0);
        });
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn build_qr(ctx: &egui::Context, url: &str) -> TextureHandle {
    let code = QrCode::new(url.as_bytes()).expect("QrCode::new failed");
    let img  = code.render::<Luma<u8>>().quiet_zone(true).build();
    let w    = img.width() as usize;
    let h    = img.height() as usize;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for p in img.pixels() {
        let v = p[0];
        rgba.extend_from_slice(&[v, v, v, 255]);
    }
    ctx.load_texture(
        "qr_code",
        ColorImage::from_rgba_unmultiplied([w, h], &rgba),
        TextureOptions::NEAREST,
    )
}

/// Write the scan log to a temp file and open it in the OS default editor.
fn save_log(log: &[(LineKind, String)]) {
    let path = std::env::temp_dir().join("jds6600_sweep.txt");
    let text: String = log.iter().map(|(_, s)| format!("{}\n", s)).collect();
    match std::fs::write(&path, &text) {
        Ok(_) => {
            // Open in default text editor (cross-platform).
            let _ = open::that(&path);
        }
        Err(e) => eprintln!("[UI] Save log failed: {}", e),
    }
}
