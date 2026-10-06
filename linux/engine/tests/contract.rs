//! Contract test: the routes Coco depends on must exist at their documented
//! paths, with the documented shapes.
//!
//! This is the guard against OpenCode drift. Every endpoint in here is one Coco
//! would break on silently if it moved, so the failure message names the route
//! rather than showing a serde error.
//!
//! Skipped when no service is running — a laptop without OpenCode open must not
//! turn `cargo test` red. Run it in CI with the service started to get real
//! protection:
//!
//! ```sh
//! opencode service start && cargo test -p coco-engine --test contract
//! ```

use coco_engine::{Client, PermissionReply, Source};

/// Reads the same registration the app reads, so the test exercises discovery
/// rather than a parallel code path.
fn client() -> Option<Client> {
    Client::connect()
}

async fn contract(name: &str, result: Result<String, coco_engine::Error>) -> String {
    match result {
        Ok(detail) => {
            println!("  ok   {name} — {detail}");
            String::new()
        }
        Err(coco_engine::Error::Unreachable(_)) => {
            println!("  skip {name} — no OpenCode service running");
            String::new()
        }
        Err(err) => {
            println!("  FAIL {name} — {err}");
            format!("{name}: {err}")
        }
    }
}

#[tokio::test]
async fn the_routes_coco_depends_on_still_exist() {
    let Some(client) = client() else {
        println!("  skip — OpenCode is not running; contract not checked");
        return;
    };
    assert_eq!(client.source, Source::Registration, "discovery should read the service registration");

    let dir = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut failures: Vec<String> = Vec::new();

    // `/openapi.json` is itself auth-protected: a 401 here means the password
    // went stale, not that the route moved.
    failures.push(
        contract(
            "GET /openapi.json",
            client.openapi().await.map(|spec| {
                let routes = spec["paths"].as_object().map(|p| p.len()).unwrap_or(0);
                assert!(routes > 50, "openapi.json declared only {routes} paths");
                format!("{routes} paths")
            }),
        )
        .await,
    );

    failures.push(
        contract(
            "GET /api/info",
            client.info().await.map(|i| format!("v{} pid {}", i.version, i.pid)),
        )
        .await,
    );

    failures.push(
        contract(
            "GET /api/model",
            client
                .models(&dir)
                .await
                .map(|m| format!("{} models across {} providers", m.len(), {
                    let mut p: Vec<_> = m.iter().map(|x| x.provider_id.clone()).collect();
                    p.sort();
                    p.dedup();
                    p.len()
                })),
        )
        .await,
    );

    failures.push(
        contract(
            "GET /api/agent",
            client.agents(&dir).await.map(|a| format!("{} agents", a.len())),
        )
        .await,
    );

    failures.push(
        contract(
            "GET /api/session",
            client.sessions(&dir).await.map(|s| format!("{} sessions", s.len())),
        )
        .await,
    );

    // The approval card cannot work without this one.
    failures.push(
        contract(
            "GET /api/permission/request",
            client
                .pending_permissions(&dir)
                .await
                .map(|p| format!("{} pending", p.len())),
        )
        .await,
    );

    // MCP registration is how system control stays inside the permission path.
    failures.push(
        contract(
            "GET /api/mcp",
            client.mcp_servers(&dir).await.map(|m| {
                format!("{} servers", m.get("data").and_then(|d| d.as_array()).map(|a| a.len()).unwrap_or(0))
            }),
        )
        .await,
    );

    // The SSE feed is the island's live state; refusing it would be silent.
    failures.push(
        contract(
            "GET /api/event (SSE)",
            client.events(&dir).await.map(|r| format!("HTTP {}", r.status())),
        )
        .await,
    );

    // The approval card's feed: connecting must work even with nothing pending.
    // Nothing is consumed — a live permission.asked would belong to a real
    // agent session, and this test must never answer it.
    failures.push(
        contract(
            "GET /api/event permission watch",
            client.watch_permissions(&dir).await.map(|_| "stream opens".to_string()),
        )
        .await,
    );

    failures.retain(|f| !f.is_empty());
    assert!(
        failures.is_empty(),
        "OpenCode's API contract changed under Coco:\n  {}",
        failures.join("\n  ")
    );
}

/// The permission reply payload must match `Permission.Reply` from the schema.
/// A typo here means every approval card silently fails with a 400.
#[tokio::test]
async fn permission_reply_words_match_the_published_schema() {
    let Some(client) = client() else {
        // The pure assertion still runs — it does not need a server.
        assert_eq!(PermissionReply::Once.as_str(), "once");
        assert_eq!(PermissionReply::Always.as_str(), "always");
        assert_eq!(PermissionReply::Reject.as_str(), "reject");
        return;
    };

    let Ok(spec) = client.openapi().await else {
        assert_eq!(PermissionReply::Once.as_str(), "once");
        return;
    };

    let schema = &spec["components"]["schemas"]["Permission.Reply"];
    let values = schema["enum"].as_array().cloned().unwrap_or_default();
    if values.is_empty() {
        // Schema shape changed; the typed enum is our source of truth, so make
        // the drift visible instead of passing quietly.
        panic!("Permission.Reply no longer has an `enum` — Coco's PermissionReply is stale");
    }
    let words: Vec<&str> = values.iter().filter_map(|v| v.as_str()).collect();
    for reply in [PermissionReply::Once, PermissionReply::Always, PermissionReply::Reject] {
        assert!(
            words.contains(&reply.as_str()),
            "{reply} is not in the published Permission.Reply enum {words:?}"
        );
    }
    println!("  ok   Permission.Reply = {words:?}");
}
