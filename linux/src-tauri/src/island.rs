// Island window: placement on the chosen display, the two window sizes
// (full panel / invisible wake strip), click-through and the cursor poll.
//
// There is no notch on a PC, so the island is a black shape drawn at the top
// centre of the screen inside a borderless, transparent, always-on-top window
// that never takes focus.
//
// Linux specifics, all of them verified by spike/island_spike.py on the target
// machine (Hyprland / XWayland):
//
//   * A NORMAL window gets tiled by the compositor — `move()` is simply
//     ignored and the window ends up wherever the tile happens to be. Setting
//     `_NET_WM_WINDOW_TYPE` to `dock` makes it a panel: exact position held,
//     kept above, out of the taskbar. This is the single most important line
//     in this file.
//   * Click-through is an X *input shape*, set from the island's own rect via
//     the SHAPE extension (`apply_input_shape`). It is deliberately not decided
//     from a polled cursor: on XWayland an empty input shape stops the pointer
//     reaching us, which freezes the poll exactly where it armed — permanently.
//     Tauri's all-or-nothing `set_ignore_cursor_events` remains only as the
//     fallback when SHAPE or X11 is unavailable.
//   * The global cursor is read with XQueryPointer against the XWayland root.
//     Measured on this machine: it is blind while the pointer is over native
//     Wayland windows, so the poll reports position only and never arms or
//     disarms anything. A pure-Wayland session has no X display at all and
//     `cursor_physical()` returns `None`.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewWindow};

/// Logical size of the full window — the largest island view, like the macOS panel.
pub const PANEL_W: f64 = 720.0;
pub const PANEL_H: f64 = 320.0;
/// Logical size of the invisible strip that wakes the island when it is hidden.
pub const STRIP_W: f64 = 240.0;
pub const STRIP_H: f64 = 6.0;

/// A window created smaller than this is never mapped by GTK — it stays
/// `IsUnmapped` with its 10×10 placeholder geometry, invisible to the compositor
/// and unable to composite. Measured on this machine: the strip is 6 px tall and
/// a window created at that size never appeared, while the same window created at
/// panel size mapped and resized to the strip fine. So the *created* size has to
/// be a real one; the strip is only ever a size we resize *to*.
const MIN_MAPPABLE: f64 = 48.0;

pub const WINDOW_LABEL: &str = "island";

/// Margin around the island that still counts as "on the island", in logical px.
/// Wider than the macOS 6 pt because a click must never be swallowed.
const HIT_MARGIN: f64 = 14.0;

/// _NET_WM_WINDOW_TYPE_DOCK — a panel, not a program window.
const NET_WM_WINDOW_TYPE: &str = "_NET_WM_WINDOW_TYPE";
const NET_WM_TYPE_DOCK: &str = "_NET_WM_WINDOW_TYPE_DOCK";
/// Fallback if dock is refused: at least a dialog, which compositors float.
const NET_WM_TYPE_UTILITY: &str = "_NET_WM_WINDOW_TYPE_UTILITY";

#[derive(Serialize, Clone)]
pub struct CursorPayload {
    pub x: f64,
    pub y: f64,
}

#[derive(Serialize, Clone)]
pub struct ScreenInfo {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

/// The island shape in window-logical coordinates, pushed by the front end.
/// It drives the X input shape directly (see `apply_input_shape`), which is why
/// the island keeps working when the global cursor cannot be trusted.
#[derive(Clone, Copy, Default)]
pub struct IslandRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl IslandRect {
    /// Grow by `m` on every side — the same entry margin the cursor test used,
    /// so a button is already reachable by the time the pointer reaches it.
    pub fn expanded(self, m: f64) -> Self {
        Self { x: self.x - m, y: self.y - m, w: self.w + m * 2.0, h: self.h + m * 2.0 }
    }

    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }
}

