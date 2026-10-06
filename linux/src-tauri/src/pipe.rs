// Unix-domain socket server for coco-hook.
//
// `$XDG_RUNTIME_DIR/coco-<uid>.sock` (fallback `/tmp`) — one connection per hook
// event. Every hook event is forwarded to the island as a `hook` event.
// `PermissionRequest` is the only one that keeps its connection open: it waits
// for the island's decision and writes it back on the same socket, which is how
// approving from the island works.
//
// Claude Code is never blocked by us. Three things guarantee it:
//   * coco-hook gives the connection 300 ms and exits cleanly if we are closed;
//   * we only wait for a human once the island has *confirmed* the card is on
//     screen, so a paused island or a webview that is not listening costs a few
//     hundred milliseconds, not two minutes;
//   * whatever happens we drop the connection after the decision timeout, and
//     the terminal takes over.
//
// What we write back is the bare word `allow` or `deny`. Turning that into the
// documented hookSpecificOutput JSON is coco-hook's job, so the wire format
// Claude Code expects lives in exactly one place.

use std::collections::HashMap;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::UnixListener as TokioListener;
use tokio::sync::mpsc;

use crate::island::WINDOW_LABEL;
use crate::log;

/// Slightly under coco-hook's own 110 s wait, so we always answer first.
const DECISION_TIMEOUT: Duration = Duration::from_secs(108);
/// How long the island gets to say "the card is up". Without it, an island that
/// is paused, hidden behind a crashed webview or simply not listening would leave
/// Claude Code staring at a prompt nobody can see for nearly two minutes.
const ACK_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_PAYLOAD: usize = 1 << 20;

/// What the island can say about a permission request.
pub enum Reply {
    /// The card is on screen and a human can act on it.
    Ack,
    /// A human clicked: `allow` or `deny`.
    Decision(String),
    /// Nobody can act on it — paused, or another request already holds the card.
    Decline,
}

/// Permission requests the island has been told about.
#[derive(Default)]
pub struct Pending(pub Mutex<HashMap<String, mpsc::Sender<Reply>>>);

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// `$XDG_RUNTIME_DIR/coco-<uid>.sock`, falling back to `/tmp` — must match
/// coco-hook's `socket_path()` exactly.
pub fn socket_path() -> PathBuf {
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
        })
        .and_then(|u| u.parse::<u32>().ok())
        .unwrap_or_else(|| unsafe { geteuid() });
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("coco-{uid}.sock"))
}

extern "C" {
    #[link_name = "geteuid"]
    fn geteuid() -> u32;
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let path = socket_path();

        // A socket left behind by a crashed or SIGKILLed run would make every
        // future connect fail with ECONNREFUSED — Coco would look "installed"
        // while nothing is listening, and hooks would silently do nothing. The
        // file is ours (it carries our uid in the name), so removing a stale one
        // is safe; a *live* one is not, and we refuse to start on top of it.
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.file_type().is_socket() {
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    log::line(format!("relay socket already served by another Coco: {}", path.display()));
                    return;
                }
            }
            log::line(format!("removing stale relay socket {}", path.display()));
            let _ = std::fs::remove_file(&path);
        }

        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(err) => {
                log::line(format!("cannot open the relay socket: {err}"));
                return;
            }
        };
        // 0600: only we may connect, even on the /tmp fallback path.
        #[allow(clippy::permissions_set_readonly_false)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        // tokio refuses a *blocking* listener outright — it panics inside
        // `from_std` (tokio#7172) rather than returning an error, which killed
        // the relay task at startup and would have made every Claude Code
        // approval silently do nothing. std listeners are blocking by default.
        if let Err(err) = listener.set_nonblocking(true) {
            log::line(format!("relay socket cannot go non-blocking: {err}"));
            return;
        }
        let listener = match TokioListener::from_std(listener) {
            Ok(l) => l,
            Err(err) => {
                log::line(format!("cannot hand relay socket to tokio: {err}"));
                return;
            }
        };

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move { handle(app, stream).await });
                }
                Err(err) => {
                    log::line(format!("relay accept failed: {err}"));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
}

