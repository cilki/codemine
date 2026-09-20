//! The always-on web UI: one background thread running axum on a
//! current-thread tokio runtime. Serves the status page, the event stream it
//! updates from, and the settings API the runner is configured through.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::{ForgeKind, MODELS};
use crate::settings::{Settings, SharedSettings};
use crate::status::{Activity, Shared, Status, epoch_now};

/// How often the live log tail is re-read and the host details refreshed.
/// Status updates are pushed the moment they happen; a log file growing or a
/// temperature moving signals nothing, so they need a timer.
const LOG_INTERVAL: Duration = Duration::from_secs(2);

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
                // Time as well as IO: the event stream sleeps between log
                // reads and its keep-alive runs on a timer.
                .enable_all()
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
        .route("/emblem.svg", get(emblem))
        .route("/api/status", get(api_status))
        .route("/api/log", get(api_log))
        .route("/api/host", get(api_host))
        .route("/api/turns/{started}/log", get(api_turn_log))
        .route("/api/events", get(api_events))
        .route("/api/settings", get(api_settings).put(api_put_settings))
        .route("/api/paused", axum::routing::put(api_put_paused))
        .route("/api/options", get(api_options))
        .route("/api/repos/{forge}", get(api_repos))
        .with_state(state);
    let listener = tokio::net::TcpListener::from_std(listener)?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// The codemine emblem the page shows in place of a textual title.
async fn emblem() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        crate::emblem::SVG.as_str(),
    )
}

async fn api_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(status_value(&state.status))
}

/// A fresh host-details snapshot; the event stream pushes the same shape.
async fn api_host() -> Json<crate::host::Host> {
    Json(crate::host::snapshot())
}

/// The live log tail while a turn is running, else the last finished turn's.
async fn api_log(State(state): State<AppState>) -> String {
    log_tail(&state.status)
}

/// One finished turn's full log, identified by its start epoch. Served raw,
/// ANSI escapes and all; the page renders them.
async fn api_turn_log(State(state): State<AppState>, Path(started): Path<u64>) -> Response {
    let path = state
        .status
        .lock()
        .turns
        .iter()
        .find(|record| record.started == started)
        .map(|record| record.log_path.clone());
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "no such turn").into_response();
    };
    match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned().into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "the turn's log is gone").into_response(),
    }
}

