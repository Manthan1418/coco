// Claude API client — the same integration as ClaudeService.swift: multi-turn
// chat with web search, and files sent as document/image/text blocks.
//
// Everything happens here rather than in the island: the API key never leaves
// the Secret Service, and file bytes never cross the IPC boundary.
//
// Multi-provider: `provider` picks the wire format. `anthropic` is the native
// Messages API below; `openrouter` and `openai` share the OpenAI-compatible
// chat-completions shape (`POST …/v1/chat/completions`, Bearer key), which is
// what makes OpenRouter's `:free` models work with no other special-casing.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::secrets;

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server-side fallback: on a policy decline the API retries the same request on
/// a fallback model inside the same call, so the island never shows a dead end.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 4096;
/// Text and code files are inlined; anything larger is skipped, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;

/// OpenRouter's OpenAI-compatible endpoint. Free models are ordinary model IDs
/// ending in `:free` — the user picks them in settings, we just pass them through.
const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";

pub const DEFAULT_MODEL: &str = "claude-opus-5";
/// The default provider — direct Anthropic, i.e. exactly what the island did
/// before providers existed. Old settings.json files get this via serde default.
pub const DEFAULT_PROVIDER: &str = "anthropic";

const SYSTEM_PROMPT: &str = "You are Mochi, a personal AI assistant living at the top of the user's screen. \
You have web search access and can help with absolutely anything — research, coding, finding places, recommendations, tasks, questions. \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.
You have tools (notify, open_app, find_files, music, run_command): prefer using them over describing actions. run_command and open_app ask the user first — call them anyway when needed and wait for the result.";

#[derive(Default)]
pub struct Chat {
    /// Full multi-turn history, including tool_use / tool_result blocks.
    messages: Mutex<Vec<Value>>,
    /// Local conversation id backing this history (see brain.rs chats dir).
    /// None until the first send after boot/reset.
    convo: Mutex<Option<String>>,
}

impl Chat {
    pub fn reset(&self) {
        self.messages.lock().unwrap().clear();
        *self.convo.lock().unwrap() = None;
    }

    pub(crate) fn convo(&self) -> Option<String> {
        self.convo.lock().unwrap().clone()
    }

    pub(crate) fn set_convo(&self, id: &str) {
        *self.convo.lock().unwrap() = Some(id.to_string());
    }

    pub(crate) fn restore(&self, messages: Vec<Value>, convo: &str) {
        *self.messages.lock().unwrap() = messages;
        *self.convo.lock().unwrap() = Some(convo.to_string());
    }

    fn is_empty(&self) -> bool {
        self.messages.lock().unwrap().is_empty()
    }

    fn push(&self, message: Value) {
        self.messages.lock().unwrap().push(message);
    }

    fn pop(&self) {
        self.messages.lock().unwrap().pop();
    }

    pub(crate) fn snapshot(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
}

/// Direct chat for the native Anthropic provider only. The OpenAI-compatible
/// providers (openrouter, openai, custom) run the tool loop in brain.rs
/// instead — same endpoint, but with hands. Calling this for those providers
/// is a wiring bug, hence the loud error rather than a silent fallback.
pub async fn send(
    chat: &Chat,
    provider: &str,
    model: &str,
    _endpoint: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    if provider == "openrouter" || provider == "openai" || provider == "custom" {
        return Err("wiring bug: compat providers must run the brain tool loop".to_string());
    }
    send_anthropic(chat, model, query, context).await
}

/// The original direct-Anthropic path, unchanged.
async fn send_anthropic(
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let key = secrets::get("anthropic-api-key")
        .ok_or_else(|| "API key missing. Open settings.".to_string())?;

    let mut content: Vec<Value> = Vec::new();

    // File / window context rides along with the first message only, exactly
    // like ClaudeService.chat().
    if chat.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                if let Some(block) = file_block(path) {
                    content.push(block);
                }
                content.push(json!({ "type": "text", "text": format!("File: {name}") }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut text = format!("Context — App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    text.push_str(&format!(", URL: {url}"));
                }
                content.push(json!({ "type": "text", "text": text }));
            }
            None => {}
        }
    }
    content.push(json!({ "type": "text", "text": query }));

    chat.push(json!({ "role": "user", "content": content }));

    let body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": SYSTEM_PROMPT,
        "tools": [{ "type": "web_search_20260209", "name": "web_search", "max_uses": 5 }],
        "fallbacks": "default",
        "messages": chat.snapshot(),
    });

    let response = match call(&key, &body).await {
        Ok(v) => v,
        Err(err) => {
            chat.pop(); // keep the history consistent with what the model saw
            return Err(err);
        }
    };

    // A policy decline comes back as HTTP 200 with stop_reason "refusal".
    if response.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        chat.pop();
        let why = response
            .get("stop_details")
            .and_then(|d| d.get("explanation"))
            .and_then(Value::as_str)
            .unwrap_or("Claude declined this one.");
        return Err(why.to_string());
    }

    let Some(blocks) = response.get("content").and_then(Value::as_array).cloned() else {
        chat.pop();
        return Err("Unexpected API response.".into());
    };

    // Store the whole content — tool_use / tool_result blocks included — so the
    // next turn has the right context.
    chat.push(json!({ "role": "assistant", "content": blocks.clone() }));

    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() {
        return Err("No response text.".into());
    }
    Ok(ChatReply { text })
}