/// Wakes / parks the cursor poll thread so a hidden island costs literally nothing.
pub struct PollGate {
    active: Mutex<bool>,
    cv: Condvar,
    pub collapsed: AtomicBool,
}

impl PollGate {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(false),
            cv: Condvar::new(),
            collapsed: AtomicBool::new(true),
        }
    }

    pub fn set_active(&self, on: bool) {
        let mut guard = self.active.lock().unwrap();
        *guard = on;
        self.cv.notify_all();
    }

    fn wait_until_active(&self) {
        let mut guard = self.active.lock().unwrap();
        while !*guard {
            guard = self.cv.wait(guard).unwrap();
        }
    }

    fn is_active(&self) -> bool {
        *self.active.lock().unwrap()
    }
}

pub fn window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(WINDOW_LABEL)
}

// ── X11 plumbing ─────────────────────────────────────────────────────────────

use x11rb::connection::Connection as _;
use x11rb::protocol::shape::ConnectionExt as _;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::wrapper::ConnectionExt as _;

/// Our own X connection, opened once and reused. XWayland only: a pure-Wayland
/// session has no X display at all, and every caller below falls back cleanly
/// when this is `None`.
///
/// Cached rather than opened per call because `cursor_physical()` runs at 60 Hz
/// — a fresh connection every tick would be a syscall storm.
static XCONN: Mutex<Option<x11rb::rust_connection::RustConnection>> = Mutex::new(None);

fn with_x<T>(f: impl FnOnce(&x11rb::rust_connection::RustConnection) -> Option<T>) -> Option<T> {
    let mut guard = XCONN.lock().ok()?;
    if guard.is_none() {
        *guard = x11rb::connect(None).ok().map(|(conn, _num)| conn);
    }
    f(guard.as_ref()?)
}

/// The X11 window id of the Tauri window, if we are on X11 at all.
fn xid_of(win: &WebviewWindow) -> Option<u32> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = win.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Xlib(h) => Some(h.window as u32),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        // Wayland: no global coordinates, no input shapes, nothing to poke.
        _ => None,
    }
}

fn intern(conn: &x11rb::rust_connection::RustConnection, name: &str) -> Option<u32> {
    conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok().map(|r| r.atom)
}

fn set_window_type(conn: &x11rb::rust_connection::RustConnection, xid: u32, atom: u32, value: u32) -> bool {
    conn.change_property32(
        x11rb::protocol::xproto::PropMode::REPLACE,
        xid,
        atom,
        x11rb::protocol::xproto::AtomEnum::ATOM,
        &[value],
    )
    .is_ok()
        && conn.flush().is_ok()
}

/// Tell the compositor this is a panel, not a program window.
///
/// Without it a tiling compositor ignores `set_position` entirely and the island
/// appears wherever the tile happens to land — measured, not guessed: a NORMAL
/// window came back at (71,21) sized 895x1010 when asked for (600,0) 720x320.
/// `dock` (with `utility` as the fallback for compositors that refuse a dock)
/// holds the requested geometry exactly. See docs/M0-PLATFORM-FINDINGS.md.
///
/// Applied after the window exists, and safe to call repeatedly: it re-asserts
/// the same property, which is idempotent.
/// Follow the user across workspaces: `_NET_WM_STATE_STICKY` is the EWMH way of
/// saying "on every workspace", which is what "summon from anywhere" needs.
/// Set directly because GTK's `stick()` (what tao's `visibleOnAllWorkspaces`
/// calls) left the atom empty on this machine — measured, not guessed.
///
/// Called everywhere `claim_panel_type` is: idempotent, and re-asserted on
/// every geometry pass because a compositor may rewrite state on map.
pub fn claim_sticky(win: &WebviewWindow) {
    let Some(xid) = xid_of(win) else { return };
    with_x(|conn| {
        let Some(state_atom) = intern(conn, "_NET_WM_STATE") else { return None };
        let Some(sticky) = intern(conn, "_NET_WM_STATE_STICKY") else { return None };
        let current: Vec<u32> = conn
            .get_property(
                false,
                xid,
                state_atom,
                x11rb::protocol::xproto::AtomEnum::ATOM,
                0,
                32,
            )
            .ok()?
            .reply()
            .ok()?
            .value32()
            .map(|v| v.collect())
            .unwrap_or_default();
        if current.contains(&sticky) {
            return Some(());
        }
        let mut next = current;
        next.push(sticky);
        conn.change_property32(
            x11rb::protocol::xproto::PropMode::REPLACE,
            xid,
            state_atom,
            x11rb::protocol::xproto::AtomEnum::ATOM,
            &next,
        )
        .ok()?
        .check()
        .ok()?;
        Some(())
    });
}

