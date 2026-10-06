// The brain: direct multi-provider chat with local hands, plus the local
// conversation store.
//
// Deliberately NOT the OpenCode service. The island talks straight to the
// configured provider endpoint (omnirouter-style OpenAI-compatible gateway
// included), runs a local tool loop over coco-mcp for actions, and keeps its
// own history on disk — one JSON file per conversation under the data dir.
// Sensitive tools go through the approval card, same as the hook relay.
//
// Kept separate from `pipe.rs` on purpose: the pipe is the Claude Code relay
// for external terminal sessions, with its own approval cards. This module is
// the island's own chat and nothing else.

use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use crate::claude::{Chat, ChatContext, ChatReply};
use crate::log;

// ── Local conversation store ──────────────────────────────────────────────

fn chats_dir() -> std::path::PathBuf {
    crate::settings::data_dir().join("chats")
}

#[derive(Serialize, Deserialize)]
struct StoredConvo {
    id: String,
    title: String,
    updated: f64,
    messages: Vec<serde_json::Value>,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn convo_id() -> String {
    format!("c{}", now_secs().to_string().replace('.', ""))
}

/// Title from the first message, ChatGPT-style: the opening words, not the
/// whole question. Anything blank falls back so a row never renders empty.
fn title_for(query: &str) -> String {
    let first_line = query.lines().next().unwrap_or("").trim();
    let short: String = first_line.chars().take(42).collect();
    if short.trim().is_empty() {
        "Untitled".to_string()
    } else {
        short
    }
}

fn load_convo(id: &str) -> Option<StoredConvo> {
    if id.contains('/') || id.contains('\\') || id.contains('\0') || id.is_empty() {
        return None;
    }
    let bytes = std::fs::read(chats_dir().join(format!("{id}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_convo(convo: &StoredConvo) {
    let _ = std::fs::create_dir_all(chats_dir());
    if let Ok(json) = serde_json::to_vec_pretty(convo) {
        let _ = std::fs::write(chats_dir().join(format!("{}.json", convo.id)), json);
    }
}

// ── Brain dependency: the gateway ─────────────────────────────────────────

/// omnirouter's port. The island's provider endpoint points here, so a silent
/// dead gateway reads as "chat is broken" with no further explanation.
const OMNI_PORT: u16 = 20128;
const OMNI_BIN: &str = "/usr/bin/omniroute";

/// True when OUR gateway answers: TCP open plus GET /v1/models → 200. The
/// models check matters — an open port alone could be anything squatting
/// there, and chatting at a stranger would fail weirdly instead of clearly.
fn gateway_alive() -> bool {
    use std::io::{Read, Write};
    let addr = format!("127.0.0.1:{OMNI_PORT}");
    let Ok(mut stream) = std::net::TcpStream::connect(&addr) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    if stream.write_all(b"GET /v1/models HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return false;
    }
    let mut head = [0u8; 64];
    let Ok(n) = stream.read(&mut head) else { return false };
    String::from_utf8_lossy(&head[..n]).contains("200")
}

/// Make sure the brain is running, starting it if it isn't. Fire-and-forget
/// thread: boot must never block on node startup (catalog load takes many
/// seconds), and turns that arrive early fail fast with the usual connection
/// error instead of hanging.
///
/// Only starts when the port is closed, so a manually launched gateway (or a
/// previous daemon) is left strictly alone. The daemon supervises itself
/// (auto-restart on crash); Coco just knocks the first domino.
pub fn ensure_omnirouter() {
    std::thread::spawn(|| {
        if gateway_alive() {
            log::line("omniroute already up".to_string());
            return;
        }
        log::line("omniroute down — starting daemon".to_string());
        let log_path = crate::settings::data_dir().join("omniroute.log");
        let open_log = || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .ok()
                .map(std::process::Stdio::from)
                .unwrap_or(std::process::Stdio::null())
        };
        // --daemon forks and returns; --no-open/--no-tray keep a headless
        // boot free of surprise browser windows and tray icons.
        let spawned = std::process::Command::new(OMNI_BIN)
            .args(["serve", "--daemon", "--no-open", "--no-tray"])
            .stdin(std::process::Stdio::null())
            .stdout(open_log())
            .stderr(open_log())
            .spawn();
        if let Err(err) = spawned {
            log::line(format!("omniroute failed to start: {err}"));
            return;
        }
        for _ in 0..120 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if gateway_alive() {
                log::line("omniroute ready".to_string());
                return;
            }
        }
        log::line("omniroute did not answer within 60s — see omniroute.log".to_string());
    });
}

/// Fast probe before a turn: if the gateway died mid-session, say so plainly
/// and kick a restart instead of letting the turn die on a bare connection
/// error. Costs ~1 ms when healthy.
fn require_gateway() -> Result<(), String> {
    if gateway_alive() {
        return Ok(());
    }
    ensure_omnirouter();
    Err("brain is starting — ask again in a few seconds.".to_string())
}

/// What the model would otherwise burn a probe per turn discovering: distro
/// name from os-release, package manager inferred from it, today's date.
/// Read once per turn; failure degrades to "unknown", never to an error.
fn distro_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                l.strip_prefix("PRETTY_NAME=")
                    .map(|v| v.trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| "Linux".to_string())
}

fn package_manager() -> &'static str {
    let Ok(text) = std::fs::read_to_string("/etc/os-release") else {
        return "unknown";
    };
    let id = text
        .lines()
        .find_map(|l| l.strip_prefix("ID="))
        .unwrap_or("")
        .trim_matches('"');
    let like = text
        .lines()
        .find_map(|l| l.strip_prefix("ID_LIKE="))
        .unwrap_or("")
        .trim_matches('"');
    for token in format!("{id} {like}").split_whitespace() {
        match token {
            "arch" | "manjaro" | "endeavouros" | "cachyos" => return "pacman",
            "debian" | "ubuntu" | "linuxmint" | "pop" => return "apt",
            "fedora" | "rhel" | "centos" => return "dnf",
            "opensuse" | "suse" => return "zypper",
            "alpine" => return "apk",
            "nixos" => return "nix",
            _ => {}
        }
    }
    "unknown"
}

