//! The always-on web UI: one background thread running axum on a
//! current-thread tokio runtime. Serves the status page and the settings API
//! the runner is configured through.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use crate::config::ForgeKind;
use crate::settings::{Settings, SharedSettings};
use crate::status::{Activity, Shared, Status, epoch_now};

static INDEX_HTML: &str = include_str!("webui.html");

#[derive(Clone)]
struct AppState {
    status: Shared,
    settings: SharedSettings,
}

/// Bind and serve on a background thread. Binding happens synchronously so a
/// bad address fails startup instead of silently serving nothing; the bound
/// address is returned for logging.
pub fn spawn(addr: SocketAddr, status: Shared, settings: SharedSettings) -> Result<SocketAddr> {
    let listener = std::net::TcpListener::bind(addr)
        .with_context(|| format!("failed to bind webui on {addr}"))?;
    let local = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    std::thread::Builder::new()
        .name("webui".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .expect("failed to build webui runtime");
            runtime
                .block_on(serve(listener, AppState { status, settings }))
                .expect("webui server failed");
        })?;
    Ok(local)
}

async fn serve(listener: std::net::TcpListener, state: AppState) -> Result<()> {
    let app = axum::Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/log", get(api_log))
        .route("/api/settings", get(api_settings).put(api_put_settings))
        .route("/api/repos/{forge}", get(api_repos))
        .with_state(state);
    let listener = tokio::net::TcpListener::from_std(listener)?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn api_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut value = serde_json::to_value(&*lock(&state.status)).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        // Server time, so the page computes elapsed/remaining without
        // trusting the browser clock.
        object.insert("now".into(), epoch_now().into());
    }
    Json(value)
}

/// The live log tail while a turn is running, else the last finished turn's.
async fn api_log(State(state): State<AppState>) -> String {
    let (log_path, fallback) = {
        let status = lock(&state.status);
        let path = match &status.activity {
            Activity::Running { log_path, .. } => Some(log_path.clone()),
            _ => None,
        };
        (path, status.log_tail.clone())
    };
    log_path
        .and_then(|path| {
            let mut file = std::fs::File::open(path).ok()?;
            crate::turn::read_tail(&mut file, 16 * 1024).ok()
        })
        .unwrap_or(fallback)
}

/// The settings with tokens redacted to a `token_set` flag; tokens are
/// write-only and never leave the server.
async fn api_settings(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.settings.snapshot().0.redacted())
}

/// Replace the settings. An empty or absent forge token keeps the stored
/// one; a nonempty token overwrites it.
async fn api_put_settings(
    State(state): State<AppState>,
    Json(incoming): Json<Settings>,
) -> Response {
    match state.settings.update(|settings| settings.apply_update(incoming)) {
        Ok(()) => Json(state.settings.snapshot().0.redacted()).into_response(),
        Err(err) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
    }
}

/// Live repository listing for one forge, merged with the disabled set so
/// the UI can render checkboxes. Disabled repositories missing from the
/// listing still appear, so an unreachable forge can't silently drop them.
async fn api_repos(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    let Some(kind) = ForgeKind::from_slug(&slug) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown forge: {slug}") })),
        )
            .into_response();
    };
    let (settings, _) = state.settings.snapshot();
    let Some(forge) = settings.runtime_forge(kind) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("{} is not configured", kind.name()) })),
        )
            .into_response();
    };
    let disabled = forge.disabled_repos.clone();
    // The listing shells out to the forge CLI; keep it off the current-thread
    // runtime so status polling stays responsive meanwhile.
    let listed = tokio::task::spawn_blocking(move || crate::turn::list_repos(&forge)).await;
    match listed {
        Ok(Ok(repos)) => {
            let names: BTreeSet<String> =
                repos.into_iter().chain(disabled.iter().cloned()).collect();
            let repos: Vec<_> = names
                .into_iter()
                .map(|name| json!({ "enabled": !disabled.contains(&name), "name": name }))
                .collect();
            Json(json!({ "repos": repos })).into_response()
        }
        Ok(Err(err)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{err}") })),
        )
            .into_response(),
    }
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Status> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingsStore;
    use std::io::{Read, Write};
    use std::sync::Arc;

    fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> String {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn body_json(response: &str) -> serde_json::Value {
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        serde_json::from_str(body).unwrap()
    }

    fn serve_in_tempdir() -> (SocketAddr, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::load(dir.path().join("config.json")).unwrap());
        let addr = spawn("127.0.0.1:0".parse().unwrap(), Status::new(), store).unwrap();
        (addr, dir)
    }

    #[test]
    fn serves_page_and_status() {
        let (addr, _dir) = serve_in_tempdir();

        let response = request(addr, "GET", "/api/status", "");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let value = body_json(&response);
        assert_eq!(value["activity"]["state"], "starting");
        assert!(value["now"].is_u64());

        assert!(request(addr, "GET", "/", "").contains("<html"));
    }

    #[test]
    fn settings_round_trip_redacts_tokens() {
        let (addr, dir) = serve_in_tempdir();

        let response = request(addr, "GET", "/api/settings", "");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let value = body_json(&response);
        assert_eq!(value["github"]["token_set"], false);
        assert!(value["github"].get("token").is_none());
        assert_eq!(value["command"], "sweep");

        let update = json!({
            "github": { "enabled": true, "token": "secret" },
            "model": "anthropic/claude",
            "author_name": "Bot",
            "author_email": "bot@example.com",
        });
        let response = request(addr, "PUT", "/api/settings", &update.to_string());
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let value = body_json(&response);
        assert_eq!(value["github"]["token_set"], true);
        assert!(value["github"].get("token").is_none());
        assert!(!response.contains("secret"));

        // Persisted to the workspace, and the token survives a token-less PUT.
        assert!(dir.path().join("config.json").exists());
        let response = request(addr, "PUT", "/api/settings", &update.to_string());
        assert_eq!(body_json(&response)["github"]["token_set"], true);
    }

    #[test]
    fn settings_validation_fails_with_422() {
        let (addr, _dir) = serve_in_tempdir();
        let update = json!({ "nice": 40 });
        let response = request(addr, "PUT", "/api/settings", &update.to_string());
        assert!(response.starts_with("HTTP/1.1 422"), "{response}");
        assert!(body_json(&response)["error"].as_str().unwrap().contains("nice"));
    }

    #[test]
    fn repos_endpoint_guards() {
        let (addr, _dir) = serve_in_tempdir();
        let response = request(addr, "GET", "/api/repos/bogus", "");
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        let response = request(addr, "GET", "/api/repos/github", "");
        assert!(response.starts_with("HTTP/1.1 409"), "{response}");
    }
}
