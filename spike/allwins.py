#!/usr/bin/env python3
"""Every top-level X window with the fields needed to tell mapped from ghost.

`xlist_windows.py` skips unnamed windows, which is exactly how a Coco window
disappears from view: an unmapped GTK window can lose its WM name. This lists
everything, so a missing island is visible as a nameless 10x10 rather than as
silence.
"""

from Xlib import X, display

STATES = {
    X.IsUnmapped: "IsUnmapped",
    X.IsUnviewable: "IsUnviewable",
    X.IsViewable: "IsViewable",
}


def main():
    d = display.Display()
    root = d.screen().root

    children = root.query_tree().children
    print(f"{len(children)} top-level windows on root {hex(root.id)}")

    for w in children:
        try:
            attrs = w.get_attributes()
            geom = w.get_geometry()
        except Exception as e:  # noqa: BLE001
            print(f"  {hex(w.id):>12}  <error: {e}>")
            continue

        try:
            cls = (w.get_wm_class() or ("", ""))[1]
        except Exception:  # noqa: BLE001
            cls = "?"
        try:
            name = w.get_wm_name() or ""
        except Exception:  # noqa: BLE001
            name = "?"

        print(
            f"  {hex(w.id):>12}  {STATES.get(attrs.map_state, '?'):<12}"
            f" {geom.width:>5}x{geom.height:<5}+{geom.x}+{geom.y}"
            f"  class={cls!r} name={name!r}"
        )


if __name__ == "__main__":
    main()
