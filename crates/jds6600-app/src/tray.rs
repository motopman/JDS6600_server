/// System tray icon management.
///
/// # Platform requirements
/// * **Windows/macOS**: must be created on the main thread.
/// * **Linux (X11/Wayland)**: requires `libappindicator` or `libayatana-appindicator`.
///   Install: `sudo apt install libayatana-appindicator3-dev`
///
/// The tray icon is kept alive via the returned `TrayIcon` which MUST be
/// stored in `main()` until the process exits.

use std::cell::RefCell;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, MenuId},
    Icon, TrayIcon, TrayIconBuilder,
};
use eframe::egui;

// ── Thread-local menu ID storage ──────────────────────────────────────────

thread_local! {
    static MENU_IDS: RefCell<Option<(MenuId, MenuId)>> = RefCell::new(None);
}

fn store(show: MenuId, quit: MenuId) {
    MENU_IDS.with(|m| *m.borrow_mut() = Some((show, quit)));
}

fn ids() -> (MenuId, MenuId) {
    MENU_IDS.with(|m| m.borrow().clone().expect("Tray not initialised"))
}

// ── Public API ────────────────────────────────────────────────────────────

/// Create the tray icon.  Must be called from the main thread.
/// The returned `TrayIcon` must remain alive for the life of the process.
pub fn create() -> TrayIcon {
    let icon_rgba = draw_icon(32);
    let icon      = Icon::from_rgba(icon_rgba, 32, 32).expect("Icon creation failed");

    let menu = Menu::new();
    let show = MenuItem::new("Show / Hide", true, None);
    let quit = MenuItem::new("Quit",        true, None);
    menu.append(&show).unwrap();
    menu.append(&quit).unwrap();
    store(show.id().clone(), quit.id().clone());

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("JDS6600 Server – right-click for options")
        .with_icon(icon)
        .build()
        .expect("Tray icon creation failed")
}

/// Poll pending tray menu events.  Call every egui frame.
pub fn poll(ctx: &egui::Context, visible: &mut bool) {
    let (show_id, quit_id) = ids();

    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        if ev.id == show_id {
            *visible = !*visible;
        } else if ev.id == quit_id {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

// ── Procedural icon (dark blue circle + yellow sine wave) ─────────────────
// No asset file required – generated at startup.

fn draw_icon(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center   = size as f32 / 2.0;

    for y in 0..size {
        for x in 0..size {
            let i   = ((y * size + x) * 4) as usize;
            let dx  = x as f32 - center;
            let dy  = y as f32 - center;
            let r   = (dx * dx + dy * dy).sqrt();

            if r > center { continue; } // transparent outside circle

            // Background: dark navy blue.
            rgba[i]   = 18;
            rgba[i+1] = 52;
            rgba[i+2] = 140;
            rgba[i+3] = 255;

            // Sine wave: yellow.
            let phase   = (x as f32 / size as f32) * std::f32::consts::TAU * 2.5;
            let wave_y  = center + phase.sin() * center * 0.35;
            if (y as f32 - wave_y).abs() < 1.6 {
                rgba[i]   = 255;
                rgba[i+1] = 210;
                rgba[i+2] = 0;
            }

            // Thin circle border: light blue.
            if (r - center + 1.5).abs() < 1.5 {
                rgba[i]   = 140;
                rgba[i+1] = 170;
                rgba[i+2] = 255;
            }
        }
    }
    rgba
}
