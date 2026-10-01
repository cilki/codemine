//! Claude auth health, as reported by CLIProxyAPI. The proxy owns the
//! subscription OAuth login and refreshes it continuously on its own —
//! codemine never touches tokens. Logins happen out-of-band (`cliproxyapi
//! --claude-login` on the host); this module only asks the proxy's
//! management API how the account is doing, for the web UI's card and the
//! main loop's gate.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::settings::ClaudeSettings;

/// Where CLIProxyAPI listens by default; both the Anthropic-compatible API
/// opencode talks to and the management API share it.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8317";

/// A point-in-time picture of the proxy and its Claude account for the web
/// UI and the main loop's gate. Without a management key only reachability
/// is known and the account fields stay empty.
#[derive(Clone, PartialEq, Serialize)]
pub struct AuthHealth {
    /// The proxy answered HTTP at all.
    pub proxy_up: bool,
    /// The fields below come from the management API rather than being
    /// assumed; false when no management key is configured.
    pub managed: bool,
    /// At least one live Claude credential is logged into the proxy.
    /// Assumed true while unmanaged — turns find out the hard way.
    pub connected: bool,
    pub email: Option<String>,
    pub last_refresh: Option<String>,
    /// The credential's own status is healthy; refreshing is the proxy's
    /// continuous background job, so false means the login needs redoing.
    pub refresh_ok: bool,
    pub success: Option<u64>,
    pub failed: Option<u64>,
    /// The management query itself failed (rejected key, unexpected shape);
    /// account fields fall back to assumptions while this is set.
    pub error: Option<String>,
}

/// What blocks turns from running, as a message for the account card; None
/// when the proxy looks usable. A failed management query does not block —
/// inference may still work, and the error shows on the card instead.
pub fn problem(claude: &ClaudeSettings) -> Option<String> {
    let health = health(claude);
    if !health.proxy_up {
        return Some(format!(
            "CLIProxyAPI is unreachable at {}; is the service running?",
            claude.base_url
        ));
    }
    if health.managed && !health.connected {
        return Some(
            "no Claude account is logged into CLIProxyAPI; \
             run `cliproxyapi --claude-login` on the host"
                .into(),
        );
    }
    None
}

/// How long a health reading stands before the proxy is asked again: the 5s
/// main loop and the 2s event stream both read it, and each probe shells out
/// to curl.
const HEALTH_TTL: Duration = Duration::from_secs(10);

static HEALTH: Mutex<Option<(Instant, String, AuthHealth)>> = Mutex::new(None);

/// The current health, probed through the management API when a key is
/// configured and by plain reachability otherwise; cached briefly.
pub fn health(claude: &ClaudeSettings) -> AuthHealth {
    let key = format!("{}|{}", claude.base_url, claude.management_key);
    let mut cached = HEALTH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, for_key, health)) = &*cached
        && at.elapsed() < HEALTH_TTL
        && *for_key == key
    {
        return health.clone();
    }
    let health = probe(claude);
    *cached = Some((Instant::now(), key, health.clone()));
    health
}

fn probe(claude: &ClaudeSettings) -> AuthHealth {
    if claude.management_key.is_empty() {
        // Any HTTP answer at all proves the proxy is there; whether a login
        // is installed can't be known without the management API.
        let up = curl_get(&claude.base_url, None).is_ok();
        return AuthHealth {
            proxy_up: up,
            managed: false,
            connected: up,
            email: None,
            last_refresh: None,
            refresh_ok: true,
            success: None,
            failed: None,
            error: None,
        };
    }
    let url = format!("{}/v0/management/auth-files", claude.base_url);
    match curl_get(&url, Some(&claude.management_key)) {
        Ok((200, body)) => match serde_json::from_str(&body) {
            Ok(files) => health_from(&files),
            Err(_) => unmanaged(true, "the management API returned unexpected output"),
        },
        Ok((status, _)) => unmanaged(true, &format!("the management API answered HTTP {status}")),
        Err(_) => unmanaged(false, "the proxy is unreachable"),
    }
}

/// Health when the management API couldn't be read: reachability is all
/// that's known, so the account is assumed fine and the error is surfaced.
fn unmanaged(proxy_up: bool, error: &str) -> AuthHealth {
    AuthHealth {
        proxy_up,
        managed: false,
        connected: proxy_up,
        email: None,
        last_refresh: None,
        refresh_ok: true,
        success: None,
        failed: None,
        error: proxy_up.then(|| error.to_owned()),
    }
}

