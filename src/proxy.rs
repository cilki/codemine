//! CLIProxyAPI health, as reported by its management API. The proxy owns the
//! subscription OAuth login and refreshes it continuously on its own —
//! codemine never touches tokens. Logins happen out-of-band (`cliproxyapi
//! --claude-login` on the host); this module only asks the proxy how the
//! account it holds is doing, for the web UI's card and the main loop's gate.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::settings::{Problem, ProxySettings};

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

/// What blocks turns from running, tied to the field that fixes it; None
/// when the proxy looks usable and will serve `model`. A failed management
/// query does not block — inference may still work, and the error shows on
/// the card instead.
pub fn problem(proxy: &ProxySettings, model: &str) -> Option<Problem> {
    blocking(
        &proxy.base_url,
        model,
        &health(proxy),
        served_models(proxy).as_ref(),
    )
}

/// The decision behind [`problem`], kept pure (readings in, problem out) so
/// every arm is testable without a proxy to answer them.
fn blocking(
    base_url: &str,
    model: &str,
    health: &AuthHealth,
    served: Option<&BTreeSet<String>>,
) -> Option<Problem> {
    if !health.proxy_up {
        return Some(Problem::new(
            "proxy-card",
            format!("CLIProxyAPI is unreachable at {base_url}; is the service running?"),
        ));
    }
    if health.managed && !health.connected {
        return Some(Problem::new(
            "proxy-card",
            "no Claude account is logged into CLIProxyAPI; \
             run `cliproxyapi --claude-login` on the host",
        ));
    }
    // The settings page only offers models the proxy serves, but the one in
    // config.json was picked against whatever it served then — and opencode
    // lists undated aliases (`claude-sonnet-4-5`) that CLIProxyAPI answers
    // for only under their dated IDs, so configs written before this check
    // existed can name a model that was never servable. Left to the turn it
    // dies on `unknown provider for model ...`, which is also exactly what a
    // credential-less proxy says: the runner reads it as a proxy auth
    // failure, blames the login, and gates turns five minutes at a time
    // forever. Naming the model instead says what to fix.
    if let Some(served) = served
        && !servable(model, served)
    {
        return Some(Problem::new(
            "s-model",
            format!("CLIProxyAPI does not serve {model}; pick a model it lists"),
        ));
    }
    None
}

/// Whether CLIProxyAPI's listing covers a `provider/model` ID. Only the
/// `anthropic` provider is pointed at the proxy (see
/// [`crate::prompts::install_provider`]); any other provider resolves
/// through opencode's own auth, so its models are not the proxy's to serve
/// and pass regardless.
pub fn servable(id: &str, served: &BTreeSet<String>) -> bool {
    match id.split_once('/') {
        Some(("anthropic", model)) => served.contains(model),
        _ => true,
    }
}

/// How long a health reading stands before the proxy is asked again: the 5s
/// main loop and the 2s event stream both read it, and each probe is a
/// blocking request.
const HEALTH_TTL: Duration = Duration::from_secs(10);

/// A cached probe: when it was taken, the proxy settings it was taken
/// against, and what it found.
type Cached<T> = Option<(Instant, String, T)>;

static HEALTH: Mutex<Cached<AuthHealth>> = Mutex::new(None);

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

static SERVED: Mutex<Cached<Option<BTreeSet<String>>>> = Mutex::new(None);

/// The model IDs the proxy will answer for, from its own listing; cached on
/// the same terms as the health reading, since the settings page and the
/// main loop both ask. None means the listing couldn't be read, and the
/// callers fall back to trusting whatever opencode offered: a probe that
/// failed is no grounds for narrowing the choice to nothing or for calling
/// a working model unservable.
pub fn served_models(proxy: &ProxySettings) -> Option<BTreeSet<String>> {
    let key = format!("{}|{}", proxy.base_url, proxy.api_key);
    let mut cached = SERVED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, for_key, served)) = &*cached
        && at.elapsed() < HEALTH_TTL
        && *for_key == key
    {
        return served.clone();
    }
    let served = probe_served(proxy);
    *cached = Some((Instant::now(), key, served.clone()));
    served
}

fn probe_served(proxy: &ProxySettings) -> Option<BTreeSet<String>> {
    let url = format!("{}/v1/models", proxy.base_url);
    // The client key, when there is one: a proxy with a nonempty `api-keys`
    // list rejects an unauthenticated listing, and a rejected listing would
    // read as a proxy that serves nothing.
    let key = (!proxy.api_key.is_empty()).then_some(("x-api-key", proxy.api_key.as_str()));
    match crate::http::get(&url, key, PROBE_TIMEOUT_SECS) {
        Ok((200, body)) => parse_served(&body),
        _ => None,
    }
}