pub fn claim_panel_type(win: &WebviewWindow) {
    let Some(xid) = xid_of(win) else { return };
    with_x(|conn| {
        let Some(type_atom) = intern(conn, NET_WM_WINDOW_TYPE) else { return None };
        let Some(dock) = intern(conn, NET_WM_TYPE_DOCK) else { return None };
        if !set_window_type(conn, xid, type_atom, dock) {
            return None;
        }

        // Read it back: a compositor is free to rewrite `_NET_WM_WINDOW_TYPE` on
        // map, and discovering that later would mean a misplaced island.
        let reply = conn
            .get_property(false, xid, type_atom, x11rb::protocol::xproto::AtomEnum::ATOM, 0, 8)
            .ok()?
            .reply()
            .ok()?;
        let accepted = reply.value32().map(|mut v| v.any(|a| a == dock)).unwrap_or(false);
        if !accepted {
            if let Some(utility) = intern(conn, NET_WM_TYPE_UTILITY) {
                set_window_type(conn, xid, type_atom, utility);
                crate::log::line("compositor refused _NET_WM_WINDOW_TYPE_DOCK — using utility".to_string());
            }
        }
        Some(())
    });
}

/// Global cursor position. On X11/XWayland this is XQueryPointer; the pure
/// Wayland path returns `None` and the caller stops polling, leaving the island
/// to react to pointer enter/leave on its own window instead.
///
/// **Known limitation, measured on this machine:** XWayland only learns pointer
/// motion for surfaces that receive it, so this reads a frozen screen-centre
/// value while the pointer is over a native Wayland window. It is still correct
/// while the pointer is over an X window — including our own island. The cursor
/// poll treats a value that never changes as "no information" rather than as a
/// position, so a frozen pointer cannot park the eyes or trap a click-through.
fn cursor_physical() -> Option<(f64, f64)> {
    with_x(|conn| {
        let root = conn.setup().roots.first()?.root;
        let reply = conn.query_pointer(root).ok()?.reply().ok()?;
        Some((reply.root_x as f64, reply.root_y as f64))
    })
}

