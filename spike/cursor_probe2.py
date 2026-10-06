#!/usr/bin/env python3
"""Coco spike v3 — the one question left: does XWayland know the pointer when it
is over *our own* X window?

This decides the whole click-through design.

    XQueryPointer can only report what XWayland has been told, and XWayland is
    only told about pointer motion for surfaces that receive pointer focus. An
    empty input shape means our window gets no focus, so:

      * if XQueryPointer updates while the island is INTERACTIVE (phase A),
        XWayland does see us, and we can build click-through on it;
      * if it also updates with click-through ARMED (phase B), we are fine
        either way;
      * if phase A works and phase B does not, we have the classic trap:
        armed = blind, so we could never detect the pointer coming back and
        the island would go permanently dead.

Compares against `hyprctl cursorpos` throughout as the ground truth.

Run it and move the mouse ON AND OFF THE ISLAND WINDOW during each phase.
"""
import ctypes
import ctypes.util
import os
import subprocess
import time

os.environ["GDK_BACKEND"] = "x11"  # before gi loads; session default is wayland

import gi  # noqa: E402

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gtk, Gdk, GLib  # noqa: E402

import cairo  # noqa: E402

PANEL_W, PANEL_H = 720, 320
PHASE_SECS = 6.0

lib = ctypes.CDLL(ctypes.util.find_library("X11"))
lib.XOpenDisplay.restype = ctypes.c_void_p
lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
lib.XDefaultRootWindow.restype = ctypes.c_ulong
lib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
lib.XQueryPointer.restype = ctypes.c_int
lib.XQueryPointer.argtypes = [
    ctypes.c_void_p, ctypes.c_ulong,
    ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
    ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(ctypes.c_uint),
]
dpy = lib.XOpenDisplay(None)
root = lib.XDefaultRootWindow(dpy)


def x_ptr():
    rr, cr = ctypes.c_ulong(), ctypes.c_ulong()
    rx, ry, wx, wy = (ctypes.c_int() for _ in range(4))
    mask = ctypes.c_uint()
    ok = lib.XQueryPointer(dpy, root, ctypes.byref(rr), ctypes.byref(cr),
                           ctypes.byref(rx), ctypes.byref(ry),
                           ctypes.byref(wx), ctypes.byref(wy), ctypes.byref(mask))
    return (rx.value, ry.value) if ok else None


def hyp_ptr():
    try:
        out = subprocess.run(["hyprctl", "cursorpos"], capture_output=True,
                             text=True, timeout=1).stdout.strip()
        parts = [p.strip() for p in out.replace(",", " ").split()]
        return (int(parts[0]), int(parts[1]))
    except Exception:
        return None