/// The model IDs out of the proxy's OpenAI-shaped `/v1/models` answer. Kept
/// pure for tests, and None rather than an empty set when the answer isn't
/// that shape — an empty listing is indistinguishable from one this can't
/// read, and both have to mean "don't narrow" rather than "nothing works".
fn parse_served(body: &str) -> Option<BTreeSet<String>> {
    let listing: serde_json::Value = serde_json::from_str(body).ok()?;
    let ids: BTreeSet<String> = listing["data"]
        .as_array()?
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect();
    (!ids.is_empty()).then_some(ids)
}

fn probe(proxy: &ProxySettings) -> AuthHealth {
    if proxy.management_key.is_empty() {
        // Any HTTP answer at all proves the proxy is there; whether a login
        // is installed can't be known without the management API.
        let up = crate::http::get(&proxy.base_url, None, PROBE_TIMEOUT_SECS).is_ok();
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
    let url = format!("{}/v0/management/auth-files", proxy.base_url);
    let header = ("X-Management-Key", proxy.management_key.as_str());
    match crate::http::get(&url, Some(header), PROBE_TIMEOUT_SECS) {
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

    #[test]
    fn served_models_are_read_from_the_listing() {
        // Trimmed from a real CLIProxyAPI /v1/models answer.
        let listing = r#"{"data":[
            {"created":1759276800,"id":"claude-haiku-4-5-20251001","object":"model","owned_by":"anthropic"},
            {"created":1770318000,"id":"claude-opus-4-6","object":"model","owned_by":"anthropic"}
        ],"object":"list"}"#;
        assert_eq!(
            parse_served(listing),
            Some(
                ["claude-haiku-4-5-20251001", "claude-opus-4-6"]
                    .map(String::from)
                    .into()
            )
        );
        // Nothing to narrow by is not the same as nothing being servable, so
        // an unreadable or empty answer has to read as "don't narrow".
        for body in [r#"{"data":[]}"#, "{}", r#"{"data":"soon"}"#, "<html>", ""] {
            assert_eq!(parse_served(body), None, "{body}");
        }
    }

    #[test]
    fn only_the_proxys_own_provider_is_held_to_its_listing() {
        let served = ["claude-sonnet-4-5-20250929"].map(String::from).into();
        assert!(servable("anthropic/claude-sonnet-4-5-20250929", &served));
        // opencode offers the undated alias; the proxy answers for it only
        // under the dated ID, and a turn on it dies instantly.
        assert!(!servable("anthropic/claude-sonnet-4-5", &served));
        // Every other provider resolves through opencode's own auth, so the
        // proxy's listing says nothing about it.
        assert!(servable("opencode/big-pickle", &served));
        assert!(servable("openai/gpt-5", &served));
        assert!(servable("no-slash", &served));
    }

    #[test]
    fn an_unservable_model_is_blamed_on_the_model_not_the_login() {
        let healthy = AuthHealth {
            proxy_up: true,
            managed: true,
            connected: true,
            email: None,
            last_refresh: None,
            refresh_ok: true,
            success: None,
            failed: None,
            error: None,
        };
        let served: BTreeSet<String> = ["claude-sonnet-4-5-20250929"].map(String::from).into();

        let problem = blocking(
            "http://127.0.0.1:8317",
            "anthropic/claude-sonnet-4-5",
            &healthy,
            Some(&served),
        )
        .expect("a model the proxy won't answer for blocks turns");
        // The field is what the page marks: this is fixed in the model
        // selector, not by redoing the proxy login.
        assert_eq!(problem.field, "s-model");
        assert!(
            problem.message.contains("anthropic/claude-sonnet-4-5"),
            "{}",
            problem.message
        );

        // A model it does serve is no problem, and neither is a proxy whose
        // listing couldn't be read.
        assert!(
            blocking(
                "http://127.0.0.1:8317",
                "anthropic/claude-sonnet-4-5-20250929",
                &healthy,
                Some(&served),
            )
            .is_none()
        );
        assert!(
            blocking(
                "http://127.0.0.1:8317",
                "anthropic/claude-sonnet-4-5",
                &healthy,
                None,
            )
            .is_none()
        );

        // An unreachable proxy or a missing login still answers for itself,
        // and does so before the model is looked at.
        let down = AuthHealth {
            proxy_up: false,
            ..healthy.clone()
        };
        assert_eq!(
            blocking("http://nope:1", "anthropic/claude-sonnet-4-5", &down, None)
                .unwrap()
                .field,
            "proxy-card"
        );
        let logged_out = AuthHealth {
            connected: false,
            ..healthy.clone()
        };
        assert_eq!(
            blocking(
                "http://127.0.0.1:8317",
                "anthropic/claude-sonnet-4-5",
                &logged_out,
                Some(&served),
            )
            .unwrap()
            .field,
            "proxy-card"
        );
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
