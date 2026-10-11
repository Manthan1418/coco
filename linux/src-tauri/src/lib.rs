// Coco for Linux — app wiring and the commands the island calls.

mod claude;
mod files;
mod hotkey;
mod hooks;
mod integrations;
mod island;
pub mod log;
mod mail;
mod mcp_call;
mod pipe;
mod secrets;
mod settings;
mod tray;
// The OpenCode brain. Separate module, separate transport: see brain.rs for
// why it shares nothing with the Claude Code pipe relay.
mod brain;

use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{ManagerExt, MacosLauncher};

use claude::Chat;
use files::DroppedFile;
use hooks::{HookPreview, HookStatus};
use island::{PollGate, ScreenInfo};
use pipe::Pending;
use settings::Settings;

pub struct Shared {
    pub settings: Mutex<Settings>,
    pub gate: Arc<PollGate>,
    /// Last island shape pushed by the front end. Stored so re-expanding can
    /// restore it instantly instead of falling back to "the whole window eats
    /// every click" for a frame.
    pub rect: Mutex<island::IslandRect>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootInfo {
    settings: Settings,
    screen: ScreenInfo,
    version: String,
    hook_path: String,
}

#[tauri::command]
fn boot(app: AppHandle, shared: State<Shared>) -> BootInfo {
    let mut settings = shared.settings.lock().unwrap().clone();
    // The real state of ~/.claude/settings.json wins over whatever we stored.
    settings.hooks_installed = hooks::status().installed;
    let screen = island::screen_info(&app, &settings.screen);
    BootInfo {
        settings,
        screen,
        version: env!("CARGO_PKG_VERSION").to_string(),
        hook_path: settings::hook_exe_path().to_string_lossy().to_string(),
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, shared: State<Shared>, settings: Settings) {
    let (screen_changed, autostart_changed, hotkey_changed) = {
        let mut current = shared.settings.lock().unwrap();
        let screen_changed = current.screen != settings.screen;
        let autostart_changed = current.autostart != settings.autostart;
        let hotkey_changed = current.hotkey != settings.hotkey;
        *current = settings.clone();
        (screen_changed, autostart_changed, hotkey_changed)
    };
    if let Err(err) = settings::save(&settings) {
        eprintln!("[coco] could not save settings: {err}");
    }
    if autostart_changed {
        let manager = app.autolaunch();
        let result = if settings.autostart { manager.enable() } else { manager.disable() };
        if let Err(err) = result {
            eprintln!("[coco] autostart: {err}");
        }
    }
    if screen_changed {
        let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
        island::apply_geometry(&app, &settings.screen, collapsed);
    }
    if hotkey_changed {
        hotkey::register(&app, &settings.hotkey);
    }
    // Keep the other window in step (island ⇄ settings window).
    let _ = app.emit("settings-changed", settings);
}

/// Hidden island → shrink the window to the invisible wake strip and park the
/// cursor poll; anything else → full panel and 60 Hz polling.
#[tauri::command]
fn set_collapsed(app: AppHandle, shared: State<Shared>, collapsed: bool) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    shared.gate.collapsed.store(collapsed, Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);

    // The input shape comes from the window's role, never from a cursor read:
    // while hidden the window *is* the wake strip, so all of it takes the mouse;
    // while shown it takes the mouse only over the island the front end drew.
    let shape = if collapsed {
        island::IslandRect { x: 0.0, y: 0.0, w: island::STRIP_W, h: island::STRIP_H }
    } else {
        let r = *shared.rect.lock().unwrap();
        if r.is_empty() {
            // No shape pushed yet (first expand): take the panel rather than a
            // 28 px blob in the corner — the real rect lands a frame later.
            island::IslandRect { x: 0.0, y: 0.0, w: island::PANEL_W, h: island::PANEL_H }
        } else {
            r
        }
    };
    island::apply_input_shape(&app, shape);
    shared.gate.set_active(!collapsed);
}

/// The front end pushes the island shape; the X input shape follows it.
#[tauri::command]
fn set_island_rect(app: AppHandle, shared: State<Shared>, x: f64, y: f64, width: f64, height: f64) {
    let r = island::IslandRect { x, y, w: width, h: height };
    *shared.rect.lock().unwrap() = r;
    if !shared.gate.collapsed.load(Ordering::Relaxed) {
        island::apply_input_shape(&app, r);
    }
}

#[tauri::command]
fn focus_window(app: AppHandle, focused: bool) {
    let Some(win) = island::window(&app) else { return };
    island::set_activating(&win, focused);
    if focused {
        let _ = win.set_focus();
    }
    // GTK only lets widgets be touched on the main thread.
    let w = win.clone();
    let _ = app.run_on_main_thread(move || island::gtk_take_focus(&w, focused));
}

#[tauri::command]
fn reposition(app: AppHandle, shared: State<Shared>) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
}

