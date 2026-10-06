// The OpenCode brain: reachability, the approval card's server side, and the
// permission event feed.
//!
//! Three commands back the island: `opencode_status` (is the service there),
//! `opencode_permissions` (what is waiting) and `opencode_reply` (answer it).
//! The live feed is a background task that emits `opencode-permission` /
//! `opencode-replied` / `opencode-pending` to the island and reconnects forever.
//!
//! Kept separate from `pipe.rs` on purpose: the pipe is the Claude Code relay
//! over a unix socket with ack windows and terminal fallbacks; this is plain
//! HTTP against the OpenCode service with none of that machinery. Sharing code
//! between them would couple two transports that fail in different ways.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use coco_engine::{Client, PermissionReply};

use crate::claude::{Chat, ChatContext};
use crate::{integrations, log};

/// One island conversation = one OpenCode session. Created lazily on the first
/// send after boot/reset; dropped (forgotten, not deleted — the transcript
/// stays server-side) on reset. `busy` mirrors "a turn is running": sending
/// into an active turn is a 409, so the island refuses instead of failing.
#[derive(Default)]
pub struct Brain {
    session_id: std::sync::Mutex<Option<String>>,
    busy: AtomicBool,
}

impl Brain {
    fn session(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    fn forget(&self) {
        *self.session_id.lock().unwrap() = None;
        self.busy.store(false, Ordering::Relaxed);
    }
}

/// Reachability, never an error: the island renders "offline" instead of the
/// command failing, because a stopped service is a state, not a bug.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeStatus {
    pub reachable: bool,
    pub version: Option<String>,
    pub pid: Option<u32>,
    pub source: Option<String>,
}

impl OpencodeStatus {
    fn offline() -> Self {
        Self { reachable: false, version: None, pid: None, source: None }
    }
}

/// The approval card's wire shape. `PermissionRequest` is Deserialize-only, so
/// this is the serializable twin — and the place the frontend contract lives.
/// camelCase: Tauri's command macro converts snake_case params to camelCase
/// args by default, and emits use the same convention.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OpencodePermission {
    pub id: String,
    pub session_id: String,
    pub action: String,
    pub resources: Vec<String>,
    pub message: Option<String>,
}

impl From<&coco_engine::PermissionRequest> for OpencodePermission {
    fn from(r: &coco_engine::PermissionRequest) -> Self {
        Self {
            id: r.id.clone(),
            session_id: r.session_id.clone(),
            action: r.action.clone(),
            resources: r.resources.clone(),
            message: r.message.clone(),
        }
    }
}

/// Home is the default scope. Probes showed the service normalizes odd
/// directory values to home anyway, and a home-scoped event feed observes
/// sessions in every subdirectory.
fn directory() -> String {
    crate::settings::home().to_string_lossy().to_string()
}

fn connect() -> Result<Client, String> {
    Client::connect().ok_or_else(|| "OpenCode is not running".to_string())
}

/// A finished turn, either way. The island shows `text`, or the `error` note
/// view — never an empty bubble.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TurnResult {
    pub text: Option<String>,
    pub error: Option<String>,
}

// ── Chat turns ──────────────────────────────────────────────────────────────

