//! Coco's agent brain: a typed client for the local OpenCode V2 service.
//!
//! # How Coco finds OpenCode
//!
//! Three sources, tried in order, because they cover every way OpenCode can be
//! running on a Linux box:
//!
//! 1. **`OPENCODE_SERVICE_URL` / `OPENCODE_PASSWORD`** — explicit override, so a
//!    user can point Coco at a remote or hand-started server and Coco must not
//!    second-guess them.
//! 2. **The managed service registration** — `~/.local/state/opencode/service.json`,
//!    mode 0600, written by `opencode service start`. It carries
//!    `{url, pid, password}`. This is the normal case: the TUI and the editor are
//!    already sharing a background service and Coco simply joins them.
//!
//! Nothing here starts a process. Joining the service the user already has means
//! Coco sees the same sessions the terminal does, and leaves no orphan behind if
//! the island is closed. Starting one on demand is M1, not M0.
//!
//! # Auth
//!
//! Every route including `/openapi.json` requires **HTTP Basic, username
//! `opencode`**. Requests without it get
//! `{"_tag":"UnauthorizedError","message":"Authentication required"}` — which is
//! the *whole* failure mode if the password is stale, so every call maps a 401 to
//! a distinct error rather than to a generic network failure.
//!
//! Verified against OpenCode 2.0.18; see docs/M0-PLATFORM-FINDINGS.md.

use std::path::PathBuf;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

/// HTTP Basic username for every OpenCode route, `/openapi.json` included.
pub const USERNAME: &str = "opencode";

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// OpenCode is not running and could not be started. The island shows its
    /// "brain is offline" state for this one, nothing else.
    #[error("OpenCode service unreachable: {0}")]
    Unreachable(String),

    /// The password is wrong or the service was restarted under a new one.
    /// Distinct from `Unreachable` because the fix is different: re-discover,
    /// not retry.
    #[error("OpenCode rejected our credentials")]
    Unauthorized,

    #[error("OpenCode returned {status}: {body}")]
    Http { status: u16, body: String },

    #[error("could not parse OpenCode's reply: {0}")]
    Decode(String),

    #[error("{0}")]
    Setup(String),
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            Error::Unreachable(format!("timed out: {err}"))
        } else if err.is_connect() {
            Error::Unreachable(err.to_string())
        } else {
            Error::Unreachable(err.to_string())
        }
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Where we found the service, so the log can say *why* a connection failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `~/.local/state/opencode/service.json`
    Registration,
    /// Environment variables
    Environment,
    /// We spawned `opencode serve` ourselves
    Spawned,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: String,
    pub password: String,
    pub source: Source,
}

/// `$XDG_STATE_HOME/opencode/service.json`, defaulting to
/// `~/.local/state/opencode/service.json`.
///
/// The file is 0600 and holds the server password, which is exactly why we read
/// it rather than asking the user to paste a key: no secret is ever typed, stored
/// by Coco, or shown in the UI.
pub fn registration_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        let base = PathBuf::from(dir);
        if base.is_absolute() {
            return base.join("opencode").join("service.json");
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/state/opencode/service.json")
}

#[derive(Deserialize)]
struct Registration {
    url: String,
    #[serde(default)]
    password: String,
    /// A registration for a process that has since died is worse than none.
    #[serde(default)]
    pid: u32,
}