#[tauri::command]
fn open_url(url: String) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    // xdg-open is the freedesktop equivalent of the rundll32 call upstream used:
    // one spawn, no shell, and the path/URL stays a single argument.
    if let Ok(code) = Command::new("xdg-open").arg(&url).spawn().map(|mut c| c.try_wait()) {
        let _ = code;
        return;
    }
    // Minimal fallbacks in case xdg-open is missing from a stripped install.
    for browser in ["gio", "gnome-open", "kde-open"] {
        let mut cmd = Command::new(browser);
        if browser == "gio" {
            cmd.arg("open");
        }
        if cmd.arg(&url).spawn().is_ok() {
            return;
        }
    }
}

/// "Open terminal" opens the working folder in VS Code when `code` is on PATH,
/// and falls back to the file manager otherwise.
#[tauri::command]
fn open_in_vscode(path: Option<String>) -> bool {
    // No shell anywhere near this. The path is a project folder chosen by
    // whoever is using Claude Code, and a shell would happily read `&`, `;` and
    // `$` in a folder name as syntax. Finding the launcher ourselves and handing
    // the path over as a separate argument keeps it a path.
    if let Some(code) = find_on_path("code") {
        let mut cmd = Command::new(code);
        if let Some(p) = path.as_deref().filter(|p| !p.is_empty()) {
            cmd.arg(p);
        }
        if cmd.spawn().is_ok() {
            return true;
        }
    }
    if let Some(p) = path.as_deref().filter(|p| !p.is_empty()) {
        let _ = Command::new("xdg-open").arg(p).spawn();
    }
    false
}

/// Our own `command -v`: walks $PATH looking for an executable of exactly that
/// name. No PATHEXT and no shell on Linux — an entry must be executable.
fn find_on_path(stem: &str) -> Option<std::path::PathBuf> {
    let dirs = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&dirs) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(stem);
        // is_file() is true for a symlink to a file too, which is how distros
        // ship /usr/bin/code; the executable bit is what actually matters.
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Tray → Pause. Paused means paused: the pollers stop talking to the network,
/// not just the island stopping showing things.
#[tauri::command]
fn set_paused(paused: bool) {
    integrations::set_paused(paused);
}

// ── Claude Code hooks ─────────────────────────────────────────────────────────

#[tauri::command]
fn hooks_status() -> HookStatus {
    hooks::status()
}

/// Returns the diff the user has to look at before anything is written.
#[tauri::command]
fn hooks_preview(install: bool) -> Result<HookPreview, String> {
    hooks::preview(install)
}

/// Only ever called from an explicit click in the settings window.
#[tauri::command]
fn hooks_apply(
    app: AppHandle,
    shared: State<Shared>,
    install: bool,
    fingerprint: String,
) -> Result<String, String> {
    // The fingerprint comes from the preview the user actually looked at, so a
    // settings.json that changed in between is refused rather than overwritten.
    let backup = hooks::write(install, &fingerprint)?;
    let updated = {
        let mut current = shared.settings.lock().unwrap();
        current.hooks_installed = install;
        let _ = settings::save(&current);
        current.clone()
    };
    let _ = app.emit("settings-changed", updated);
    Ok(backup)
}