/// One island message = one OpenCode turn. Returns as soon as the prompt is
/// accepted; the answer arrives later as an `opencode-turn` event, because
/// POST /prompt only enqueues — the reply streams over SSE after tools,
/// permission cards and whatever else the turn needed.
#[tauri::command]
pub async fn chat_send(
    brain: State<'_, Brain>,
    query: String,
    context: Option<ChatContext>,
) -> Result<(), String> {
    if brain.busy.load(Ordering::Relaxed) {
        return Err("Coco is still working on the last turn — hold on.".to_string());
    }
    let client = connect()?;
    let dir = directory();

    let mut text = query;
    match context {
        // The agent has shell and file tools; pointing at the inbox copy is
        // enough — no bytes cross the IPC.
        Some(ChatContext::File { name, path }) => {
            text = format!("Attached file {name} at {path} — read it if it is relevant.\n\n{text}");
        }
        Some(ChatContext::Window { app_name, title, url }) => {
            let where_at = url.map(|u| format!(" ({u})")).unwrap_or_default();
            text = format!("The user is looking at {app_name} — {title}{where_at}.\n\n{text}");
        }
        None => {}
    }

    let session_id = match brain.session() {
        Some(id) => id,
        None => {
            let session = client
                .create_session(&dir)
                .await
                .map_err(|e| format!("could not start a Coco session: {e}"))?;
            log::line(format!("opencode chat session {}", session.id));
            *brain.session_id.lock().unwrap() = Some(session.id.clone());
            session.id
        }
    };

    // A server restart orphans the stored session: one retry on a fresh one,
    // then the error is real.
    let prompted = client.prompt(&dir, &session_id, &text).await;
    if let Err(coco_engine::Error::Http { status: 404, .. }) = prompted {
        log::line(format!("opencode session {session_id} is gone — starting over"));
        let session = client
            .create_session(&dir)
            .await
            .map_err(|e| format!("could not start a Coco session: {e}"))?;
        *brain.session_id.lock().unwrap() = Some(session.id.clone());
        client
            .prompt(&dir, &session.id, &text)
            .await
            .map_err(|e| e.to_string())?;
    } else if let Err(err) = prompted {
        return Err(err.to_string());
    }

    brain.busy.store(true, Ordering::Relaxed);
    Ok(())
}

/// New conversation: local history clears in the island, and the next send
/// opens a fresh server session. Also the escape hatch if a turn ever sticks
/// busy — reset always unblocks sending.
#[tauri::command]
pub fn chat_reset(chat: State<'_, Chat>, brain: State<'_, Brain>) {
    chat.reset();
    brain.forget();
}

// ── Conversations ─────────────────────────────────────────────────────────

/// One row of the conversation list. Titles are agent-set or untitled;
/// `updated` is unix seconds for the island's relative-time labels.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionCard {
    pub id: String,
    pub title: String,
    pub updated: f64,
}

/// Every session the service knows at home scope, newest first. Never errors
/// on empty — a fresh install simply has no conversations yet.
#[tauri::command]
pub async fn opencode_sessions() -> Result<Vec<SessionCard>, String> {
    let client = connect()?;
    let dir = directory();
    let mut cards: Vec<SessionCard> = client
        .sessions(&dir)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|s| {
            let updated = s
                .time
                .as_ref()
                .and_then(|t| t.updated)
                .or_else(|| s.time.as_ref().map(|t| t.created))
                .unwrap_or(0.0);
            SessionCard {
                id: s.id,
                title: s.title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Untitled".to_string()),
                updated,
            }
        })
        .collect();
    cards.sort_by(|a, b| b.updated.partial_cmp(&a.updated).unwrap_or(std::cmp::Ordering::Equal));
    Ok(cards)
}

/// One chat bubble in a loaded transcript.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChatMsg {
    pub role: String,
    pub text: String,
}

/// Resume a conversation: point the brain at it and hand back its transcript
/// for display. Clears `busy` — switching conversations orphans any running
/// turn on the old session, and its late answer is ignored by generation
/// (see State.chatGen), so nothing can land in the wrong history.
#[tauri::command]
pub async fn opencode_open(brain: State<'_, Brain>, session_id: String) -> Result<Vec<ChatMsg>, String> {
    let client = connect()?;
    let dir = directory();
    let transcript = client.messages(&dir, &session_id).await.map_err(|e| e.to_string())?;
    *brain.session_id.lock().unwrap() = Some(session_id);
    brain.busy.store(false, Ordering::Relaxed);
    log::line("opencode conversation resumed".to_string());
    Ok(coco_engine::transcript_texts(&transcript)
        .into_iter()
        .map(|(_, role, text)| ChatMsg { role, text })
        .collect())
}

// ── MCP hands ───────────────────────────────────────────────────────────────

