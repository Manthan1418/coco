# Coco operations — things that are true whether or not they seem reasonable

## Builds
- Installable binaries come ONLY from `tauri build`. Plain `cargo build` (any
  profile) bakes in `devUrl` and points the island at a vite server that will
  not be there. cfg(dev) is feature-driven (`!custom-protocol`), not
  profile-driven.
- `npm run tauri build` runs `beforeBuildCommand` (`npm run build`, whose
  `prebuild` builds `coco-hook` + `coco-mcp`, then tsc + vite) and bundles
  **deb and appimage** into `target/release/bundle/`. `npm run pack` copies
  them to `release/` as `Coco-Linux-<arch>[-<version>].deb|.AppImage`; the same
  script still produces the Windows NSIS names, chosen by platform.
- `coco-hook` and `coco-mcp` are bundle resources, and `ensure_*` copies them
  into `~/.local/share/coco/bin/` on boot. If `coco-mcp` is missing the log
  lists every path tried — the island then chats without hands instead of
  failing, which is exactly why the line has to be findable.
- Local dev install: copy `target/release/coco` **and** `target/release/coco-mcp`
  to `~/.local/bin/` and restart. Copying over a running binary fails with
  "Text file busy": write `coco.new`, then `mv` it into place.

## Hyprland rules (rules.lua, backed up as .bak-coco*)
- Rules apply at window OPEN. Changing a rule needs a **Coco restart**, not
  just `hyprctl reload` — reload leaves the live window on stale rules.
- `hl.window_rule` has NO `no_border` (verified against the binary's field
  table; passing it red-bars the config). Border is removed with
  `decorate = false` (screenshot-verified).
- EWMH `_NET_WM_STATE_STICKY` set by the app is ignored for this window;
  cross-workspace visibility comes from the `pin = true` rule.
- Current Coco rule: `pin + no_blur + no_shadow + decorate = false`.

## Window facts
- The island is a 720×320 transparent XWayland window; collapsed outer size
  stays 720×320 (GTK clamps to the 720×320 page minimum) with only the input
  shape shrinking to the strip. Harmless now that blur/border/shadow opt out.
- `visible=Ok(true)` + `outer=(0,0)` means GTK never mapped it; only the X
  map state tells the truth (`x=` in window-state lines).
- The X stub `0x...001` (10×10 unmapped, class Coco) is NOT the island; probes
  must prefer the mapped window.
- The island window had no `CloseRequested` guard — the settings window always
  had one — so a close request from the compositor (a `Super+Q` over a focused
  island, a window rule, `WM_DELETE`) destroyed the only visible window and
  left a process nothing could summon. It now refuses, logs
  `island close request refused`, and logs `Destroyed` separately, so the next
  `ISLAND WINDOW LOST (reason: …)` distinguishes "Tauri's registry lost the
  toplevel" (`no window`) from "our X connection is blind to it"
  (`X unavailable`).

## The brain — local omniroute gateway
- The island's provider is `custom` → `http://localhost:20128/v1`: an
  OpenAI-compatible **omniroute** gateway. Deliberately NOT the OpenCode
  service — that wiring was removed; `pipe.rs` stays as the Claude Code relay
  for external terminal sessions, with its own approval cards.
- `ensure_omnirouter()` probes port 20128 **plus** `GET /v1/models` (an open
  port could be anything squatting there) and spawns
  `omniroute serve --daemon --no-open --no-tray` only when it is down; the
  daemon supervises itself.
- Binary lookup: `COCO_OMNI_BIN` → `PATH` → `/usr/bin/omniroute`. It is an
  external npm package, never bundled; a miss logs the install command rather
  than leaving chat mysteriously dead.
- A turn is a tool loop over `coco-mcp` (`MAX_ITERS = 8`): read-only tools run
  straight away, ask-first tools go through the approval card, every tool
  failure degrades to words. Cancel aborts the in-flight request — the stream
  too — instead of waiting it out.
- Replies **stream**: `stream: true` SSE deltas ride the `chat-delta` event
  (batched every 80 ms / 400 B) so the bubble grows word by word, and the
  final reply is reassembled from the same fragments, leaving `reply_text`,
  `parse_tool_calls` and every caller untouched. A gateway that ignores
  `stream` answers whole; one that rejects it is retried whole — before a word
  has been shown, never after.
- Conversations are one JSON file per id under `~/.local/share/coco/chats/`.
  The id **is** the filename, so anything containing a separator is refused
  rather than sanitised.
