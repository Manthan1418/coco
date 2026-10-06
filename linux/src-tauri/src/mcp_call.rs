// Local tool transport: speak MCP to the installed coco-mcp over a fresh
// process per call.
//
// One process per call looks wasteful until you consider the alternative: a
// persistent handle needs reconnects, health checks and poison recovery for a
// server that answers in milliseconds. Spawn (~10 ms) is noise next to LLM
// latency, and a process that can only live for one exchange cannot wedge,
// leak, or desynchronize. Timeouts kill the whole child (`kill_on_drop`).
//
// Logs stay in coco-mcp itself (it audits every invocation); this module only
// reports transport failures.

use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(150);

fn server_binary() -> Result<std::path::PathBuf, String> {
    let dest = crate::settings::mcp_exe_path();
    if dest.exists() {
        return Ok(dest);
    }
    // Not installed yet (fresh dev tree): run it from beside the app binary,
    // same candidate order as ensure_mcp.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            for p in [parent.join("coco-mcp"), parent.join("../release/coco-mcp")] {
                if p.exists() {
                    return Ok(p);
                }
            }
        }
    }
    Err("coco-mcp not installed".to_string())
}

/// One full NDJSON exchange: the given requests in order, responses matched by
/// id. Anything malformed, slow, or crashing comes back as Err — never a
/// half-answer, so the caller can degrade to tool-less instead of hanging.
async fn exchange(requests: Vec<Value>) -> Result<Vec<Value>, String> {
    let bin = server_binary()?;
    let mut child = tokio::process::Command::new(&bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start coco-mcp: {e}"))?;

    let work = async {
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let mut out = Vec::with_capacity(requests.len());
        for req in &requests {
            stdin
                .write_all(format!("{}\n", serde_json::to_string(req).unwrap()).as_bytes())
                .await
                .map_err(|e| format!("mcp write failed: {e}"))?;
        }
        stdin.flush().await.map_err(|e| format!("mcp flush failed: {e}"))?;
        // Responses arrive in order; notifications get no reply, so only
        // id-bearing requests consume lines here.
        let want: usize = requests.iter().filter(|r| r.get("id").is_some()).count();
        for _ in 0..want {
            let line = lines
                .next_line()
                .await
                .map_err(|e| format!("mcp read failed: {e}"))?
                .ok_or("coco-mcp ended the exchange early")?;
            out.push(serde_json::from_str(&line).map_err(|e| format!("mcp spoke garbage: {e}"))?);
        }
        // Closing stdin ends the server's read loop; wait for a clean exit so
        // no grandchildren outlive the call (the server itself kills none —
        // tools it spawned already returned, or the call timed out there).
        drop(stdin);
        let _ = child.wait().await;
        Ok::<_, String>(out)
    };
    tokio::time::timeout(EXCHANGE_TIMEOUT, work)
        .await
        .map_err(|_| "coco-mcp exchange timed out".to_string())?
}

/// The tool catalog as OpenAI function definitions: name, description and
/// inputSchema pass straight through as `parameters`. Zero drift by
/// construction — whatever coco-mcp lists is what the model is offered.
pub async fn tool_definitions() -> Result<Value, String> {
    let resps = exchange(vec![
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2024-11-05", "capabilities": {}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ])
    .await?;
    let tools = resps
        .iter()
        .find_map(|r| r.get("result").and_then(|v| v.get("tools")).cloned())
        .ok_or("tools/list returned no tools")?;
    let defs: Vec<Value> = tools
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.get("name"),
                    "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": t.get("inputSchema").cloned().unwrap_or(json!({"type": "object"})),
                }
            })
        })
        .collect();
    if defs.is_empty() {
        return Err("coco-mcp listed no tools".to_string());
    }
    Ok(Value::Array(defs))
}

/// Call one tool. Returns (is_error, text) exactly as the server reported —
/// transport problems are Err, tool failures are Ok-tuple, so the model always
/// hears what happened in its own language.
pub async fn call_tool(name: &str, args: &Value) -> Result<(bool, String), String> {
    let resps = exchange(vec![
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2024-11-05", "capabilities": {}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": name, "arguments": args}}),
    ])
    .await?;
    let result = resps
        .iter()
        .find_map(|r| r.get("result").cloned())
        .ok_or("tools/call returned no result")?;
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let mut text = String::new();
    for part in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
        }
    }
    Ok((is_error, text))
}