/// Push state to the page so it never has to poll: a `status` event on every
/// change to the shared state, a `log` event whenever the tail moves, and a
/// `host` event as the host details drift. All carry JSON, which keeps a log
/// line's own newlines and carriage returns out of the SSE framing.
async fn api_events(State(state): State<AppState>) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut changes = state.status.subscribe();
        // All start as None so the first pass always sends a full snapshot,
        // even when the log tail is legitimately empty.
        let (mut sent_status, mut sent_log, mut sent_host) = (None, None, None);
        loop {
            // Compared without the server timestamp, which moves on its own
            // and would make every state look new.
            let snapshot = serde_json::to_string(&*state.status.lock()).unwrap_or_default();
            if sent_status.as_ref() != Some(&snapshot) {
                let payload = serde_json::to_string(&status_value(&state.status))
                    .unwrap_or_else(|_| "{}".into());
                if send(&tx, "status", &payload).await.is_err() {
                    return;
                }
                sent_status = Some(snapshot);
            }
            let log =
                serde_json::to_string(&log_tail(&state.status)).unwrap_or_else(|_| "\"\"".into());
            if sent_log.as_ref() != Some(&log) {
                if send(&tx, "log", &log).await.is_err() {
                    return;
                }
                sent_log = Some(log);
            }
            let host = serde_json::to_string(&crate::host::snapshot())
                .unwrap_or_else(|_| "{}".into());
            if sent_host.as_ref() != Some(&host) {
                if send(&tx, "host", &host).await.is_err() {
                    return;
                }
                sent_host = Some(host);
            }
            // Wake on the next state change, or on the log timer.
            tokio::select! {
                _ = changes.changed() => {}
                _ = tokio::time::sleep(LOG_INTERVAL) => {}
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// Queue one event; an error means the page hung up and the task is done.
async fn send(
    tx: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
    name: &str,
    data: &str,
) -> Result<(), ()> {
    tx.send(Ok(Event::default().event(name).data(data)))
        .await
        .map_err(|_| ())
}

/// The status as the page consumes it, stamped with server time so elapsed
/// and remaining are computed without trusting the browser clock.
fn status_value(status: &Shared) -> serde_json::Value {
    let mut value = serde_json::to_value(&*status.lock()).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("now".into(), epoch_now().into());
    }
    value
}

/// The running turn's log tail, else the last finished turn's.
fn log_tail(status: &Shared) -> String {
    let (log_path, fallback) = {
        let status = status.lock();
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

/// The fixed choices the settings form renders: the models the runner can be
/// pointed at and the task pool, which comes from the embedded sweep command
/// so the checkboxes can't drift from the prompt.
async fn api_options() -> Json<serde_json::Value> {
    Json(json!({
        "models": MODELS,
        "tasks": crate::prompts::tasks(),
    }))
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
    match state
        .settings
        .update(|settings| settings.apply_update(incoming))
    {
        Ok(()) => {
            let (settings, _) = state.settings.snapshot();
            // The main loop only mirrors the limit into the status at the
            // next turn boundary; reflect it now so the page's counter
            // doesn't lag a running turn.
            Status::update(&state.status, |s| s.hourly_limit = settings.hourly_limit);
            Json(settings.redacted()).into_response()
        }
        Err(err) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct Paused {
    paused: bool,
}

/// Pause or resume the running turn by SIGSTOPping or SIGCONTing the agent's
/// whole process group. Only meaningful while a turn is running; the state is
/// not persisted, so a restart always starts running.
async fn api_put_paused(State(state): State<AppState>, Json(incoming): Json<Paused>) -> Response {
    let pgid = match &state.status.lock().activity {
        Activity::Running { pgid, .. } => *pgid,
        _ => 0,
    };
    if pgid <= 0 {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "no turn is running" })),
        )
            .into_response();
    }
    match crate::workspace::pause_group(pgid, incoming.paused) {
        Ok(()) => {
            Status::update(&state.status, |s| s.paused = incoming.paused);
            Json(json!({ "paused": incoming.paused })).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingsStore;
    use crate::status::Status;
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
        let addr = spawn("127.0.0.1:0".parse().unwrap(), Shared::new(), store).unwrap();
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

        let response = request(addr, "GET", "/emblem.svg", "");
        assert!(response.contains("image/svg+xml"), "{response}");
        assert!(response.contains("<svg"), "{response}");
    }

    #[test]
    fn settings_round_trip_redacts_tokens() {
        let (addr, dir) = serve_in_tempdir();

        let response = request(addr, "GET", "/api/settings", "");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let value = body_json(&response);
        assert_eq!(value["github"]["token_set"], false);
        assert!(value["github"].get("token").is_none());

        let update = json!({
            "github": { "enabled": true, "token": "secret" },
            "model": "anthropic/claude",
            "author_name": "Bot",
            "author_email": "bot@example.com",
            "hourly_limit": 0.5,
        });
        let response = request(addr, "PUT", "/api/settings", &update.to_string());
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");

        // The hourly limit lands in the status right away, not at the next
        // turn boundary.
        let status = body_json(&request(addr, "GET", "/api/status", ""));
        assert_eq!(status["hourly_limit"], 0.5);
        let value = body_json(&response);
        assert_eq!(value["github"]["token_set"], true);
        assert!(value["github"].get("token").is_none());
        assert!(!response.contains("secret"));

        // Persisted to the workspace, and the token survives a token-less PUT.
        assert!(dir.path().join("config.json").exists());
        let response = request(addr, "PUT", "/api/settings", &update.to_string());
        assert_eq!(body_json(&response)["github"]["token_set"], true);
    }

    /// The scheduler state letter from /proc/pid/stat, polled until it
    /// matches so signal delivery timing can't flake the test.
    fn await_state(pid: u32, expected: &str) -> String {
        let mut state = String::new();
        for _ in 0..100 {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            state = stat
                .rsplit(") ")
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or_default()
                .to_owned();
            if state == expected {
                return state;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        state
    }

    #[test]
    fn pause_stops_and_resumes_the_running_group() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::load(dir.path().join("config.json")).unwrap());
        let status = Shared::new();
        let addr = spawn("127.0.0.1:0".parse().unwrap(), status.clone(), store).unwrap();

        // Nothing running: the toggle has nothing to signal.
        let response = request(addr, "PUT", "/api/paused", r#"{"paused":true}"#);
        assert!(response.starts_with("HTTP/1.1 409"), "{response}");

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        Status::update(&status, |s| {
            s.activity = Activity::Running {
                task: "simplify".into(),
                repo: "o/r".into(),
                forge: "github".into(),
                workspace: "/w".into(),
                log_path: dir.path().join("log"),
                pgid: child.id() as i32,
                started: 0,
            }
        });

        let response = request(addr, "PUT", "/api/paused", r#"{"paused":true}"#);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(await_state(child.id(), "T"), "T");
        assert_eq!(
            body_json(&request(addr, "GET", "/api/status", ""))["paused"],
            true
        );

        let response = request(addr, "PUT", "/api/paused", r#"{"paused":false}"#);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(await_state(child.id(), "S"), "S");
        assert_eq!(
            body_json(&request(addr, "GET", "/api/status", ""))["paused"],
            false
        );

        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn settings_validation_fails_with_422() {
        let (addr, _dir) = serve_in_tempdir();
        let update = json!({ "nice": 40 });
        let response = request(addr, "PUT", "/api/settings", &update.to_string());
        assert!(response.starts_with("HTTP/1.1 422"), "{response}");
        assert!(
            body_json(&response)["error"]
                .as_str()
                .unwrap()
                .contains("nice")
        );
    }

    /// Read from an open stream until `needle` shows up or it goes quiet, so
    /// a never-delivered event fails the test instead of hanging it.
    fn read_until(stream: &mut std::net::TcpStream, needle: &str) -> String {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        while !seen.contains(needle) {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
        seen
    }

    #[test]
    fn events_stream_pushes_status_and_log() {
        let (addr, _dir) = serve_in_tempdir();
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        write!(stream, "GET /api/events HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();

        // Both events arrive on connect, before anything has changed.
        let seen = read_until(&mut stream, "event: log");
        assert!(seen.starts_with("HTTP/1.1 200"), "{seen}");
        assert!(seen.contains("content-type: text/event-stream"), "{seen}");
        assert!(seen.contains("event: status"), "{seen}");
        // JSON payloads, so a log line's own newlines can't break the framing.
        assert!(seen.contains(r#""state":"starting""#), "{seen}");
        assert!(seen.contains("data: \"\""), "{seen}");
    }

    #[test]
    fn turn_log_serves_full_file_by_start_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("100.log");
        std::fs::write(&path, "\x1b[32mall good\x1b[0m\n").unwrap();
        let status = Shared::new();
        Status::update(&status, |s| {
            s.record_turn(crate::status::TurnRecord {
                task: "todo".into(),
                repo: "o/r".into(),
                forge: "gitea".into(),
                started: 100,
                duration_secs: 1,
                outcome: crate::status::Outcome::Completed,
                tokens: None,
                log_path: path.clone(),
            })
        });
        let store = Arc::new(SettingsStore::load(dir.path().join("config.json")).unwrap());
        let addr = spawn("127.0.0.1:0".parse().unwrap(), status, store).unwrap();

        // The full log comes back raw, ANSI escapes included.
        let response = request(addr, "GET", "/api/turns/100/log", "");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\x1b[32mall good\x1b[0m"), "{response:?}");

        assert!(request(addr, "GET", "/api/turns/999/log", "").starts_with("HTTP/1.1 404"));
        std::fs::remove_file(&path).unwrap();
        assert!(request(addr, "GET", "/api/turns/100/log", "").starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn log_tail_follows_the_running_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turn.log");
        std::fs::write(&path, "first\n").unwrap();
        let status = Shared::new();

        // Idle: the last finished turn's stored tail.
        Status::update(&status, |s| s.log_tail = "from the last turn".into());
        assert_eq!(log_tail(&status), "from the last turn");

        // Running: the live file, re-read each time.
        Status::update(&status, |s| {
            s.activity = Activity::Running {
                task: "simplify".into(),
                repo: "o/r".into(),
                forge: "github".into(),
                workspace: "/w".into(),
                log_path: path.clone(),
                pgid: 0,
                started: 0,
            }
        });
        assert_eq!(log_tail(&status), "first\n");
        std::fs::write(&path, "first\nsecond\n").unwrap();
        assert_eq!(log_tail(&status), "first\nsecond\n");
    }

    #[test]
    fn options_lists_models_and_tasks() {
        let (addr, _dir) = serve_in_tempdir();
        let value = body_json(&request(addr, "GET", "/api/options", ""));
        assert_eq!(value["models"][0]["id"], "anthropic/claude-fable-5");
        assert!(value["models"].as_array().unwrap().iter().all(|m| {
            m["id"].as_str().unwrap().starts_with("anthropic/") && m["label"].is_string()
        }));
        let tasks = value["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), crate::prompts::default_tasks().len());
        // Each task carries the instructions it selects, for the UI tooltip.
        assert!(tasks.iter().all(|task| {
            task["slug"].as_str().is_some_and(|slug| !slug.is_empty())
                && task["description"]
                    .as_str()
                    .is_some_and(|d| d.starts_with("- "))
        }));
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