/// Install the system-action server binary (same layout as the hook relay),
/// then register it with the service. Called on every boot: the PUT is cheap
/// and idempotent, so a service restart never leaves the brain handless.
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
            "coco-mcp not found — the brain has no hands. Looked beside the app binary."
                .to_string(),
        );
    }
}

/// PUT the server config at home scope. Async because the service may still be
/// starting when Coco boots; failure just logs (the next boot retries).
pub async fn register_mcp() {
    let Some(client) = Client::connect() else {
        crate::log::line("coco-mcp registration skipped — OpenCode is not running".to_string());
        return;
    };
    let dir = directory();
    let exe = crate::settings::mcp_exe_path().to_string_lossy().to_string();
    let config = serde_json::json!({
        "type": "local",
        "command": [exe],
        "timeout": {"startup": 10, "catalog": 15, "execution": 120},
    });
    match client.put_mcp(&dir, "coco-system", config).await {
        Ok(()) => crate::log::line("coco-system MCP registered".to_string()),
        Err(err) => crate::log::line(format!("coco-system MCP registration failed: {err}")),
    }
}

#[tauri::command]
pub async fn opencode_status() -> OpencodeStatus {
    let Some(client) = Client::connect() else {
        return OpencodeStatus::offline();
    };
    match client.info().await {
        Ok(info) => OpencodeStatus {
            reachable: true,
            version: Some(info.version),
            pid: Some(info.pid),
            source: Some(format!("{:?}", client.source).to_lowercase()),
        },
        // A stale password after a service restart reads as unreachable, not
        // as an auth error: the fix is the same (the service owns the secret).
        Err(err) => {
            log::line(format!("opencode status: service found but unreadable: {err}"));
            OpencodeStatus::offline()
        }
    }
}

#[tauri::command]
pub async fn opencode_permissions(directory: Option<String>) -> Result<Vec<OpencodePermission>, String> {
    let client = connect()?;
    let dir = directory.unwrap_or_else(self::directory);
    client
        .pending_permissions(&dir)
        .await
        .map(|list| list.iter().map(OpencodePermission::from).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn opencode_reply(
    session_id: String,
    request_id: String,
    decision: String,
) -> Result<(), String> {
    let reply = match decision.as_str() {
        "allow" => PermissionReply::Once,
        "always" => PermissionReply::Always,
        "deny" => PermissionReply::Reject,
        _ => return Err(format!("unknown decision {decision:?} — want allow|always|deny")),
    };
    let client = connect()?;
    let dir = directory();
    match client.reply_permission(&dir, &session_id, &request_id, reply).await {
        Ok(()) => {
            log::line(format!("opencode permission {request_id} answered {reply}"));
            Ok(())
        }
        Err(err) => {
            // The common case is answering a request that is already gone
            // (replied in the TUI, session ended). That is not a failure worth
            // an error card — the card just clears.
            log::line(format!("opencode reply for {request_id} did not land: {err}"));
            Err(err.to_string())
        }
    }
}

/// The permission feed: connects, reconciles, streams, reconnects forever.
///
/// Reconnect backoff is capped so a dead service costs one log line every 30 s
/// instead of a hot loop. Nothing here ever throws: the task outlives every
/// failure by design.
pub fn spawn_permission_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut backoff_secs = 2u64;
        loop {
            run_watch(&app).await;
            log::line(format!("opencode event stream ended — reconnecting in {backoff_secs}s"));
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    });
}

