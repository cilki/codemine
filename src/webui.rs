//! The always-on web UI: one background thread running axum on a
//! current-thread tokio runtime. Serves the status page, the event stream it
//! updates from, and the settings API the runner is configured through.
//!
//! The UI is unauthenticated and binds 0.0.0.0 by default, so everything
//! here — settings, forge tokens, the proxy keys — is writable by
//! anyone who can reach the port. Tokens never travel back to the browser,
//! but the bind address should still be a loopback or trusted network.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
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

use crate::config::ForgeKind;
use crate::settings::{Settings, SharedSettings};
use crate::status::{Activity, Shared, Status, epoch_now};

/// How often the live log tail is re-read and the host details refreshed.
/// Status updates are pushed the moment they happen; a log file growing or a
/// temperature moving signals nothing, so they need a timer.
const LOG_INTERVAL: Duration = Duration::from_secs(2);

/// How much of a log the live view shows. The page keeps the same window, so
/// a reader who joins mid-turn and one who has been watching since it started
/// see the same thing.
const LOG_WINDOW: u64 = 16 * 1024;

static INDEX_HTML: &str = include_str!("webui.html");

#[derive(Clone)]
struct AppState {
    status: Shared,
    settings: SharedSettings,
    /// The workspace root, which is all the page wants of it: the disk
    /// figures in the host details describe the filesystem it sits on.
    #[cfg_attr(not(feature = "hostinfo"), allow(dead_code))]
    workspace: PathBuf,
}

/// Bind and serve on a background thread. Binding happens synchronously so a
/// bad address fails startup instead of silently serving nothing; the bound
/// address is returned for logging.
pub fn spawn(
    addr: SocketAddr,
    status: Shared,
    settings: SharedSettings,
    workspace: PathBuf,
) -> Result<SocketAddr> {
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
                .block_on(serve(
                    listener,
                    AppState {
                        status,
                        settings,
                        workspace,
                    },
                ))
                .expect("webui server failed");
        })?;
    Ok(local)
}

async fn serve(listener: std::net::TcpListener, state: AppState) -> Result<()> {
    let app = axum::Router::new()
        .route("/", get(index))
        .route("/emblem.svg", get(emblem))
        .route("/favicon.svg", get(favicon))
        .route("/api/status", get(api_status))
        .route("/api/log", get(api_log))
        .route("/api/turns/{started}/log", get(api_turn_log))
        .route("/api/events", get(api_events))
        .route("/api/settings", get(api_settings).put(api_put_settings))
        .route("/api/paused", axum::routing::put(api_put_paused))
        .route("/api/cancel", axum::routing::post(api_post_cancel))
        .route("/api/options", get(api_options))
        .route("/api/repos/{forge}", get(api_repos));
    #[cfg(feature = "hostinfo")]
    let app = app.route("/api/host", get(api_host));
    let app = app.with_state(state);
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

/// The emblem's icon alone, as the tab favicon.
async fn favicon() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        crate::emblem::ICON.as_str(),
    )
}

/// The whole status, turn list included: nothing polls this, so the one-off
/// cost of the history is worth paying to keep the endpoint a complete view
/// of the state. The event stream is where the two are kept apart.
async fn api_status(State(state): State<AppState>) -> impl IntoResponse {
    let status = stamped(&status_json(&state.status));
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        with_member(&status, "turns", &turns_json(&state.status)),
    )
}

