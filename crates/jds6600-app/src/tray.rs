/// System tray icon management.
///
/// # Root cause of the original Show/Hide bug
/// When `eframe` hides the window it stops scheduling repaints, so
/// `App::update()` is never called, so `MenuEvent::receiver().try_recv()`
/// never fires.  Fix: always call `ctx.request_repaint_after(50ms)` even
/// in the hidden state so the egui event loop keeps ticking.
///
/// # Additional fix: close-to-tray
/// The window X button now hides rather than exits.  We intercept the
/// `close_requested` input flag, cancel it, then hide instead.

use std::cell::RefCell;
use std::time::Duration;

use eframe::egui;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, MenuId},
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

// ── Thread-local storage for menu item IDs ────────────────────────────────

thread_local! {
    static IDS: RefCell<Option<(MenuId, MenuId)>> = RefCell::new(None);
}

fn store(show: MenuId, quit: MenuId) {
    IDS.with(|c| *c.borrow_mut() = Some((show, quit)));
}

fn ids() -> (MenuId, MenuId) {
    IDS.with(|c| c.borrow().clone().expect("call tray::create() first"))
}

// ── Public API ────────────────────────────────────────────────────────────

/// Create the tray icon.  Must be called from the main OS thread.
/// Keep the returned value alive for the life of the process.
pub fn create() -> TrayIcon {
    let icon = Icon::from_rgba(draw_icon(32), 32, 32)
        .expect("Tray icon pixel data invalid");

    let menu = Menu::new();
    let show = MenuItem::new("Show / Hide Window", true, None);
    let quit = MenuItem::new("Quit",               true, None);
    menu.append(&show).unwrap();
    menu.append(&quit).unwrap();
    store(show.id().clone(), quit.id().clone());

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("JDS6600 Server – right-click for options")
        .with_icon(icon)
        .build()
        .expect("Failed to create tray icon")
}

/// Poll tray + menu events.  Call at the top of every `App::update()`.
/// Mutates `visible`; also requests a repaint so the loop stays alive.
pub fn poll(ctx: &egui::Context, visible: &mut bool) {
    let (show_id, quit_id) = ids();

    // Right-click context menu events
    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        if ev.id == show_id {
            *visible = !*visible;
            if *visible {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        } else if ev.id == quit_id {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    // Direct icon click / double-click
    while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
        if let TrayIconEvent::Click { .. } = ev {
            *visible = !*visible;
            if *visible {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
    }

    // THE key fix: keep the event loop alive even when the window is hidden.
    // Without this eframe stops calling update() and tray events are never polled.
    ctx.request_repaint_after(Duration::from_millis(50));
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

            // Navy background
            rgba[i]   = 18;
            rgba[i+1] = 52;
            rgba[i+2] = 140;
            rgba[i+3] = 255;

            // Yellow sine wave
            let phase  = (x as f32 / size as f32) * std::f32::consts::TAU * 2.5;
            let wave_y = center + phase.sin() * center * 0.35;
            if (y as f32 - wave_y).abs() < 1.6 {
                rgba[i]   = 255;
                rgba[i+1] = 210;
                rgba[i+2] = 0;
            }

            // Light-blue border ring
            if (r - center + 1.5).abs() < 1.5 {
                rgba[i]   = 140;
                rgba[i+1] = 170;
                rgba[i+2] = 255;
            }
        }
    }
    rgba
}
