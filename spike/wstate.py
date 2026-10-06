#!/usr/bin/env python3
"""The real Coco island window's X state.

There are two X windows here and picking the wrong one hides everything:
`0x800001` is an unmapped 10x10 stub, and the island everyone actually sees is
`0x800003`. A probe that matched on class alone reported the stub forever and
made a working island look broken, so windows are ranked — a mapped window
always wins over an unmapped one of the same class.

Also worth keeping in mind: GTK and X can disagree. `is_visible()` returned true
while the window was never mapped, so the X map state is the only trustworthy
answer to "is the island on screen?".
"""

import sys

from Xlib import X, display
from Xlib.ext import shape

STATES = {
    X.IsUnmapped: "IsUnmapped",
    X.IsUnviewable: "IsUnviewable",
    X.IsViewable: "IsViewable",
}


def rect_summary(reply):
    """`shape_get_rectangles` hands back a reply object, not a list."""
    rects = getattr(reply, "rectangles", None)
    if rects is None:
        return f"unreadable: {reply!r}"
    return [f"{r.x},{r.y} {r.width}x{r.height}" for r in rects] or "EMPTY"


def describe(w):
    g = w.get_geometry()
    attrs = w.get_attributes()
    return {
        "xid": hex(w.id),
        "name": w.get_wm_name(),
        "class": w.get_wm_class(),
        "geometry": f"{g.width}x{g.height}+{g.x}+{g.y}",
        "map_state": STATES.get(attrs.map_state, attrs.map_state),
        "bounding_shape": rect_summary(w.shape_get_rectangles(shape.SK.Bounding)),
        "input_shape": rect_summary(w.shape_get_rectangles(shape.SK.Input)),
    }


def main():
    d = display.Display()
    root = d.screen().root

    wanted = sys.argv[1] if len(sys.argv) > 1 else "coco"
    matches = []

    for w in root.query_tree().children:
        try:
            cls = (w.get_wm_class() or ("", ""))[1]
            name = w.get_wm_name() or ""
        except Exception:  # noqa: BLE001
            continue
        if wanted.lower() not in (cls + name).lower():
            continue
        try:
            matches.append((w, describe(w)))
        except Exception:  # noqa: BLE001
            continue

    if not matches:
        print(f"  no window matching {wanted!r}")
        return

    # Mapped windows are the ones on screen; an unmapped one of the same class is
    # a stub. Report every match but lead with the one that matters.
    matches.sort(key=lambda m: (m[1]["map_state"] != "IsViewable", m[0].id))
    for i, (_w, info) in enumerate(matches):
        if i == 0:
            print("  <== THE ISLAND" if len(matches) > 1 else "  <==")
        for key, value in info.items():
            print(f"  {key:20} {value}")
        print()


if __name__ == "__main__":
    main()
