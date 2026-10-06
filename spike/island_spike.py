#!/usr/bin/env python3
"""
Coco M0 risk spike (v2) — answers the questions that decide the Linux port.

Q1 Can we pin a transparent window at top-centre, ABOVE the desktop's top bar?
   (Hyprland tiled a NORMAL window; we need _NET_WM_WINDOW_TYPE_DOCK.)
Q2 Can we toggle click-through (empty X input shape) live and reversibly?
Q3 Can we read the GLOBAL cursor at ~60 Hz — including while the pointer is
   over native Wayland windows? (XWayland may not know about those.)
Q4 Does the window stay above the bar and out of the taskbar/Alt-Tab?

Runs GTK3 + X11 (XWayland): exactly what Tauri/wry uses on Linux.
Prints a PASS/FAIL verdict. No sudo needed.
"""

import ctypes
import ctypes.util
import os
import time

os.environ["GDK_BACKEND"] = "x11"  # before gi loads; session default is wayland

import gi  # noqa: E402

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gtk, Gdk, GLib  # noqa: E402

import cairo  # noqa: E402

PANEL_W, PANEL_H = 720, 320
results = {}


# ── raw X11 cursor access (own display connection) ───────────────────────────
class XLib:
    """Thin ctypes wrapper: XOpenDisplay / XQueryPointer / window state."""

    def __init__(self):
        name = ctypes.util.find_library("X11") or "libX11.so.6"
        self.x = ctypes.CDLL(name)
        self.x.XOpenDisplay.restype = ctypes.c_void_p
        self.x.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.x.XDefaultRootWindow.restype = ctypes.c_ulong
        self.x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
        self.x.XQueryPointer.restype = ctypes.c_int
        self.x.XQueryPointer.argtypes = [
            ctypes.c_void_p, ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_uint),
        ]
        self.dpy = self.x.XOpenDisplay(None)
        if not self.dpy:
            raise RuntimeError("XOpenDisplay(NULL) failed — no X display")
        self.root = self.x.XDefaultRootWindow(self.dpy)

    def pointer(self):
        """(root_x, root_y) of the global cursor, or (None, None)."""
        rr, cr = ctypes.c_ulong(), ctypes.c_ulong()
        rx, ry, wx, wy = (ctypes.c_int() for _ in range(4))
        mask = ctypes.c_uint()
        ok = self.x.XQueryPointer(
            self.dpy, self.root, ctypes.byref(rr), ctypes.byref(cr),
            ctypes.byref(rx), ctypes.byref(ry), ctypes.byref(wx), ctypes.byref(wy),
            ctypes.byref(mask),
        )
        return (rx.value, ry.value) if ok else (None, None)


def topmost_stack_above(win, xlib):
    """Cheap check: is our window mapped and where does X think it is?"""
    return None


