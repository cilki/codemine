//! CLIProxyAPI health, as reported by its management API. The proxy owns the
//! subscription OAuth login and refreshes it continuously on its own —
//! codemine never touches tokens. Logins happen out-of-band (`cliproxyapi
//! --claude-login` on the host); this module only asks the proxy how the
//! account it holds is doing, for the web UI's card and the main loop's gate.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::settings::ProxySettings;

/// Where CLIProxyAPI listens by default; both the Anthropic-compatible API
/// opencode talks to and the management API share it.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8317";

/// A point-in-time picture of the proxy and the account it holds for the web
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

/// What blocks turns from running, as a message for the proxy card; None
/// when the proxy looks usable. A failed management query does not block —
/// inference may still work, and the error shows on the card instead.
pub fn problem(proxy: &ProxySettings) -> Option<String> {
    let health = health(proxy);
    if !health.proxy_up {
        return Some(format!(
            "CLIProxyAPI is unreachable at {}; is the service running?",
            proxy.base_url
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
/// main loop and the 2s event stream both read it, and each probe is a
/// blocking request.
const HEALTH_TTL: Duration = Duration::from_secs(10);

static HEALTH: Mutex<Option<(Instant, String, AuthHealth)>> = Mutex::new(None);

/// The current health, probed through the management API when a key is
/// configured and by plain reachability otherwise; cached briefly.
pub fn health(proxy: &ProxySettings) -> AuthHealth {
    let key = format!("{}|{}", proxy.base_url, proxy.management_key);
    let mut cached = HEALTH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, for_key, health)) = &*cached
        && at.elapsed() < HEALTH_TTL
        && *for_key == key
    {
        return health.clone();
    }
    let health = probe(proxy);
    *cached = Some((Instant::now(), key, health.clone()));
    health
}

/// How long the proxy has to answer a probe. It is normally a loopback
/// service, and the probe sits on the event stream's tick.
const PROBE_TIMEOUT_SECS: u64 = 5;

fn probe(proxy: &ProxySettings) -> AuthHealth {
    if proxy.management_key.is_empty() {
        // Any HTTP answer at all proves the proxy is there; whether a login
        // is installed can't be known without the management API. Nothing
        // went wrong, so there is no error to carry.
        let up = crate::http::get(&proxy.base_url, None, PROBE_TIMEOUT_SECS).is_ok();
        return unmanaged(up, None);
    }
    let url = format!("{}/v0/management/auth-files", proxy.base_url);
    let header = ("X-Management-Key", proxy.management_key.as_str());
    match crate::http::get(&url, Some(header), PROBE_TIMEOUT_SECS) {
        Ok((200, body)) => match serde_json::from_str(&body) {
            Ok(files) => health_from(&files),
            Err(_) => unmanaged(true, Some("the management API returned unexpected output")),
        },
        Ok((status, _)) => unmanaged(
            true,
            Some(&format!("the management API answered HTTP {status}")),
        ),
        Err(_) => unmanaged(false, Some("the proxy is unreachable")),
    }
}

/// Health when the management API said nothing usable — or was never asked,
/// there being no key for it: reachability is all that's known, so the
/// account is assumed fine and whatever went wrong is surfaced. An error on
/// an unreachable proxy is dropped, since "unreachable" is already the whole
/// of what the card has to say.
fn unmanaged(proxy_up: bool, error: Option<&str>) -> AuthHealth {
    AuthHealth {
        proxy_up,
        managed: false,
        connected: proxy_up,
        email: None,
        last_refresh: None,
        refresh_ok: true,
        success: None,
        failed: None,
        error: error.filter(|_| proxy_up).map(str::to_owned),
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
/// an explicit provider field (`provider`, `type`, `channel`) or a
/// claude-prefixed file name (`id`, `name`) — whichever of the five this
/// proxy version spells, since the same test answers for all of them.
fn is_claude_entry(entry: &serde_json::Value) -> bool {
    ["provider", "type", "channel", "id", "name"]
        .iter()
        .any(|key| {
            entry[key]
                .as_str()
                .is_some_and(|value| value.contains("claude") || value.contains("anthropic"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Every reading that stops short of the management API comes out of one
    /// place, so they can't drift: nothing is claimed about the account, the
    /// proxy is taken at its reachability, and an error is shown only when
    /// there is a reachable proxy for it to be about.
    #[test]
    fn an_unmanaged_reading_claims_nothing_about_the_account() {
        // No key configured: reachable and nothing went wrong.
        let keyless = unmanaged(true, None);
        assert!(keyless.proxy_up && keyless.connected && keyless.refresh_ok);
        assert!(!keyless.managed);
        assert_eq!(keyless.error, None);
        assert_eq!(keyless.email, None);
        assert_eq!((keyless.success, keyless.failed), (None, None));

        // A key that got a useless answer: the reason shows on the card.
        let refused = unmanaged(true, Some("the management API answered HTTP 401"));
        assert_eq!(
            refused.error.as_deref(),
            Some("the management API answered HTTP 401")
        );

        // Nothing there at all: "unreachable" is the whole story, so the
        // card isn't given a second line saying the same thing.
        let down = unmanaged(false, Some("the proxy is unreachable"));
        assert!(!down.proxy_up && !down.connected);
        assert_eq!(down.error, None);
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