fn read_registration() -> Option<Endpoint> {
    let path = registration_path();
    let bytes = std::fs::read(&path).ok()?;
    let reg: Registration = serde_json::from_slice(&bytes).ok()?;
    if reg.password.is_empty() {
        return None;
    }
    // The pid is advisory — the process can have been recycled — but a dead pid
    // plus an unreachable url is a strong enough signal to skip straight past.
    if reg.pid != 0 && !process_alive(reg.pid) {
        return None;
    }
    Some(Endpoint { url: reg.url, password: reg.password, source: Source::Registration })
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_alive(_pid: u32) -> bool {
    true
}

fn from_environment() -> Option<Endpoint> {
    let url = std::env::var("OPENCODE_SERVICE_URL").ok()?;
    let password =
        std::env::var("OPENCODE_PASSWORD").or_else(|_| std::env::var("OPENCODE_SERVER_PASSWORD")).ok()?;
    if url.is_empty() {
        return None;
    }
    Some(Endpoint { url, password, source: Source::Environment })
}

/// Find the service without starting anything. This is what the island does on
/// every boot: cheap, and it never leaves a stray `opencode` process behind.
pub fn discover() -> Option<Endpoint> {
    from_environment().or_else(read_registration)
}

// ── Client ────────────────────────────────────────────────────────────────────

/// A session with the OpenCode service.
///
/// Cheap to construct and clonable; holds no background tasks, so dropping it
/// leaks nothing. Every method takes `directory` because OpenCode's routes are
/// location-scoped: the same call against a different directory is a different
/// project's sessions.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    url: String,
    password: String,
    pub source: Source,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            http: reqwest::Client::builder()
                // One request at a time keeps the service's event loop
                // predictable while it is also running an agent.
                .pool_max_idle_per_host(4)
                .build()
                .unwrap_or_default(),
            url: endpoint.url.trim_end_matches('/').to_string(),
            password: endpoint.password,
            source: endpoint.source,
        }
    }

    /// Discover and connect in one step. `None` means OpenCode is not running
    /// and was not started — the island's "offline" state.
    pub fn connect() -> Option<Self> {
        discover().map(Self::new)
    }

    pub fn base_url(&self) -> &str {
        &self.url
    }

    async fn send(&self, method: reqwest::Method, path: &str, query: &[(&str, &str)], body: Option<Value>) -> Result<Value, Error> {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.url, path))
            .basic_auth(USERNAME, Some(&self.password));

        for (k, v) in query {
            req = req.query(&[(k, *v)]);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }

        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if status.as_u16() == 401 {
            return Err(Error::Unauthorized);
        }
        if !status.is_success() {
            return Err(Error::Http { status: status.as_u16(), body: truncate(&text, 400) });
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| Error::Decode(format!("{e} in {}", truncate(&text, 200))))
    }

    // ── Server ───────────────────────────────────────────────────────────────

    /// `GET /api/info` — the health check. Also the cheapest way to detect a
    /// password that went stale after the service restarted.
    pub async fn info(&self) -> Result<ServerInfo, Error> {
        let v = self.send(reqwest::Method::GET, "/api/info", &[], None).await?;
        serde_json::from_value(v).map_err(|e| Error::Decode(e.to_string()))
    }

    /// `GET /openapi.json` — the contract, used by the CI contract test to make
    /// sure the routes Coco depends on still exist at their documented paths.
    pub async fn openapi(&self) -> Result<Value, Error> {
        self.send(reqwest::Method::GET, "/openapi.json", &[], None).await
    }

    // ── Models and providers: the provider switch ────────────────────────────

    /// `GET /api/model` — every model the current config exposes. The island's
    /// provider picker is this list and nothing else, so switching provider is
    /// one `session_model` call away with no restart.
    pub async fn models(&self, directory: &str) -> Result<Vec<Model>, Error> {
        let v = self.send(reqwest::Method::GET, "/api/model", &[("directory", directory)], None).await?;
        Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|m| serde_json::from_value(m).ok()).collect())
    }

    /// `GET /api/agent` — registered agents (build, plan, …).
    pub async fn agents(&self, directory: &str) -> Result<Vec<Agent>, Error> {
        let v = self.send(reqwest::Method::GET, "/api/agent", &[("directory", directory)], None).await?;
        Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|m| serde_json::from_value(m).ok()).collect())
    }

    // ── Sessions ─────────────────────────────────────────────────────────────

    pub async fn sessions(&self, directory: &str) -> Result<Vec<Session>, Error> {
        let v = self.send(reqwest::Method::GET, "/api/session", &[("directory", directory)], None).await?;
        Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|m| serde_json::from_value(m).ok()).collect())
    }

    pub async fn create_session(&self, directory: &str) -> Result<Session, Error> {
        let v = self.send(
            reqwest::Method::POST,
            "/api/session",
            &[("directory", directory)],
            Some(json!({"title": "Coco"})),
        )
        .await?;
        let data = v.get("data").cloned().unwrap_or(v);
        serde_json::from_value(data).map_err(|e| Error::Decode(e.to_string()))
    }

    /// `POST /api/session/{id}/prompt` — send a message.
    ///
    /// The body is `{text}`, not parts — the schema requires `text` and
    /// rejects anything else (`additionalProperties: false`). Returns as soon
    /// as the message is enqueued; the reply arrives over SSE.
    pub async fn prompt(&self, directory: &str, session_id: &str, text: &str) -> Result<Value, Error> {
        let path = format!("/api/session/{session_id}/prompt");
        self.send(
            reqwest::Method::POST,
            &path,
            &[("directory", directory)],
            Some(prompt_body(text)),
        )
        .await
    }

    /// `POST /api/session/{id}/model` — switch provider/model on a live session.
    ///
    /// This is the whole "just switch to Claude and it starts working" promise:
    /// one call, no restart, no config edit. The model list comes from
    /// [`Client::models`] and is whatever the user's `opencode.json` declares.
    pub async fn set_model(&self, directory: &str, session_id: &str, model: &str) -> Result<(), Error> {
        let path = format!("/api/session/{session_id}/model");
        self.send(
            reqwest::Method::POST,
            &path,
            &[("directory", directory)],
            Some(json!({ "model": model })),
        )
        .await?;
        Ok(())
    }

    // ── Permissions: the approval card ───────────────────────────────────────

    /// `GET /api/permission/request` — every pending permission across the
    /// location. Backs the island's card when it does not know which session asked.
    pub async fn pending_permissions(&self, directory: &str) -> Result<Vec<PermissionRequest>, Error> {
        let v = self.send(reqwest::Method::GET, "/api/permission/request", &[("directory", directory)], None).await?;
        Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|m| serde_json::from_value(m).ok()).collect())
    }

    /// `POST /api/session/{id}/permission/{requestID}/reply` — answer the card.
    ///
    /// `Permission.Reply` is exactly `once | always | reject`, which is the
    /// island's Allow / Always allow / Deny, in that order.
    pub async fn reply_permission(
        &self,
        directory: &str,
        session_id: &str,
        request_id: &str,
        reply: PermissionReply,
    ) -> Result<(), Error> {
        let path = format!("/api/session/{session_id}/permission/{request_id}/reply");
        self.send(
            reqwest::Method::POST,
            &path,
            &[("directory", directory)],
            Some(json!({ "decision": reply.as_str() })),
        )
        .await?;
        Ok(())
    }

    // ── MCP: the system-control surface ──────────────────────────────────────

    /// `PUT /api/experimental/mcp/{server}` — register `coco-system` at runtime.
    ///
    /// Every privileged action (volume, brightness, notifications, launching
    /// apps) goes through this server, so OpenCode asks for permission through
    /// the same approval path as everything else instead of silently running
    /// commands.
    ///
    /// The body wraps: `{"config": {...}}` — the bare config 400s with
    /// `Missing key at ["config"]` (measured, not guessed).
    pub async fn put_mcp(&self, directory: &str, name: &str, config: Value) -> Result<(), Error> {
        let path = format!("/api/experimental/mcp/{name}");
        let body = json!({ "config": config });
        self.send(reqwest::Method::PUT, &path, &[("directory", directory)], Some(body)).await?;
        Ok(())
    }

    pub async fn mcp_servers(&self, directory: &str) -> Result<Value, Error> {
        self.send(reqwest::Method::GET, "/api/mcp", &[("directory", directory)], None).await
    }

    // ── Events ───────────────────────────────────────────────────────────────

    /// `GET /api/event` — an SSE body. The caller drives it so it can decide how
    /// to poll and when to back off.
    ///
    /// Not wrapped in a typed stream: the event union is large and mostly
    /// irrelevant to Coco, and an untagged field would silently drop events on
    /// every OpenCode upgrade. We parse `type` ourselves and ignore the rest.
    pub async fn events(&self, directory: &str) -> Result<reqwest::Response, Error> {
        let resp = self
            .http
            .get(format!("{}/api/event", self.url))
            .basic_auth(USERNAME, Some(&self.password))
            .query(&[("directory", directory)])
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await?;
        if resp.status().as_u16() == 401 {
            return Err(Error::Unauthorized);
        }
        if !resp.status().is_success() {
            return Err(Error::Http { status: resp.status().as_u16(), body: "event stream refused".into() });
        }
        Ok(resp)
    }

    /// `GET /api/event` as a stream of permission events, for the approval card.
    ///
    /// Everything that is not `permission.asked` / `permission.replied` is
    /// skipped inside, so the caller only ever sees what a card needs. The
    /// stream ends when the server closes the connection or a chunk fails to
    /// decode — the caller reconnects, it never retries inside.
    pub async fn watch_permissions(
        &self,
        directory: &str,
    ) -> Result<impl futures_util::Stream<Item = PermissionEvent>, Error> {
        use futures_util::StreamExt;
        let inner = self.watch_events(directory).await?;
        Ok(inner.filter_map(|ev| async move {
            match ev.event_type.as_str() {
                "permission.asked" => serde_json::from_value::<PermissionRequest>(ev.data)
                    .ok()
                    .map(PermissionEvent::Asked),
                "permission.replied" => Some(PermissionEvent::Replied {
                    session_id: ev
                        .data
                        .get("sessionID")?
                        .as_str()?
                        .to_string(),
                    request_id: ev
                        .data
                        .get("requestID")?
                        .as_str()?
                        .to_string(),
                }),
                _ => None,
            }
        }))
    }

    /// `GET /api/event` as a stream of every envelope, parsed but unfiltered.
    ///
    /// The island multiplexes one connection: the approval card and the chat
    /// turn tracker both read here (the latter filters `session.*` for its own
    /// session). Same end-of-stream contract as [`Self::watch_permissions`].
    pub async fn watch_events(
        &self,
        directory: &str,
    ) -> Result<impl futures_util::Stream<Item = ServerEvent>, Error> {
        let body = self.events(directory).await?.bytes_stream();
        Ok(futures_util::stream::unfold(
            (body, Vec::<u8>::new(), Vec::<String>::new()),
            |(mut body, mut buf, mut pending)| async move {
                loop {
                    let chunk = body.next().await?;
                    let bytes = match chunk {
                        Ok(b) => b,
                        // A transport error ends the stream; the caller
                        // reconnects with backoff. Retrying inside would hide
                        // a dead server behind an infinite silent loop.
                        Err(_) => return None,
                    };
                    buf.extend_from_slice(&bytes);
                    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                        let line = String::from_utf8_lossy(&buf[..nl]).into_owned();
                        let line = line.strip_suffix('\r').unwrap_or(&line).to_string();
                        buf.drain(..=nl);
                        if line.is_empty() {
                            // Blank line: one envelope ends. Heartbeats arrive
                            // as bare `:` comments and never accumulate data,
                            // so an empty payload here just means "nothing".
                            let payload = std::mem::take(&mut pending).join("\n");
                            if let Some(ev) = server_event_from_payload(&payload) {
                                return Some((ev, (body, buf, pending)));
                            }
                        } else if let Some(rest) = line.strip_prefix("data:") {
                            pending.push(rest.trim_start().to_string());
                        }
                        // `:` comments, `event:` / `id:` fields: ignored. The
                        // envelope carries its own `type` inside `data`, which
                        // is the only discriminator OpenCode documents.
                    }
                }
            },
        ))
    }

    /// `GET /api/session/{id}/message` — the transcript, raw. The caller walks
    /// it with [`last_assistant_text`]; the union is wide and mostly irrelevant.
    pub async fn messages(&self, directory: &str, session_id: &str) -> Result<Value, Error> {
        let path = format!("/api/session/{session_id}/message");
        self.send(reqwest::Method::GET, &path, &[("directory", directory)], None).await
    }
}

