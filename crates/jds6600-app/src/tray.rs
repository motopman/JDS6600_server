//! System tray — OS-callback architecture, corrected for Send+Sync.
//!
//! ## Why MenuItem cannot go into set_event_handler callbacks
//!
//! `MenuItem` internally holds `Rc<MenuId>` (non-Send, non-Sync).
//! `set_event_handler` requires `F: Send + Sync + 'static`.
//!
//! Solution: the callbacks capture only Send+Sync values:
//!   • `MenuId`  — a plain u32 wrapper, Copy
//!   • `Arc<AtomicBool>` — Send+Sync
//!   • `egui::Context`  — Clone+Send+Sync
//!
//! `MenuItem::set_enabled()` is called from `TrayController::apply()`,
//! which runs on the egui main thread every frame — no threading needed.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, MenuId, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

// ── TrayController ────────────────────────────────────────────────────────

pub struct TrayController {
    is_hidden:    Arc<AtomicBool>,
    hide_item:    MenuItem,
    restore_item: MenuItem,
    _tray:        TrayIcon,
}

impl TrayController {
    /// True when window should be hidden.
    pub fn is_hidden(&self) -> bool {
        self.is_hidden.load(Ordering::SeqCst)
    }

    /// Sync menu item enabled state to current visibility.
    /// Call once per frame from `update()` — runs on the egui main thread,
    /// so `MenuItem` (non-Send) is safe to call here.
    pub fn apply_menu_state(&self) {
        let hidden = self.is_hidden.load(Ordering::SeqCst);
        self.hide_item.set_enabled(!hidden);
        self.restore_item.set_enabled(hidden);
    }

    /// Hide the window — called from the in-window Minimize button or ✕.
    pub fn hide(&self, ctx: &eframe::egui::Context) {
        self.is_hidden.store(true, Ordering::SeqCst);
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
        ctx.request_repaint();
    }
}

// ── Factory ───────────────────────────────────────────────────────────────

pub fn create(ctx: eframe::egui::Context) -> TrayController {
    let icon = Icon::from_rgba(draw_icon(32), 32, 32)
        .expect("Tray icon pixel data invalid");

    let menu         = Menu::new();
    let hide_item    = MenuItem::new("Hide Window", true,  None);
    let restore_item = MenuItem::new("Restore",     false, None);
    let quit_item    = MenuItem::new("Quit",        true,  None);

    menu.append(&hide_item).unwrap();
    menu.append(&restore_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&quit_item).unwrap();

    let is_hidden = Arc::new(AtomicBool::new(false));

    // Only Send+Sync values go into the callbacks:
    // MenuId (Copy), Arc<AtomicBool>, egui::Context
    let hide_id    = hide_item.id().clone();
    let restore_id = restore_item.id().clone();
    let quit_id    = quit_item.id().clone();

    // ── Menu event handler ─────────────────────────────────────────────────
    {
        let ctx2      = ctx.clone();
        let is_hidden2 = Arc::clone(&is_hidden);

        MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
            if ev.id == hide_id {
                is_hidden2.store(true, Ordering::SeqCst);
                ctx2.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
                ctx2.request_repaint();

            } else if ev.id == restore_id {
                is_hidden2.store(false, Ordering::SeqCst);
                ctx2.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
                ctx2.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
                ctx2.request_repaint();

            } else if ev.id == quit_id {
                // ViewportCommand::Close silently fails when window is hidden.
                std::process::exit(0);
            }
        }));
    }

    // ── Tray icon click handler ────────────────────────────────────────────
    {
        let ctx3       = ctx.clone();
        let is_hidden3 = Arc::clone(&is_hidden);

        TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
            if let TrayIconEvent::Click { .. } = ev {
                let was_hidden = is_hidden3.load(Ordering::SeqCst);
                let now_hidden = !was_hidden;
                is_hidden3.store(now_hidden, Ordering::SeqCst);
                ctx3.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(!now_hidden));
                if !now_hidden {
                    ctx3.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
                }
                ctx3.request_repaint();
            }
        }));
    }

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("JDS6600 Server")
        .with_icon(icon)
        .build()
        .expect("Failed to create tray icon");

    TrayController { is_hidden, hide_item, restore_item, _tray: tray }
}

// ── Procedural icon ────────────────────────────────────────────────────────

fn draw_icon(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center   = size as f32 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let i  = ((y * size + x) * 4) as usize;
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let r  = (dx * dx + dy * dy).sqrt();
            if r > center { continue; }

            rgba[i] = 18; rgba[i+1] = 52; rgba[i+2] = 140; rgba[i+3] = 255;

            let phase  = (x as f32 / size as f32) * std::f32::consts::TAU * 2.5;
            let wave_y = center + phase.sin() * center * 0.35;
            if (y as f32 - wave_y).abs() < 1.6 {
                rgba[i] = 255; rgba[i+1] = 210; rgba[i+2] = 0;
            }
            if (r - center + 1.5).abs() < 1.5 {
                rgba[i] = 140; rgba[i+1] = 170; rgba[i+2] = 255;
            }
        }
    }
    rgba
}
