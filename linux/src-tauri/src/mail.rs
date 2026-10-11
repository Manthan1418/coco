// Sending a file (or plain note) by email, over SMTP.
//
// The macOS original handed the message to Mail.app over AppleScript. Linux has
// no universal mail client, so Coco speaks SMTP itself. Only the password is a
// secret (Secret Service, `smtp-password`); host/port/security and the addresses
// are ordinary settings anyone can read.

use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use serde::Deserialize;
use tauri::State;

use crate::settings::Settings;
use crate::{secrets, Shared};

/// One message the island wants to send. `body` may be empty — the spec makes it
/// optional — and the file fields are only set when a dropped file is attached.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailRequest {
    pub to: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
}

/// Best-effort MIME type from the file name, so a PDF does not arrive as a
/// nameless blob. Anything unknown is application/octet-stream.
fn content_type_for(name: &str) -> ContentType {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let parsed = match ext.as_str() {
        "pdf" => Some("application/pdf"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "txt" | "md" | "log" => Some("text/plain; charset=utf-8"),
        "csv" => Some("text/csv; charset=utf-8"),
        "json" => Some("application/json"),
        "zip" => Some("application/zip"),
        _ => None,
    };
    parsed
        .and_then(|m| ContentType::parse(m).ok())
        .unwrap_or(ContentType::TEXT_PLAIN)
}

/// Build and send one message. Pure blocking I/O — the command wraps it in
/// `spawn_blocking`, never runs it on the UI thread.
fn send(settings: &Settings, request: &MailRequest) -> Result<(), String> {
    let host = settings.smtp_host.trim();
    if host.is_empty() {
        return Err("No SMTP server set. Add one in Settings → Email.".into());
    }
    let user = settings.smtp_user.trim();
    let from = if settings.smtp_from.trim().is_empty() {
        user
    } else {
        settings.smtp_from.trim()
    };
    if from.is_empty() {
        return Err("No From address set. Add one in Settings → Email.".into());
    }
    let to = request.to.trim();
    if to.is_empty() {
        return Err("Who should the mail go to?".into());
    }

    let from_box: Mailbox = from
        .parse()
        .map_err(|_| format!("'{from}' is not a valid address"))?;
    let to_box: Mailbox = to
        .parse()
        .map_err(|_| format!("'{to}' is not a valid address"))?;

    // A plain-text body, plus the dropped file when there is one.
    let mut parts = MultiPart::mixed().singlepart(SinglePart::plain(request.body.clone()));
    if let Some(path) = request.file_path.as_deref().filter(|p| !p.is_empty()) {
        let bytes = std::fs::read(path).map_err(|e| format!("could not read the file: {e}"))?;
        let name = request
            .file_name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "attachment".to_string());
        let part = Attachment::new(name.clone()).body(bytes, content_type_for(&name));
        parts = parts.singlepart(part);
    }

    let message = Message::builder()
        .from(from_box)
        .to(to_box)
        .subject(request.subject.clone())
        .multipart(parts)
        .map_err(|e| format!("could not build the message: {e}"))?;

    let login = settings.smtp_user.trim().to_string();
    let password = secrets::get("smtp-password").unwrap_or_default();

    let mut builder = match settings.smtp_security.as_str() {
        // 465-style implicit TLS, and the rare plaintext relay, otherwise
        // 587-style STARTTLS — the default the settings ship with.
        "none" => SmtpTransport::builder_dangerous(host),
        "tls" => SmtpTransport::relay(host).map_err(|e| e.to_string())?,
        _ => SmtpTransport::starttls_relay(host).map_err(|e| e.to_string())?,
    }
    .port(settings.smtp_port)
    .timeout(Some(Duration::from_secs(30)));

    // No username means no login — a local relay wants the mail without AUTH,
    // and offering empty credentials makes most servers reject the session.
    if !login.is_empty() {
        builder = builder.credentials(Credentials::new(login, password));
    }

    builder
        .build()
        .send(&message)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The island's "Send" button.
#[tauri::command]
pub async fn mail_send(shared: State<'_, Shared>, request: MailRequest) -> Result<(), String> {
    let settings = shared.settings.lock().unwrap().clone();
    tauri::async_runtime::spawn_blocking(move || send(&settings, &request))
        .await
        .map_err(|e| format!("the mail worker stopped: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions_get_a_real_type_and_the_rest_falls_back() {
        let ct = |s: &str| ContentType::parse(s).unwrap();
        assert_eq!(content_type_for("report.pdf"), ct("application/pdf"));
        assert_eq!(content_type_for("photo.PNG"), ct("image/png"));
        assert_eq!(content_type_for("notes.txt"), ct("text/plain; charset=utf-8"));
        assert_eq!(content_type_for("mystery.xyzzy"), ContentType::TEXT_PLAIN);
    }

    #[test]
    fn an_empty_host_is_refused_before_any_network_call() {
        let mut settings = Settings::default();
        settings.smtp_host = String::new();
        let request = MailRequest {
            to: "someone@example.com".into(),
            subject: "hi".into(),
            body: String::new(),
            file_path: None,
            file_name: None,
        };
        assert!(send(&settings, &request).unwrap_err().contains("No SMTP server"));
    }

    #[test]
    fn a_bad_recipient_is_named_as_the_problem() {
        let mut settings = Settings::default();
        settings.smtp_from = "me@example.com".into();
        let request = MailRequest {
            to: "not an address".into(),
            subject: "hi".into(),
            body: String::new(),
            file_path: None,
            file_name: None,
        };
        let err = send(&settings, &request).unwrap_err();
        assert!(err.contains("not a valid address"), "got: {err}");
    }

    /// The whole path — MIME build, TLS-less SMTP dialogue, attachment — against
    /// a throwaway server on loopback. No credentials, no network, no flakiness.
    #[test]
    fn live_smtp_delivers_a_message_with_an_attachment() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut writer, _) = listener.accept().unwrap();
            writer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut reader = BufReader::new(writer.try_clone().unwrap());
            let mut transcript = String::new();
            let mut line = String::new();

            let reply = |w: &mut std::net::TcpStream, s: &str| {
                w.write_all(s.as_bytes()).unwrap();
                w.flush().unwrap();
            };
            reply(&mut writer, "220 coco-test ESMTP\r\n");

            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                transcript.push_str(&line);
                let cmd = line.trim_end().to_ascii_uppercase();
                if cmd.starts_with("EHLO") || cmd.starts_with("HELO") {
                    reply(&mut writer, "250-coco-test\r\n250 SIZE 10485760\r\n");
                } else if cmd.starts_with("DATA") {
                    reply(&mut writer, "354 End data with <CR><LF>.<CR><LF>\r\n");
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == ".\r\n" {
                            break;
                        }
                        transcript.push_str(&line);
                    }
                    reply(&mut writer, "250 OK: queued\r\n");
                } else if cmd.starts_with("QUIT") {
                    reply(&mut writer, "221 Bye\r\n");
                    break;
                } else {
                    // MAIL FROM / RCPT TO / RSET / NOOP all just want a 250.
                    reply(&mut writer, "250 OK\r\n");
                }
            }
            transcript
        });

        let mut settings = Settings::default();
        settings.smtp_host = "127.0.0.1".into();
        settings.smtp_port = port;
        settings.smtp_security = "none".into();
        settings.smtp_from = "coco@example.com".into();
        settings.smtp_user = String::new();

        let dir = std::env::temp_dir().join("coco-mail-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hello.txt");
        std::fs::write(&path, b"the file body").unwrap();

        let request = MailRequest {
            to: "friend@example.com".into(),
            subject: "A file for you".into(),
            body: "here it is".into(),
            file_path: Some(path.to_string_lossy().into_owned()),
            file_name: Some("hello.txt".into()),
        };
        send(&settings, &request).expect("the local server accepted the message");

        let transcript = server.join().unwrap();
        assert!(transcript.contains("RCPT TO:<friend@example.com>"), "{transcript}");
        assert!(transcript.contains("Subject: A file for you"), "{transcript}");
        assert!(transcript.contains("hello.txt"), "{transcript}");
        assert!(transcript.contains("Content-Disposition: attachment"), "{transcript}");
    }
}