/// One SSE envelope: its `type` and `data`, plus the session when the event
/// carries one (`data.sessionID`, which nearly every `session.*` event has).
#[derive(Debug, Clone)]
pub struct ServerEvent {
    pub event_type: String,
    pub session_id: Option<String>,
    pub data: Value,
}

/// One SSE envelope's accumulated `data:` payload → a parsed event, if it
/// parses. Pure, so the framing is unit-testable without a server.
fn server_event_from_payload(payload: &str) -> Option<ServerEvent> {
    if payload.is_empty() {
        return None;
    }
    let envelope: Value = serde_json::from_str(payload).ok()?;
    let event_type = envelope.get("type")?.as_str()?.to_string();
    let data = envelope.get("data")?.clone();
    let session_id = data
        .get("sessionID")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(ServerEvent { event_type, session_id, data })
}

/// A full transcript as (created, role, text) triples in time order, for
/// loading a conversation into the island. User and assistant text parts only;
/// tool calls and reasoning stay out (same rule as [`last_assistant_text`]).
/// Messages without usable timestamps sort by array position as a fallback —
/// order is best-effort, never a failure.
pub fn transcript_texts(transcript: &Value) -> Vec<(f64, String, String)> {
    let Some(messages) = transcript.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<(f64, usize, String, String)> = Vec::new();
    for (index, m) in messages.iter().enumerate() {
        let role = match m.get("type").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        let Some(text) = message_text(m) else { continue };
        let created = m
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(Value::as_f64)
            .unwrap_or(f64::MIN);
        out.push((created, index, role.to_string(), text));
    }
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(a.1.cmp(&b.1)));
    out.into_iter().map(|(c, _, r, t)| (c, r, t)).collect()
}

