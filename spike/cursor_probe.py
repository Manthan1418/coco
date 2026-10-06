#!/usr/bin/env python3
"""Q3 diagnosis: is XWayland blind to the cursor, or was the mouse just still?

Samples XQueryPointer (XWayland) and `hyprctl cursorpos` (native Wayland)
side by side for 5 s and reports whether EITHER changes.
Run this and move the mouse for the whole time.
"""
import ctypes
import ctypes.util
import os
import subprocess
import time

os.environ.setdefault("GDK_BACKEND", "x11")

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
    except Exception as e:
        return f"err:{e}"


print("MOVE THE MOUSE CONTINUOUSLY FOR 5 SECONDS")
xs, hs = [], []
t_end = time.time() + 5
while time.time() < t_end:
    x = x_ptr()
    h = hyp_ptr()
    xs.append(x)
    hs.append(h)
    time.sleep(0.05)

xu = set(x for x in xs if x)
hu = set(h for h in hs if not isinstance(h, str))
print(f"\nXQueryPointer : {len(xs)} samples, {len(xu)} unique -> {sorted(xu)[:8]}")
print(f"hyprctl       : {len(hs)} samples, {len(hu)} unique -> {sorted(hu)[:8]}")
errs = set(h for h in hs if isinstance(h, str))
if errs:
    print(f"hyprctl errors: {errs}")
print()
print(f"XWayland updates : {'YES' if len(xu) > 3 else 'NO (blind)'}")
print(f"hyprctl updates  : {'YES' if len(hu) > 3 else 'NO'}")