/// Distill the management API's auth-files listing into health. Kept pure
/// (JSON in, health out) so the parse is testable against captured fixtures.
fn health_from(files: &serde_json::Value) -> AuthHealth {
    let empty = Vec::new();
    let claude_entries: Vec<&serde_json::Value> = files["files"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter(|entry| is_claude_entry(entry))
        .collect();
    // The first entry that isn't disabled speaks for the account; codemine
    // deployments hold one login, not a load-balanced pool.
    let live = claude_entries
        .iter()
        .find(|entry| !entry["disabled"].as_bool().unwrap_or(false));
    AuthHealth {
        proxy_up: true,
        managed: true,
        connected: live.is_some(),
        email: live.and_then(|entry| entry["email"].as_str().map(String::from)),
        // last_refresh is null until the proxy's first refresh; the auth
        // file's mtime moves on every rotation, so it stands in.
        last_refresh: live.and_then(|entry| {
            ["last_refresh", "modtime"]
                .iter()
                .find_map(|key| entry[key].as_str().map(String::from))
        }),
        refresh_ok: live.is_some_and(|entry| {
            // "active" is the healthy status; unknown ones get the benefit
            // of the doubt so a proxy upgrade can't read as a dead login.
            !entry["unavailable"].as_bool().unwrap_or(false)
                && !matches!(
                    entry["status"].as_str().unwrap_or("active"),
                    "error" | "failed" | "expired" | "invalid"
                )
        }),
        success: live.and_then(|entry| entry["success"].as_u64()),
        failed: live.and_then(|entry| entry["failed"].as_u64()),
        error: None,
    }
}

/// Whether an auth-files entry belongs to the Claude provider. The listing
/// covers every provider the proxy holds; Claude entries are recognized by
/// an explicit provider field or a claude-prefixed file name.
fn is_claude_entry(entry: &serde_json::Value) -> bool {
    ["provider", "type", "channel"].iter().any(|key| {
        entry[key]
            .as_str()
            .is_some_and(|value| value.contains("claude") || value.contains("anthropic"))
    }) || ["id", "name"].iter().any(|key| {
        entry[key]
            .as_str()
            .is_some_and(|value| value.contains("claude") || value.contains("anthropic"))
    })
}

/// GET a URL through curl, returning the HTTP status and body; Err means no
/// HTTP conversation happened at all. The management key goes over stdin via
/// curl's config syntax rather than argv, where it would be readable in the
/// process listing.
fn curl_get(url: &str, management_key: Option<&str>) -> Result<(u16, String)> {
    let mut command = Command::new("curl");
    command.args(["-sS", "--max-time", "5", "-w", "\n%{http_code}", url]);
    let mut child = match management_key {
        Some(_) => command.args(["--config", "-"]),
        None => &mut command,
    }
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .context("failed to run curl")?;
    if let Some(key) = management_key {
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(format!("header = \"X-Management-Key: {key}\"\n").as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "curl exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    split_status(&String::from_utf8_lossy(&output.stdout))
}

/// Split curl's output into body and the trailing status line its -w format
/// appends. Kept pure for tests.
fn split_status(stdout: &str) -> Result<(u16, String)> {
    let (body, status) = stdout
        .trim_end()
        .rsplit_once('\n')
        .unwrap_or(("", stdout.trim_end()));
    let status: u16 = status
        .trim()
        .parse()
        .context("curl reported no HTTP status")?;
    Ok((status, body.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_status_separates_body_and_code() {
        assert_eq!(
            split_status("{\"files\":[]}\n200").unwrap(),
            (200, "{\"files\":[]}".into())
        );
        assert_eq!(split_status("\n404").unwrap(), (404, "".into()));
        // curl reports 000 when the connection never happened.
        assert_eq!(split_status("000").unwrap().0, 0);
        assert!(split_status("").is_err());
        assert!(split_status("not a status").is_err());
    }

    #[test]
    fn health_reads_a_live_claude_entry() {
        // Trimmed from a real CLIProxyAPI 7.3.2 auth-files response.
        let files = serde_json::json!({ "files": [{
            "account": "user@example.com",
            "account_type": "oauth",
            "disabled": false,
            "email": "user@example.com",
            "failed": 1,
            "id": "claude-user@example.com.json",
            "last_refresh": null,
            "modtime": "2026-09-29T22:13:47.616218931-05:00",
            "provider": "claude",
            "status": "active",
            "status_message": "",
            "success": 12,
            "type": "claude",
            "unavailable": false,
        }], "observed_at": "2026-09-30T03:13:13Z" });
        let health = health_from(&files);
        assert!(health.proxy_up && health.managed && health.connected);
        assert!(health.refresh_ok);
        assert_eq!(health.email.as_deref(), Some("user@example.com"));
        // A null last_refresh falls back to the auth file's mtime.
        assert_eq!(
            health.last_refresh.as_deref(),
            Some("2026-09-29T22:13:47.616218931-05:00")
        );
        assert_eq!((health.success, health.failed), (Some(12), Some(1)));
        assert_eq!(health.error, None);
    }

    #[test]
    fn health_without_claude_entries_is_disconnected() {
        for files in [
            serde_json::json!({ "files": [] }),
            serde_json::json!({}),
            // Another provider's login is not a Claude login.
            serde_json::json!({ "files": [{ "id": "gemini-x.json", "status": "ready" }] }),
            // A disabled credential doesn't count either.
            serde_json::json!({ "files": [{ "id": "claude-x.json", "disabled": true }] }),
        ] {
            let health = health_from(&files);
            assert!(health.proxy_up && health.managed);
            assert!(!health.connected, "{files}");
            assert!(!health.refresh_ok);
        }
    }

    #[test]
    fn health_flags_a_failing_credential() {
        for entry in [
            serde_json::json!({ "id": "claude-x.json", "status": "error" }),
            serde_json::json!({ "id": "claude-x.json", "status": "active", "unavailable": true }),
        ] {
            let health = health_from(&serde_json::json!({ "files": [entry] }));
            assert!(health.connected);
            assert!(!health.refresh_ok);
        }

        // An unknown status is not treated as failure.
        let files = serde_json::json!({ "files": [{
            "id": "claude-x.json",
            "status": "cooling",
            "disabled": false,
        }]});
        assert!(health_from(&files).refresh_ok);
    }

    #[test]
    fn claude_entries_are_recognized_by_field_or_name() {
        for entry in [
            serde_json::json!({ "provider": "claude" }),
            serde_json::json!({ "type": "claude" }),
            serde_json::json!({ "channel": "anthropic" }),
            serde_json::json!({ "id": "claude-user@example.com.json" }),
            serde_json::json!({ "name": "anthropic-main.json" }),
        ] {
            assert!(is_claude_entry(&entry), "{entry}");
        }
        for entry in [
            serde_json::json!({ "id": "gemini-user.json" }),
            serde_json::json!({ "provider": "codex" }),
            serde_json::json!({}),
        ] {
            assert!(!is_claude_entry(&entry), "{entry}");
        }
    }
}
