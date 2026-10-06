# Coco operations — things that are true whether or not they seem reasonable

## Builds
- Installable binaries come ONLY from `tauri build --no-bundle`. Plain
  `cargo build` (any profile) bakes in `devUrl` and points the island at a
  vite server that will not be there. cfg(dev) is feature-driven
  (`!custom-protocol`), not profile-driven.
- Release flow: `npm run build` (via prebuild) → `tauri build --no-bundle` →
  copy `target/release/coco` AND `target/release/coco-mcp` to `~/.local/bin/` →
  restart. The app self-installs both into `~/.local/share/coco/bin/` on boot.
- Replacing `coco-mcp` while the service holds it spawned fails with
  "Text file busy": POST `/disconnect`, copy, POST `/connect`.

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

## OpenCode service
- PUT prompt body is `{text}`, not parts. PUT mcp body wraps `{"config": …}`.
- Turn ends: `session.execution.succeeded/failed/interrupted` — `session.idle`
  does NOT reliably fire.
- Message list order is NOT chronological; pick max `time.created`.
- MCP `failed` status is sticky: re-PUT does not retry, new sessions do not
  retry — POST `/connect` (or DELETE + PUT) forces a fresh spawn.
- The binary must exist BEFORE the first PUT for a scope, or the failure
  caches and every later check misleads.
- The service allows MCP tool calls by default (no opencode.json rules on
  this machine): nothing asks unless policy says so. Approval cards (M1)
  engage only under an asking policy — that is P5, not a bug in M1.