/// A fresh host-details snapshot; the event stream pushes the same shape.
#[cfg(feature = "hostinfo")]
async fn api_host(State(state): State<AppState>) -> Json<crate::host::Host> {
    Json(crate::host::snapshot(&state.workspace))
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
/// change to the shared state, a `turns` event whenever a turn joins or
/// leaves the history, a `log` or `log-append` event whenever the tail
/// moves, and `proxy`, `forges` and `host` events as those readings drift.
/// All carry JSON,
/// which keeps a log line's own newlines and carriage returns out of the SSE
/// framing.
async fn api_events(State(state): State<AppState>) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut changes = state.status.subscribe();
        // All start as None so the first pass always sends a full snapshot,
        // even when the log tail is legitimately empty.
        let (mut sent_status, mut sent_proxy, mut sent_forges) = (None, None, None);
        let mut sent_turns: Option<u64> = None;
        let mut log_cursor = None;
        #[cfg(feature = "hostinfo")]
        let mut sent_host = None;
        loop {
            // Status is compared without the server timestamp, which moves on
            // its own and would make every state look new, but sent with it;
            // the host and proxy events compare and send the same JSON. The
            // log carries its own memory of what the page has been told, so
            // it can send what was appended rather than the window again.
            let snapshot = status_json(&state.status);
            let status = stamped(&snapshot);
            let log = log_update(&state.status, &mut log_cursor);
            #[cfg(feature = "hostinfo")]
            let host = serde_json::to_string(&crate::host::snapshot(&state.workspace))
                .unwrap_or_else(|_| "{}".into());
            // Re-derived each tick, like the host snapshot: the main loop is
            // inside a turn for hours at a time, so proxy health can't ride
            // the status it owns. The probe makes a blocking request (behind
            // a short cache), so it runs off this current-thread runtime.
            let settings = state.settings.snapshot().0;
            // Each forge's reachability, for the dot on its card. Never
            // blocks: the probes run on threads of their own.
            let forges = serde_json::to_string(&crate::identity::health(&settings))
                .unwrap_or_else(|_| "{}".into());
            let proxy_settings = settings.proxy;
            let proxy = tokio::task::spawn_blocking(move || {
                serde_json::to_string(&crate::proxy::health(&proxy_settings))
                    .unwrap_or_else(|_| "null".into())
            })
            .await
            .unwrap_or_else(|_| "null".into());
            // The history rides an event of its own so a status that only
            // moved its token sample doesn't drag 44 KB of finished turns
            // along with it, and goes out ahead of the status so the page's
            // first status event already has rows to render beside it. Its
            // revision is what decides whether to send, rather than the JSON
            // the others compare: building those 44 KB every tick just to
            // find them unchanged is the cost this is here to avoid.
            let revision = state.status.lock().turns_revision;
            if sent_turns != Some(revision) {
                if send(&tx, "turns", &turns_json(&state.status)).await.is_err() {
                    return;
                }
                sent_turns = Some(revision);
            }
            #[cfg_attr(not(feature = "hostinfo"), allow(unused_mut))]
            let mut channels = vec![
                ("status", &snapshot, &status, &mut sent_status),
                ("proxy", &proxy, &proxy, &mut sent_proxy),
                ("forges", &forges, &forges, &mut sent_forges),
            ];
            #[cfg(feature = "hostinfo")]
            channels.push(("host", &host, &host, &mut sent_host));
            for (name, key, payload, sent) in channels {
                if sent.as_ref() == Some(key) {
                    continue;
                }
                if send(&tx, name, payload).await.is_err() {
                    return;
                }
                *sent = Some(key.clone());
            }
            if let Some((name, text)) = log.event() {
                let data = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".into());
                if send(&tx, name, &data).await.is_err() {
                    return;
                }
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

/// The status as JSON, which is what both its readers start from: the event
/// stream compares it to tell a real change from the clock ticking, and
/// `stamped` turns it into what the page consumes. The finished-turn list is
/// not in it — see [`turns_json`].
fn status_json(status: &Shared) -> String {
    serde_json::to_string(&*status.lock()).unwrap_or_else(|_| "{}".into())
}

/// The finished turns as JSON, serialized apart from the status they live in.
///
/// The list is almost the whole of a status's bytes — 200 records is 44 KB
/// against 0.6 KB for every other field together — and it moves only when a
/// turn finishes, perhaps three times an hour. The rest of the status moves
/// every few seconds for the whole of a running turn, because the token
/// sample lands on it, and the stream used to answer each of those samples
/// with the entire history again: a minute of a running turn pushed 538 KB
/// to every open page, 31 MB an hour, for the dozen token counts that had
/// actually changed.
fn turns_json(status: &Shared) -> String {
    serde_json::to_string(&status.lock().turns).unwrap_or_else(|_| "[]".into())
}

/// The status as the page consumes it, stamped with server time so elapsed
/// and remaining are computed without trusting the browser clock.
fn stamped(status: &str) -> String {
    with_member(status, "now", &epoch_now().to_string())
}

/// One more member spliced onto the front of a serialized object's own, the
/// way the server stamp and the turn list reach the page's view of a status.
///
/// Spliced rather than serialized alongside the status, because both of its
/// callers want the status more than one way — the event stream with the
/// stamp to send and without it to compare, `/api/status` with the turn list
/// the stream carries separately — and each is just one more member of the
/// object a status already serializes as. Building those copies through a
/// `serde_json::Value` instead cost a second full serialization plus the
/// whole value tree, which on a runner with a few hundred turns behind it
/// was the most expensive thing on the stream's tick.
///
/// Anything that isn't an object is passed through untouched: there is
/// nothing to splice into, and a half-object would be worse than a status
/// without a clock on it.
fn with_member(object: &str, name: &str, value: &str) -> String {
    let Some(fields) = object.strip_prefix('{') else {
        return object.to_owned();
    };
    // A status always has fields, but an empty object would otherwise come
    // back with a trailing comma in it.
    let separator = if fields.starts_with('}') { "" } else { "," };
    format!("{{\"{name}\":{value}{separator}{fields}")
}

/// The running turn's log tail, else the last finished turn's.
fn log_tail(status: &Shared) -> String {
    let (log_path, fallback) = live_log(status);
    log_path
        .and_then(|path| {
            let mut file = std::fs::File::open(path).ok()?;
            crate::turn::read_tail(&mut file, LOG_WINDOW).ok()
        })
        .unwrap_or(fallback)
}

/// Where the live log is being read from right now: the running turn's log
/// file if there is one, and the last finished turn's stored tail to fall
/// back on when there isn't (or when the file can't be opened).
fn live_log(status: &Shared) -> (Option<PathBuf>, String) {
    let status = status.lock();
    let path = match &status.activity {
        Activity::Running { log_path, .. } => Some(log_path.clone()),
        _ => None,
    };
    (path, status.log_tail.clone())
}

/// How far the page has been told the log goes, so the next tick can send
/// what was added instead of the whole window again.
enum LogCursor {
    /// Tailing a running turn's log: which file, and the offset the page has
    /// been sent everything up to.
    Live { path: PathBuf, offset: u64 },
    /// No turn running: the stored tail, as it was last sent.
    Idle(String),
}

/// What to tell the page about the log on this tick.
///
/// A log only ever grows while the turn that owns it runs, so resending the
/// whole window every two seconds sent the same sixteen kilobytes over and
/// over for the handful of bytes the agent had actually written. Only the
/// new bytes go now, and the window is restated solely when the page cannot
/// append to what it has: the first tick of a stream, a turn starting or
/// ending, a log replaced by the next turn's, or one that shrank.
enum LogUpdate {
    /// The whole window; the page shows this instead of what it had.
    Replace(String),
    /// What was appended since the last tick; the page adds it to the end.
    Append(String),
    Unchanged,
}

impl LogUpdate {
    /// The SSE event name and payload, or None when there is nothing to say.
    fn event(self) -> Option<(&'static str, String)> {
        match self {
            LogUpdate::Replace(text) => Some(("log", text)),
            LogUpdate::Append(text) => Some(("log-append", text)),
            LogUpdate::Unchanged => None,
        }
    }
}

fn log_update(status: &Shared, cursor: &mut Option<LogCursor>) -> LogUpdate {
    let (log_path, fallback) = live_log(status);
    let open = log_path.and_then(|path| {
        let file = std::fs::File::open(&path).ok()?;
        let len = file.metadata().ok()?.len();
        Some((path, file, len))
    });
    let Some((path, mut file, len)) = open else {
        // Nothing running: the idle page shows the last turn's stored tail,
        // restated only when it changes.
        if matches!(cursor, Some(LogCursor::Idle(sent)) if *sent == fallback) {
            return LogUpdate::Unchanged;
        }
        *cursor = Some(LogCursor::Idle(fallback.clone()));
        return LogUpdate::Replace(fallback);
    };
    // Appending is only possible against this very file, read no further
    // than it now reaches, and only while what was added still fits in the
    // window — a turn that dumped more than that in one tick would have
    // every byte of the overflow trimmed off the page the moment it landed,
    // so the window itself is the cheaper thing to send. Anything else and
    // the page starts over.
    let appendable = match cursor {
        Some(LogCursor::Live { path: seen, offset }) => {
            (*seen == path && *offset <= len && len - *offset <= LOG_WINDOW).then_some(*offset)
        }
        _ => None,
    };
    match appendable {
        Some(offset) if offset == len => LogUpdate::Unchanged,
        Some(offset) => match appended(&mut file, offset) {
            // A read that produced nothing usable (a lone continuation byte
            // mid-write) leaves the cursor where it was for the next tick.
            Ok((text, _)) if text.is_empty() => LogUpdate::Unchanged,
            Ok((text, reached)) => {
                *cursor = Some(LogCursor::Live {
                    path,
                    offset: reached,
                });
                LogUpdate::Append(text)
            }
            Err(_) => LogUpdate::Unchanged,
        },
        None => match crate::turn::read_tail(&mut file, LOG_WINDOW) {
            Ok(tail) => {
                *cursor = Some(LogCursor::Live { path, offset: len });
                LogUpdate::Replace(tail)
            }
            Err(_) => LogUpdate::Unchanged,
        },
    }
}

/// The text appended to `file` past `offset`, and the offset it was read up
/// to. A tick can land in the middle of a multi-byte character the agent is
/// still writing, so an incomplete one at the end is left behind for the
/// next read rather than decoded into a replacement character the page would
/// then be stuck with.
fn appended(file: &mut std::fs::File, offset: u64) -> std::io::Result<(String, u64)> {
    use std::io::{Read, Seek, SeekFrom};

    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let consumed = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        // `error_len` is None only for a sequence cut short by the end of
        // the buffer, which is the half-written character; anything else is
        // a byte the log will never make valid, decoded lossily like the
        // whole-window read does.
        Err(err) if err.error_len().is_none() => err.valid_up_to(),
        Err(_) => buf.len(),
    };
    buf.truncate(consumed);
    Ok((
        String::from_utf8_lossy(&buf).into_owned(),
        offset + consumed as u64,
    ))
}

/// The choices the settings form renders: the models that can actually run a
/// turn (an `opencode models` listing fetched once and cached, narrowed to
/// what the proxy serves, so a provider whose auth didn't load and a model
/// the proxy won't answer for are both visibly absent) and the task pool,
/// which comes from the embedded sweep command so the checkboxes can't drift
/// from the prompt.
async fn api_options(State(state): State<AppState>) -> Json<serde_json::Value> {
    let proxy = state.settings.snapshot().0.proxy;
    // A cache miss shells out to opencode, and the narrowing makes a blocking
    // request to the proxy; keep both off the current-thread runtime so status
    // polling stays responsive meanwhile.
    let models = tokio::task::spawn_blocking(move || crate::config::models(&proxy))
        .await
        .unwrap_or_default();
    Json(json!({
        "models": models,
        "tasks": crate::prompts::tasks(),
    }))
}

/// The settings with tokens redacted to a `token_set` flag; tokens are
/// write-only and never leave the server.
async fn api_settings(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.settings.snapshot().0.redacted())
}

/// Replace the settings. An empty or absent secret (forge token, proxy key)
/// keeps the stored one; a nonempty one overwrites it.
async fn api_put_settings(
    State(state): State<AppState>,
    Json(incoming): Json<Settings>,
) -> Response {
    let before = state.settings.snapshot().0.proxy;
    match state
        .settings
        .update(|settings| settings.apply_update(incoming))
    {
        Ok(()) => {
            let (settings, _) = state.settings.snapshot();
            // A changed proxy target rewires opencode's provider config and
            // refetches the model listing against it, off the request path
            // like the startup warmup. This must happen here rather than at
            // the next turn boundary: with no models listed the settings
            // can't even become runnable.
            if (&settings.proxy.base_url, &settings.proxy.api_key)
                != (&before.base_url, &before.api_key)
            {
                let proxy = settings.proxy.clone();
                std::thread::Builder::new()
                    .name("provider".into())
                    .spawn(move || {
                        if let Err(err) = crate::prompts::install_provider(
                            &crate::prompts::opencode_config_dir(),
                            &proxy,
                        ) {
                            tracing::error!("failed to update opencode's provider config: {err:#}");
                            return;
                        }
                        crate::config::invalidate_models();
                        drop(crate::config::models(&proxy));
                    })
                    .ok();
            }
            // The main loop is inside a turn for hours at a time, so the new
            // ceiling has to land now rather than at the next turn boundary
            // — a raised limit is spendable immediately, a lowered one
            // spills what no longer fits. This is the loop's own bucket, so
            // it needs no arithmetic of its own.
            Status::update(&state.status, |s| s.allowance.resize(settings.hourly_limit));
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

/// Cancel the running turn and move on to the next: the flag is picked up at
/// the turn runner's next wait hop (within seconds), which kills the agent's
/// process tree, records the turn as canceled, and skips the between-turn
/// sleep. A paused tree is killable too — the kill SIGCONTs it on the way.
async fn api_post_cancel(State(state): State<AppState>) -> Response {
    let mut status = state.status.lock();
    if !matches!(status.activity, Activity::Running { .. }) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "no turn is running" })),
        )
            .into_response();
    }
    status.cancel_requested = true;
    Json(json!({ "canceled": true })).into_response()
}