fn chrono_lite_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

// ── Chat ────────────────────────────────────────────────────────────────────

/// Cancel flag for the in-flight turn, if any. Set by chat_cancel, read at
/// every loop boundary (iteration top, before/after each tool). An LLM POST
/// already in flight is the only thing that outruns it (client timeout
/// bounds that at 90 s); everything else stops within a step.
static CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Stop the running turn, if any. The loop checks the flag at every boundary
/// and unwinds itself (history stays consistent); the island's await then
/// resolves like any other turn end. No turn running: harmless no-op.
#[tauri::command]
pub async fn chat_cancel() -> Result<(), String> {
    CANCEL.store(true, Ordering::Relaxed);
    Ok(())
}

fn cancelled() -> bool {
    CANCEL.swap(false, Ordering::Relaxed)
}

/// Sleep-poll twin of cancelled() for racing an in-flight HTTP call: dropping
/// the request future aborts the connection for real, so cancel lands in
/// ~100 ms even mid-download instead of at the next loop boundary (or the
/// 90 s client timeout). Shares the flag with cancelled() — whichever arm
/// fires first owns the abort, and both lead to the same unwind.
async fn cancelled_async() {
    loop {
        if CANCEL.swap(false, Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// One chat turn, straight to the configured provider. The API key and any
/// file bytes stay on the Rust side; the transcript persists locally.
#[tauri::command]
pub async fn chat_send(
    app: AppHandle,
    pending: State<'_, LocalPending>,
    shared: State<'_, crate::Shared>,
    chat: State<'_, Chat>,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let (provider, model, endpoint) = {
        let s = shared.settings.lock().unwrap();
        (s.provider.clone(), s.model.clone(), s.endpoint.clone())
    };
    // Local gateway only: a remote custom endpoint is someone else's server,
    // and probing OUR port for it would lie.
    if provider == "custom" && (endpoint.contains("localhost") || endpoint.contains("127.0.0.1")) {
        require_gateway()?;
    }
    // A conversation starts with its first send, titled from it. Resuming
    // sets the id (see chat_open); reset clears it (see chat_reset).
    if chat.convo().is_none() {
        let id = convo_id();
        chat.set_convo(&id);
        save_convo(&StoredConvo {
            id,
            title: title_for(&query),
            updated: now_secs(),
            messages: Vec::new(),
        });
    }
    if provider == "anthropic" {
        return direct_anthropic(&shared, &chat, &provider, &model, &endpoint, query, context).await;
    }
    agent_turn(app, pending, shared, chat, &provider, &model, &endpoint, query, context).await
}

/// The native-Anthropic path: single shot, no tools. Unchanged behavior.
async fn direct_anthropic(
    shared: &State<'_, crate::Shared>,
    chat: &State<'_, Chat>,
    provider: &str,
    model: &str,
    endpoint: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let (provider, model, endpoint) = (provider.to_string(), model.to_string(), endpoint.to_string());
    let host = provider.clone();
    cancelled(); // stale clicks from a quiet island must not kill this turn
    let reply = tokio::select! {
        biased;
        _ = cancelled_async() => {
            crate::claude::pop_last(chat);
            persist(&shared, &chat);
            log::line("chat cancelled by user".to_string());
            return Err("Cancelled.".to_string());
        }
        r = crate::claude::send(chat, &provider, &model, &endpoint, query, context) => r.map_err(|err| {
            log::line(format!("chat FAILED provider={provider} model={model} via={host}: {err}"));
            err
        })?,
    };
    persist(&shared, &chat);
    log::line(format!("chat ok provider={provider} model={model} via={host}"));
    Ok(reply)
}

/// Write the current wire history back to this conversation's file. Titles are
/// set at creation and never rewritten — the opening words stay the label.
fn persist(shared: &State<'_, crate::Shared>, chat: &State<'_, Chat>) {
    let _ = shared;
    if let Some(id) = chat.convo() {
        let mut convo = load_convo(&id).unwrap_or(StoredConvo {
            id: id.clone(),
            title: "Untitled".to_string(),
            updated: 0.0,
            messages: Vec::new(),
        });
        convo.messages = chat.snapshot();
        convo.updated = now_secs();
        save_convo(&convo);
    }
}

// ── Agent loop ──────────────────────────────────────────────────────────────

/// Tools the model may use freely: read-only or trivially reversible. Anything
/// else (open_app, run_command, anything unrecognized) goes to the approval
/// card — the island's human-in-the-loop, same as the hook relay's.
const AUTO_TOOLS: &[&str] = &["notify", "find_files", "music", "memory_save", "memory_recall", "web_search", "web_fetch"];
const ASK_TOOLS: &[&str] = &["open_app", "run_command"];
const MAX_ITERS: usize = 8;
static LOCAL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Answers to pending local approval cards, by request id. The turn holds the
/// chat command open while the human decides — minutes are fine, restart is
/// the only thing that orphans a wait (and reset clears the flag anyway).
pub struct LocalPending(std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>);

impl Default for LocalPending {
    fn default() -> Self {
        Self(std::sync::Mutex::new(std::collections::HashMap::new()))
    }
}

/// The island answered a local card. Unknown ids are ignored — a double-click
/// or a stale card can never resolve someone else's wait.
#[tauri::command]
pub fn local_decide(pending: State<'_, LocalPending>, request_id: String, allow: bool) {
    if let Some(tx) = pending.0.lock().unwrap().remove(&request_id) {
        let _ = tx.send(allow);
    }
}

/// One agentic turn: prompt, then tool calls until the model answers plainly.
/// Every approved/denied/auto tool result re-enters the conversation, so the
/// final text already accounts for what happened — the island shows words,
// never a transcript of the machinery.
#[allow(clippy::too_many_arguments)]
async fn agent_turn(
    app: AppHandle,
    pending: State<'_, LocalPending>,
    shared: State<'_, crate::Shared>,
    chat: State<'_, Chat>,
    provider: &str,
    model: &str,
    endpoint: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    use crate::claude as C;

    let (url, key) = C::openai_credentials(provider, endpoint)?;
    let host = if provider == "custom" { endpoint.to_string() } else { provider.to_string() };
    let first = chat.snapshot().is_empty();
    let user_text = C::openai_user_text(first, &context, &query);
    C::push_user(&chat, &user_text);
    let profile = {
        let s = shared.settings.lock().unwrap();
        let mut parts = Vec::new();
        // Ground truth the model would otherwise probe for (one uname/echo
        // per turn, forever): OS, distro + package manager, desktop, date.
        parts.push(format!(
            "System: {} {} on {}, desktop {}, package manager {}, date {}",
            std::env::consts::OS,
            distro_name(),
            std::env::consts::ARCH,
            std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into()),
            package_manager(),
            chrono_lite_date(),
        ));
        if !s.user_name.trim().is_empty() {
            parts.push(format!("Name: {}", s.user_name.trim()));
        }
        if !s.music_taste.trim().is_empty() {
            parts.push(format!("Music: {}", s.music_taste.trim()));
        }
        if !s.setup_notes.trim().is_empty() {
            parts.push(format!("Setup: {}", s.setup_notes.trim()));
        }
        parts.join("\n")
    };
    // Remembered rules snapshot once per turn: edits mid-turn apply next
    // turn, so a turn's policy can never shift under it.
    let rules = shared.settings.lock().unwrap().tool_rules.clone();
    let mut messages = C::openai_base_messages(&chat, &profile);

    // Hands, fetched live so the model is offered exactly what exists.
    // Failure degrades to tool-less chat, never to a failed turn.
    let tools = match crate::mcp_call::tool_definitions().await {
        Ok(Value::Array(defs)) if !defs.is_empty() => Some(Value::Array(defs)),
        Ok(_) => None,
        Err(err) => {
            log::line(format!("mcp tools unavailable, chatting tool-less: {err}"));
            None
        }
    };

    // Per-turn argv memory, across all iterations: exact repeats return
    // cached answers (see run_one_tool) instead of re-running and re-asking.
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // A cancel from an earlier no-turn click must not kill this one.
    cancelled();
    // Shared abort: pop the unanswered question, persist the steady state,
    // clear the progress line, report like any other turn end.
    let abort = |chat: &State<'_, Chat>| {
        C::pop_last(chat);
        persist(&shared, chat);
        log::line("chat cancelled by user".to_string());
    };
    for _ in 0..MAX_ITERS {
        if cancelled() {
            abort(&chat);
            progress(&app, "", &json!({}));
            return Err("Cancelled.".to_string());
        }
        let reply = match post_turn(&url, provider, &key, model, &messages, tools.as_ref()).await? {
            Some(r) => r,
            // Cancelled mid-download: the request is already dead, unwind.
            None => {
                abort(&chat);
                progress(&app, "", &json!({}));
                return Err("Cancelled.".to_string());
            }
        };
        let text = C::reply_text(&reply);
        let calls = C::parse_tool_calls(&reply);
        if calls.is_empty() {
            if text.is_empty() {
                C::pop_last(&chat);
                let detail = C::reply_error(&reply).unwrap_or_else(|| "No response text.".to_string());
                progress(&app, "", &json!({}));
                log::line(format!("chat FAILED provider={provider} model={model} via={host}: {detail}"));
                return Err(detail);
            }
            C::push_assistant(&chat, &text);
            persist(&shared, &chat);
            progress(&app, "", &json!({}));
            log::line(format!("chat ok provider={provider} model={model} via={host} (+tools)"));
            return Ok(ChatReply { text });
        }
        // The assistant's move, with its calls attached — the API requires the
        // assistant message to precede the tool results that follow.
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": if text.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(text) },
            "tool_calls": calls.iter().map(|c| serde_json::json!({
                "id": c.id, "type": "function",
                "function": {"name": c.name, "arguments": serde_json::to_string(&c.args).unwrap_or_default()},
            })).collect::<Vec<_>>(),
        }));
        for call in calls {
            let args = match &call.args {
                Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                v @ Value::Object(_) => v.clone(),
                _ => json!({}),
            };
            let result = run_one_tool(&app, &pending, &rules, &mut seen, &call.name, &args).await;
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": result,
            }));
        }
    }
    // Eight rounds of tools and still going: stop asking, take plain text.
    if cancelled() {
        abort(&chat);
        progress(&app, "", &json!({}));
        return Err("Cancelled.".to_string());
    }
    let reply = match post_turn(&url, provider, &key, model, &messages, None).await? {
        Some(r) => r,
        None => {
            abort(&chat);
            progress(&app, "", &json!({}));
            return Err("Cancelled.".to_string());
        }
    };
    let text = C::reply_text(&reply);
    if text.is_empty() {
        C::pop_last(&chat);
        progress(&app, "", &json!({}));
        return Err("the turn ran long without answering.".to_string());
    }
    C::push_assistant(&chat, &text);
    persist(&shared, &chat);
    progress(&app, "", &json!({}));
    log::line(format!("chat ok provider={provider} model={model} via={host} (+tools, capped)"));
    Ok(ChatReply { text })
}

