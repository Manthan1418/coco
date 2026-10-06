// coco-mcp: the hands of the brain.
//
// An MCP server over stdio (newline-delimited JSON-RPC), speaking exactly the
// four messages OpenCode needs: initialize, notifications/initialized,
// tools/list, tools/call — plus ping. Nothing else is implemented on purpose:
// every extra method is surface the audit log would have to cover.
//
// Two properties make this safe to give an agent:
//
// 1. No shell, anywhere. Every tool execs argv arrays directly, so command
//    injection is impossible by construction — there is no string for the
//    model to smuggle metacharacters through.
// 2. Every invocation is appended to coco.log with its full argv and outcome.
//    The audit trail is structural, not a flag someone can forget.
//
// What the tools may touch: notifications, launching GUI apps by desktop id
// (never with arguments), reading the file tree under $HOME, media players,
// and running commands — the last only with the user's approval on the card
// (OpenCode asks first) plus a server-side deny-list for the catastrophic
// shapes no approval should ever cover.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

// ── Protocol ────────────────────────────────────────────────────────────────

const SUPPORTED_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-11-25"];

fn respond(id: &Value, result: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
    );
    let _ = out.flush();
}

fn fail(id: &Value, code: i64, message: String) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
    );
    let _ = out.flush();
}

fn ok_text(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn tool_error(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

// ── Small helpers ───────────────────────────────────────────────────────────

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Confine a path under $HOME. Absolute paths must already be inside; relative
/// ones are resolved against home. Anything else is refused — tools that read
/// or run must not be steered into /etc, /proc or another user's tree.
fn confine_home(path: &str) -> Result<PathBuf, String> {
    let home = home();
    let abs = if path.is_empty() {
        home.clone()
    } else {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            home.join(p)
        }
    };
    // Normalize `..` lexically (no symlink resolution: the target may not
    // exist yet, and resolving would TOCTOU anyway).
    let mut clean = PathBuf::new();
    for comp in abs.components() {
        use std::path::Component::*;
        match comp {
            RootDir => clean.push("/"),
            Prefix(p) => clean.push(p.as_os_str()),
            CurDir => {}
            ParentDir => {
                clean.pop();
            }
            Normal(c) => clean.push(c),
        }
    }
    if clean.starts_with(&home) {
        Ok(clean)
    } else {
        Err(format!("outside $HOME, refused: {path}"))
    }
}

fn arg<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Result<Option<T>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| format!("bad argument {key:?}: {e}")),
    }
}

fn required<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    arg(args, key)?.ok_or_else(|| format!("missing required argument {key:?}"))
}

// ── Tools ───────────────────────────────────────────────────────────────────

fn tool_notify(args: &Value) -> Value {
    let title: String = match required(args, "title") {
        Ok(t) => t,
        Err(e) => return tool_error(e),
    };
    let body: String = arg(args, "body").unwrap_or_default().unwrap_or_default();
    match Command::new("notify-send").arg("-a").arg("Coco").arg(&title).arg(&body).status() {
        Ok(_) => ok_text("notified".to_string()),
        Err(e) => tool_error(format!("notify-send failed: {e}")),
    }
}

fn desktop_files() -> Vec<(String, PathBuf)> {
    // User entries shadow system ones; dedupe by file name, user first.
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in [home().join(".local/share/applications"), PathBuf::from("/usr/share/applications")] {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e != "desktop").unwrap_or(true) {
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if seen.insert(name.clone()) {
                out.push((name, path));
            }
        }
    }
    out
}

fn desktop_name(path: &Path) -> Option<String> {
    // Minimal .desktop parse: first `Name=` under [Desktop Entry], no locale
    // handling — display names in the user's locale are a nicety, exactness
    // here matters more than localization.
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && line.starts_with("Name=") && !line.starts_with("Name[") {
            return Some(line["Name=".len()..].to_string());
        }
    }
    None
}