/// Where the window takes the mouse, in window-logical coordinates.
///
/// **Why this exists instead of deciding from the cursor.** The obvious design
/// — poll the global pointer at 60 Hz, arm click-through when it leaves the
/// island — is self-poisoning on XWayland. An empty input shape means our
/// surface stops receiving pointer focus, which means XWayland stops being told
/// where the pointer is, which means the poll freezes on the last point it saw:
/// *outside* the island. So it never disarms, the pointer can never reach us
/// again, and the island goes permanently dead. Measured: XQueryPointer sits at
/// one frozen value while the pointer moves over native Wayland windows
/// (spike/cursor_probe.py).
///
/// The island's rect is pushed by the front end and cannot lie about where the
/// pointer has been, so it drives the input shape. The cursor poll is left with
/// the one job it can still do honestly — reporting position while the pointer
/// is actually over an X window — and is never allowed to arm or disarm.
/// What X thinks of the island window right now.
///
/// Tauri and GTK disagree with each other here in a way that hides the real
/// fault: `is_visible()` and `outer_size()` describe what GTK *intends*, while a
/// window GTK never mapped is `IsUnmapped` and keeps its 10×10 placeholder
/// geometry. Measured here: `visible=Ok(true)`, `outer=(0,0)` and, on the X
/// server, `IsUnmapped 10x10` — the island was "visible" and invisible at once,
/// which is why `hyprctl clients` never listed it and no summon could show it.
/// Only the X map state tells the truth, so that is what gets logged.
pub fn x_map_state(app: &AppHandle) -> String {
    let Some(win) = window(app) else { return "no window".to_string() };
    let Some(xid) = xid_of(&win) else { return "no xid (not X11)".to_string() };
    with_x(|conn| {
        let attrs = conn.get_window_attributes(xid).ok()?.reply().ok()?;
        let state = match attrs.map_state {
            x11rb::protocol::xproto::MapState::UNMAPPED => "IsUnmapped",
            x11rb::protocol::xproto::MapState::UNVIEWABLE => "IsUnviewable",
            x11rb::protocol::xproto::MapState::VIEWABLE => "IsViewable",
            _ => "unknown",
        };
        // Size is not part of the attributes reply; ask for it separately. An
        // unmapped window keeps its placeholder 10x10 here, which is itself the
        // clearest sign that nothing ever drew it.
        let geom = conn
            .get_geometry(xid)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|g| format!(" {w}x{h}", w = g.width, h = g.height))
            .unwrap_or_default();
        Some(format!("{state}{geom}"))
    })
    .unwrap_or_else(|| "X unavailable".to_string())
}

/// Forgets the X server's idea of the window so a re-`show()` actually re-maps
/// it. `XMapRaised` is a no-op on an already-mapped window, so this is safe to
/// call whenever geometry is (re)applied.
pub fn nudge_map(app: &AppHandle) {
    let Some(win) = window(app) else { return };
    let Some(xid) = xid_of(&win) else { return };
    let _ = with_x(|conn| { conn.map_window(xid).ok()?.check().ok() });
}

pub fn apply_input_shape(app: &AppHandle, rect: IslandRect) {
    let Some(win) = window(app) else { return };
    let r = rect.expanded(HIT_MARGIN);

    if let Some(xid) = xid_of(&win) {
        let applied = with_x(|conn| {
            let cl = |v: f64| v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
            let rects = [x11rb::protocol::xproto::Rectangle {
                x: cl(r.x),
                y: cl(r.y),
                width: cl(r.w).max(0) as u16,
                height: cl(r.h).max(0) as u16,
            }];
            let cookie = conn
                .shape_rectangles(
                    x11rb::protocol::shape::SO::SET,
                    x11rb::protocol::shape::SK::INPUT,
                    x11rb::protocol::xproto::ClipOrdering::UNSORTED,
                    xid,
                    0,
                    0,
                    &rects,
                )
                .ok()?;
            cookie.check().ok()?;
            conn.flush().ok()?;
            Some(())
        });
        if applied.is_some() {
            // Deliberately not logged: this runs on every animation frame the
            // island springs open (verified working — see M0-PLATFORM-FINDINGS).
            // A line per frame would turn the log into an I/O bottleneck.
            return;
        }
        // SHAPE is missing: fall through to the all-or-nothing flag.
        crate::log::line("X SHAPE unavailable — falling back to whole-window click-through".to_string());
    }

    // No X11 at all (pure Wayland): the flag is the only tool there is.
    let _ = win.set_ignore_cursor_events(r.w <= 0.0 || r.h <= 0.0);
}

fn monitor_contains(m: &Monitor, x: f64, y: f64) -> bool {
    let p = m.position();
    let s = m.size();
    x >= p.x as f64
        && x < (p.x + s.width as i32) as f64
        && y >= p.y as f64
        && y < (p.y + s.height as i32) as f64
}