/// POST with a live cancel race. `Ok(None)` means the user cancelled: the
/// request future was dropped mid-flight (aborting the connection), and the
/// caller unwinds the same way as a boundary cancel.
async fn post_turn(
    url: &str,
    provider: &str,
    key: &str,
    model: &str,
    messages: &[Value],
    tools: Option<&Value>,
) -> Result<Option<Value>, String> {
    use crate::claude as C;
    tokio::select! {
        biased;
        _ = cancelled_async() => Ok(None),
        r = C::post_chat_json(url, provider, key, model, messages, tools) => r.map(Some),
    }
}
/// First matching remembered rule, if any. Matching is substring on the
/// command line (run_command, sudo-prefixed when elevated) or the app name
/// (open_app); an empty match covers the whole tool. Order is file order.
fn rule_decision(
    rules: &[crate::settings::ToolRule],
    name: &str,
    args: &Value,
) -> Option<bool> {
    for rule in rules {
        if rule.tool != name {
            continue;
        }
        if rule.r#match.trim().is_empty() {
            return Some(rule.allow);
        }
        let haystack = match name {
            "run_command" => {
                let argv: Vec<String> = args
                    .get("command")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let sudo = args.get("sudo").and_then(Value::as_bool).unwrap_or(false);
                format!("{}{}", if sudo { "sudo " } else { "" }, argv.join(" "))
            }
            "open_app" => args.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            _ => continue,
        };
        if haystack.to_lowercase().contains(&rule.r#match.to_lowercase()) {
            return Some(rule.allow);
        }
    }
    None
}

