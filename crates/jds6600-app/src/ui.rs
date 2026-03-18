use eframe::egui;
use eframe::egui::{ColorImage, TextureHandle, TextureOptions};
use image::Luma;
use qrcode::QrCode;

use crate::tray;

pub struct ServerApp {
    ws_url:     String,
    qr_texture: Option<TextureHandle>,
    visible:    bool,
}

impl ServerApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, ws_url: String) -> Self {
        Self { ws_url, qr_texture: None, visible: true }
    }
}

impl eframe::App for ServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll tray events every frame.
        tray::poll(ctx, &mut self.visible);

        if !self.visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(18.0);
                ui.label(
                    egui::RichText::new("JDS6600 Remote Server")
                        .size(19.0)
                        .strong()
                );
                ui.add_space(6.0);
                ui.label("Scan with the mobile app to connect:");
                ui.add_space(14.0);

                // Lazily generate QR texture on first frame.
                let texture = self.qr_texture.get_or_insert_with(|| {
                    build_qr_texture(ctx, &self.ws_url)
                });
                ui.add(egui::Image::new((texture.id(), egui::vec2(260.0, 260.0))));

                ui.add_space(12.0);

                // Read-only URL for manual entry.
                let mut url = self.ws_url.clone();
                ui.add(
                    egui::TextEdit::singleline(&mut url)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(340.0)
                        .interactive(false),
                );

                ui.add_space(14.0);

                // Green status dot.
                ui.label(
                    egui::RichText::new("● Server running")
                        .color(egui::Color32::from_rgb(60, 200, 60))
                        .strong(),
                );

                ui.add_space(10.0);
                if ui.button("Minimize to Tray").clicked() {
                    self.visible = false;
                }
            });
        });
    }
}

fn build_qr_texture(ctx: &egui::Context, url: &str) -> TextureHandle {
    let code  = QrCode::new(url.as_bytes()).expect("QrCode generation failed");
    let img   = code.render::<Luma<u8>>().quiet_zone(true).build();
    let w     = img.width() as usize;
    let h     = img.height() as usize;

    let mut rgba = Vec::with_capacity(w * h * 4);
    for p in img.pixels() {
        let v = p[0];
        rgba.extend_from_slice(&[v, v, v, 255]);
    }

    ctx.load_texture("qr_code", ColorImage::from_rgba_unmultiplied([w, h], &rgba), TextureOptions::NEAREST)
}