fn tool_open_app(args: &Value) -> Value {
    let want: String = match required(args, "name") {
        Ok(n) => n,
        Err(e) => return tool_error(e),
    };
    let want_lower = want.to_lowercase();
    let entries = desktop_files();

    // Exact display-name match first, then executable basename, then substring.
    // In that order, so "code" finds VS Code rather than something containing it.
    let mut pick: Option<(String, String)> = None;
    for (id, path) in &entries {
        if desktop_name(path).map(|n| n.to_lowercase() == want_lower).unwrap_or(false) {
            pick = Some((id.clone(), desktop_name(path).unwrap()));
            break;
        }
    }
    if pick.is_none() {
        for (id, path) in &entries {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
            if stem == want_lower {
                pick = Some((id.clone(), desktop_name(path).unwrap_or_else(|| id.clone())));
                break;
            }
        }
    }
    if pick.is_none() {
        for (id, path) in &entries {
            let name = desktop_name(path).unwrap_or_default();
            if name.to_lowercase().contains(&want_lower) {
                pick = Some((id.clone(), name));
                break;
            }
        }
    }
    let Some((id, name)) = pick else {
        return tool_error(format!("no application matching {want:?}"));
    };
    // gtk-launch resolves Exec (with its %codes and quoting) itself; the agent
    // supplies only an id, never arguments — there is nothing to inject.
    match Command::new("gtk-launch")
        .arg(id.trim_end_matches(".desktop"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => ok_text(format!("opened {name}")),
        Err(e) => tool_error(format!("could not open {name}: {e}")),
    }
}

fn tool_find_files(args: &Value) -> Value {
    let query: String = match required(args, "query") {
        Ok(q) => q,
        Err(e) => return tool_error(e),
    };
    let root: String = arg(args, "root").unwrap_or_default().unwrap_or_default();
    let max: usize = arg(args, "max_results").unwrap_or_default().unwrap_or(30).clamp(1, 100);
    let root = match confine_home(if root.is_empty() { "" } else { &root }) {
        Ok(r) => r,
        Err(e) => return tool_error(e),
    };
    // rg --files lists fast and respects .gitignore; the query filters after.
    // Explicit prunes for the trees that are never what "find my file" means.
    let out = Command::new("rg")
        .arg("--files")
        .arg("--glob")
        .arg("!.git/*")
        .arg("--glob")
        .arg("!node_modules/*")
        .arg("--glob")
        .arg("!.cache/*")
        .arg(&root)
        .output();
    let Ok(out) = out else {
        return tool_error("ripgrep (rg) is not installed".to_string());
    };
    let list = String::from_utf8_lossy(&out.stdout);
    let is_glob = query.contains(['*', '?', '[']);
    let mut hits: Vec<String> = Vec::new();
    for line in list.lines() {
        let matched = if is_glob {
            glob_match(&query, Path::new(line).file_name().unwrap_or_default().to_string_lossy().as_ref())
        } else {
            line.to_lowercase().contains(&query.to_lowercase())
        };
        if matched {
            hits.push(line.to_string());
            if hits.len() >= max {
                break;
            }
        }
    }
    if hits.is_empty() {
        return ok_text(format!("nothing matching {query:?} under {}", root.display()));
    }
    ok_text(hits.join("\n"))
}

/// Substring glob: `*`, `?`, `[abc]` — enough for filenames, no regex engine
/// for the model to footgun itself with.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(px: &[u8], nx: &[u8]) -> bool {
        match px.first() {
            None => nx.is_empty(),
            Some(b'*') => {
                let mut rest = &px[1..];
                while rest.first() == Some(&b'*') {
                    rest = &rest[1..];
                }
                (0..=nx.len()).any(|i| go(rest, &nx[i..]))
            }
            Some(b'?') => !nx.is_empty() && go(&px[1..], &nx[1..]),
            Some(b'[') => {
                let Some(end) = px.iter().position(|&c| c == b']') else { return false };
                let (cls, rest) = (&px[1..end], &px[end + 1..]);
                let (&c, nrest) = match nx.first() {
                    Some(c) => (c, &nx[1..]),
                    None => return false,
                };
                // [abc] and [a-z], case-insensitive like the substring path.
                let cl = cls.to_ascii_lowercase();
                let mut ok = false;
                let mut i = 0;
                while i < cl.len() {
                    if i + 2 < cl.len() && cl[i + 1] == b'-' {
                        if (cl[i]..=cl[i + 2]).contains(&c.to_ascii_lowercase()) {
                            ok = true;
                        }
                        i += 3;
                    } else {
                        if cl[i] == c.to_ascii_lowercase() {
                            ok = true;
                        }
                        i += 1;
                    }
                }
                ok && go(rest, nrest)
            }
            Some(&p) => {
                nx.first().map(|&c| c.to_ascii_lowercase() == p.to_ascii_lowercase()).unwrap_or(false)
                    && go(&px[1..], &nx[1..])
            }
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

fn tool_music(args: &Value) -> Value {
    let action: String = match required(args, "action") {
        Ok(a) => a,
        Err(e) => return tool_error(e),
    };
    if !["status", "play", "pause", "next", "previous"].contains(&action.as_str()) {
        return tool_error(format!("unknown music action {action:?} — want status|play|pause|next|previous"));
    }
    let mut cmd = Command::new("playerctl");
    if let Some(player) = arg::<String>(args, "player").unwrap_or_default() {
        cmd.arg("--player").arg(player);
    }
    if action == "status" {
        cmd.arg("metadata").arg("--format").arg("{{artist}} — {{title}} [{{status}}]");
    } else {
        cmd.arg(action.clone());
    }
    match cmd.output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tool_error("playerctl is not installed — music control needs it".to_string())
        }
        Err(e) => tool_error(format!("playerctl failed: {e}")),
        Ok(out) if !out.status.success() => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            tool_error(if err.is_empty() { "no player seems to be running".to_string() } else { err })
        }
        Ok(out) => ok_text(
            if action == "status" {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            } else {
                format!("music {action}")
            },
        ),
    }
}