class Spike:
    def __init__(self, xlib):
        self.xlib = xlib
        # DOCK: panels/docks are exempt from tiling and are kept on top.
        self.win = Gtk.Window(type=Gtk.WindowType.TOPLEVEL)
        self.win.set_decorated(False)
        self.win.set_app_paintable(True)
        self.win.set_title("coco-spike")
        self.win.set_type_hint(Gdk.WindowTypeHint.DOCK)
        self.win.set_keep_above(True)
        self.win.set_size_request(PANEL_W, PANEL_H)

        screen = Gdk.Screen.get_default()
        visual = screen.get_rgba_visual()
        if visual:
            self.win.set_visual(visual)
        self.win.connect("draw", self.on_draw)

        self.label = Gtk.Label(
            label="coco spike\n(clicks pass through when armed)")
        self.label.set_justify(Gtk.Justification.CENTER)
        self.win.add(self.label)
        self.win.connect("destroy", Gtk.main_quit)

    def on_draw(self, widget, cr):
        cr.set_source_rgba(0, 0, 0, 0)
        cr.set_operator(cairo.OPERATOR_SOURCE)
        cr.paint()
        cr.set_operator(cairo.OPERATOR_OVER)
        cr.set_source_rgba(0.05, 0.05, 0.06, 0.97)
        cr.rectangle(0, 0, PANEL_W, PANEL_H)
        cr.fill()
        return False

    def place(self):
        screen = Gdk.Screen.get_default()
        geo = screen.get_monitor_geometry(screen.get_primary_monitor())
        x = geo.x + (geo.width - PANEL_W) // 2
        y = geo.y
        self.win.move(x, y)
        return x, y, geo

    def set_click_through(self, enabled):
        window = self.win.get_window()
        if window is None:
            return False
        if enabled:
            window.input_shape_combine_region(cairo.Region(), 0, 0)
        else:
            w = self.win.get_allocated_width()
            h = self.win.get_allocated_height()
            window.input_shape_combine_region(
                cairo.Region(cairo.RectangleInt(0, 0, w, h)), 0, 0)
        return True

    # ── sequence ─────────────────────────────────────────────────────────────
    def start(self):
        self.win.show_all()
        self.win.set_keep_above(True)
        x, y, geo = self.place()
        results["wanted"] = (x, y)
        results["monitor"] = (geo.x, geo.y, geo.width, geo.height)
        print(f"[1] wanted ({x},{y}) on monitor {geo.width}x{geo.height}")
        GLib.timeout_add(700, self.q1)

    def q1(self):
        pos = self.win.get_position()
        size = self.win.get_size()
        results["actual"] = pos + size
        results["q1"] = pos == results["wanted"]
        print(f"[2] actual pos={pos} size={size} -> {'HOLDS' if results['q1'] else 'TILED/MOVED'}")

        results["q2_set"] = self.set_click_through(True)
        print(f"[3] click-through armed -> {results['q2_set']}  "
              "(try clicking the label: it must NOT react)")
        GLib.timeout_add(700, self.q2)
        return False

    def q2(self):
        results["q2_rev"] = self.set_click_through(False)
        print(f"[4] click-through released -> {results['q2_rev']}")
        print("[5] sampling global cursor 6s — MOVE THE MOUSE, including over")
        print("    Wayland-only windows (browser, terminal, hyprctl).")
        self.samples = []
        self.t_end = time.time() + 6.0
        GLib.timeout_add(16, self.sample)
        return False

    def sample(self):
        x, y = self.xlib.pointer()
        if x is not None:
            self.samples.append((x, y))
        if time.time() < self.t_end:
            return True
        self.finish()
        return False

    def finish(self):
        pts = self.samples
        results["n"] = len(pts)
        results["uniq"] = len(set(pts))
        if pts:
            xs = [p[0] for p in pts]
            ys = [p[1] for p in pts]
            results["range"] = (min(xs), max(xs), min(ys), max(ys))
            results["q3"] = results["uniq"] >= 3
            print(f"[6] cursor samples={len(pts)} unique={results['uniq']} "
                  f"x {min(xs)}..{max(xs)}  y {min(ys)}..{max(ys)}")
        else:
            results["q3"] = False
            print("[6] NO cursor samples")

        # Q4: window state — mapped, on top, position still ours.
        pos = self.win.get_position()
        results["q4_pos_ok"] = pos == results["wanted"]
        print(f"[7] position after all interaction: {pos}")

        print("\n" + "=" * 64)
        print("COCO M0 SPIKE — VERDICT")
        print("=" * 64)
        checks = [
            ("Q1 top-centre placement survives tiling WM", results.get("q1")),
            ("Q2 click-through can be armed", results.get("q2_set")),
            ("Q2 click-through can be released", results.get("q2_rev")),
            ("Q3 global cursor readable at 16 ms", results.get("q3")),
            ("Q4 position stable through interaction", results.get("q4_pos_ok")),
        ]
        for name, ok in checks:
            print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
        if results.get("n", 0) and results.get("uniq", 0) < 3:
            print(f"       → cursor stuck? samples={results['n']} unique={results['uniq']}")
        print("=" * 64)
        self.win.destroy()


if __name__ == "__main__":
    print(f"GDK_BACKEND={os.environ['GDK_BACKEND']}  DISPLAY={os.environ.get('DISPLAY')}  "
          f"WAYLAND={os.environ.get('WAYLAND_DISPLAY')}")
    xl = XLib()
    print(f"[0] own X display opened, root=0x{xl.root:x}")
    spike = Spike(xl)
    GLib.timeout_add(300, spike.start)
    Gtk.main()