#[tauri::command]
fn approval_decision(app: AppHandle, request_id: String, decision: String) {
    pipe::answer(&app, &request_id, &decision);
}

/// The island has the card on screen, so the long wait for a human may begin.
/// Until this arrives the relay only waits a few hundred milliseconds, which is
/// what stops a paused or unresponsive island from freezing Claude Code.
#[tauri::command]
fn approval_ack(app: AppHandle, request_id: String) {
    pipe::acknowledge(&app, &request_id);
}

/// Nobody can act on this request — the island is paused, or another card is
/// already up. Claude Code falls back to asking in the terminal immediately.
#[tauri::command]
fn approval_decline(app: AppHandle, request_id: String) {
    pipe::decline(&app, &request_id);
}

// ── Chat, files and secrets ───────────────────────────────────────────────────
// chat_send / chat_reset live in brain.rs now: every island message is an
// OpenCode turn. The direct-provider path (claude.rs) is superseded but kept
// compiled — the settings screen still stores provider keys, and ripping the
// module out would take the Chat/ChatContext types with it.

/// Copies a dropped file into the inbox and reports its name back.
#[tauri::command]
fn ingest_file(path: String) -> Result<DroppedFile, String> {
    files::ingest(&path)
}

/// The island may only ask whether a key exists — never read it.
#[tauri::command]
fn secret_present(key: String) -> bool {
    secrets::present(&key)
}

#[tauri::command]
fn secret_set(key: String, value: String) -> Result<(), String> {
    secrets::set(&key, &value)
}

#[tauri::command]
fn secret_clear(key: String) -> Result<(), String> {
    secrets::clear(&key)
}

/// Opens the configured n8n instance — the URL lives in the Secret Service.
#[tauri::command]
fn open_n8n() {
    if let Some(url) = secrets::get("n8n-url") {
        open_url(url);
    }
}

/// Refresh buttons in the integration cards.
#[tauri::command]
async fn refresh_integration(app: AppHandle, id: String) {
    integrations::poll_once(app, &id).await;
}

/// Lets the island write to the same log as the Rust side.
#[tauri::command]
fn log_line(message: String) {
    log::line(format!("ui  {message}"));
}

// ── Settings window ───────────────────────────────────────────────────────────

/// In a dev build the pages are served by Vite, so the second window needs the
/// absolute dev URL; a bundled build resolves it inside the app resources.
fn settings_page_url(app: &AppHandle) -> WebviewUrl {
    #[cfg(dev)]
    if let Some(mut base) = app.config().build.dev_url.clone() {
        base.set_path("/settings.html");
        return WebviewUrl::External(base);
    }
    let _ = app;
    WebviewUrl::App("settings.html".into())
}

/// The settings window is created hidden at launch and only ever shown and
/// hidden afterwards, so closing it can never destroy it.
fn create_settings_window(app: &AppHandle) {
    let url = settings_page_url(app);
    match WebviewWindowBuilder::new(app, "settings", url)
        .title("Settings — Coco")
        .inner_size(560.0, 680.0)
        .min_inner_size(460.0, 480.0)
        .resizable(true)
        .visible(false)
        .center()
        .build()
    {
        Ok(win) => {
            // Closing it must only hide it, or it could never be reopened.
            let hidden = win.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = hidden.hide();
                }
            });
        }
        Err(err) => log::line(format!("settings window failed: {err}")),
    }
}

pub fn show_settings_window(app: &AppHandle) {
    let Some(win) = app.get_webview_window("settings") else {
        log::line("settings window missing");
        return;
    };
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();
}

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    show_settings_window(&app);
}