/// Catastrophic shapes, refused before any approval is even asked. This list
/// is the backstop, not the policy: the approval card stays the real gate
/// (the user sees the exact argv), because no static list can name everything
/// dangerous. What it names is only what must never run even approved by
/// accident: disk destruction, forced root recursion, fork bombs.
fn denied(argv: &[String]) -> Option<&'static str> {
    let joined = argv.join(" ");
    const RULES: &[(&str, &str)] = &[
        ("--no-preserve-root", "refuses --no-preserve-root"),
        ("mkfs", "disk formatting is never agent business"),
        (":(){ :|:& };:", "fork bombs are never agent business"),
    ];
    for (pat, why) in RULES {
        if joined.contains(pat) {
            return Some(why);
        }
    }
    // rm -rf aimed at / or at a top-level dir: exact-argv match only, so
    // `rm -rf ./build` keeps working while `rm -rf /` never can.
    if argv.first().map(|c| c == "rm" || c.ends_with("/rm")).unwrap_or(false)
        && argv.iter().any(|a| a == "-rf" || a == "-fr" || a == "-rRf")
        && argv.iter().any(|a| a == "/" || a == "/*")
    {
        return Some("rm -rf aimed at filesystem root");
    }
    // dd straight at a device.
    if argv.first().map(|c| c == "dd" || c.ends_with("/dd")).unwrap_or(false)
        && argv.iter().any(|a| a.starts_with("of=/dev/"))
    {
        return Some("dd aimed at a device");
    }
    None
}

fn tool_run_command(args: &Value) -> Value {
    let command: Vec<String> = match required::<Vec<String>>(args, "command") {
        Ok(c) if !c.is_empty() => c,
        _ => return tool_error("command must be a non-empty argv array".to_string()),
    };
    if command.iter().any(|a| a.contains('\0')) {
        return tool_error("NUL bytes in argv".to_string());
    }
    if let Some(why) = denied(&command) {
        return tool_error(format!("refused ({why})"));
    }
    let cwd: Option<String> = arg(args, "cwd").unwrap_or_default();
    let cwd = match cwd.map(|c| confine_home(&c)).transpose() {
        Ok(c) => c,
        Err(e) => return tool_error(e),
    };
    let sudo: bool = arg(args, "sudo").unwrap_or_default().unwrap_or(false);
    let timeout_secs: u64 = arg(args, "timeout_secs").unwrap_or_default().unwrap_or(30).clamp(1, 300);

    // sudo goes through polkit: the system shows its own password dialog and
    // Coco never sees the secret. argv stays an array — pkexec takes the
    // command the same way, so the no-shell property survives elevation.
    let (prog, argv): (&str, Vec<&str>) = if sudo {
        let mut full = vec!["pkexec"];
        full.extend(command.iter().map(String::as_str));
        ("pkexec", full)
    } else {
        (command[0].as_str(), command.iter().map(String::as_str).collect())
    };

    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(prog);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return tool_error(format!("could not start {prog}: not installed?"))
        }
        Err(e) => return tool_error(format!("could not start {prog}: {e}")),
    };
    let start = Instant::now();
    let waited: Option<std::process::Output> = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(timeout_secs) {
                    // Whole process group: grandchildren must not outlive the call.
                    unsafe {
                        libc_killpg(child.id());
                    }
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };
    let Some(out) = waited else {
        return tool_error(format!("timed out after {timeout_secs}s (process group killed)"));
    };
    // Cap the transcript: the island shows a summary, not megabytes.
    const MAX: usize = 8000;
    let mut stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    for s in [&mut stdout, &mut stderr] {
        if s.len() > MAX {
            s.truncate(MAX);
            s.push_str("\n…(truncated)");
        }
    }
    ok_text(format!(
        "exit={}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code().unwrap_or(-1),
        stdout.trim_end(),
        stderr.trim_end()
    ))
}