async fn call(key: &str, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;

    let response = client
        .post(ENDPOINT)
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", FALLBACK_BETA)
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Surface the API's own message, which is what makes a bad key obvious.
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("Claude API {status}: {detail}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))
}

/// OpenRouter / OpenAI path: the OpenAI chat-completions shape, which is also
/// what OpenRouter normalises every model to (see openrouter.ai/docs).
///
/// History is stored in Anthropic block form, so past turns are reduced to
/// their text before sending — tool_use/result blocks become a placeholder
/// rather than breaking the request. File context goes in as plain text.

/// Turn whatever the user pasted into a chat-completions URL. A full URL ending
/// in `chat/completions` is used as-is; a bare base URL gets the standard
/// OpenAI suffix. Empty stays an error, not a request to nowhere.
fn normalize_endpoint(endpoint: &str) -> Result<String, String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("Endpoint URL missing. Open settings and add it.".to_string());
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err("Endpoint URL must start with http:// or https://.".to_string());
    }
    if trimmed.ends_with("chat/completions") {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("{trimmed}/v1/chat/completions"))
    }
}
/// Reduce an Anthropic content-block array to plain text for the OpenAI shape.
fn extract_text(content: Option<&Value>) -> String {
    let Some(blocks) = content.and_then(Value::as_array) else {
        return content.and_then(Value::as_str).unwrap_or("").to_string();
    };
    let mut out: Vec<&str> = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    out.push(t);
                }
            }
            Some(other) => out.push(match other {
                "tool_use" => "[tool call]",
                "tool_result" => "[tool result]",
                _ => "[non-text content]",
            }),
            None => {}
        }
    }
    out.join("\n")
}

/// Credentials + URL for the OpenAI-compatible providers. `custom` gateways
/// (usually localhost) need no key; hosted ones fail loudly without theirs.
pub(crate) fn openai_credentials(provider: &str, endpoint: &str) -> Result<(String, String), String> {
    let url = if provider == "custom" {
        normalize_endpoint(endpoint)?
    } else if provider == "openai" {
        OPENAI_ENDPOINT.to_string()
    } else {
        OPENROUTER_ENDPOINT.to_string()
    };
    let key = match secrets::get(secrets::key_for_provider(provider)) {
        Some(k) => k,
        None if provider == "custom" => String::new(),
        None => return Err("API key missing. Open settings.".to_string()),
    };
    Ok((url, key))
}

/// The message list for a chat-completions call: system prompt plus the stored
/// history reduced to plain text. Tool-call history is deliberately NOT
/// replayed — only final texts persist (see brain.rs) — so every turn starts
/// from words, never from stale call ids.
pub(crate) fn openai_base_messages(chat: &Chat) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "system", "content": SYSTEM_PROMPT })];
    for past in chat.snapshot() {
        let role = past.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = extract_text(past.get("content"));
        messages.push(json!({ "role": role, "content": text }));
    }
    messages
}

/// First-turn user text: file contents or window context, then the question.
/// Later turns send the question bare — context was already banked.
pub(crate) fn openai_user_text(first_turn: bool, context: &Option<ChatContext>, query: &str) -> String {
    let mut user_text = String::new();
    if first_turn {
        match context {
            Some(ChatContext::File { name, path }) => {
                let len = std::fs::metadata(path).ok().map(|m| m.len()).unwrap_or(u64::MAX);
                if len <= MAX_INLINE_TEXT {
                    if let Ok(text) = std::fs::read_to_string(path) {
                        user_text.push_str(&format!("File {name} contents:\n{text}\n\n"));
                    }
                }
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                user_text.push_str(&format!("Context — App: {app_name}, Window: {title}"));
                if let Some(url) = url {
                    user_text.push_str(&format!(", URL: {url}"));
                }
                user_text.push_str("\n\n");
            }
            None => {}
        }
    }
    user_text.push_str(query);
    user_text
}

pub(crate) fn push_user(chat: &Chat, text: &str) {
    chat.push(json!({ "role": "user", "content": [{ "type": "text", "text": text }] }));
}

pub(crate) fn push_assistant(chat: &Chat, text: &str) {
    chat.push(json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] }));
}

pub(crate) fn pop_last(chat: &Chat) {
    chat.pop();
}

/// One tool call the model requested.
pub(crate) struct CompatCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// Raw chat-completions POST: status-checked, JSON-parsed. The caller owns the
/// body (tools or not) and the response (content, calls, or provider errors).
pub(crate) async fn post_chat_json(
    url: &str,
    provider: &str,
    key: &str,
    model: &str,
    messages: &[Value],
    tools: Option<&Value>,
) -> Result<Value, String> {
    let mut body = json!({ "model": model, "messages": messages });
    if let Some(t) = tools {
        body["tools"] = t.clone();
    }
    let text = post_compat(url, provider, key, &body).await?;
    serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))
}

