//! Connectivity check for the YouTube Music / pear-desktop API server.
//! Run with `pear-music-widget --doctor` when the widget shows a setup screen.
//!
//! Lives in the app binary rather than a script so the repo needs no second
//! runtime — the whole project is one Rust crate and a folder of static files.

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

const CLIENT_ID: &str = "PearMusicWidget-doctor";

fn ok(message: impl AsRef<str>) {
    println!("  \x1b[32m✓\x1b[0m {}", message.as_ref());
}
fn bad(message: impl AsRef<str>) {
    println!("  \x1b[31m✗\x1b[0m {}", message.as_ref());
}
fn info(message: impl AsRef<str>) {
    println!("    \x1b[2m{}\x1b[0m", message.as_ref());
}

/// Returns the process exit code.
pub async fn run() -> i32 {
    let host = std::env::var("PMW_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("PMW_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(26538);
    let base = format!("http://{host}:{port}");

    println!("\nChecking {base}\n");

    let http = reqwest::Client::new();
    let mut code = 0;

    // 1. Is anything listening?
    let doc = http
        .get(format!("{base}/doc"))
        .timeout(Duration::from_secs(3))
        .send()
        .await;

    let doc: Value = match doc {
        Ok(res) => match res.json().await {
            Ok(doc) => doc,
            Err(err) => {
                bad("API server answered, but /doc was not JSON");
                info(err.to_string());
                return 1;
            }
        },
        Err(err) => {
            bad("API server not reachable");
            info(if err.is_connect() {
                "Nothing is listening on that port.".to_string()
            } else {
                err.to_string()
            });
            info("Open YouTube Music → menu → Plugins → enable \"API Server\", and check its port.");
            return 1;
        }
    };

    let title = doc
        .pointer("/info/title")
        .and_then(Value::as_str)
        .unwrap_or("unknown server");
    let version = doc
        .pointer("/info/version")
        .and_then(Value::as_str)
        .unwrap_or("?");
    ok(format!("API server reachable — {title} v{version}"));

    // The widget is written against /api/v1; say so loudly if that has moved.
    let mut versions: Vec<String> = doc
        .get("paths")
        .and_then(Value::as_object)
        .map(|paths| {
            paths
                .keys()
                .filter_map(|path| {
                    let rest = path.strip_prefix("/api/")?;
                    let version = rest.split('/').next()?;
                    let is_version = version.starts_with('v')
                        && version.len() > 1
                        && version[1..].chars().all(|c| c.is_ascii_digit());
                    is_version.then(|| version.to_string())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    versions.sort();
    versions.dedup();

    if !versions.is_empty() && !versions.iter().any(|v| v == "v1") {
        bad(format!(
            "Server speaks {} but this widget targets v1",
            versions.join(", ")
        ));
        info("Update API in src-tauri/src/api.rs and the socket path in src-tauri/src/ws.rs.");
        code = 1;
    } else {
        let listed = if versions.is_empty() {
            "unknown".to_string()
        } else {
            versions.join(", ")
        };
        ok(format!("API version v1 present (server exposes: {listed})"));
    }

    // 2. Auth. With authStrategy AUTH_AT_FIRST this pops a dialog in the app.
    let token = match http.post(format!("{base}/auth/{CLIENT_ID}")).send().await {
        Ok(res) if res.status() == 403 => {
            bad("Authorisation denied in YouTube Music");
            return 1;
        }
        Ok(res) => match res.json::<Value>().await {
            Ok(body) => match body.get("accessToken").and_then(Value::as_str) {
                Some(token) => {
                    ok("Access token issued");
                    token.to_string()
                }
                None => {
                    bad("Auth response contained no token");
                    return 1;
                }
            },
            Err(err) => {
                bad(format!("Auth response was not JSON: {err}"));
                return 1;
            }
        },
        Err(err) => {
            bad(format!("Auth request failed: {err}"));
            return 1;
        }
    };

    // 3. Authenticated read.
    match http
        .get(format!("{base}/api/v1/song"))
        .bearer_auth(&token)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(res) if res.status() == 204 => ok("Song endpoint OK — nothing playing"),
        Ok(res) if res.status().is_success() => {
            let song: Value = res.json().await.unwrap_or(Value::Null);
            let what = song
                .get("title")
                .and_then(Value::as_str)
                .map(|title| format!("\"{title}\""))
                .unwrap_or_else(|| "nothing playing".into());
            ok(format!("Song endpoint OK — {what}"));
        }
        Ok(res) => {
            bad(format!("Song endpoint returned HTTP {}", res.status().as_u16()));
            code = 1;
        }
        Err(err) => {
            bad(format!("Song endpoint failed: {err}"));
            code = 1;
        }
    }

    // 4. Realtime channel.
    let url = format!("ws://{host}:{port}/api/v1/ws?token={token}");
    match tokio::time::timeout(Duration::from_secs(5), tokio_tungstenite::connect_async(url)).await {
        Ok(Ok((socket, _))) => {
            let (_sink, mut stream) = socket.split();
            match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                Ok(Some(Ok(message))) => {
                    let kind = message
                        .into_text()
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|payload| {
                            payload.get("type").and_then(Value::as_str).map(str::to_string)
                        })
                        .unwrap_or_else(|| "unknown".into());
                    ok(format!("WebSocket streaming — first frame: {kind}"));
                }
                Ok(Some(Err(err))) => {
                    bad(format!("WebSocket failed: {err}"));
                    code = 1;
                }
                Ok(None) => {
                    bad("WebSocket closed without sending anything");
                    code = 1;
                }
                Err(_) => {
                    bad("WebSocket connected but sent no initial state within 5s");
                    code = 1;
                }
            }
        }
        Ok(Err(err)) => {
            bad(format!("WebSocket failed: {err}"));
            code = 1;
        }
        Err(_) => {
            bad("WebSocket handshake timed out");
            code = 1;
        }
    }

    println!();
    code
}