async fn run_watch(app: &AppHandle) {
    let Some(client) = Client::connect() else {
        let _ = app.emit("opencode-status", OpencodeStatus::offline());
        return;
    };
    let dir = directory();

    // Reconcile on every (re)connect: the card is frontend state and the
    // server is the truth. Anything pending without a card gets surfaced by
    // the island; a card whose request is gone is dropped there.
    match client.pending_permissions(&dir).await {
        Ok(list) => {
            let pending: Vec<OpencodePermission> =
                list.iter().map(OpencodePermission::from).collect();
            log::line(format!(
                "opencode feed connected ({} pending)",
                pending.len()
            ));
            let _ = app.emit("opencode-pending", &pending);
        }
        Err(err) => {
            log::line(format!("opencode reconcile failed: {err}"));
            return;
        }
    }

    let stream = match client.watch_events(&dir).await {
        Ok(s) => s,
        Err(err) => {
            log::line(format!("opencode event stream refused: {err}"));
            return;
        }
    };
    let mut stream = Box::pin(stream);
    while let Some(ev) = futures_util::StreamExt::next(&mut stream).await {
        match ev.event_type.as_str() {
            "permission.asked" => {
                let Ok(req) =
                    serde_json::from_value::<coco_engine::PermissionRequest>(ev.data)
                else {
                    continue;
                };
                if integrations::PAUSED.load(Ordering::Relaxed) {
                    // A paused island cannot answer, and OpenCode waits
                    // indefinitely — an unanswered request wedges the agent.
                    // Rejecting lets it continue with a denial. (The Claude
                    // relay declines so the terminal takes the question; there
                    // is no terminal fallback here, so a denial is the
                    // least-wedge answer.)
                    match client
                        .reply_permission(&dir, &req.session_id, &req.id, PermissionReply::Reject)
                        .await
                    {
                        Ok(()) => log::line(format!(
                            "opencode permission {} auto-rejected (paused)",
                            req.id
                        )),
                        Err(err) => log::line(format!(
                            "opencode auto-reject for {} failed: {err}",
                            req.id
                        )),
                    }
                    continue;
                }
                let _ = app.emit("opencode-permission", OpencodePermission::from(&req));
            }
            "permission.replied" => {
                let session_id =
                    ev.data.get("sessionID").and_then(|v| v.as_str()).unwrap_or("");
                let request_id =
                    ev.data.get("requestID").and_then(|v| v.as_str()).unwrap_or("");
                let _ = app.emit(
                    "opencode-replied",
                    serde_json::json!({ "sessionId": session_id, "requestId": request_id }),
                );
            }
            // A turn ends when its execution does — succeeded, failed or
            // interrupted. (session.idle / session.error exist in the event
            // vocabulary and are kept as triggers too, but a plain prompt
            // turn was observed ending in execution.succeeded with no idle
            // after it — measured, not guessed.) Anything else for our session
            // (deltas, steps, tool calls) is progress the island shows as
            // thinking dots, not as content.
            "session.execution.succeeded"
            | "session.execution.failed"
            | "session.execution.interrupted"
            | "session.idle"
            | "session.error" => {
                // Fresh guard every event: the managed Brain is the only
                // owner of session/busy, never a snapshot.
                let Some(st) = app.try_state::<Brain>() else { continue };
                let ours = st.session();
                let is_ours = match (&ours, &ev.session_id) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if is_ours {
                    finish_turn(app, &client, &dir, &st).await;
                }
            }
            _ => {}
        }
    }
}

/// A turn finished: fetch the transcript, take the last thing the agent said,
/// hand it to the island, unblock sending. Every path clears `busy` — a stuck
/// flag would brick chat until restart, and reset is only the backup for that.
async fn finish_turn(app: &AppHandle, client: &Client, dir: &str, brain: &Brain) {
    let result = match client
        .messages(dir, &brain.session().unwrap_or_default())
        .await
    {
        Ok(transcript) => match coco_engine::last_assistant_text(&transcript) {
            Some(text) => {
                log::line(format!("opencode turn done ({} chars)", text.len()));
                TurnResult { text: Some(text), error: None }
            }
            None => TurnResult {
                text: None,
                error: Some("Coco finished without an answer — ask again.".to_string()),
            },
        },
        Err(err) => TurnResult {
            text: None,
            error: Some(format!("could not read the reply: {err}")),
        },
    };
    brain.busy.store(false, Ordering::Relaxed);
    let _ = app.emit("opencode-turn", &result);
}