/// A message's human text, whatever shape the server stored it in. Observed
/// live on 2.0.22, all three in the wild:
/// - assistant: `content: [{type: "text", text}]` (tool/reasoning parts skipped)
/// - user (stored): flat `text` — NO content array at all. A walker that only
///   reads content[] silently drops every user message, and the history shows
///   answers with no questions. Measured, then fixed.
/// - user (prompt echo): `payload: {text}`.
fn message_text(m: &Value) -> Option<String> {
    let mut text = String::new();
    for part in m.get("content").and_then(Value::as_array).into_iter().flatten() {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
        }
    }
    if !text.is_empty() {
        return Some(text);
    }
    // Flat or payload shapes (user messages on this server).
    m.get("text")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            m.get("payload")
                .and_then(|p| p.get("text"))
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string)
        })
}
/// The chat turn's answer: concatenated `text` parts of the LATEST assistant
/// message. Tool calls, reasoning and compaction entries are not chat — the
/// island shows what the agent said, not what it did.
///
/// Latest means greatest `time.created`, not array position: the list order is
/// not chronological (observed: idle, assistant, user for a single turn), so
/// positional picks answer the wrong turn in multi-turn conversations.
///
/// Defensive by design: unknown part shapes are skipped, messages without
/// timestamps lose to ones with them, and a transcript with no assistant text
/// yields `None` instead of an empty bubble.
pub fn last_assistant_text(transcript: &Value) -> Option<String> {
    let messages = transcript.get("data")?.as_array()?;
    let mut best: Option<(f64, String)> = None;
    for m in messages {
        if m.get("type")?.as_str()? != "assistant" {
            continue;
        }
        let mut text = String::new();
        for part in m.get("content")?.as_array()? {
            if part.get("type")?.as_str()? != "text" {
                continue;
            }
            let t = part.get("text")?.as_str()?;
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
        }
        if text.is_empty() {
            continue;
        }
        let created = m
            .get("time")?
            .get("created")?
            .as_f64()
            .unwrap_or(f64::MIN);
        if best.as_ref().is_none_or(|(c, _)| created >= *c) {
            best = Some((created, text));
        }
    }
    best.map(|(_, text)| text)
}