/// The display the island lives on: the primary one, or the one under the cursor.
fn target_monitor(app: &AppHandle, pref: &str) -> Option<Monitor> {
    let monitors = app.available_monitors().ok()?;
    if pref == "cursor" {
        if let Some((cx, cy)) = cursor_physical() {
            if let Some(m) = monitors.iter().find(|m| monitor_contains(m, cx, cy)) {
                return Some(m.clone());
            }
        }
    }
    app.primary_monitor()
        .ok()
        .flatten()
        .or_else(|| monitors.into_iter().next())
}

pub fn screen_info(app: &AppHandle, pref: &str) -> ScreenInfo {
    match target_monitor(app, pref) {
        Some(m) => {
            let scale = m.scale_factor();
            let p = m.position();
            let s = m.size();
            ScreenInfo {
                x: p.x as f64 / scale,
                y: p.y as f64 / scale,
                width: s.width as f64 / scale,
                height: s.height as f64 / scale,
                scale,
            }
        }
        None => ScreenInfo { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0, scale: 1.0 },
    }
}

/// Reserved strip on each edge of a display, in **logical** px
/// `(top, right, bottom, left)`, or `None` when nothing can tell us.
///
/// This matters more than it sounds. The top bar here is a layer-shell surface
/// reserving 60 px, and layer-shell reservations are **not** X11 struts — so
/// `_NET_WORKAREA` (and therefore `gdk_monitor().workarea()`, which is what
/// `Monitor::work_area()` returns) reports the untouched 1920×1080. Measured:
/// `workarea=(0,0) 1920x1080` while the bar genuinely eats y=0..60. Placing the
/// island at the work-area origin buries it under the bar — alive, correctly
/// sized, and completely invisible.
///
/// Hyprland exposes the reservation through `hyprctl monitors -j`, which is the
/// only channel an X11 client has. Where that is unavailable we fall back to the
/// work area, which is right for X11 sessions with real struts and correct
/// (i.e. an empty reservation) for a compositor that reserves nothing.
fn reserved_edges(monitor: Option<&str>) -> Option<(i32, i32, i32, i32)> {
    let name = monitor?;
    let out = std::process::Command::new("hyprctl").args(["-j", "monitors"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let monitors = value.as_array()?;
    for mon in monitors {
        if mon.get("name").and_then(serde_json::Value::as_str) != Some(name) {
            continue;
        }
        let r = mon.get("reserved")?.as_array()?;
        if r.len() < 4 {
            return None;
        }
        let n = |i: usize| r[i].as_i64().unwrap_or(0) as i32;
        return Some((n(0), n(1), n(2), n(3)));
    }
    None
}

/// Places and sizes the window. `collapsed` picks the wake strip instead of the panel.
pub fn apply_geometry(app: &AppHandle, pref: &str, collapsed: bool) {
    let Some(win) = window(app) else { return };
    let Some(m) = target_monitor(app, pref) else { return };

    let scale = m.scale_factor();
    let mp = *m.position();
    let ms = *m.size();
    let wa = m.work_area();
    // `hyprctl` reports logical px; Tauri wants physical. Same on this 1× panel,
    // different the moment a HiDPI display is plugged in.
    let px = |logical: i32| (logical as f64 * scale).round() as i32;

    // The usable region is the monitor minus whatever reserves it — see
    // `reserved_edges` for why the work area alone is not enough here.
    let (ax, ay, aw, ah) = match reserved_edges(m.name().map(String::as_str)) {
        Some((top, right, bottom, left)) => (
            mp.x + px(left),
            mp.y + px(top),
            ms.width as i32 - px(left) - px(right),
            ms.height as i32 - px(top) - px(bottom),
        ),
        None => (wa.position.x, wa.position.y, wa.size.width as i32, wa.size.height as i32),
    };

    // The strip is what the front end *draws* and what the input shape covers,
    // but the window itself keeps a mappable height: a 6 px window is never
    // mapped, and an unmapped window cannot composite or take input at all. The
    // extra pixels are transparent (`html`/`body` have no background), so this
    // is invisible — see MIN_MAPPABLE.
    let (lw, lh) = if collapsed {
        (STRIP_W, STRIP_H.max(MIN_MAPPABLE))
    } else {
        (PANEL_W, PANEL_H)
    };
    let pw = (lw * scale).round().max(1.0) as u32;
    let ph = (lh * scale).round().max(1.0) as u32;
    // Centred inside the usable region, not the raw monitor, so a side panel
    // that shifts the usable area also shifts the island with it.
    let x = ax + (aw - pw as i32) / 2;
    let y = ay;

    crate::log::line(format!(
        "geometry collapsed={collapsed} monitor=({},{}) {}x{} usable=({},{}) {}x{} -> ({},{}) {}x{}",
        mp.x, mp.y, ms.width, ms.height, ax, ay, aw, ah, x, y, pw, ph
    ));

    let size_res = win.set_size(PhysicalSize::new(pw, ph));
    let pos_res = win.set_position(PhysicalPosition::new(x, y));
    // Moving across displays can rescale the window: re-assert the physical size.
    let _ = win.set_size(PhysicalSize::new(pw, ph));
    let _ = win.set_always_on_top(true);
    // A panel stays out of Alt-Tab and the taskbar.
    let _ = win.set_skip_taskbar(true);
    // Both orderings reach here — the window is built `visible: true`, but the
    // collapsed strip is 6 px tall and GTK declines to map a window that small,
    // which leaves it IsUnmapped and invisible to the compositor entirely.
    // Showing it only after the real geometry is set gives the map something to
    // work with; `show()` on an already-visible window is a no-op, so this is
    // safe to repeat on every reposition.
    let _ = win.show();
    // `show()` alone proved insufficient on this machine — GTK left the window
    // IsUnmapped even though it reported itself visible. Mapping it explicitly
    // makes the state match the intent, and XMapRaised on an already-mapped
    // window is a no-op, so there is no cost when it already worked.
    nudge_map(app);
    claim_panel_type(&win);
    claim_sticky(&win);

    // Is the window actually on screen? `set_size`/`set_position` return Ok for
    // an unmapped window, so a success here proves nothing on its own — a
    // compositor can drop a window while every call we make still "works".
    crate::log::line(format!(
        "window-state visible={:?} foc={:?} outer={:?} inner={:?} size_err={:?} pos_err={:?} x={}",
        win.is_visible(),
        win.is_focused().ok(),
        win.outer_size().ok(),
        win.inner_size().ok(),
        size_res.err().map(|e| e.to_string()),
        pos_res.err().map(|e| e.to_string()),
        // The X map state is the only field here that tells the truth about
        // whether the island is actually on screen.
        x_map_state(app),
    ));
}

/// WS_EX_NOACTIVATE keeps clicks from stealing focus; WS_EX_TOOLWINDOW keeps the
/// island out of Alt-Tab. On Linux the equivalent is `set_focusable`, which
/// tao implements as GTK's `set_focusable`.
pub fn make_non_activating(win: &WebviewWindow) {
    let _ = win.set_focusable(false);
}

/// Temporarily allow activation so a text field inside the island can be typed in.
pub fn set_activating(win: &WebviewWindow, activating: bool) {
    let _ = win.set_focusable(activating);
}

/// Position, size and scale of the monitor the island lives on. Any change here
/// means the island has to be placed again.
fn current_screen_key(app: &AppHandle) -> Option<(i32, i32, u32, u32, u64)> {
    let pref = app
        .try_state::<crate::Shared>()
        .map(|s| s.settings.lock().unwrap().screen.clone())
        .unwrap_or_else(|| "primary".into());
    let m = target_monitor(app, &pref)?;
    let p = m.position();
    let size = m.size();
    Some((p.x, p.y, size.width, size.height, m.scale_factor().to_bits()))
}

/// Emits `cursor` (window-logical coordinates) at ~60 Hz while the island is
/// visible. Parked on a condvar the rest of the time.
///
/// Deliberately *not* responsible for click-through — see `apply_input_shape`.
/// All it does is report a position when XWayland actually has one; a value that
/// stops changing is skipped rather than repeated, so a frozen pointer can never
/// be mistaken for a real one.
/// Watchdog for the vanishing-island fault: the island's X toplevel has been
/// observed gone mid-process (mapped and healthy at one check, absent from the
/// root's children at the next) while everything else — WebKit, SSE, pipe,
/// tray — kept running. Nothing in our code closes it, so the cause is still
/// unknown; this ticker exists to timestamp the next loss and correlate it
/// with whatever the user was doing, instead of leaving another silent gap in
/// the log between "healthy" and "gone".
///
/// Slow on purpose: 10 s granularity is plenty for correlation, and a lost
/// window stays lost — there is nothing to recover, only to report.
pub fn spawn_window_watch(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        let gone = match window(&app) {
            None => true,
            Some(_) => {
                let state = x_map_state(&app);
                state == "no xid (not X11)" || state == "X unavailable"
            }
        };
        if gone {
            // Forensics, not just an alarm: the trigger is still unknown, so
            // capture which workspace was focused when it died. Shelling out
            // from a 5 s ticker is fine — this branch runs once per loss.
            let workspace = std::process::Command::new("hyprctl")
                .args(["activeworkspace", "-j"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_else(|| "unknown".to_string());
            crate::log::line(format!(
                "ISLAND WINDOW LOST (focused workspace: {}) — the toplevel is gone \
                 but the process lives on. If Super+C does nothing, restart Coco.",
                workspace.trim(),
            ));
            // One line per loss, not one per 5 s: the state persists until a
            // restart, and a flooded log would bury the timestamp that matters.
            std::thread::sleep(Duration::from_secs(300));
        }
    });
}

pub fn spawn_cursor_poll(app: AppHandle, gate: Arc<PollGate>) {
    std::thread::spawn(move || {
        // Remembered across wakes so a display change while hidden is noticed the
        // moment the island comes back.
        let mut last_screen: Option<(i32, i32, u32, u32, u64)> = None;
        loop {
            gate.wait_until_active();
            let mut last = (f64::MIN, f64::MIN);
            let mut ticks: u32 = 0;
            while gate.is_active() {
                std::thread::sleep(Duration::from_millis(16));

                ticks = ticks.wrapping_add(1);
                if ticks % 30 == 0 {
                    let now = current_screen_key(&app);
                    if now.is_some() && now != last_screen {
                        let first = last_screen.is_none();
                        last_screen = now;
                        if !first {
                            crate::log::line("display layout changed — repositioning".to_string());
                            let _ = app.emit_to(WINDOW_LABEL, "screen-changed", ());
                        }
                    }
                }

                let Some(win) = window(&app) else { continue };
                let Ok(origin) = win.outer_position() else { continue };
                let scale = win.scale_factor().unwrap_or(1.0);
                let Some((cx, cy)) = cursor_physical() else {
                    // No X at all (pure Wayland): nothing to report, and nothing
                    // downstream depends on it — the island is interactive from
                    // its own input shape.
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                let x = (cx - origin.x as f64) / scale;
                let y = (cy - origin.y as f64) / scale;
                // XWayland only learns pointer motion for surfaces that receive
                // it, so this freezes while the pointer is over native Wayland
                // windows. A position that never changes is not a position — it
                // is the absence of one — so drop it instead of feeding the
                // front end a stale point forever.
                if (x - last.0).abs() < 1.0 && (y - last.1).abs() < 1.0 {
                    continue;
                }
                last = (x, y);

                let _ = win.emit("cursor", CursorPayload { x, y });
            }
        }
    });
}
