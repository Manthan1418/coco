# Coco — M0 platform findings (Linux / Hyprland, XWayland)

Everything below was measured on the target machine, not assumed.

---

## 1. The environment is Hyprland (Wayland), not X11

```
XDG_SESSION_TYPE=wayland
XDG_CURRENT_DESKTOP=Hyprland      Hyprland 0.56.2
DISPLAY=:0                        xorg-xwayland 24.1.13   (XWayland available)
Monitor eDP-2  1920x1080@144  reserved: 60 10 10 10
```

So "X11 first" means **an X11 app running under XWayland**. The spike forced
`GDK_BACKEND=x11` — the session exports `GDK_BACKEND=wayland`, so it has to be set
*before* `gi`/GTK loads, not with `setdefault`.

Top bar: `caelestia-drawers` layer-shell surface at level `top`, full 1920x1080,
reserving 60 px at the top. The island must anchor below that, not at y=0.

## 2. Spike verdict — `spike/island_spike.py`

| | Question | Result |
|---|---|---|
| Q1 | Top-centre placement survives a tiling compositor | **PASS** |
| Q2 | Click-through armed live | **PASS** |
| Q2 | Click-through released live | **PASS** |
| Q3 | Global cursor readable at 16 ms | **FAIL** — see §3 |
| Q4 | Position stable through interaction | **PASS** |

**The decisive result:** `NORMAL` windows get tiled — `move()` is silently ignored
and the window lands at `(71,21)` sized `895x1010`. Setting
`_NET_WM_WINDOW_TYPE` makes the compositor treat it as a panel instead:

```
NORMAL       pos=(71,21)  size=895x1010   MOVED
DOCK         pos=(600,0)  size=400x200    OK
UTILITY      pos=(600,0)  size=400x200    OK
NOTIFICATION pos=(70,20)  size=895x1010   MOVED
SPLASH       pos=(600,0)  size=400x200    OK
MENU         pos=(600,0)  size=400x200    OK
```

Requested `(600,0)` = top-centre of 1920 wide, 720 wide window. **This is the
single most important line in the port** (`island.rs::claim_panel_type`).

## 3. The global cursor — RESOLVED: XWayland is blind

Rerun with a pointer moving continuously for 5 s
(`spike/cursor_probe.py`, 90 samples/side):

```
XQueryPointer : 90 samples, 1 unique -> [(960, 540)]
hyprctl       : 90 samples, 60 unique -> [(221, 788), (294, 763), (304, 380), …]

XWayland updates : NO   (blind)
hyprctl updates  : YES
```

`XQueryPointer` sits on **exactly one value — screen centre**, the classic
stale/default reading — while `hyprctl cursorpos` reports 60 distinct points
over the same interval. So `hyprctl` is live and XWayland is frozen: it is only
told where the pointer is for surfaces that actually receive pointer focus.

**Confirmed consequences:**

- A global cursor read is **not available** on this machine. Anything that needs
  "where is the pointer, anywhere" must not be built on `XQueryPointer`.
- `hyprctl cursorpos` works but is Hyprland-specific (a subprocess per sample)
  and gives GNOME/KDE nothing. Not a foundation.
- **Hover-to-wake does not need it** — pointer enter/leave on the window itself
  works, and that is the part that must not break. ✅
- **Eyes-follow-mouse across the whole screen is unavailable** under XWayland.
  The `cursor` event still reports position *while the pointer is over an X
  window* (including our own island), and the poll drops a value that stops
  changing rather than repeating it — see §4.

### The click-through design was reversed — see §4

The obvious design — poll the global pointer at 60 Hz and arm click-through
when it leaves the island — is **self-poisoning** on XWayland, and this is the
most important trap in the port:

> An empty input shape means our surface stops receiving pointer focus →
> XWayland stops being told where the pointer is → the poll freezes on the last
> point it saw, which is *outside* the island → it never disarms → the pointer
> can never reach us again → **the island goes permanently dead.**