/// Commands with no side effects beyond answering: system facts, directory
/// listings (names, never contents), and read-only package queries. These run
/// without a card — asking twelve times for `uname` is how trust dies.
/// Everything that reads FILE CONTENTS (cat, grep of files), writes, installs
/// or touches the network still asks, because outputs flow to a third-party
/// model and contents are exfiltration-shaped.
fn readonly_command(argv: &[String]) -> bool {
    let (first, rest) = match argv.split_first() {
        Some(pair) => pair,
        None => return false,
    };
    let base = first.rsplit('/').next().unwrap_or(first);
    match base {
        "uname" | "echo" | "printf" | "whoami" | "date" | "pwd" | "hostname" | "who" | "id"
        | "which" | "ls" | "true" | "basename" | "dirname" => true,
        "pacman" => rest.first().map(|f| f.starts_with("-Q")).unwrap_or(false),
        _ => false,
    }
}

/// One tool call through policy and, where required, the human. `seen` is the
/// turn's memory: an exact-repeat argv returns its cached answer with a nudge
/// instead of re-running, because models that just probed successfully probe
/// again (observed: the same echo four times in one turn).
async fn run_one_tool(
    app: &AppHandle,
    pending: &State<'_, LocalPending>,
    rules: &[crate::settings::ToolRule],
    seen: &mut std::collections::HashMap<String, String>,
    name: &str,
    args: &Value,
) -> String {
    if AUTO_TOOLS.contains(&name) {
        return mcp_text(name, args).await;
    }
    if !ASK_TOOLS.contains(&name) {
        // Not offered, so the model invented it. Say so outright — a silent
        // drop would teach it that calls vanish.
        return format!("unknown tool {name:?}: only notify, open_app, find_files, music, run_command, memory_save, memory_recall, web_search and web_fetch exist.");
    }
    // Remembered rules, first match wins. Written by the human in settings,
    // never by the model — this is the "don't ask me for ls again" list.
    // Unmatched tools fall through below.
    if let Some(allow) = rule_decision(rules, name, args) {
        if allow {
            log::line(format!("tool rule allowed {name}"));
            progress(app, name, args);
            return mcp_text(name, args).await;
        }
        log::line(format!("tool rule denied {name}"));
        return "Blocked by your remembered rules. Tell the user plainly and stop.".to_string();
    }
    // Read-only commands run free (see readonly_command for the boundary).
    // Everything else that reaches here asks.
    if name == "run_command" {
        let argv: Vec<String> = args
            .get("command")
            .and_then(|c| c.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if readonly_command(&argv) {
            return mcp_text(name, args).await;
        }
        // Exact repeats hit the turn cache instead of re-running: same answer,
        // plus a nudge the model actually obeys (it fetched this already).
        let key = format!("run_command\0{}", argv.join("\0"));
        if let Some(prev) = seen.get(&key) {
            return format!("{prev}\n(already ran this exact command — same result, do not call again.)");
        }
        let out = mcp_text(name, args).await;
        seen.insert(key, out.clone());
        return out;
    }
    if crate::integrations::PAUSED.load(Ordering::Relaxed) {
        // Paused means paused: no cards, no waits, no side effects. The turn
        // continues with a denial, same as the hook relay's decline.
        return "The user paused the island — denied. Say so briefly and stop.".to_string();
    }
    // Approval card, same furniture as the hook relay: pin, sound, alert are
    // the frontend's job once the event lands (see main.ts local-permission).
    let id = format!("loc-{}", LOCAL_SEQ.fetch_add(1, Ordering::Relaxed));
    let target = tool_target(name, args);
    let (tx, rx) = tokio::sync::oneshot::channel();
    pending.0.lock().unwrap().insert(id.clone(), tx);
    let _ = app.emit(
        "local-permission",
        serde_json::json!({ "requestId": id, "tool": name, "command": target }),
    );
    match rx.await {
        Ok(true) => {
            log::line(format!("local permission {id} allowed ({name})"));
            progress(app, name, args);
            let out = mcp_text(name, args).await;
            // Cache approved runs too, or the model re-asks and re-runs them.
            if name == "run_command" {
                if let Some(argv) = args.get("command").and_then(|c| c.as_array()) {
                    let argv: Vec<String> = argv
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect();
                    seen.insert(format!("run_command\0{}", argv.join("\0")), out.clone());
                }
            }
            out
        }
        // Denied — or the app died mid-wait and the sender vanished. Either
        // way the turn continues with words, never with a hang.
        _ => {
            log::line(format!("local permission {id} denied ({name})"));
            "The user denied this action. Say what happened and stop — do not try alternatives.".to_string()
        }
    }
}

/// Tell the island what the turn is doing right now, in words. Instant tools
/// (notify, lookups) skip this — narration is for the slow ones, where the
/// alternative is a minute of dots and a user reaching for a kill switch they
/// don't have. Empty tool clears the line (turn end).
fn progress(app: &AppHandle, tool: &str, args: &Value) {
    let detail = if tool.is_empty() { String::new() } else { tool_target(tool, args) };
    let _ = app.emit("tool-progress", serde_json::json!({ "tool": tool, "detail": detail }));
}

/// What the Allow button authorises, in the card's own words.
fn tool_target(name: &str, args: &Value) -> String {
    let str_arg = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    match name {
        "run_command" => {
            let argv: Vec<String> = args
                .get("command")
                .and_then(|c| c.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let sudo = args.get("sudo").and_then(Value::as_bool).unwrap_or(false);
            format!("{}{}", if sudo { "sudo " } else { "" }, argv.join(" "))
        }
        "open_app" => format!("open {}", str_arg("name")),
        _ => name.to_string(),
    }
}

/// The tool's own answer, success or failure phrased for the model. Transport
/// trouble also comes back as words — the turn degrades, never dies.
async fn mcp_text(name: &str, args: &Value) -> String {
    match crate::mcp_call::call_tool(name, args).await {
        Ok((false, text)) => {
            if text.trim().is_empty() {
                "(done, no output)".to_string()
            } else {
                text
            }
        }
        Ok((true, text)) => format!("The tool reported an error:\n{text}"),
        Err(err) => format!("The tool could not run: {err}"),
    }
}

/// New conversation: history clears here and in the island; the next send
/// starts a fresh file. Anything unsent is simply gone — reset means reset.
#[tauri::command]
pub fn chat_reset(chat: State<'_, Chat>) {
    chat.reset();
}

// ── History ─────────────────────────────────────────────────────────────────

/// One row of the conversation list.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionCard {
    pub id: String,
    pub title: String,
    /// Unix seconds, for the island's relative-time labels.
    pub updated: f64,
}

/// Every local conversation, newest first. The directory IS the database:
/// one file per conversation, no index to corrupt, unparseable files skipped.
#[tauri::command]
pub fn chat_sessions() -> Result<Vec<SessionCard>, String> {
    let mut cards = Vec::new();
    let Ok(entries) = std::fs::read_dir(chats_dir()) else {
        return Ok(cards);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(convo): Result<StoredConvo, _> = serde_json::from_slice(&bytes) else { continue };
        cards.push(SessionCard { id: convo.id, title: convo.title, updated: convo.updated });
    }
    cards.sort_by(|a, b| b.updated.partial_cmp(&a.updated).unwrap_or(std::cmp::Ordering::Equal));
    Ok(cards)
}

/// One chat bubble for display.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChatMsg {
    pub role: String,
    pub text: String,
}

