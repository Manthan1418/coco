<div align="center">

<img src="src-tauri/icons/128x128.png" width="96" alt="Coco icon">

# Coco for Linux

**The island at the top of your screen** — a small companion that shows your
coding-agent sessions, asks before it touches anything, and chats with a local
brain. Tauri 2 + Rust + TypeScript, no framework, no telemetry, no account.

![Linux](https://img.shields.io/badge/Linux-Hyprland-0078D4?logo=linux)
![Tauri 2](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)
![Rust](https://img.shields.io/badge/Rust-backend-000?logo=rust)
![License: MIT](https://img.shields.io/badge/license-MIT-green)

</div>

---

## What it does

| | |
|---|---|
| **Island** | A 720×320 transparent XWayland window pinned at the top-centre of the screen, click-through until you need it. `Super+C` summons it, `Esc` sends it away. |
| **Chat** | Streaming replies from a **local omniroute gateway** (OpenAI-compatible, `http://localhost:20128/v1`), with history on disk, rename/delete, cancel mid-reply. |
| **Hands** | A local tool loop over `coco-mcp`: run commands, find and open files, launch apps, notify, play music, search the web, save and recall memory. |
| **Approvals** | Destructive commands arrive as an approval card first. Read-only commands (list, show, find) run straight away; `sudo` goes through `pkexec`. |
| **Email** | Drop a file and pick **Send by email** — Coco sends it over your own SMTP server, with a To / Subject / optional line form. The macOS build handed this to Mail.app; here Coco speaks SMTP itself. |
| **Voice** *(optional)* | "hey coco" opens the island and it greets you back — local whisper wake word + Piper TTS. |

Everything happens on your machine: keys live in the Secret Service
(gnome-keyring / KWallet), conversations in `~/.local/share/coco/`, and nothing
phones home.

## Requirements

Coco leans on a few things already on a normal desktop box. Every one of them
degrades to a smaller feature rather than a crash — but you get the real thing
only if they are there.

| | Needed for | Without it |
|---|---|---|
| [`omniroute`](https://www.npmjs.com/package/omniroute) — `npm i -g omniroute` | the brain | chat says "the brain has no gateway" |
| `rg` (ripgrep), `curl` | file search, web fetch | those tools fail, chat explains |
| `playerctl` | music control | music commands fail politely |
| `notify-send` | desktop notifications | no notifications |
| `pkexec` (polkit) | `sudo` commands | a sudo command fails with a message instead of prompting |
| Secret Service (gnome-keyring / KWallet) | storing API keys | keys are not saved between runs |
| `libayatana-appindicator` | the tray icon | island still works; GNOME also needs the AppIndicator extension |

Voice additionally wants `parecord` + `pw-play` (PipeWire), `faster-whisper`
and a [Piper](https://github.com/rhasspy/piper) voice — see [`voice/coco-voice`](voice/coco-voice).

## Install

Grab `Coco-Linux-amd64.deb` or `Coco-Linux-amd64.AppImage` from `release/`
(or build them below).

```bash
sudo apt install ./Coco-Linux-amd64.deb     # Debian/Ubuntu
# AppImage: chmod +x, then run it
```

To start it with your session on Hyprland:

```lua
-- ~/.config/hypr/hyprland/execs.lua
hl.exec_cmd("$HOME/.local/bin/coco")   -- or the installed path
```

(There is also an **Autostart** toggle in Settings; it defaults to off.)

## Build it yourself

```bash
npm install
npm run pack          # tsc + vite, coco-hook + coco-mcp, the app, then deb + AppImage → release/
```

For day-to-day development:

```bash
npm run tauri dev     # vite dev server, debug build, live reload
```

Plain `cargo build` is **not** enough — without Tauri's `custom-protocol`
feature it bakes the dev URL (`http://localhost:1420`) into the binary, so the
island stays blank unless the Vite server happens to be running. The app now
says which document it loaded at boot: `island loaded tauri://localhost` is a
good production build, `WARNING island loaded http://localhost:1420` means it
was built wrong.

Installable binaries come from `tauri build` / `npm run pack`; for a quick local
build pass `--features custom-protocol` (a `[features]` entry now forwards it),
copy `target/release/coco` **and** `target/release/coco-mcp` into `~/.local/bin/`
and restart — the app self-installs both into `~/.local/share/coco/bin/` on boot.

## Where things live

| Path | What |
|---|---|
| `~/.config/coco/settings.json` | settings: provider, model, endpoint, hotkey, screen, autostart, SMTP server + addresses |
| `~/.local/share/coco/chats/` | one JSON file per conversation |
| `~/.local/share/coco/memory/` | the assistant's memory, written by its memory tools |
| `~/.local/share/coco/bin/` | `coco-hook`, `coco-mcp` — installed on first boot |
| `~/.local/state/coco/coco.log` | the log — start here when something is wrong (rotates at 1 MB) |
| `~/.local/share/coco/omniroute.log` | the gateway's own output |

## The brain

Coco does not ship a model. It talks to a **local omniroute gateway** — an
OpenAI-compatible endpoint on port `20128` — and starts it on boot if it is not
already running (`omniroute serve --daemon`). The gateway in turn fronts
whatever providers you configure in omniroute, so the island's Settings only
need *endpoint + model* (defaults: `http://localhost:20128/v1`,
`auto/best-fast`).

- Binary found on `PATH`, then `/usr/bin/omniroute`; set **`COCO_OMNI_BIN`** to
  point somewhere else.
- Gateway down mid-session? The turn says so plainly and kicks a restart
  instead of dying on a bare connection error.

## Troubleshooting

Start with `tail -f ~/.local/state/coco/coco.log`.

| Log says | Means |
|---|---|
| `omniroute not found at …` | the gateway is not installed — `npm i -g omniroute`, or set `COCO_OMNI_BIN` |
| `brain is starting — ask again in a few seconds` | the gateway is booting; retry in a moment |
| `coco-mcp not found` | actions are disabled — rebuild (`cargo build --release -p coco-mcp`) and restart |
| `ISLAND WINDOW LOST (reason: …)` | the toplevel vanished — the reason in the line says whether Tauri lost the window or our X connection went blind. Restart, and if it recurs the log is the bug report |
| `island close request refused` | something asked the island to close (a `Super+Q` over a focused island, a window rule). It was ignored on purpose: the island hides, it does not die |
| nothing, and no window | `hyprctl clients` should list `Coco`; if not, check `GDK_BACKEND … forced to "x11"` — the island needs XWayland (`DISPLAY`) for input shapes and click-through |

## Repository layout

```
linux/
├── src-tauri/     Rust: window, tray, hotkey, hooks relay, the brain (chat + tool loop)
├── src/           TypeScript: island UI, chat, settings — no framework
├── mcp/           coco-mcp — the hands (run, search, open, music, notify, web, memory)
├── hook/          coco-hook — the Claude Code hooks relay
├── voice/         "hey coco" wake word + TTS
├── scripts/       icons + pack
└── engine/        the legacy OpenCode client crate — not part of the app
```

The macOS/Windows app is the rest of this repository; see the root
[README](../README.md).