The first tick after start-up would have done it (frozen `(960,540)` is below
the panel, so it arms immediately and never recovers).

**The fix, now in `island.rs::apply_input_shape`:** click-through is driven by
the island's own rect — pushed by the front end over `set_island_rect` — applied
as an X *input shape* via the SHAPE extension (`SO::SET` + `SK::INPUT`, so only
a rectangle needs to be computed, no region maths). The cursor poll was demoted
to reporting position only and is **never** allowed to arm or disarm anything.

Tauri's `set_ignore_cursor_events` is kept solely as the fallback for a session
without X11 or without SHAPE.


## 4. What Tauri gives us for free — and what it doesn't

| | |
|---|---|
| ✅ | `tao`'s `set_ignore_cursor_events` is `input_shape_combine_region(…)` / `None`, i.e. the same X mechanism the spike proved works — but it is **all-or-nothing**, which is exactly what §3 showed we cannot rely on. Kept only as a fallback. |
| ⚠️ | `tao` has **no window type-hint API at all**, so `_NET_WM_WINDOW_TYPE_DOCK` is set by us: `WebviewWindow` implements `HasWindowHandle` → read the XID → `x11rb`. This is the single most important line in the port. |
| ⚠️ | Click-through over an arbitrary rect needs the **SHAPE extension**, which is *not* on by default in x11rb — `x11rb = { version = "0.13", features = ["shape"] }` or `x11rb::protocol::shape` simply does not exist. |
| ❌ | `tao`'s `cursor_position()`: "iOS / Android / **Linux(Wayland): Unsupported, returns 0,0**". Confirmed by measurement — see §3. |

Everything X11-specific lives behind `with_x(...)` in `island.rs` and degrades
to a no-op when there is no display, so a future pure-Wayland build still runs.


## 5. OpenCode V2 API — measured

Auth is **HTTP Basic, username `opencode`**. The managed service registration is
at `~/.local/state/opencode/service.json`, mode **0600**, containing
`{id, version, url, pid, password}`. (Env fallback: `OPENCODE_PASSWORD` /
`OPENCODE_SERVER_PASSWORD`.)

```sh
PW=$(python3 -c "import json;print(json.load(open('$HOME/.local/state/opencode/service.json'))['password'])")
URL=$(python3 -c "import json;print(json.load(open('$HOME/.local/state/opencode/service.json'))['url'])")
curl -s -u "opencode:$PW" "$URL/api/info"
```

`opencode serve` on a bare port prints `server password <pw>` and rejects
everything without it — `{"_tag":"UnauthorizedError"}`. Note `/openapi.json` is
*also* auth-protected, so the CI contract test needs credentials too.

Verified working:

| Endpoint | Result |
|---|---|
| `GET /api/info` | `{"version":"2.0.18","pid":…,"urls":[…]}` |
| `GET /openapi.json` | 200, 251 892 bytes |
| `GET /api/model?directory=` | 8 models |
| `GET /api/agent?directory=` | `build` agent, permissions ruleset |
| `GET /api/session?directory=` | live sessions with tokens/cost/time |
| `GET /api/permission/saved` | saved permission rules |
| `GET /api/event` | **SSE works** — `data: {...}\n\n` + `: heartbeat` |

### The approval path is first-class — better than expected

OpenCode has **native permission routes**, so Coco does not have to synthesise a
Claude-Code-shaped hook protocol for it:

```
GET  /api/permission/request?directory=…            list pending
GET  /api/session/{id}/permission                   list for a session
POST /api/session/{id}/permission/{requestID}/reply reply to one
```

`Permission.Request` = `{id, sessionID, action, resources[], save[], metadata, source, message}`
`Permission.Reply` = `"once" | "always" | "reject"`

That maps **exactly** onto the island's approval card:
`once` → Allow, `always` → Always allow, `reject` → Deny.