class Probe:
    def __init__(self):
        self.win = Gtk.Window(type=Gtk.WindowType.TOPLEVEL)
        self.win.set_decorated(False)
        self.win.set_app_paintable(True)
        self.win.set_title("coco-cursor-probe")
        self.win.set_type_hint(Gdk.WindowTypeHint.DOCK)
        self.win.set_keep_above(True)
        self.win.set_size_request(PANEL_W, PANEL_H)
        screen = Gdk.Screen.get_default()
        visual = screen.get_rgba_visual()
        if visual:
            self.win.set_visual(visual)
        self.win.connect("draw", self.on_draw)
        # Do we receive our own pointer motion? This is independent of
        # XQueryPointer and is what a GTK-event-driven design would rely on.
        self.win.add_events(Gdk.EventMask.POINTER_MOTION_MASK
                            | Gdk.EventMask.ENTER_NOTIFY_MASK
                            | Gdk.EventMask.LEAVE_NOTIFY_MASK)
        self.win.connect("motion-notify-event", self.on_motion)
        self.win.connect("enter-notify-event", self.on_enter)
        self.win.connect("leave-notify-event", self.on_leave)
        self.gtk_motion = 0
        self.gtk_enter = 0
        self.gtk_leave = 0
        self.label = Gtk.Label(label="probe")
        self.win.add(self.label)
        self.win.connect("destroy", Gtk.main_quit)

    def on_draw(self, widget, cr):
        cr.set_source_rgba(0, 0, 0, 0)
        cr.set_operator(cairo.OPERATOR_SOURCE)
        cr.paint()
        cr.set_operator(cairo.OPERATOR_OVER)
        cr.set_source_rgba(0.8, 0.2, 0.2, 0.9)   # visible = interactive phase
        cr.rectangle(0, 0, PANEL_W, PANEL_H)
        cr.fill()
        return False

    def on_motion(self, *_a):
        self.gtk_motion += 1
        return False

    def on_enter(self, *_a):
        self.gtk_enter += 1
        return False

    def on_leave(self, *_a):
        self.gtk_leave += 1
        return False

    def set_click_through(self, on):
        w = self.win.get_window()
        if on:
            w.input_shape_combine_region(cairo.Region(), 0, 0)
        else:
            aw = self.win.get_allocated_width()
            ah = self.win.get_allocated_height()
            w.input_shape_combine_region(
                cairo.Region(cairo.RectangleInt(0, 0, aw, ah)), 0, 0)

    def place(self):
        s = Gdk.Screen.get_default()
        geo = s.get_monitor_geometry(s.get_primary_monitor())
        self.win.move(geo.x + (geo.width - PANEL_W) // 2, geo.y)


def sample(label, click_through, probe, results):
    """Samples XQueryPointer + hyprctl for PHASE_SECS."""
    probe.set_click_through(click_through)
    probe.gtk_motion = probe.gtk_enter = probe.gtk_leave = 0
    print(f"\n--- {label}: click_through={click_through} for {PHASE_SECS:.0f}s ---")
    print("    move the mouse ONTO the red window, around, and OFF it, repeatedly.")

    xs, hs = [], []
    t_end = time.time() + PHASE_SECS
    while time.time() < t_end:
        x = x_ptr()
        h = hyp_ptr()
        xs.append(x)
        hs.append(h)
        time.sleep(0.05)

    xu = {p for p in xs if p}
    hu = {p for p in hs if p}
    # Is X ever *inside* the window, and does it ever change?
    results[label] = {
        "x_unique": len(xu),
        "hyp_unique": len(hu),
        "gtk_motion": probe.gtk_motion,
        "gtk_enter": probe.gtk_enter,
        "gtk_leave": probe.gtk_leave,
        "x_samples": sorted(xu)[:6],
    }
    print(f"    XQueryPointer : {len(xs)} samples, {len(xu)} unique  e.g. {sorted(xu)[:4]}")
    print(f"    hyprctl       : {len(hs)} samples, {len(hu)} unique  e.g. {sorted(hu)[:4]}")
    print(f"    GTK events    : motion={probe.gtk_motion} "
          f"enter={probe.gtk_enter} leave={probe.gtk_leave}")


def main():
    results = {}
    probe = Probe()
    probe.win.show_all()
    probe.place()
    print("window placed; waiting 1s to map…")
    t = time.time()
    while time.time() - t < 1.0:
        Gtk.main_iteration_do(False)
        time.sleep(0.02)

    sample("A_interactive", False, probe, results)

    t = time.time()
    while time.time() - t < 1.0:
        Gtk.main_iteration_do(False)
        time.sleep(0.02)

    sample("B_clickthrough", True, probe, results)

    print("\n" + "=" * 68)
    print("CURSOR / CLICK-THROUGH VERDICT")
    print("=" * 68)
    a = results["A_interactive"]
    b = results["B_clickthrough"]

    checks = [
        ("A: XQueryPointer updates over our own window (interactive)",
         a["x_unique"] > 3),
        ("A: GTK receives our own pointer motion",
         a["gtk_motion"] > 5),
        ("B: XQueryPointer still updates (click-through armed)",
         b["x_unique"] > 3),
        ("B: GTK still receives motion (click-through armed)",
         b["gtk_motion"] > 5),
        ("ground truth: hyprctl moved in both phases",
         a["hyp_unique"] > 3 and b["hyp_unique"] > 3),
    ]
    for name, ok in checks:
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}")

    print("\nInterpretation:")
    if a["x_unique"] > 3 and b["x_unique"] > 3:
        print("  → XWayland sees the pointer even while click-through is armed.")
        print("    A global-cursor design works. Eyes-follow-mouse is still")
        print("    limited to positions XWayland knows about.")
    elif a["x_unique"] > 3 and b["x_unique"] <= 3:
        print("  → THE TRAP. X sees us only while interactive; once armed we are")
        print("    blind and could never detect the pointer returning. Do NOT")
        print("    build click-through on XQueryPointer. Drive it from GTK motion")
        print("    events on the island window instead.")
    else:
        print("  → XQueryPointer never sees our window at all. X11 global cursor")
        print("    is unusable under this compositor; use hyprctl/GTK events only.")

    if b["gtk_motion"] > 5:
        print("  → GTK motion survives click-through: an event-driven design is viable.")
    elif a["gtk_motion"] > 5:
        print("  → GTK motion only arrives while interactive: normal, and expected.")
    else:
        print("  → GTK delivered no motion at all — check the event mask.")

    print("=" * 68)
    probe.win.destroy()


if __name__ == "__main__":
    print(f"GDK_BACKEND={os.environ['GDK_BACKEND']}  DISPLAY={os.environ.get('DISPLAY')}")
    GLib.timeout_add(300, lambda: (main(), False)[1])
    Gtk.main()