/// The reply's plain text (may be empty when the turn is all tool calls).
pub(crate) fn reply_text(reply: &Value) -> String {
    reply
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The reply's tool calls, if any. Malformed entries are skipped, never fatal:
// a model that fumbles the shape gets one more chance next iteration.
pub(crate) fn parse_tool_calls(reply: &Value) -> Vec<CompatCall> {
    let Some(calls) = reply
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|c| {
            let f = c.get("function")?;
            Some(CompatCall {
                id: c.get("id").and_then(Value::as_str).unwrap_or("call_0").to_string(),
                name: f.get("name").and_then(Value::as_str)?.to_string(),
                args: f.get("arguments").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// The provider's own error message when the call failed but returned JSON.
pub(crate) fn reply_error(reply: &Value) -> Option<String> {
    reply
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn post_compat(endpoint: &str, provider: &str, key: &str, body: &Value) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;

    let mut req = client.post(endpoint).header("content-type", "application/json");
    if !key.is_empty() {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    if provider == "openrouter" {
        // Optional attribution headers; harmless if unset.
        req = req.header("X-Title", "Coco");
    }
    let response = req.json(body).send().await.map_err(|e| format!("Network error: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| {
                        e.get("message").and_then(Value::as_str).map(str::to_string).or_else(|| {
                            // OpenAI nests message under error; be liberal.
                            serde_json::to_string(e).ok()
                        })
                    })
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("{provider} API {status}: {detail}"));
    }
    Ok(text)
}

/// PDF → document block, image → image block, text/code → inline text.
/// Mirrors readFileAsBlock() in ClaudeService.swift.
fn file_block(path: &str) -> Option<Value> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "pdf" => Some(("document", "application/pdf")),
        "jpg" | "jpeg" => Some(("image", "image/jpeg")),
        "png" => Some(("image", "image/png")),
        "gif" => Some(("image", "image/gif")),
        "webp" => Some(("image", "image/webp")),
        _ => None,
    };

    if let Some((block_type, media)) = media_type {
        let bytes = std::fs::read(path).ok()?;
        return Some(json!({
            "type": block_type,
            "source": { "type": "base64", "media_type": media, "data": base64(&bytes) },
        }));
    }

    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(json!({ "type": "text", "text": format!("File contents:\n{text}") }))
}

/// Small standalone base64 encoder — not worth another dependency.
/// Also used for Stripe's basic auth.
pub(crate) fn base64_for(bytes: &[u8]) -> String {
    base64(bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64, extract_text, normalize_endpoint, parse_tool_calls, reply_text};

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn tool_calls_parse_name_args_and_skip_garbage() {
        // Shape observed live on auto/best-fast: arguments arrive stringified.
        let reply = serde_json::json!({
            "choices": [{"message": {
                "content": "",
                "tool_calls": [
                    {"id": "a1", "type": "function",
                     "function": {"name": "music", "arguments": "{\"action\":\"pause\"}"}},
                    {"id": "a2", "type": "function",
                     "function": {"name": "x"}},
                    {"id": "a3", "type": "function", "function": {}},
                ],
            }}]
        });
        let calls = parse_tool_calls(&reply);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "a1");
        assert_eq!(calls[0].name, "music");
        assert_eq!(reply_text(&reply), "");
        // Nameless entry dropped, nameless-function dropped with it.
        assert!(calls.iter().all(|c| !c.name.is_empty()));
    }

    #[test]
    fn plain_reply_has_text_and_no_calls() {
        let reply = serde_json::json!({
            "choices": [{"message": {"content": "  hi  "}}]
        });
        assert_eq!(reply_text(&reply), "hi");
        assert!(parse_tool_calls(&reply).is_empty());
    }

    #[test]
    fn history_text_extraction_keeps_text_and_marks_the_rest() {
        let content = serde_json::json!([
            { "type": "text", "text": "hello" },
            { "type": "tool_use", "name": "x" },
            { "type": "image", "source": {} },
        ]);
        assert_eq!(extract_text(Some(&content)), "hello\n[tool call]\n[non-text content]");
        assert_eq!(extract_text(None), "");
    }

    #[test]
    fn endpoint_normalisation_accepts_both_shapes() {
        // Full URL passes through untouched.
        assert_eq!(
            normalize_endpoint("https://gw.example.com/v1/chat/completions").unwrap(),
            "https://gw.example.com/v1/chat/completions"
        );
        // A bare base gets the standard suffix, trailing slashes tolerated.
        assert_eq!(
            normalize_endpoint("https://gw.example.com///").unwrap(),
            "https://gw.example.com/v1/chat/completions"
        );
        assert_eq!(
            normalize_endpoint("http://localhost:8000").unwrap(),
            "http://localhost:8000/v1/chat/completions"
        );
        // Empty or scheme-less input is an error, never a request to nowhere.
        assert!(normalize_endpoint("   ").is_err());
        assert!(normalize_endpoint("gw.example.com").is_err());
    }
}
