//! Optional read-only web UI: one background thread running axum on a
//! current-thread tokio runtime. The synchronous main loop only writes
//! `Status`; the server only reads it.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;

use crate::status::{Activity, Shared, Status, epoch_now};

static INDEX_HTML: &str = include_str!("webui.html");

/// Bind and serve on a background thread. Binding happens synchronously so a
/// bad address fails startup instead of silently serving nothing; the bound
/// address is returned for logging.
pub fn spawn(addr: SocketAddr, status: Shared) -> Result<SocketAddr> {
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
                .block_on(serve(listener, status))
                .expect("webui server failed");
        })?;
    Ok(local)
}

async fn serve(listener: std::net::TcpListener, status: Shared) -> Result<()> {
    let app = axum::Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/log", get(api_log))
        .with_state(status);
    let listener = tokio::net::TcpListener::from_std(listener)?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn api_status(State(status): State<Shared>) -> Json<serde_json::Value> {
    let mut value = serde_json::to_value(&*lock(&status)).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        // Server time, so the page computes elapsed/remaining without
        // trusting the browser clock.
        object.insert("now".into(), epoch_now().into());
    }
    Json(value)
}

/// The live log tail while a turn is running, else the last finished turn's.
async fn api_log(State(status): State<Shared>) -> String {
    let (log_path, fallback) = {
        let status = lock(&status);
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

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Status> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn get(addr: SocketAddr, path: &str) -> String {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn serves_page_and_status() {
        let status = Status::new(Some(5));
        let addr = spawn("127.0.0.1:0".parse().unwrap(), status).unwrap();

        let response = get(addr, "/api/status");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["activity"]["state"], "starting");
        assert_eq!(value["daily_limit"], 5);
        assert!(value["now"].is_u64());

        assert!(get(addr, "/").contains("<html"));
    }
}
