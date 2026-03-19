//! System tray – production-grade implementation.
//!
//! # Architecture
//!
//! The previous implementation had a fundamental dependency chain:
//!
//! ```text
//! tray events processed
//!   → only in poll()
//!     → only called from App::update()
//!       → only called when eframe schedules a repaint
//!         → eframe stops repaints when window is hidden
//! ```
//!
//! This means **Quit and Show/Hide silently stopped working** the moment
//! the window was hidden.  `request_repaint_after(50ms)` was not a fix —
//! it only works if `update()` is already being called.
//!
//! # Correct design
//!
//! ```text
//! ┌─ OS Main Thread ──────────────────────────────────────────────────────┐
//! │  create() → spawns tray thread, returns (TrayIcon, TrayController)    │
//! │  eframe::run_native()                                                 │
//! │    └─ App::new()    → TrayController::register_egui_ctx(cc.egui_ctx) │
//! │       App::update() → TrayController::apply(ctx, &mut visible)        │
//! │                       reads AtomicBool, applies Visible(), sets label │
//! └───────────────────────────────────────────────────────────────────────┘
//!
//! ┌─ Tray Thread (dedicated) ─────────────────────────────────────────────┐
//! │  loop {                                                                │
//! │    try_recv MenuEvent    ← never blocks; 10 ms sleep between polls    │
//! │    ├─ show/hide → flip AtomicBool; ctx.request_repaint() → wakes eframe│
//! │    └─ quit      → std::process::exit(0)  ← always works              │
//! │    try_recv TrayIconEvent                                              │
//! │    └─ click     → same as show/hide                                   │
//! │  }                                                                     │
//! └───────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Why `AtomicBool` and not a channel
//! The tray thread writes; eframe reads on every frame.  The semantics are
//! "what is the *current desired visibility*", not "deliver every toggle
//! event" — an AtomicBool with `Ordering::SeqCst` is the exact right tool.
//!
//! ## Why `Arc<Mutex<Option<egui::Context>>>` for waking eframe
//! `egui::Context` is `Clone + Send + Sync`.  We share one clone with the
//! tray thread so it can call `ctx.request_repaint()` to wake eframe after
//! changing the AtomicBool.  The `Option` wrapper handles the startup window
//! where the egui context does not yet exist.
//!
//! ## Why `std::process::exit(0)` for Quit
//! `ViewportCommand::Close` is silently dropped by eframe when the window
//! is hidden.  `exit(0)` terminates the process unconditionally.  This is
//! the correct and only reliable quit path from a system tray.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, MenuId},
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

// ── Shared state between OS thread and tray thread ────────────────────────

/// Everything the tray thread and egui thread need to share.
struct Shared {
    /// Desired window visibility.
    /// Written by tray thread via `store`.
    /// Read by egui thread via `load`.
    want_visible: AtomicBool,

    /// Populated by the egui app in `App::new()`; read by tray thread
    /// to call `ctx.request_repaint()` after mutating `want_visible`.
    egui_ctx: Mutex<Option<egui::Context>>,
}

/// Handle held by the egui app.  Calls `apply()` every frame.
pub struct TrayController {
    shared:    Arc<Shared>,
    show_id:   MenuId,
    /// Kept for `set_text()` — called only from the egui (main) thread.
    show_item: MenuItem,
}

impl TrayController {
    /// Register the egui context so the tray thread can wake eframe.
    /// Call once from `App::new()` with `cc.egui_ctx.clone()`.
    pub fn register_egui_ctx(&self, ctx: egui::Context) {
        *self.shared.egui_ctx.lock().unwrap() = Some(ctx);
    }

    /// Read desired visibility, update the menu label, apply viewport commands.
    /// Call at the top of every `App::update()`.
    /// Returns the new `visible` value that the app should use.
    pub fn apply(&self, ctx: &egui::Context, current_visible: bool) -> bool {
        let want = self.shared.want_visible.load(Ordering::SeqCst);

        // Update the menu label to show what clicking WILL DO next.
        self.show_item.set_text(if want { "Hide Window" } else { "Show Window" });

        // Apply the viewport state whenever it differs from what eframe sees.
        if want != current_visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(want));
            if want {
                // Bring the window to the foreground after un-hiding.
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        want
    }

    /// Call when the user hides the window via the in-window button or ✕.
    /// Keeps the AtomicBool in sync so the menu label stays correct.
    pub fn set_hidden(&self) {
        self.shared.want_visible.store(false, Ordering::SeqCst);
    }
}

// ── Factory ───────────────────────────────────────────────────────────────

/// Build the system tray icon and spawn the event-processing thread.
///
/// **Must be called from the OS main thread** (macOS/Windows requirement).
///
/// Keep the returned `TrayIcon` alive for the lifetime of the process.
/// Pass the `TrayController` to `ServerApp::new()`.
pub fn create() -> (TrayIcon, TrayController) {
    let icon = Icon::from_rgba(draw_icon(32), 32, 32)
        .expect("Tray icon pixel data invalid");

    let menu      = Menu::new();
    let show_item = MenuItem::new("Hide Window", true, None);
    let quit_item = MenuItem::new("Quit",        true, None);
    menu.append(&show_item).unwrap();
    menu.append(&quit_item).unwrap();

    let shared = Arc::new(Shared {
        want_visible: AtomicBool::new(true),  // window starts visible
        egui_ctx:     Mutex::new(None),
    });

    let controller = TrayController {
        shared:    Arc::clone(&shared),
        show_id:   show_item.id().clone(),
        show_item,
    };

    // IDs needed by the tray thread; clone before moving into the thread.
    let thread_shared  = Arc::clone(&shared);
    let thread_show_id = controller.show_id.clone();
    let thread_quit_id = quit_item.id().clone();

    // ── Tray event thread ─────────────────────────────────────────────────
    // This thread owns the event polling loop.  It has no dependency on the
    // eframe/egui frame loop — it runs independently at all times.
    std::thread::Builder::new()
        .name("jds-tray-events".into())
        .spawn(move || {
            run_tray_event_loop(
                thread_shared,
                thread_show_id,
                thread_quit_id,
            );
        })
        .expect("Failed to spawn tray event thread");

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("JDS6600 Server")
        .with_icon(icon)
        .build()
        .expect("Failed to create tray icon");

    (tray, controller)
}

// ── Tray event loop (runs in its own thread forever) ──────────────────────

fn run_tray_event_loop(
    shared:    Arc<Shared>,
    show_id:   MenuId,
    quit_id:   MenuId,
) {
    loop {
        // ── Menu events (right-click → item selected) ─────────────────
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == show_id {
                toggle_visibility(&shared);
            } else if ev.id == quit_id {
                // Unconditional process exit — no dependency on eframe.
                std::process::exit(0);
            }
        }

        // ── Tray icon events (direct click on the icon) ───────────────
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { .. } = ev {
                toggle_visibility(&shared);
            }
        }

        // 10 ms poll interval.  Imperceptible to humans (<1 frame @ 60 fps)
        // and negligible CPU cost (~0.01% of one core).
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn toggle_visibility(shared: &Arc<Shared>) {
    let current = shared.want_visible.load(Ordering::SeqCst);
    shared.want_visible.store(!current, Ordering::SeqCst);

    // Wake eframe so App::update() runs and picks up the new AtomicBool value.
    // If the context is not yet registered, the wake is skipped — eframe
    // is still initialising and will pick up the correct value on its own.
    if let Ok(guard) = shared.egui_ctx.lock() {
        if let Some(ctx) = guard.as_ref() {
            ctx.request_repaint();
        }
    }
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
