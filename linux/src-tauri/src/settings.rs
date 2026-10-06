// Preferences, stored as plain JSON at ~/.config/coco/settings.json.
// No secret ever lands here — API keys live in the Secret Service.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub sound_enabled: bool,
    pub sound_volume: f64,
    pub auto_close_interval: f64,
    pub absence_interval: f64,
    pub active_integrations: Vec<String>,
    /// "primary" = the main display, "cursor" = whichever display the mouse is on.
    pub screen: String,
    pub autostart: bool,
    pub hooks_installed: bool,
    /// Claude model used by the chat. Changeable in the settings window.
    /// Defaulted explicitly so a settings.json written by an older build still loads.
    #[serde(default = "default_model")]
    pub model: String,
    /// Chat provider: `anthropic` (direct API), `openrouter`, or `openai`.
    /// Defaulted so a settings.json written before providers existed still loads.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Base or full URL of a custom OpenAI-compatible endpoint (multi-model
    /// gateways, local servers). Only used when `provider` is `custom`.
    /// Plain JSON, not a secret — the key stays in the Secret Service.
    #[serde(default)]
    pub endpoint: String,
    /// Global hotkey that summons the island, e.g. `Super+C`. Parsed by
    /// tauri-plugin-global-shortcut; an unparsable value falls back to the
    /// default rather than leaving Coco unsummonable.
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    /// Who the user is, in their own words. Free text, shown to the model.
    #[serde(default)]
    pub user_name: String,
    /// Music taste, mood-to-genre and all. Free text, shown to the model.
    #[serde(default)]
    pub music_taste: String,
    /// How they like their setup: workspace, routines, preferences that do
    /// not fit anywhere else. Free text, shown to the model.
    #[serde(default)]
    pub setup_notes: String,
    /// Remembered approval rules, first match wins: {tool, match, allow}.
    /// `match` is a substring on the joined argv (run_command) or the app
    /// name (open_app); empty matches the whole tool. Unmatched tools always
    /// ask — nothing is silently allowed except the hardcoded read-only set.
    #[serde(default)]
    pub tool_rules: Vec<ToolRule>,
}

/// One remembered answer. Written from the settings screen, never by the
/// model — the island asks the human, the human's answer can be remembered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRule {
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub r#match: String,
    #[serde(default)]
    pub allow: bool,
}

fn default_hotkey() -> String {
    crate::hotkey::DEFAULT_HOTKEY.to_string()
}

fn default_provider() -> String {
    crate::claude::DEFAULT_PROVIDER.to_string()
}

fn default_model() -> String {
    crate::claude::DEFAULT_MODEL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sound_enabled: true,
            sound_volume: 0.12,
            auto_close_interval: 15.0,
            absence_interval: 180.0,
            active_integrations: vec![
                "integration_resend".into(),
                "integration_n8n".into(),
                "integration_vercel".into(),
                "integration_github".into(),
            ],
            screen: "primary".into(),
            autostart: false,
            hooks_installed: false,
            model: default_model(),
            provider: default_provider(),
            endpoint: String::new(),
            hotkey: default_hotkey(),
            user_name: String::new(),
            music_taste: String::new(),
            setup_notes: String::new(),
            tool_rules: Vec::new(),
        }
    }
}

/// `$XDG_CONFIG_HOME/coco` (default `~/.config/coco`).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        let base = PathBuf::from(dir);
        if base.is_absolute() {
            return base.join("coco");
        }
    }
    home().join(".config").join("coco")
}

/// `$XDG_STATE_HOME/coco` (default `~/.local/state/coco`) — the log lives here.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        let base = PathBuf::from(dir);
        if base.is_absolute() {
            return base.join("coco");
        }
    }
    home().join(".local").join("state").join("coco")
}

/// `$XDG_DATA_HOME/coco` (default `~/.local/share/coco`) — the hook relay and
/// the file inbox live here.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        let base = PathBuf::from(dir);
        if base.is_absolute() {
            return base.join("coco");
        }
    }
    home().join(".local").join("share").join("coco")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The relay binary we install for Claude Code's hooks.
pub fn hook_exe_path() -> PathBuf {
    data_dir().join("bin").join("coco-hook")
}

/// The system-action MCP server the brain spawns. Same install layout as the
/// hook relay: built by cargo, copied next to the running app on first boot.
pub fn mcp_exe_path() -> PathBuf {
    data_dir().join("bin").join("coco-mcp")
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn load() -> Settings {
    match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(settings_path(), json)
}
