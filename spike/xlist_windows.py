#!/usr/bin/env python3
"""List top-level X windows: name + geometry. Deliberately minimal (no
XGetWindowAttributes — its struct layout is easy to get wrong and segfault)."""
import ctypes, ctypes.util, sys

lib = ctypes.CDLL(ctypes.util.find_library("X11"))

lib.XOpenDisplay.restype = ctypes.c_void_p
lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
lib.XDefaultRootWindow.restype = ctypes.c_ulong
lib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
lib.XQueryTree.restype = ctypes.c_int
lib.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
                           ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
                           ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)),
                           ctypes.POINTER(ctypes.c_uint)]
lib.XFetchName.restype = ctypes.c_int
lib.XFetchName.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
                           ctypes.POINTER(ctypes.c_char_p)]
lib.XGetGeometry.restype = ctypes.c_int
lib.XGetGeometry.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
                             ctypes.POINTER(ctypes.c_ulong),
                             ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
                             ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(ctypes.c_uint),
                             ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(ctypes.c_uint)]
lib.XGetWindowAttributes.restype = ctypes.c_int
lib.XTranslateCoordinates.restype = ctypes.c_int
lib.XTranslateCoordinates.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong,
                                      ctypes.c_int, ctypes.c_int,
                                      ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
                                      ctypes.POINTER(ctypes.c_ulong)]


def main():
    dpy = lib.XOpenDisplay(None)
    if not dpy:
        print("cannot open display")
        return 1
    root = lib.XDefaultRootWindow(dpy)
    root_ret, parent = ctypes.c_ulong(), ctypes.c_ulong()
    children = ctypes.POINTER(ctypes.c_ulong)()
    n = ctypes.c_uint()
    if not lib.XQueryTree(dpy, root, ctypes.byref(root_ret), ctypes.byref(parent),
                          ctypes.byref(children), ctypes.byref(n)):
        print("XQueryTree failed")
        return 1

    print(f"{n.value} top-level windows on root {root:#x}")
    for i in range(n.value):
        w = children[i]
        namebuf = ctypes.c_char_p()
        title = b"(unnamed)"
        if lib.XFetchName(dpy, w, ctypes.byref(namebuf)) and namebuf.value:
            title = namebuf.value
        gx, gy = ctypes.c_int(), ctypes.c_int()
        ww, hh = ctypes.c_uint(), ctypes.c_uint()
        bw, depth = ctypes.c_uint(), ctypes.c_uint()
        geom = "?"
        if lib.XGetGeometry(dpy, w, ctypes.byref(root_ret), ctypes.byref(gx),
                            ctypes.byref(gy), ctypes.byref(ww), ctypes.byref(hh),
                            ctypes.byref(bw), ctypes.byref(depth)):
            # These are root-relative only for root children of a reparenting WM;
            # XTranslateCoordinates gives true root coords.
            absx, absy = ctypes.c_int(), ctypes.c_int()
            child = ctypes.c_ulong()
            lib.XTranslateCoordinates(dpy, w, root, 0, 0,
                                      ctypes.byref(absx), ctypes.byref(absy),
                                      ctypes.byref(child))
            geom = f"at ({absx.value},{absy.value}) {ww.value}x{hh.value}"
        t = title.decode("utf-8", "replace")
        mark = "   <== COCO" if "coco" in t.lower() else ""
        print(f"  {w:#010x}  {t!r:44} {geom}{mark}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
