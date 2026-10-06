// The global hotkey that summons the island from anywhere.
//
// `Super+C` is free on this machine (checked against the Hyprland binds —
// Super+Shift+C is the colour picker, plain Super+C is unassigned) and does not
// collide with the usual suspects elsewhere. Under XWayland the shortcut plugin
// grabs the key at the X server, so it works whatever has focus.
//
// The value lives in settings and is parsed by tauri-plugin-global-shortcut, so
// anything it understands (`Super+C`, `Ctrl+Alt+Space`, …) can be typed in the
// settings window. An unparsable value falls back to the default instead of
// leaving Coco unsummonable with no message.

use tauri::{AppHandle, Emitter};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

/// What a fresh install (or a settings.json from before hotkeys) summons with.
pub const DEFAULT_HOTKEY: &str = "Super+C";

/// (Re)registers the summon hotkey. Clears any previous registration first so a
/// changed value never leaves two hotkeys armed.
pub fn register(app: &AppHandle, hotkey: &str) {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let chosen = if hotkey.trim().is_empty() { DEFAULT_HOTKEY } else { hotkey };
    match gs.register(chosen) {
        Ok(()) => crate::log::line(format!("hotkey registered: {chosen}")),
        Err(err) => {
            // Whatever went wrong (taken by the compositor, unparsable), fall
            // back to the default rather than leaving no way to summon.
            crate::log::line(format!("hotkey {chosen} failed ({err}) — trying {DEFAULT_HOTKEY}"));
            if chosen != DEFAULT_HOTKEY {
                match gs.register(DEFAULT_HOTKEY) {
                    Ok(()) => crate::log::line(format!("hotkey registered: {DEFAULT_HOTKEY}")),
                    Err(err) => crate::log::line(format!("hotkey {DEFAULT_HOTKEY} failed too: {err}")),
                }
            }
        }
    }
}

/// What pressing the hotkey does: exactly what the tray's "Open Coco" does —
/// the island wakes with its default view. One summon path, not two.
pub fn summon(app: &AppHandle) {
    use crate::island::WINDOW_LABEL;
    crate::log::line("hotkey pressed — summoning island".to_string());
    let _ = app.emit_to(WINDOW_LABEL, "tray", "open".to_string());
}