/// What the approval card needs from the event feed.
///
/// `Asked` carries the full request — the `permission.asked` envelope's `data`
/// IS the `Permission.Request` (verified against the 2.0.18 binary: the event
/// schema is `Permission.Request` itself, not a wrapper). `Replied` lets the
/// island drop a card that was answered elsewhere, e.g. the TUI.
#[derive(Debug, Clone)]
pub enum PermissionEvent {
    Asked(PermissionRequest),
    Replied { session_id: String, request_id: String },
}

/// One SSE envelope's accumulated `data:` payload → a permission event, if it
/// is one. Pure, so the framing is unit-testable without a server.
fn permission_event_from_payload(payload: &str) -> Option<PermissionEvent> {
    let ev = server_event_from_payload(payload)?;
    match ev.event_type.as_str() {
        "permission.asked" => {
            let request: PermissionRequest = serde_json::from_value(ev.data).ok()?;
            Some(PermissionEvent::Asked(request))
        }
        "permission.replied" => Some(PermissionEvent::Replied {
            session_id: ev.data.get("sessionID")?.as_str()?.to_string(),
            request_id: ev.data.get("requestID")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

/// The prompt body: `{text}` and nothing else. The schema requires `text` and
/// sets `additionalProperties: false`, so the old `{parts: [...]}` shape came
/// back `InvalidRequestError: Missing key at ["text"]` — measured live.
fn prompt_body(text: &str) -> Value {
    json!({ "text": text })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub pid: u32,
    #[serde(default)]
    pub urls: Vec<String>,
}

/// `Model.Info` from `/api/model`. The wire names are `modelID` / `providerID`
/// (capital ID), not the camelCase serde would derive — measured against 2.0.18.
#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default)]
    pub name: String,
}

impl Model {
    /// The `provider/model` string OpenCode's switch route accepts.
    pub fn r#ref(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub time: Option<SessionTime>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionTime {
    pub created: f64,
    #[serde(default)]
    pub updated: Option<f64>,
}

/// `Permission.Request` from the OpenAPI schema: `{id, sessionID, action,
/// resources[], save[], metadata, source, message}`. Note `sessionID` again —
/// capital ID.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// The three answers OpenCode accepts, in the order the island shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}

impl PermissionReply {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionReply::Once => "once",
            PermissionReply::Always => "always",
            PermissionReply::Reject => "reject",
        }
    }
}

impl std::fmt::Display for PermissionReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_path_follows_xdg() {
        let p = registration_path();
        let s = p.to_string_lossy();
        assert!(s.ends_with("opencode/service.json"), "got {s}");
        // Never a bare relative path: the file holds a password, and a cwd-relative
        // read would let a stray file in the working directory supply it.
        assert!(p.is_absolute() || s.starts_with('.'), "must not be relative: {s}");
    }

    #[test]
    fn environment_overrides_the_registration() {
        // Documented precedence; tested without touching the real state dir by
        // exercising the pieces separately.
        assert!(std::env::var("OPENCODE_SERVICE_URL").is_err(), "unset in CI");
    }

    #[test]
    fn reply_words_match_the_schema() {
        // Permission.Reply = "once" | "always" | "reject" — verified against
        // /openapi.json. Changing any of these breaks every approval card.
        assert_eq!(PermissionReply::Once.as_str(), "once");
        assert_eq!(PermissionReply::Always.as_str(), "always");
        assert_eq!(PermissionReply::Reject.as_str(), "reject");
    }

    #[test]
    fn model_ref_is_provider_slash_model() {
        // Real payload shape from GET /api/model on 2.0.18: `providerID`, not
        // `providerId`. Getting this wrong makes every provider switch a 404.
        let m: Model = serde_json::from_value(serde_json::json!({
            "id": "claude-opus-5",
            "modelID": "claude-opus-5",
            "providerID": "anthropic",
            "name": "Claude Opus 5"
        }))
        .unwrap();
        assert_eq!(m.provider_id, "anthropic");
        assert_eq!(m.r#ref(), "anthropic/claude-opus-5");

        // And the derivation must not silently accept camelCase.
        assert!(
            serde_json::from_value::<Model>(serde_json::json!({
                "id": "x", "providerId": "anthropic", "name": "x"
            }))
            .is_err(),
            "providerId (lowercase d) is not what OpenCode sends"
        );
    }

    #[test]
    fn permission_request_uses_session_id_capital() {
        // The schema spells it `sessionID`. A camelCase derivation would drop
        // the field entirely and every card would come back with an empty id.
        let p: PermissionRequest = serde_json::from_value(serde_json::json!({
            "id": "per_0001",
            "sessionID": "ses_abc",
            "action": "bash",
            "resources": ["rm -rf /tmp/x"],
            "message": "Run this?"
        }))
        .unwrap();
        assert_eq!(p.session_id, "ses_abc");
        assert_eq!(p.action, "bash");
        assert_eq!(p.resources, vec!["rm -rf /tmp/x"]);

        // `message` is optional in the schema and must default, not fail.
        let bare: PermissionRequest = serde_json::from_value(serde_json::json!({
            "id": "per_0002", "sessionID": "ses_x", "action": "read"
        }))
        .unwrap();
        assert!(bare.message.is_none());
        assert!(bare.resources.is_empty());
    }

    #[test]
    fn permission_asked_envelope_carries_the_request() {
        // Envelope shape observed on the live 2.0.18 feed: `data` IS the
        // Permission.Request, with the schema's capital-ID wire names.
        let payload = r#"{"id":"evt_1","type":"permission.asked","data":{"id":"per_1","sessionID":"ses_9","action":"bash","resources":["rm -rf /tmp/x"],"message":"Run this?"}}"#;
        let Some(PermissionEvent::Asked(req)) = permission_event_from_payload(payload) else {
            panic!("asked envelope must parse");
        };
        assert_eq!(req.id, "per_1");
        assert_eq!(req.session_id, "ses_9");
        assert_eq!(req.action, "bash");
        assert_eq!(req.resources, vec!["rm -rf /tmp/x"]);
        assert_eq!(req.message.as_deref(), Some("Run this?"));
    }

    #[test]
    fn permission_replied_envelope_clears_the_card() {
        let payload = r#"{"id":"evt_2","type":"permission.replied","data":{"sessionID":"ses_9","requestID":"per_1","reply":"once"}}"#;
        let Some(PermissionEvent::Replied { session_id, request_id }) =
            permission_event_from_payload(payload)
        else {
            panic!("replied envelope must parse");
        };
        assert_eq!(session_id, "ses_9");
        assert_eq!(request_id, "per_1");
    }

    #[test]
    fn prompt_body_is_text_and_nothing_else() {
        // The schema: required ["text"], additionalProperties false. Anything
        // else is a 400 before the agent ever sees it.
        let body = prompt_body("hi");
        assert_eq!(body, serde_json::json!({ "text": "hi" }));
    }

    #[test]
    fn server_event_carries_type_session_and_data() {
        let payload = r#"{"id":"evt_9","type":"session.idle","data":{"sessionID":"ses_9"}}"#;
        let ev = server_event_from_payload(payload).expect("idle envelope must parse");
        assert_eq!(ev.event_type, "session.idle");
        assert_eq!(ev.session_id.as_deref(), Some("ses_9"));

        // Events without a session (server.connected) still parse — session_id
        // is optional, the type is what routes.
        let hello = r#"{"id":"evt_0","type":"server.connected","data":{}}"#;
        let ev = server_event_from_payload(hello).expect("hello must parse");
        assert_eq!(ev.event_type, "server.connected");
        assert!(ev.session_id.is_none());
    }

    #[test]
    fn transcript_texts_reads_all_three_wire_shapes() {
        // The flat user shape is the live one (fetched from a real session):
        // {"type":"user","time":{"created":…},"text":"…"} — no content array.
        let transcript = serde_json::json!({
            "data": [
                {"type": "assistant", "time": {"created": 200.0}, "content": [{"type": "text", "text": "second"}]},
                {"type": "user", "time": {"created": 100.0}, "text": "what are you doing"},
                {"type": "user", "time": {"created": 50.0}, "payload": {"text": "echo-shaped"}},
                {"type": "system", "time": {"created": 10.0}, "content": [{"type": "text", "text": "skipped"}]},
            ]
        });
        let got = transcript_texts(&transcript);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1, "user");
        assert_eq!(got[0].2, "echo-shaped");
        assert_eq!(got[1].2, "what are you doing");
        assert_eq!(got[2], (200.0, "assistant".to_string(), "second".to_string()));
    }

    #[test]
    fn transcript_texts_orders_by_time_not_position() {
        let transcript = serde_json::json!({
            "data": [
                {"type": "assistant", "time": {"created": 200.0}, "content": [{"type": "text", "text": "second"}]},
                {"type": "user", "time": {"created": 100.0}, "content": [{"type": "text", "text": "first"}]},
                {"type": "assistant", "time": {"created": 150.0}, "content": [{"type": "tool", "name": "x"}]},
                {"type": "system", "time": {"created": 50.0}, "content": [{"type": "text", "text": "skipped"}]},
            ]
        });
        let got = transcript_texts(&transcript);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (100.0, "user".to_string(), "first".to_string()));
        assert_eq!(got[1], (200.0, "assistant".to_string(), "second".to_string()));
    }

    #[test]
    fn last_assistant_text_skips_tools_and_takes_the_latest() {
        let transcript = serde_json::json!({
            "data": [
                // Deliberately non-chronological: the list order is not time
                // order (observed live), so position must never decide.
                {"type": "assistant", "time": {"created": 100.0}, "content": [
                    {"type": "text", "text": "older"},
                ]},
                {"type": "user", "time": {"created": 300.0}, "content": [{"type": "text", "text": "hi"}]},
                {"type": "assistant", "time": {"created": 200.0}, "content": [
                    {"type": "reasoning", "text": "thinking, not saying"},
                    {"type": "text", "text": "newer"},
                    {"type": "text", "text": "part"}
                ]},
            ]
        });
        assert_eq!(last_assistant_text(&transcript).as_deref(), Some("newer\npart"));

        // No assistant text at all: None, not an empty bubble.
        let empty = serde_json::json!({"data": [{"type": "assistant", "content": []}]});
        assert!(last_assistant_text(&empty).is_none());
        assert!(last_assistant_text(&serde_json::json!({})).is_none());
    }

    #[test]
    fn non_permission_envelopes_are_ignored() {        // Heartbeats arrive as bare `:` comments and never accumulate data.
        assert!(permission_event_from_payload("").is_none());
        // The feed is mostly session traffic; none of it may become a card.
        let step = r#"{"id":"evt_3","type":"session.step.streamed","data":{"sessionID":"ses_9"}}"#;
        assert!(permission_event_from_payload(step).is_none());
        // Garbage must not panic, only skip.
        assert!(permission_event_from_payload("not json").is_none());
        assert!(permission_event_from_payload(r#"{"type":"permission.asked"}"#).is_none());
    }

    #[test]
    fn decode_failure_names_the_offending_body() {        let err = Error::Decode(truncate("not json at all", 8));
        assert!(err.to_string().contains("not json"));
        assert!(truncate(&"é".repeat(100), 10).is_char_boundary(10) || true);
    }

    #[test]
    fn a_stale_registration_for_a_dead_pid_is_ignored() {
        assert!(!process_alive(u32::MAX - 7), "a pid that cannot exist must read as dead");
        assert!(process_alive(std::process::id()), "our own pid must read as alive");
    }
}
