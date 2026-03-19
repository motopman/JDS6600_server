/// System tray icon – create, poll events, update menu label dynamically.
///
/// ## Design
/// `create()` returns a `TrayIcon` that must stay alive for the lifetime
/// of the process.  We also store two things in thread-locals:
///   • The two `MenuId`s for matching events.
///   • The show/hide `MenuItem` itself, so we can call `set_text()` to
///     show "Show Window" or "Hide Window" depending on current state.
///
/// ## Why `std::process::exit` for quit
/// `egui::ViewportCommand::Close` is silently dropped by eframe when the
/// window is in the hidden state (`Visible(false)`).  Calling exit(0)
/// directly is the only reliable cross-platform quit path from a tray icon.
///
/// ## Keep-alive repaint
/// When the window is hidden eframe stops scheduling repaints, so
/// `update()` stops being called, so menu events are never polled.
/// `poll()` always calls `ctx.request_repaint_after(50 ms)` to keep
/// the loop alive regardless of window visibility.

use std::cell::RefCell;
use std::time::Duration;

use eframe::egui;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, MenuId},
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

// ── Thread-local storage ──────────────────────────────────────────────────

struct TrayState {
    show_id: MenuId,
    quit_id: MenuId,
    /// Kept so we can call `set_text()` on it dynamically.
    show_item: MenuItem,
}

thread_local! {
    static TRAY: RefCell<Option<TrayState>> = RefCell::new(None);
}

// ── Public API ────────────────────────────────────────────────────────────

/// Create the tray icon.  **Must be called from the main OS thread.**
/// Store the returned `TrayIcon` in a binding that lives until process exit.
pub fn create() -> TrayIcon {
    let icon = Icon::from_rgba(draw_icon(32), 32, 32)
        .expect("Tray icon pixel data invalid");

    let menu      = Menu::new();
    let show_item = MenuItem::new("Hide Window", true, None);  // window starts visible
    let quit_item = MenuItem::new("Quit",        true, None);
    menu.append(&show_item).unwrap();
    menu.append(&quit_item).unwrap();

    TRAY.with(|t| {
        *t.borrow_mut() = Some(TrayState {
            show_id:   show_item.id().clone(),
            quit_id:   quit_item.id().clone(),
            show_item,
        });
    });

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("JDS6600 Server")
        .with_icon(icon)
        .build()
        .expect("Failed to create tray icon")
}

/// Poll tray events every egui frame.
///
/// * Updates the menu label to "Hide Window" or "Show Window" based on
///   the current `visible` state.
/// * Toggles `visible` when the show/hide item is clicked or the icon
///   is directly clicked.
/// * Exits the process immediately on Quit.
/// * Requests a 50 ms repaint to keep the loop alive when hidden.
pub fn poll(ctx: &egui::Context, visible: &mut bool) {
    // Update the menu label to always reflect what clicking will DO.
    TRAY.with(|t| {
        if let Some(ref state) = *t.borrow() {
            let label = if *visible { "Hide Window" } else { "Show Window" };
            state.show_item.set_text(label);
        }
    });

    // Process menu events.
    let (show_id, quit_id) = TRAY.with(|t| {
        t.borrow().as_ref().map(|s| (s.show_id.clone(), s.quit_id.clone()))
            .expect("call tray::create() first")
    });

    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        if ev.id == show_id {
            *visible = !*visible;
            if *visible {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        } else if ev.id == quit_id {
            // ViewportCommand::Close is ignored when the window is hidden.
            // std::process::exit(0) is the only reliable path.
            std::process::exit(0);
        }
    }

    // Direct click on the tray icon toggles visibility.
    while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
        if let TrayIconEvent::Click { .. } = ev {
            *visible = !*visible;
            if *visible {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
    }

    // Keep the egui event loop alive even when the window is hidden.
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