// killpg without pulling in libc: the syscall number for kill on x86_64 Linux
// is 62, negative pid = process group.
#[allow(clippy::missing_safety_doc)]
unsafe fn libc_killpg(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        // SIGKILL = 9. No checking: best-effort cleanup on a timeout path.
        kill(-(pid as i32), 9);
    }
}

// ── Dispatch ────────────────────────────────────────────────────────────────

fn tool_list() -> Value {
    json!({"tools": [
        {
            "name": "notify",
            "description": "Show the user a desktop notification. For progress, completions, and things they asked to be told about — never for anything sensitive.",
            "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {
                "title": {"type": "string"},
                "body": {"type": "string"}
            }, "required": ["title"]}
        },
        {
            "name": "open_app",
            "description": "Open a GUI application by name (e.g. \"firefox\", \"VS Code\", \"spotify\"). Takes only a name, never arguments or commands.",
            "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string", "description": "App name or executable, e.g. firefox"}
            }, "required": ["name"]}
        },
        {
            "name": "find_files",
            "description": "Search filenames under the home directory. Substring match, or a glob (*, ?, [abc]) matched against the file name. Read-only; never leaves $HOME.",
            "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string"},
                "root": {"type": "string", "description": "Subdirectory of home to search; default the whole home"},
                "max_results": {"type": "integer", "default": 30}
            }, "required": ["query"]}
        },
        {
            "name": "music",
            "description": "Control media playback (status, play, pause, next, previous) on the user's players.",
            "annotations": {"readOnlyHint": false, "openWorldHint": true},
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["status", "play", "pause", "next", "previous"]},
                "player": {"type": "string", "description": "Optional player name; default whichever is active"}
            }, "required": ["action"]}
        },
        {
            "name": "run_command",
            "description": "Run a command as an argv array (no shell) and return exit code, stdout and stderr. cwd must stay under $HOME. sudo=true elevates through the system polkit dialog — the password never touches Coco. Ask the user first for anything that writes, deletes, installs, or touches the network.",
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": true},
            "inputSchema": {"type": "object", "properties": {
                "command": {"type": "array", "items": {"type": "string"}, "description": "argv, e.g. [\"ls\", \"-la\"]"},
                "cwd": {"type": "string", "description": "Working directory under $HOME"},
                "sudo": {"type": "boolean", "description": "Elevate via the system password dialog", "default": false},
                "timeout_secs": {"type": "integer", "default": 30}
            }, "required": ["command"]}
        },
        {
            "name": "memory_save",
            "description": "Remember a fact about the user for later (preferences, setup, tastes, routines). One fact per call, plain words. Recall it with memory_recall when relevant — especially setup and music preferences.",
            "annotations": {"readOnlyHint": false},
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string", "description": "The fact, e.g. 'prefers lo-fi while coding'"},
                "tags": {"type": "string", "description": "Comma-separated tags, e.g. 'music,mood'"}
            }, "required": ["text"]}
        },
        {
            "name": "memory_recall",
            "description": "Search remembered facts. Call it whenever the user's request touches preferences, setup, music, or anything they may have told you before.",
            "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "Words to look for"},
                "max_results": {"type": "integer", "default": 5}
            }, "required": ["query"]}
        }
    ]})
}