SSE event types of interest (found in the binary):
`permission.asked`, `permission.replied`, `permission.rejected`,
`permission.prompt.fullscreen`, `session.execution.started|succeeded|failed`,
`session.idle`, `session.created|deleted`, `session.error`.

Also available and relevant to later milestones:
- `POST /api/session/{id}/prompt` — send a message
- `GET /api/event` — the SSE feed for live state
- `POST /api/pty` + `POST /api/pty/{ptyID}/connect-token` — **PTY control**, the
  route for driving CLI coding agents
- `POST /api/shell`, `GET /api/shell/{id}/output` — run and read commands
- `PUT /api/experimental/mcp/{server}` — **register `coco-system` MCP at runtime**
- `POST /api/session/{id}/switch`-style routes: `POST /api/session/{id}/model`

---

## 6. Build status

| Component | Status |
|---|---|
| `coco-hook` relay | **builds, 6/6 tests pass** |
| `coco-engine` (OpenCode client) | **builds, 9/9 tests pass incl. live contract test** |
| `coco` (Tauri app) | **`cargo check --workspace` → 0 errors**; first `tauri dev` run pending |
| Frontend | `linux/src` is **byte-identical to `windows/src`** (`diff -rq` → no output); all **25** commands the bridge calls are registered on the Rust side |

`webkit2gtk-4.1` 2.52.6 is now installed (was the blocker) along with `enchant`,
`libmanette`, `woff2`.

> **Watch out:** a system upgrade running concurrently (`paru -Syu`) broke the
> build mid-flight with
> `rustc: error while loading shared libraries: libLLVM.so.22.1` — the old
> `rustc` still needed LLVM 22 while `llvm-libs` had already moved to 23.
> Exit code 127 from `rustc`, not from our code. **Never build during `-Syu`.**

### Command surface cross-check

`bridge.ts` → `generate_handler![]` compared programmatically: **25 expected, 26
registered, 0 missing.** The one extra (`open_n8n`) is called through
`call<void>(...)`, which the naive regex misses — it is wired too.

### Contract test (live, against 2.0.18)

```
ok  Permission.Reply = ["once", "always", "reject"]
ok  GET /openapi.json — 115 paths
ok  GET /api/info — v2.0.18 pid 3418
ok  GET /api/model — 8 models across 1 providers
ok  GET /api/agent — 7 agents
ok  GET /api/session — 7 sessions
ok  GET /api/permission/request — 0 pending
ok  GET /api/mcp — 0 servers
ok  GET /api/event (SSE) — HTTP 200 OK
```

Run with `cargo test -p coco-engine --test contract` (needs
`opencode service start`). It skips rather than fails when no service is running.

**Schema details it caught** (both would have been silent runtime breakage):

- `Model` sends **`providerID`** / **`modelID`** — capital `ID`, *not* the
  `providerId` that `#[serde(rename_all = "camelCase")]` derives.
- `Permission.Request` sends **`sessionID`** — same trap.
- `message` and `resources` on a permission request are optional and must
  default, or a bare card fails to decode.

Native deps already present: gtk3 3.24.52, libayatana-appindicator 0.6.0,
x11 1.8.13, xfixes 6.0.2, libsecret 0.21.8, dbus-1 1.16.2, cairo 1.18.6.
Secret Service is **running** (`org.freedesktop.secrets` = gnome-keyring-daemon),
so `keyring` with `sync-secret-service` will work as-is.

---

## 7. First window run — measured

`GDK_BACKEND=x11 npm run tauri dev`. Forcing `x11` is mandatory: the session
exports `GDK_BACKEND=wayland`, and a native-Wayland window gives us no XID — no
window type, no input shape, no cursor, i.e. none of this design.