pub fn run() {
    let loaded = settings::load();
    let gate = Arc::new(PollGate::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A compositor bind (`bind = SUPER, C, exec, coco`) lands here: the
            // second process exits and the running one opens. This is the summon
            // path that actually works under Wayland, where an X11 key grab is
            // only consulted for X clients.
            crate::log::line("second instance requested — summoning island".to_string());
            let _ = app.emit_to(island::WINDOW_LABEL, "tray", "open".to_string());
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        hotkey::summon(app);
                    }
                })
                .build(),
        )
        .manage(Shared {
            settings: Mutex::new(loaded.clone()),
            gate: gate.clone(),
            rect: Mutex::new(island::IslandRect::default()),
        })
        .manage(Pending::default())
        .manage(Chat::default())
        .manage(brain::LocalPending::default())
        .invoke_handler(tauri::generate_handler![
            boot,
            save_settings,
            set_collapsed,
            set_island_rect,
            focus_window,
            reposition,
            open_url,
            open_in_vscode,
            quit_app,
            hooks_status,
            hooks_preview,
            hooks_apply,
            approval_decision,
            approval_ack,
            approval_decline,
            log_line,
            brain::chat_send,
            brain::chat_reset,
            brain::chat_cancel,
            brain::local_decide,
            brain::chat_sessions,
            brain::chat_open,
            brain::chat_rename,
            brain::chat_delete,
            ingest_file,
            mail::mail_send,
            secret_present,
            secret_set,
            secret_clear,
            refresh_integration,
            open_n8n,
            open_settings_window,
            set_paused,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            tray::build(&handle)?;
            // Before the island: see create_settings_window.
            create_settings_window(&handle);

            if let Some(win) = island::window(&handle) {
                island::make_non_activating(&win);
                island::apply_geometry(&handle, &loaded.screen, false);
                let _ = win.show();
                // Which document did the webview actually get? A release binary
                // built without the `custom-protocol` feature (a bare
                // `cargo build --release`) is silently treated as a dev build and
                // loads `devUrl` — the Vite server — instead of the embedded
                // assets. The island is then blank unless `npm run dev` happens
                // to be running, and every hook goes unacknowledged. That failure
                // mode is far too quiet, so name it here.
                if let Ok(url) = win.url() {
                    let dev = app
                        .config()
                        .build
                        .dev_url
                        .as_ref()
                        .is_some_and(|d| url.as_str().starts_with(d.as_str()));
                    if dev {
                        log::line(format!(
                            "WARNING island loaded {url} (Vite dev server). This binary has no \
                             embedded frontend — build with `tauri build`/`npm run pack` \
                             (or pass --features custom-protocol), not a bare cargo build."
                        ));
                    } else {
                        log::line(format!("island loaded {url}"));
                    }
                }
                // The settings window already refuses to close; the island must
                // too, and until now it did not. A close request from the
                // compositor (a Super+Q over a focused island, a window rule,
                // WM_DELETE from anywhere) destroyed the only visible window
                // and left a process nobody could summon — the "ISLAND WINDOW
                // LOST" entries in the log. Refuse it, and say so: whether this
                // line ever appears is what tells us the trigger was a close
                // request at all.
                win.on_window_event(move |event| match event {
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        log::line("island close request refused — the island hides, it does not die");
                    }
                    tauri::WindowEvent::Destroyed => {
                        log::line("island window destroyed by GTK/compositor — not a close request");
                    }
                    _ => {}
                });
            }
            gate.collapsed.store(false, Ordering::Relaxed);
            gate.set_active(true);
            island::spawn_cursor_poll(handle.clone(), gate.clone());
            island::spawn_window_watch(handle.clone());

            log::line(format!("--- Coco {} started ---", env!("CARGO_PKG_VERSION")));
            hooks::ensure_hook_exe(&handle);
            brain::ensure_mcp(&handle);
            brain::ensure_omnirouter();
            pipe::start(handle.clone());
            integrations::start(handle.clone());
            hotkey::register(&handle, &loaded.hotkey);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Coco");
}