fn memory_file() -> PathBuf {
    let dir = home().join(".local/share/coco/memory");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("facts.jsonl")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One remembered fact. Append-only; exact-duplicate saves are ignored.
/// Deletion is deliberately absent in v1 — the file is plain JSONL the user
/// owns and can edit, and forgetting on command is a policy question, not a
/// storage one.
fn tool_memory_save(args: &Value) -> Value {
    let text: String = match required(args, "text") {
        Ok(t) => t,
        Err(e) => return tool_error(e),
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return tool_error("nothing to remember".to_string());
    }
    if text.len() > 2000 {
        return tool_error("too long — one fact per call, keep it short".to_string());
    }
    let tags: Vec<String> = arg::<String>(args, "tags")
        .unwrap_or_default()
        .unwrap_or_default()
        .split(',')
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    // Exact duplicates teach nothing; skip silently (still "ok" — the fact
    // is, after all, remembered).
    if let Ok(existing) = std::fs::read_to_string(memory_file()) {
        for line in existing.lines() {
            if serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| v.get("text").and_then(Value::as_str).map(str::to_string))
                .as_deref()
                == Some(text.as_str())
            {
                return ok_text("already remembered".to_string());
            }
        }
    }
    let entry = json!({"text": text, "tags": tags, "at": now_unix()});
    let mut f = match std::fs::OpenOptions::new().create(true).append(true).open(memory_file()) {
        Ok(f) => f,
        Err(e) => return tool_error(format!("cannot write memory: {e}")),
    };
    use std::io::Write as _;
    if writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).is_err() {
        return tool_error("cannot write memory".to_string());
    }
    ok_text("remembered".to_string())
}

fn tool_memory_recall(args: &Value) -> Value {
    let query: String = match required(args, "query") {
        Ok(q) => q,
        Err(e) => return tool_error(e),
    };
    let max: usize = arg(args, "max_results").unwrap_or_default().unwrap_or(5).clamp(1, 20);
    let Ok(content) = std::fs::read_to_string(memory_file()) else {
        return ok_text("no memories yet".to_string());
    };
    // Score: tag hit beats text hit; newer beats older on ties. Substring
    // matching, case-insensitive — keyword search is honest about what it is,
    // and for a personal fact file it beats embeddings on debuggability.
    let words: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(str::to_string)
        .collect();
    if words.is_empty() {
        return ok_text("no memories yet".to_string());
    }
    let mut scored: Vec<(i64, u64, String)> = Vec::new();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let text = v.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let at = v.get("at").and_then(Value::as_u64).unwrap_or(0);
        let tags: Vec<String> = v
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_lowercase)).collect())
            .unwrap_or_default();
        let tl = text.to_lowercase();
        let mut score = 0i64;
        for w in &words {
            if tags.iter().any(|t| t.contains(w)) {
                score += 3;
            } else if tl.contains(w) {
                score += 1;
            }
        }
        if score > 0 {
            scored.push((score, at, text.to_string()));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    scored.truncate(max);
    if scored.is_empty() {
        return ok_text("nothing remembered about that yet".to_string());
    }
    ok_text(scored.into_iter().map(|(_, _, t)| format!("- {t}")).collect::<Vec<_>>().join("\n"))
}

fn audit(tool: &str, args: &Value, outcome: &str) {
    // Same file the app writes: one narrative for everything Coco did, in
    // order, whether the actor was Rust, the island, or an agent's tool call.
    let home = home();
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"));
    let path = state.join("coco/coco.log");
    let line = format!("mcp {tool} {} -> {}\n", args, outcome);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write as _;
        let _ = f.write_all(line.as_bytes());
    }
}

fn main() {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // NDJSON garbage: skip, never answer half a message.
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // Notifications carry no id and get no answer — answering one would
        // corrupt the stream with a response nobody reads.
        if id.is_null() {
            continue;
        }

        match method {
            "initialize" => {
                let client_version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let negotiated = SUPPORTED_PROTOCOLS
                    .iter()
                    .find(|v| **v == client_version)
                    .copied()
                    .unwrap_or(SUPPORTED_PROTOCOLS[SUPPORTED_PROTOCOLS.len() - 1]);
                audit("initialize", &json!({"client": client_version}), "ok");
                respond(
                    &id,
                    json!({
                        "protocolVersion": negotiated,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "coco-system", "version": env!("CARGO_PKG_VERSION")}
                    }),
                );
            }
            "ping" => respond(&id, json!({})),
            "tools/list" => respond(&id, tool_list()),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = match name {
                    "notify" => tool_notify(&args),
                    "open_app" => tool_open_app(&args),
                    "find_files" => tool_find_files(&args),
                    "music" => tool_music(&args),
                    "run_command" => tool_run_command(&args),
                    "memory_save" => tool_memory_save(&args),
                    "memory_recall" => tool_memory_recall(&args),
                    _ => tool_error(format!("unknown tool {name:?}")),
                };
                let outcome = result
                    .get("isError")
                    .and_then(Value::as_bool)
                    .map(|e| if e { "error" } else { "ok" })
                    .unwrap_or("ok");
                audit(name, &args, outcome);
                respond(&id, result);
            }
            _ => fail(&id, -32601, format!("method not implemented: {method}")),
        }
    }
}