async fn handle(app: AppHandle, mut stream: UnixStream) {
    use std::os::unix::io::AsRawFd;
    // Whoever is on the other end must be us: the socket is 0600, but on the
    // /tmp fallback the path itself could be squatted before we bind.
    if !peer_is_same_user(stream.as_raw_fd()) {
        log::line("relay refused a connection from another uid".to_string());
        return;
    }

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') || buf.len() > MAX_PAYLOAD {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let line = match buf.iter().position(|b| *b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(line) else { return };
    if !payload.is_object() {
        return;
    }

    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if event != "PermissionRequest" {
        log::line(format!("hook {event}"));
        let _ = app.emit_to(WINDOW_LABEL, "hook", payload);
        return;
    }

    let id = format!("{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let (tx, mut rx) = mpsc::channel::<Reply>(4);
    {
        let pending = app.state::<Pending>();
        pending.0.lock().unwrap().insert(id.clone(), tx);
    }
    payload["request_id"] = json!(id);
    log::line(format!("hook PermissionRequest id={id}"));
    let _ = app.emit_to(WINDOW_LABEL, "hook", payload);

    let decision = wait_for_decision(&id, &mut rx).await;
    app.state::<Pending>().0.lock().unwrap().remove(&id);

    // No decision: say nothing at all. coco-hook then writes nothing to stdout
    // and Claude Code asks in the terminal, exactly as if Coco were closed.
    if let Some(d) = decision {
        let _ = stream.write_all(format!("{d}\n").as_bytes()).await;
        let _ = stream.flush().await;
    }
}

/// SO_PEERCRED: the serving side checks the client really is our own user.
fn peer_is_same_user(fd: i32) -> bool {
    #[repr(C)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    let mut cred = Ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<Ucred>() as u32;
    let rc = unsafe {
        getsockopt(
            fd,
            1,  // SOL_SOCKET
            17, // SO_PEERCRED
            &mut cred as *mut Ucred as *mut _,
            &mut len,
        )
    };
    if rc != 0 {
        return false;
    }
    let mine = unsafe { geteuid() };
    cred.uid == mine
}

extern "C" {
    #[link_name = "getsockopt"]
    fn getsockopt(fd: i32, level: i32, name: i32, value: *mut u32, len: *mut u32) -> i32;
}

/// Two waits: a short one for "the card is up", then the long one for a human.
async fn wait_for_decision(id: &str, rx: &mut mpsc::Receiver<Reply>) -> Option<String> {
    match tokio::time::timeout(ACK_TIMEOUT, rx.recv()).await {
        Ok(Some(Reply::Ack)) => {}
        // A click that beats the ack is still a click.
        Ok(Some(Reply::Decision(d))) => {
            log::line(format!("hook id={id} answered {d}"));
            return Some(d);
        }
        Ok(Some(Reply::Decline)) => {
            log::line(format!("hook id={id} not shown — terminal takes over"));
            return None;
        }
        Ok(None) => return None,
        Err(_) => {
            log::line(format!("hook id={id} island never acknowledged — terminal takes over"));
            return None;
        }
    }

    match tokio::time::timeout(DECISION_TIMEOUT, rx.recv()).await {
        Ok(Some(Reply::Decision(d))) => {
            log::line(format!("hook id={id} answered {d}"));
            Some(d)
        }
        Ok(Some(Reply::Decline)) => {
            log::line(format!("hook id={id} released without a decision"));
            None
        }
        _ => {
            log::line(format!("hook id={id} timed out — terminal takes over"));
            None
        }
    }
}

fn send(app: &AppHandle, request_id: &str, reply: Reply, keep: bool) {
    let sender = {
        let pending = app.state::<Pending>();
        let mut map = pending.0.lock().unwrap();
        if keep { map.get(request_id).cloned() } else { map.remove(request_id) }
    };
    match sender {
        Some(tx) => {
            let _ = tx.try_send(reply);
        }
        None => log::line(format!("reply for id={request_id} — no pending request")),
    }
}

/// The island has the card on screen; the long wait may begin.
pub fn acknowledge(app: &AppHandle, request_id: &str) {
    send(app, request_id, Reply::Ack, true);
}

/// Nobody can act on this one — paused, or another card already holds the view.
pub fn decline(app: &AppHandle, request_id: &str) {
    log::line(format!("decline id={request_id}"));
    send(app, request_id, Reply::Decline, false);
}

/// Called by the island's Allow / Deny buttons. Only ever a bare word: turning
/// it into Claude Code's JSON is coco-hook's job.
pub fn answer(app: &AppHandle, request_id: &str, decision: &str) {
    let word = match decision {
        "allow" | "always" => "allow",
        _ => "deny",
    };
    log::line(format!("decision id={request_id} {word}"));
    send(app, request_id, Reply::Decision(word.to_string()), false);
}
