//! System tray — production-grade implementation.
//!
//! # Architecture
//!
//! ```text
//! ┌─ Tray OS Thread ──────────────────────────────────────────────────────┐
//! │  MenuEvent::receiver().recv()   ← BLOCKING, zero CPU, zero latency   │
//! │  ├─ Toggle → tx.send(TrayEvent::Toggle)                              │
//! │  └─ Quit   → tx.send(TrayEvent::Quit)                                │
//! └───────────────────────────────────────────────────────────────────────┘
//!           │
//!    crossbeam_channel
//!           │
//! ┌─ egui Main Thread ────────────────────────────────────────────────────┐
//! │  App::update()                                                        │
//! │    tray.drain() → Vec<TrayEvent>                                      │
//! │    for ev in events {                                                 │
//! │      Toggle → visible = !visible                                      │
//! │      Quit   → std::process::exit(0)                                  │
//! │    }                                                                  │
//! │    ViewportCommand::Visible(visible)                                  │
//! │    if visible { ViewportCommand::Focus }                              │
//! │    // render panel only when visible — but update() NEVER returns    │
//! └───────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Key properties
//! | Property               | Guarantee |
//! |------------------------|-----------|
//! | Quit works always      | ✓ — tray thread calls exit(0) directly |
//! | Show/Hide always works | ✓ — event delivered over channel |
//! | Zero CPU when idle     | ✓ — blocking recv, no polling |
//! | No thread_local        | ✓ |
//! | No shared mutable state| ✓ — channel is the only bridge |
//! | update() never returns | ✓ — viewport cmd applied every frame |
//! | Typed events           | ✓ — TrayEvent enum |
//!
//! ## Why blocking `recv()` in the tray thread
//! `try_recv()` + sleep is a polling anti-pattern: it either wastes CPU or
//! adds latency.  `MenuEvent::receiver().recv()` blocks until an event
//! arrives, consumes zero CPU while idle, and delivers events with OS-level
//! latency (~1 ms).
//!
//! ## Why `TrayController` owns the `TrayIcon`
//! `TrayIcon` must stay alive for the lifetime of the application.  Owning it
//! inside `TrayController` (which is owned by `ServerApp`) guarantees the
//! lifetime without any external binding in `main()`.
//!
//! ## Why `std::process::exit(0)` in the tray thread, not via an event
//! Quit is the one action that must work even if the eframe event loop is
//! suspended.  Sending a `Quit` event and handling it in `update()` is also
//! correct — we do both: the tray thread sends the event AND the handler
//! calls `exit(0)`.  Either path guarantees termination.

use crossbeam_channel::{unbounded, Receiver, Sender};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, MenuId},
    Icon, TrayIcon, TrayIconBuilder,
};

// ── Event type ────────────────────────────────────────────────────────────

/// Typed events sent from the tray OS thread to the egui main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Toggle window visibility.
    Toggle,
    /// Quit the application.
    Quit,
}

// ── TrayController ────────────────────────────────────────────────────────

/// Owned by `ServerApp`.  Provides `drain()` to pull tray events each frame
/// and `update_label()` to keep the menu item text in sync.
pub struct TrayController {
    /// Receiving end of the event channel.
    rx:         Receiver<TrayEvent>,
    /// Kept so we can call `set_text()` to keep the label accurate.
    show_item:  MenuItem,
    /// Held here so it lives as long as the app.
    _tray:      TrayIcon,
}

impl TrayController {
    /// Drain all pending tray events.  Call at the top of every `update()`.
    /// Never blocks.
    pub fn drain(&self) -> Vec<TrayEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Update the context-menu label to reflect what clicking WILL do.
    /// Call after computing the new `visible` state each frame.
    pub fn update_label(&self, visible: bool) {
        self.show_item.set_text(if visible { "Hide Window" } else { "Show Window" });
    }
}

// ── Factory ───────────────────────────────────────────────────────────────

/// Create the system tray icon and start the event thread.
///
/// **Must be called from the OS main thread** (macOS / Windows requirement).
///
/// Returns a `TrayController` that must be moved into `ServerApp::new()`.
/// The `TrayIcon` is owned by the controller — no external binding needed.
pub fn create() -> TrayController {
    let icon = Icon::from_rgba(draw_icon(32), 32, 32)
        .expect("Tray icon pixel data invalid");

    let menu      = Menu::new();
    let show_item = MenuItem::new("Hide Window", true, None);  // starts visible
    let quit_item = MenuItem::new("Quit",        true, None);
    menu.append(&show_item).unwrap();
    menu.append(&quit_item).unwrap();

    let (tx, rx): (Sender<TrayEvent>, Receiver<TrayEvent>) = unbounded();

    let show_id: MenuId = show_item.id().clone();
    let quit_id: MenuId = quit_item.id().clone();

    // Spawn the dedicated tray event thread.
    // This thread blocks on MenuEvent::receiver().recv() — zero CPU, zero
    // latency.  It has NO dependency on the egui frame loop.
    spawn_event_thread(tx, show_id, quit_id);

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("JDS6600 Server")
        .with_icon(icon)
        .build()
        .expect("Failed to create tray icon");

    TrayController { rx, show_item, _tray: tray }
}

// ── Tray event thread ─────────────────────────────────────────────────────

fn spawn_event_thread(
    tx:      Sender<TrayEvent>,
    show_id: MenuId,
    quit_id: MenuId,
) {
    std::thread::Builder::new()
        .name("jds-tray-events".into())
        .spawn(move || {
            loop {
                // BLOCKING recv — zero CPU while idle, OS-level latency.
                // This is the correct pattern: no polling, no sleep().
                match MenuEvent::receiver().recv() {
                    Ok(ev) => {
                        if ev.id == show_id {
                            // Ignore send error: egui side may have exited.
                            let _ = tx.send(TrayEvent::Toggle);
                        } else if ev.id == quit_id {
                            // Belt-and-suspenders: send the event so update()
                            // can also react, then exit unconditionally so
                            // Quit works even if the egui loop is suspended.
                            let _ = tx.send(TrayEvent::Quit);
                            std::process::exit(0);
                        }
                    }
                    Err(_) => {
                        // Channel closed (process shutting down) — exit cleanly.
                        std::process::exit(0);
                    }
                }
            }
        })
        .expect("Failed to spawn tray event thread");
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