/// Display text out of a stored wire block. Both shapes occur: OpenAI-style
/// `{role, content: "string"}` and Anthropic-style
/// `{role, content: [{type: "text", text}]}`. Anything else (tool blocks)
/// is not chat and stays out.
fn display_text(block: &serde_json::Value) -> Option<String> {
    match block.get("content") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(serde_json::Value::Array(parts)) => {
            let mut text = String::new();
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) != Some("text") {
                    continue;
                }
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

/// Resume a conversation: restore its exact wire history (so follow-ups keep
/// full context) and hand back display bubbles. Unknown ids and unparseable
/// files fail loudly — the island shows why instead of an empty chat.
#[tauri::command]
pub fn chat_open(chat: State<'_, Chat>, id: String) -> Result<Vec<ChatMsg>, String> {
    let convo = load_convo(&id).ok_or_else(|| "conversation not found".to_string())?;
    chat.restore(convo.messages.clone(), &convo.id);
    log::line(format!("conversation resumed: {}", convo.title));
    Ok(convo
        .messages
        .iter()
        .filter_map(|b| {
            let role = b.get("role").and_then(|r| r.as_str())?;
            if role != "user" && role != "assistant" {
                return None;
            }
            Some(ChatMsg { role: role.to_string(), text: display_text(b)? })
        })
        .collect())
}

// ── MCP hands (installed helper only) ───────────────────────────────────────

/// Install the system-action server binary (same layout as the hook relay).
/// It is NOT auto-registered anywhere: with no agent runtime consuming MCP,
/// registration would point at nothing. External CLIs (e.g. terminal
/// `claude`) can use the installed binary via their own MCP config.
pub fn ensure_mcp() {
    use std::path::PathBuf;

    let dest = crate::settings::mcp_exe_path();
    if let Some(dir) = dest.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut candidates: Vec<PathBuf> = vec![dest.clone()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("coco-mcp"));
            candidates.push(parent.join("../release/coco-mcp"));
            candidates.push(parent.join("../target/release/coco-mcp"));
        }
    }
    // A just-built binary is newer than an installed one; same-source
    // rebuilds are often byte-identical, so size alone cannot decide.
    if let Some(src) = candidates.iter().find(|p| p.exists()) {
        let fresh = match (std::fs::metadata(src), std::fs::metadata(&dest)) {
            (Ok(a), Ok(b)) => a.modified().ok() > b.modified().ok() || a.len() != b.len(),
            _ => true,
        };
        if fresh {
            if let Err(err) = std::fs::copy(src, &dest) {
                crate::log::line(format!("could not install coco-mcp: {err}"));
                return;
            }
            #[allow(clippy::permissions_set_readonly_false)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
            }
            crate::log::line(format!("installed coco-mcp from {}", src.display()));
        }
    } else {
        crate::log::line(
            "coco-mcp not found — looked beside the app binary.".to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::rule_decision;
    use crate::settings::ToolRule;
    use serde_json::json;

    fn rule(tool: &str, m: &str, allow: bool) -> ToolRule {
        ToolRule { tool: tool.into(), r#match: m.into(), allow }
    }

    #[test]
    fn rules_match_substring_first_wins_unmatched_asks() {
        let rules = vec![
            rule("run_command", "ls", true),
            rule("run_command", "", false),
            rule("open_app", "firefox", true),
        ];
        let ls = json!({"command": ["ls", "-la"]});
        assert_eq!(rule_decision(&rules, "run_command", &ls), Some(true));
        // Empty match covers the whole tool — but only after the ls rule.
        let rm = json!({"command": ["rm", "-rf", "build"]});
        assert_eq!(rule_decision(&rules, "run_command", &rm), Some(false));
        assert_eq!(
            rule_decision(&rules, "open_app", &json!({"name": "Firefox"})),
            Some(true)
        );
        assert_eq!(
            rule_decision(&rules, "open_app", &json!({"name": "spotify"})),
            None
        );
        // Sudo prefixes the match haystack, so "sudo ls" still matches "ls".
        let sudo = json!({"command": ["ls"], "sudo": true});
        assert_eq!(rule_decision(&rules, "run_command", &sudo), Some(true));
    }
}

#[cfg(test)]
mod policy_tests {
    use super::readonly_command;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn readonly_boundary() {
        // The exact probes the model burned twelve cards on: free now.
        assert!(readonly_command(&argv(&["uname", "-a"])));
        assert!(readonly_command(&argv(&["echo", "$XDG_CURRENT_DESKTOP"])));
        assert!(readonly_command(&argv(&["pacman", "-Qs", "calculator"])));
        assert!(readonly_command(&argv(&["ls", "-la"])));
        // Writes, installs, file contents, pipes: still ask.
        assert!(!readonly_command(&argv(&["pacman", "-S", "x"])));
        assert!(!readonly_command(&argv(&["cat", "/etc/passwd"])));
        assert!(!readonly_command(&argv(&["sudo", "apt", "install", "x"])));
        assert!(!readonly_command(&argv(&["bash", "-c", "echo hi"])));
        assert!(!readonly_command(&[]));
    }
}