/// Live repository listing for one forge, merged with the enabled set so
/// the UI can render checkboxes. Enabled repositories missing from the
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
    let enabled = forge.enabled_repos.clone();
    // The listing shells out to the forge CLI; keep it off the current-thread
    // runtime so status polling stays responsive meanwhile.
    let listed = tokio::task::spawn_blocking(move || crate::turn::list_repos(&forge)).await;
    match listed {
        Ok(Ok(repos)) => {
            let names: BTreeSet<String> =
                repos.into_iter().chain(enabled.iter().cloned()).collect();
            let repos: Vec<_> = names
                .into_iter()
                .map(|name| json!({ "enabled": enabled.contains(&name), "name": name }))
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
        let addr = spawn(
            "127.0.0.1:0".parse().unwrap(),
            Shared::new(),
            store,
            dir.path().to_path_buf(),
        )
        .unwrap();
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

        let response = request(addr, "GET", "/favicon.svg", "");
        assert!(response.contains("image/svg+xml"), "{response}");
        assert!(response.contains("<svg"), "{response}");
        assert!(!response.contains("<?xml"), "{response}");

        // The host endpoint rides the hostinfo feature.
        let response = request(addr, "GET", "/api/host", "");
        #[cfg(feature = "hostinfo")]
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        #[cfg(not(feature = "hostinfo"))]
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
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
            "hourly_limit": 0.5,
        });
        let response = request(addr, "PUT", "/api/settings", &update.to_string());
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");

        // The hourly limit lands in the status right away, not at the next
        // turn boundary; half a turn an hour still buys a bucket of one.
        let status = body_json(&request(addr, "GET", "/api/status", ""));
        assert_eq!(status["allowance"]["capacity"], 1.0);
        assert_eq!(status["allowance"]["available"], 1.0);
        let value = body_json(&response);
        assert_eq!(value["github"]["token_set"], true);
        assert!(value["github"].get("token").is_none());
        assert!(!response.contains("secret"));

        // Raising the limit hands over the extra turns at once rather than
        // trickling them in, and lowering it spills what no longer fits.
        let mut raised = update.clone();
        raised["hourly_limit"] = json!(4);
        assert!(
            request(addr, "PUT", "/api/settings", &raised.to_string()).starts_with("HTTP/1.1 200")
        );
        let status = body_json(&request(addr, "GET", "/api/status", ""));
        assert_eq!(status["allowance"]["capacity"], 4.0);
        assert_eq!(status["allowance"]["available"], 4.0);
        let mut lowered = update.clone();
        lowered["hourly_limit"] = json!(2);
        assert!(
            request(addr, "PUT", "/api/settings", &lowered.to_string()).starts_with("HTTP/1.1 200")
        );
        let status = body_json(&request(addr, "GET", "/api/status", ""));
        assert_eq!(status["allowance"]["capacity"], 2.0);
        assert_eq!(status["allowance"]["available"], 2.0);

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
        let addr = spawn(
            "127.0.0.1:0".parse().unwrap(),
            status.clone(),
            store,
            dir.path().to_path_buf(),
        )
        .unwrap();

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
                tokens: None,
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
    fn cancel_flags_the_running_turn() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::load(dir.path().join("config.json")).unwrap());
        let status = Shared::new();
        let addr = spawn(
            "127.0.0.1:0".parse().unwrap(),
            status.clone(),
            store,
            dir.path().to_path_buf(),
        )
        .unwrap();

        // Nothing running: nothing to cancel.
        let response = request(addr, "POST", "/api/cancel", "");
        assert!(response.starts_with("HTTP/1.1 409"), "{response}");
        assert!(!status.lock().cancel_requested);

        Status::update(&status, |s| {
            s.activity = Activity::Running {
                task: "simplify".into(),
                repo: "o/r".into(),
                forge: "github".into(),
                workspace: "/w".into(),
                log_path: dir.path().join("log"),
                pgid: 1,
                started: 0,
                tokens: None,
            }
        });
        let response = request(addr, "POST", "/api/cancel", "");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(status.lock().cancel_requested);
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

    /// A record filled out the way a real one is, so what the history costs
    /// on the wire is what it costs in the test too.
    fn record(started: u64) -> crate::status::TurnRecord {
        crate::status::TurnRecord {
            task: "benchmark".into(),
            repo: "cilki/codemine".into(),
            forge: "gitea".into(),
            started,
            duration_secs: 987,
            outcome: crate::status::Outcome::Completed,
            tokens: Some(crate::status::TokenUsage {
                input: 123_456,
                output: 7_890,
                reasoning: 0,
                cache_read: 9_876_543,
                cache_write: 123_456,
            }),
            log_path: std::path::PathBuf::from("/nonexistent"),
        }
    }

    /// The finished-turn history goes out on its own event, so a status that
    /// only moved its token sample doesn't drag the whole list along with
    /// it. A full history is 44 KB against 0.6 KB for the rest of the
    /// status, and the sample moves every five seconds for the whole of a
    /// turn — carried on the status, that was half a megabyte a minute to
    /// every open page for a dozen token counts.
    #[test]
    fn the_history_is_sent_once_and_not_on_every_token_sample() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::load(dir.path().join("config.json")).unwrap());
        let status = Shared::new();
        let sample = |n: u64| {
            Some(crate::status::TokenUsage {
                input: n,
                ..Default::default()
            })
        };
        Status::update(&status, |s| {
            s.activity = Activity::Running {
                task: "benchmark".into(),
                repo: "cilki/codemine".into(),
                forge: "gitea".into(),
                workspace: "/w".into(),
                log_path: dir.path().join("turn.log"),
                pgid: 0,
                started: 1_760_000_000,
                tokens: sample(1),
            };
            // A full history, which is what the stream used to re-send.
            for n in 0..200 {
                s.record_turn(record(1_760_000_000 - n * 1200));
            }
        });
        let addr = spawn(
            "127.0.0.1:0".parse().unwrap(),
            status.clone(),
            store,
            dir.path().to_path_buf(),
        )
        .unwrap();
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        write!(stream, "GET /api/events HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();

        // The history leads, so the page's first status event already has
        // rows to render beside it. The log event closes the tick.
        let first = read_until(&mut stream, "event: log");
        let turns = first.find("event: turns").expect("no history event");
        assert!(turns < first.find("event: status").unwrap(), "{first}");
        assert!(first.contains(r#""duration_secs":987"#), "{first}");
        let history = first.len();

        // A minute's worth of token samples, each waited out so the stream
        // answers them one at a time the way five seconds apart would.
        let mut samples = String::new();
        for n in 2..=12 {
            Status::update(&status, |s| {
                if let Activity::Running { tokens: live, .. } = &mut s.activity {
                    *live = sample(n);
                }
            });
            samples.push_str(&read_until(&mut stream, "event: status"));
        }
        // Each sample is answered, and none of them restates the history.
        assert_eq!(samples.matches("event: status").count(), 11, "{samples}");
        assert!(!samples.contains("event: turns"), "{samples}");
        assert!(!samples.contains(r#""duration_secs":987"#), "{samples}");
        // Eleven samples used to cost the history eleven times over; what is
        // left is the statuses themselves, under a single copy of it.
        assert!(samples.len() < history, "{} bytes", samples.len());

        // A turn finishing is what moves the list, and that does go out.
        Status::update(&status, |s| {
            s.activity = Activity::Sleeping { until: 0 };
            s.record_turn(record(1_760_009_999));
        });
        let finished = read_until(&mut stream, "event: turns");
        assert!(finished.contains(r#""started":1760009999"#), "{finished}");
    }

    /// `/api/status` is nobody's poll, so it stays a complete view: the turn
    /// list the stream carries separately is spliced back into it.
    #[test]
    fn the_status_endpoint_still_carries_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::load(dir.path().join("config.json")).unwrap());
        let status = Shared::new();
        Status::update(&status, |s| s.record_turn(record(1_760_000_000)));
        let addr = spawn(
            "127.0.0.1:0".parse().unwrap(),
            status,
            store,
            dir.path().to_path_buf(),
        )
        .unwrap();
        let value = body_json(&request(addr, "GET", "/api/status", ""));
        assert!(value["now"].as_u64().unwrap() > 1_700_000_000);
        assert_eq!(value["turns"][0]["started"], 1_760_000_000);
        assert_eq!(value["turns"][0]["duration_secs"], 987);
        assert_eq!(value["activity"]["state"], "starting");
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
        let addr = spawn(
            "127.0.0.1:0".parse().unwrap(),
            status,
            store,
            dir.path().to_path_buf(),
        )
        .unwrap();

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
                tokens: None,
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
        // The model list mirrors `opencode models`, so its content depends on
        // the machine; every entry is a provider/model ID either way.
        assert!(
            value["models"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| { m.as_str().is_some_and(|id| id.contains('/')) })
        );
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
    fn events_stream_pushes_auth_health() {
        let (addr, _dir) = serve_in_tempdir();
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        write!(stream, "GET /api/events HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();
        let seen = read_until(&mut stream, "event: proxy");
        assert!(seen.contains("event: proxy"), "{seen}");
        assert!(seen.contains(r#""connected":"#), "{seen}");
    }

    #[test]
    fn repos_endpoint_guards() {
        let (addr, _dir) = serve_in_tempdir();
        let response = request(addr, "GET", "/api/repos/bogus", "");
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        let response = request(addr, "GET", "/api/repos/github", "");
        assert!(response.starts_with("HTTP/1.1 409"), "{response}");
    }

    /// Stamping has to leave JSON behind whatever it is handed, since the
    /// page parses the result: the stamp joins the members a status already
    /// has, and a status that somehow serialized to something else is passed
    /// through rather than corrupted into a half-object.
    #[test]
    fn stamping_adds_a_clock_without_breaking_the_json() {
        let json = stamped(r#"{"activity":{"state":"starting"},"paused":false}"#);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["now"].as_u64().unwrap() > 1_700_000_000);
        assert_eq!(value["activity"]["state"], "starting");
        assert_eq!(value["paused"], false);

        // An object with no members of its own takes the stamp alone.
        let value: serde_json::Value = serde_json::from_str(&stamped("{}")).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 1);
        assert!(value["now"].is_u64());

        // Not an object: nothing to stamp, and nothing is invented.
        assert_eq!(stamped("null"), "null");
    }

    /// The stream sends a status event only when the status itself moved, so
    /// the JSON it compares must not carry the clock the one it sends does.
    #[test]
    fn the_compared_status_carries_no_clock() {
        let status = Shared::new();
        let snapshot = status_json(&status);
        assert!(!snapshot.contains("\"now\""), "{snapshot}");
        assert_eq!(snapshot, status_json(&status));
        assert!(stamped(&snapshot).contains("\"now\""));
    }

    /// What the page is told about the log, tick by tick. A running turn's
    /// log only grows, so after the window has been stated once only what
    /// was added goes out; the window is restated solely when there is
    /// nothing for the page to append to.
    #[test]
    fn the_log_window_is_stated_once_and_then_only_grown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turn.log");
        std::fs::write(&path, "first\n").unwrap();
        let status = Shared::new();
        let running = |path: &std::path::Path| Activity::Running {
            task: "benchmark".into(),
            repo: "o/r".into(),
            forge: "gitea".into(),
            workspace: "/w".into(),
            log_path: path.to_path_buf(),
            pgid: 0,
            started: 0,
            tokens: None,
        };
        let append = |path: &std::path::Path, text: &str| {
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            file.write_all(text.as_bytes()).unwrap();
        };
        let mut cursor = None;
        let update = |cursor: &mut Option<LogCursor>| log_update(&status, cursor).event();

        // Idle: the last finished turn's stored tail, stated once.
        Status::update(&status, |s| s.log_tail = "from the last turn".into());
        assert_eq!(
            update(&mut cursor),
            Some(("log", "from the last turn".to_owned()))
        );
        assert_eq!(update(&mut cursor), None);

        // A turn starts: the window it has so far, then only what it adds.
        Status::update(&status, |s| s.activity = running(&path));
        assert_eq!(update(&mut cursor), Some(("log", "first\n".to_owned())));
        assert_eq!(update(&mut cursor), None);
        append(&path, "second\n");
        assert_eq!(
            update(&mut cursor),
            Some(("log-append", "second\n".to_owned()))
        );
        assert_eq!(update(&mut cursor), None);

        // A character the agent is halfway through writing is left for the
        // next tick rather than sent as a replacement character.
        let snowman = "☃".as_bytes();
        append(&path, "x");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&snowman[..2])
            .unwrap();
        assert_eq!(update(&mut cursor), Some(("log-append", "x".to_owned())));
        assert_eq!(update(&mut cursor), None);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&snowman[2..])
            .unwrap();
        assert_eq!(update(&mut cursor), Some(("log-append", "☃".to_owned())));

        // The next turn's log is a different file, so there is nothing to
        // append to and the window is stated again.
        let next = dir.path().join("next.log");
        std::fs::write(&next, "next turn\n").unwrap();
        Status::update(&status, |s| s.activity = running(&next));
        assert_eq!(update(&mut cursor), Some(("log", "next turn\n".to_owned())));

        // And so is a log that shrank under the page: whatever it holds is
        // no longer a prefix of what it was sent.
        std::fs::write(&next, "cut\n").unwrap();
        assert_eq!(update(&mut cursor), Some(("log", "cut\n".to_owned())));

        // A burst longer than the window is sent as the window: everything
        // before its last bytes would be trimmed off the page anyway.
        append(&next, &"z".repeat(LOG_WINDOW as usize + 10));
        let Some((name, text)) = update(&mut cursor) else {
            panic!("the burst went unreported")
        };
        assert_eq!(name, "log");
        assert_eq!(text.len(), LOG_WINDOW as usize);

        // The turn ends: back to the stored tail.
        Status::update(&status, |s| {
            s.activity = Activity::Sleeping { until: 0 };
            s.log_tail = "cut\n".into();
        });
        assert_eq!(update(&mut cursor), Some(("log", "cut\n".to_owned())));
        assert_eq!(update(&mut cursor), None);
    }
}