| Check | Expected | Measured | |
|---|---|---|---|
| Window class / XWayland | X window | `class=Coco  xwayland=True` | ✅ |
| Top-centre placement | x=600 | `[600, …]` | ✅ |
| `DOCK` holds geometry | 720×320 | `[600,60] size [720,320]` | ✅ |
| Below the 60 px top bar | y=60 | y=60 | ✅ |
| Collapse → wake strip | 240 wide at top | `collapsed=true -> (840,60) 240x6` | ✅ |
| Input shape applied | X SHAPE region | `input-shape via X SHAPE rect=(26,-14) 668x260` | ✅ |
| Spring animation pushes rect | per-frame | 260→271→268→… within one second | ✅ |
| Relay socket created | `coco-1000.sock` | `srw------- … /run/user/1000/coco-1000.sock` | ✅ |
| Tray | appindicator | created (deprecation warning only) | ✅ |

### Bug 1 — the island was placed under the top bar

`y = monitor.y` put it at y=0, hidden beneath `caelestia-drawers`.
**`Monitor::work_area()` does not fix this** — measured:

```
monitor=(0,0) 1920x1080   workarea=(0,0) 1920x1080
```

Layer-shell reservations are not X11 struts, so `_NET_WORKAREA` (what GDK's
`workarea()` returns) reports the untouched monitor. Fixed by reading
`hyprctl monitors -j` → `reserved [top,right,bottom,left]`, with the work area
as the fallback:

```
usable=(10,60) 1900x1010 -> (600,60) 720x320
```

### Bug 2 — the relay socket panicked at startup (would have broken M3 silently)

```
thread 'tokio-rt-worker' panicked at src-tauri/src/pipe.rs:120:30:
Registering a blocking socket with the tokio runtime is unsupported.
```

`std::os::unix::net::UnixListener::bind` yields a **blocking** listener, and
`tokio::net::UnixListener::from_std` *panics* on one rather than returning an
error (tokio#7172) — so the `Err` branch never ran, the task died, and every
Claude Code approval would have silently done nothing while the app looked
perfectly healthy. Fixed with `listener.set_nonblocking(true)` first. Confirmed:
3 panics before the fix, none after.

### Open item — the window vanished once after a collapse/expand cycle

At 21:00:06 the island collapsed (`240x6`) and at 21:00:09 expanded again
(`720x320`). From then until ~21:11 the process was alive, logging normally, and
**`hyprctl clients` listed no Coco window at all** — and `XQueryTree` showed no
720×320 window either, so it was genuinely unmapped, not merely unlisted.
Workspace counts matched the client list, so it was on no workspace.

It did not reproduce: a 60-second sample at 1 Hz returned **60/60 present** at
`[600,60] size [720,320] mapped=True visible=True`.

Diagnostic now in place (`apply_geometry` logs it on every placement):

```
window-state visible=Ok(true) foc=Some(false)
             outer=Some(PhysicalSize { width: 0, height: 0 })
             inner=Some(PhysicalSize { width: 240, height: 6 })  size_err=None pos_err=None
```

Two things worth knowing from it:

- **`outer_size` reads 0×0** when sampled immediately after `set_size` — GTK has
  not processed the request yet, and `set_size` returns `Ok` regardless. So
  "the call succeeded" proves nothing about what is on screen; only a compositor
  query does. That is why the `window-state` line exists.
- **Collapsed height clamps 6 → 200**, and clamping happens *inside* GTK, so the
  size we ask for is a wish, not a fact.

Most likely a teardown/rebuild artefact of `tauri dev` rather than steady-state
behaviour, but it must be re-checked before M2 ships — an island that silently
disappears is worse than one that never appears.

### Non-issues worth recording

- **Collapsed window is 240×200, not 240×6.** GTK clamps the height. Harmless:
  `html, body` are `background: transparent`, and the input shape covers only
  the top ~20 px — exactly where `#wake-strip` sits (`position: fixed; top: 0`,
  viewport-anchored, so unaffected by the window's height). Hover-to-wake works.
- `set_island_rect` is called on every animation frame, so it applies an X
  request per frame while the island springs open. That is intended (the shape
  must track the shape) and stops when idle — but it must never log per-frame.
