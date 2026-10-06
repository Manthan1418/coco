// Coco runs with no console: the island is the whole UI.
//
// GDK_BACKEND is forced here, before GTK is touched, because the desktop session
// exports `GDK_BACKEND=wayland,x11` and GTK takes the *first* backend it can —
// so the island became a native Wayland surface and none of the X11 work the
// whole design rests on happened at all:
//
//   * no XID, so the input shape (click-through) silently did nothing and the
//     island swallowed every click meant for the app underneath it;
//   * no `_NET_WM_WINDOW_TYPE_DOCK`, so Hyprland tiled and placed the window as
//     an ordinary client;
//   * no X cursor, so the pointer state the island reads was never available.
//
// Set to `x11` before `run()` — any later is too late, GTK has already chosen.
// XWayland presents these windows to the compositor as ordinary X clients, which
// is what the SHAPE and window-type calls are written against.
fn main() {
    // An explicit `GDK_BACKEND=x11` is respected; anything else (unset, or a
    // value that lists wayland first) is overridden.
    let current = std::env::var("GDK_BACKEND").unwrap_or_default();
    let already_x11 = current
        .split(|c| c == ',' || c == ':')
        .map(str::trim)
        .any(|b| b.eq_ignore_ascii_case("x11"))
        && !current
            .split(|c| c == ',' || c == ':')
            .map(str::trim)
            .any(|b| b.eq_ignore_ascii_case("wayland"));

    if !already_x11 {
        // SAFETY: single-threaded, before any GTK call, so nothing else is
        // reading the environment concurrently.
        unsafe { std::env::set_var("GDK_BACKEND", "x11") };
        if !current.is_empty() {
            coco_lib::log::line(format!(
                "GDK_BACKEND was {current:?} — forced to \"x11\" so input shapes, window type and cursor tracking work"
            ));
        }
    }

    coco_lib::run()
}
