//! Claude OAuth credential handling. The opencode-claude-auth plugin owns
//! token *refreshing* — a refresh rotates the single-use refresh token, so
//! only one party may do it — while this module owns everything around it:
//! the browser login flow the web UI drives, the canonical credentials file
//! Claude Code also reads and writes, reconciliation of rotations the plugin
//! parked in opencode's auth.json, and a health digest of the plugin's
//! refresh log so failures are diagnosed structurally instead of scraped
//! from the turn output.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The OAuth client Claude Code itself registers; logins minted with it get
/// subscription billing rather than metered API usage.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPE: &str = "org:create_api_key user:profile user:inference";

/// Where Claude Code keeps the OAuth credentials the plugin reads and
/// refreshes; the canonical store, so an external `claude login` keeps
/// working alongside the web UI flow.
pub fn credentials_path() -> PathBuf {
    crate::config::home().join(".claude/.credentials.json")
}

/// Where opencode stores provider credentials, which the plugin mirrors
/// every token rotation into.
pub fn auth_json_path() -> PathBuf {
    crate::config::xdg_dir("XDG_DATA_HOME", ".local/share").join("opencode/auth.json")
}

/// Where the plugin logs each refresh attempt's outcome (tokens redacted)
/// when `CLAUDE_AUTH_DEBUG` names this path. Truncated at every opencode
/// start that carries the variable, so it holds the latest turn's story.
pub fn debug_log_path() -> PathBuf {
    crate::config::xdg_dir("XDG_DATA_HOME", ".local/share").join("opencode/claude-auth-debug.log")
}

/// A fingerprint of the credentials file that changes when it's rewritten,
/// so a fresh login is detectable; a missing file maps to the epoch.
pub fn credentials_stamp() -> SystemTime {
    std::fs::metadata(credentials_path())
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Whether opencode can authenticate: the plugin needs a credentials file
/// with both tokens present. Expiry is deliberately not checked — an expired
/// access token with a live refresh token is the plugin's normal case.
pub fn oauth_usable() -> bool {
    usable_at(&credentials_path())
}

fn usable_at(path: &Path) -> bool {
    read_json(path).is_some_and(|credentials| {
        ["accessToken", "refreshToken"].iter().all(|key| {
            credentials["claudeAiOauth"][key]
                .as_str()
                .is_some_and(|token| !token.is_empty())
        })
    })
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A freshly exchanged token pair, plus whatever optional metadata the token
/// endpoint volunteered. Deliberately not Debug: the fields are secrets.
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: u64,
    pub scopes: Option<Vec<String>>,
    pub subscription_type: Option<String>,
}

/// Install a token pair into the credentials file, preserving any fields it
/// already carries that this module doesn't know about — Claude Code stores
/// more than the plugin reads, and a login must not strip it.
pub fn write_credentials(path: &Path, tokens: &Tokens) -> Result<()> {
    let mut blob = read_json(path).unwrap_or_else(|| serde_json::json!({}));
    if !blob.is_object() {
        blob = serde_json::json!({});
    }
    let oauth = blob
        .as_object_mut()
        .expect("blob was just made an object")
        .entry("claudeAiOauth")
        .or_insert_with(|| serde_json::json!({}));
    if !oauth.is_object() {
        *oauth = serde_json::json!({});
    }
    let oauth = oauth
        .as_object_mut()
        .expect("entry was just made an object");
    oauth.insert("accessToken".into(), tokens.access.clone().into());
    oauth.insert("refreshToken".into(), tokens.refresh.clone().into());
    oauth.insert("expiresAt".into(), tokens.expires_at_ms.into());
    if let Some(scopes) = &tokens.scopes {
        oauth.insert("scopes".into(), scopes.clone().into());
    }
    if let Some(subscription) = &tokens.subscription_type {
        oauth.insert("subscriptionType".into(), subscription.clone().into());
    }
    write_json_600(path, &blob)
}

/// Atomically replace `path` with `value` serialized, private to the owner.
/// Readers (the plugin at turn start, the main loop every pass) only ever
/// see the old or the new file, never a torn one.
fn write_json_600(path: &Path, value: &serde_json::Value) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("failed to stage a file in {}", dir.display()))?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.persist(path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// A login the web UI has started: the URL for the user's browser and the
/// PKCE verifier the eventual code exchange must present.
pub struct Login {
    pub url: String,
    pub verifier: String,
}

pub fn begin_login() -> Result<Login> {
    let verifier = random_verifier()?;
    let url = format!(
        "{AUTHORIZE_URL}?code=true&client_id={CLIENT_ID}&response_type=code\
         &redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={verifier}",
        urlencode(REDIRECT_URI),
        urlencode(SCOPE),
        challenge(&verifier),
    );
    Ok(Login { url, verifier })
}

/// 32 bytes of kernel randomness as base64url: 43 characters from the
/// unreserved set, comfortably inside RFC 7636's 43-128 bounds. The verifier
/// is a secret, so this deliberately avoids fastrand (not a CSPRNG).
fn random_verifier() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("failed to read /dev/urandom")?;
    Ok(base64url(&bytes))
}

fn challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// RFC 4648 §5 base64url without padding.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = u32::from_be_bytes([
            0,
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ]);
        for shift in (0..=chunk.len()).map(|i| 18 - 6 * i) {
            out.push(ALPHABET[(word >> shift) as usize & 63] as char);
        }
    }
    out
}

/// Percent-encode everything outside RFC 3986's unreserved set.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Exchange the string the user pasted back for a token pair. `pasted` is
/// `code#state` as issued by the authorize page; the state must match the
/// verifier of the login this exchange belongs to, which also catches a
/// mangled paste before it costs a round trip.
pub fn exchange(pasted: &str, verifier: &str) -> Result<Tokens> {
    let (code, state) = split_code(pasted, verifier)?;
    let body = serde_json::json!({
        "code": code,
        "state": state,
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
    });
    // The body carries the single-use code and the verifier, so it goes over
    // stdin rather than argv (same reasoning as precheck's gitea_json). The
    // trailing -w line smuggles the HTTP status out alongside the body.
    let mut child = Command::new("curl")
        .args(["-sS", "--max-time", "15", "-X", "POST"])
        .args(["-H", "Content-Type: application/json"])
        .args(["--data", "@-", "-w", "\n%{http_code}", TOKEN_URL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run curl")?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(body.to_string().as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "curl exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_exchange(&String::from_utf8_lossy(&output.stdout), now_ms())
}

fn split_code<'a>(pasted: &'a str, verifier: &str) -> Result<(&'a str, &'a str)> {
    let pasted = pasted.trim();
    let Some((code, state)) = pasted.split_once('#') else {
        bail!("expected a code of the form code#state; paste it exactly as shown");
    };
    if state != verifier {
        bail!("the pasted code belongs to a different login attempt; start over");
    }
    Ok((code, state))
}

/// Split curl's output into body and trailing status line and turn a 2xx
/// into tokens. Kept pure (stdout in, tokens out) so the error paths are
/// testable without a network.
fn parse_exchange(stdout: &str, now_ms: u64) -> Result<Tokens> {
    let (body, status) = stdout
        .trim_end()
        .rsplit_once('\n')
        .unwrap_or(("", stdout.trim_end()));
    let status: u16 = status
        .trim()
        .parse()
        .context("curl reported no HTTP status")?;
    if !(200..300).contains(&status) {
        bail!("token endpoint answered HTTP {status}: {}", body.trim());
    }
    let parsed: serde_json::Value =
        serde_json::from_str(body).context("token endpoint returned unexpected output")?;
    let access = parsed["access_token"]
        .as_str()
        .context("token endpoint returned no access_token")?;
    let refresh = parsed["refresh_token"]
        .as_str()
        .context("token endpoint returned no refresh_token")?;
    Ok(Tokens {
        access: access.into(),
        refresh: refresh.into(),
        // The plugin's own fallback lifetime when the endpoint omits one.
        expires_at_ms: now_ms + parsed["expires_in"].as_u64().unwrap_or(36_000) * 1000,
        scopes: parsed["scope"]
            .as_str()
            .map(|scope| scope.split_whitespace().map(String::from).collect()),
        subscription_type: parsed["subscription_type"].as_str().map(String::from),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Drop the anthropic entry from opencode's stored credentials so the plugin
/// re-derives it from the Claude Code credentials file: a stale or hand-added
/// entry makes opencode call Anthropic as a plain third-party app, which
/// bills extra usage instead of the subscription. A missing or malformed
/// file is left for opencode to sort out.
///
/// The entry can also hold the only live refresh token: the plugin mirrors
/// every OAuth rotation into auth.json, but its write-back to the credentials
/// file can fail silently, and a rotation invalidates the refresh token it
/// was exchanged for. A newer pair is copied into the credentials file before
/// the entry is dropped, so a restart can't destroy it — and when that copy
/// fails, the entry stays put rather than be destroyed with it.
pub fn scrub_anthropic_auth(path: &Path, credentials: &Path) -> Result<()> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    let Ok(mut auth) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        tracing::warn!("{} is not valid JSON; leaving it alone", path.display());
        return Ok(());
    };
    let Some(entry) = auth
        .as_object_mut()
        .and_then(|auth| auth.remove("anthropic"))
    else {
        return Ok(());
    };
    if let Err(err) = rescue_rotation(&entry, credentials) {
        tracing::warn!(
            "leaving the anthropic entry in {}; its OAuth pair couldn't be saved: {err:#}",
            path.display()
        );
        return Ok(());
    }
    std::fs::write(path, serde_json::to_vec_pretty(&auth)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// The between-turns half of the scrub: rescue a newer rotation out of
/// auth.json without touching the entry itself — while the runner is live
/// the plugin owns that mirror, and deleting it every turn would fight the
/// plugin's own syncing for no gain.
pub fn reconcile(path: &Path, credentials: &Path) -> Result<()> {
    let Some(auth) = read_json(path) else {
        return Ok(());
    };
    match auth.get("anthropic") {
        Some(entry) => rescue_rotation(entry, credentials),
        None => Ok(()),
    }
}

/// Copy the auth.json entry's OAuth pair into the Claude Code credentials
/// file when it is a newer rotation than the one stored there, judged by
/// expiry, which only moves forward on a real refresh. Entries that aren't a
/// full OAuth pair (plain API keys, malformed leftovers) rescue nothing, and
/// a missing credentials file or one without the expected login shape is left
/// alone — the rescue repairs an existing login, it doesn't manufacture one.
fn rescue_rotation(entry: &serde_json::Value, credentials: &Path) -> Result<()> {
    let (Some(access), Some(refresh), Some(expires)) = (
        entry["access"].as_str(),
        entry["refresh"].as_str(),
        entry["expires"].as_u64(),
    ) else {
        return Ok(());
    };
    let Ok(bytes) = std::fs::read(credentials) else {
        return Ok(());
    };
    let mut blob: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not valid JSON", credentials.display()))?;
    let Some(oauth) = blob["claudeAiOauth"].as_object_mut() else {
        return Ok(());
    };
    if expires <= oauth.get("expiresAt").and_then(|v| v.as_u64()).unwrap_or(0) {
        return Ok(());
    }
    oauth.insert("accessToken".into(), access.into());
    oauth.insert("refreshToken".into(), refresh.into());
    oauth.insert("expiresAt".into(), expires.into());
    write_json_600(credentials, &blob)?;
    tracing::info!(
        "rescued a newer Claude OAuth rotation into {}",
        credentials.display()
    );
    Ok(())
}

/// The verdict of the plugin's most recent refresh attempt, distilled from
/// its debug log. Terminal means the refresh token itself was rejected —
/// only a new login helps — while transient covers rate limits, outages, and
/// anything else worth retrying.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Refresh {
    Ok,
    Terminal { reason: Option<String> },
    Transient { reason: Option<String> },
    Unavailable,
    NoData,
}

/// Distill the plugin's JSONL debug log down to one refresh verdict. The
/// log is truncated at each turn's opencode start, so it covers exactly one
/// turn. A success resets the verdict; between successes the *worst* event
/// wins rather than the last, because a dead-token failure trails follow-up
/// events (`refresh_exhausted`, `credentials_unavailable`) that would
/// otherwise mask the terminal diagnosis. Malformed lines and unrelated
/// events are skipped.
pub fn refresh_digest(path: &Path) -> Refresh {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Refresh::NoData;
    };
    let Ok(tail) = crate::turn::read_tail(&mut file, 64 * 1024) else {
        return Refresh::NoData;
    };
    let mut digest = Refresh::NoData;
    for line in tail.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let reason = || {
            ["oauthError", "error"]
                .iter()
                .find_map(|key| event[key].as_str().map(String::from))
        };
        let verdict = match event["event"].as_str() {
            Some("refresh_success") => Refresh::Ok,
            Some("refresh_terminal") => Refresh::Terminal { reason: reason() },
            Some("refresh_transient" | "refresh_exhausted") => {
                Refresh::Transient { reason: reason() }
            }
            Some("refresh_failed") => match event["kind"].as_str() {
                Some("terminal") => Refresh::Terminal { reason: reason() },
                Some("transient") => Refresh::Transient { reason: reason() },
                // The CLI-fallback failures carry no kind and say nothing
                // about the token itself.
                _ => continue,
            },
            Some("credentials_unavailable") => Refresh::Unavailable,
            _ => continue,
        };
        digest = match (severity(&digest), severity(&verdict)) {
            // A success wipes the slate; failures accumulate to the worst.
            _ if verdict == Refresh::Ok => Refresh::Ok,
            (held, new) if new > held => verdict,
            _ => digest,
        };
    }
    digest
}

/// How bad a verdict is, for worst-event-wins folding.
fn severity(refresh: &Refresh) -> u8 {
    match refresh {
        Refresh::NoData | Refresh::Ok => 0,
        Refresh::Unavailable => 1,
        Refresh::Transient { .. } => 2,
        Refresh::Terminal { .. } => 3,
    }
}

/// A point-in-time picture of Claude auth for the web UI: whether a login is
/// installed, when its access token lapses, and how the plugin's last
/// refresh went. Derived fresh on every read; nothing is stored.
#[derive(Clone, PartialEq, Serialize)]
pub struct AuthHealth {
    pub connected: bool,
    pub expires_at: Option<u64>,
    pub refresh: Refresh,
}

pub fn health() -> AuthHealth {
    health_at(&credentials_path(), &debug_log_path())
}

fn health_at(credentials: &Path, log: &Path) -> AuthHealth {
    AuthHealth {
        connected: usable_at(credentials),
        expires_at: read_json(credentials)
            .and_then(|blob| blob["claudeAiOauth"]["expiresAt"].as_u64()),
        refresh: refresh_digest(log),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc4648_vectors() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        // Bytes that exercise the url-safe alphabet ('-' and '_').
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn challenge_matches_rfc7636_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn login_url_carries_the_flow_parameters() {
        let login = begin_login().unwrap();
        assert_eq!(login.verifier.len(), 43);
        assert!(
            login
                .verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert!(login.url.starts_with("https://claude.ai/oauth/authorize?"));
        for expected in [
            "code=true",
            "response_type=code",
            "code_challenge_method=S256",
            "scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference",
            "redirect_uri=https%3A%2F%2Fconsole.anthropic.com%2Foauth%2Fcode%2Fcallback",
            &format!("state={}", login.verifier),
            &format!("code_challenge={}", challenge(&login.verifier)),
        ] {
            assert!(login.url.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn split_code_validates_the_paste() {
        assert_eq!(split_code(" abc#v ", "v").unwrap(), ("abc", "v"));
        assert!(split_code("abc", "v").is_err());
        assert!(split_code("abc#other", "v").is_err());
    }

    #[test]
    fn parse_exchange_handles_success_and_failure() {
        let ok = parse_exchange(
            "{\"access_token\":\"a\",\"refresh_token\":\"r\",\"expires_in\":3600,\
             \"scope\":\"user:inference user:profile\"}\n200",
            1_000,
        )
        .unwrap();
        assert_eq!(ok.access, "a");
        assert_eq!(ok.refresh, "r");
        assert_eq!(ok.expires_at_ms, 1_000 + 3_600_000);
        assert_eq!(
            ok.scopes.as_deref(),
            Some(&["user:inference".to_string(), "user:profile".to_string()][..])
        );
        assert!(ok.subscription_type.is_none());

        let denied = parse_exchange("{\"error\":\"invalid_grant\"}\n400", 0)
            .err()
            .expect("a 400 must not parse");
        assert!(denied.to_string().contains("HTTP 400"), "{denied:#}");
        assert!(denied.to_string().contains("invalid_grant"), "{denied:#}");

        assert!(parse_exchange("not json\n200", 0).is_err());
        assert!(parse_exchange("", 0).is_err());
    }

    #[test]
    fn write_credentials_creates_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let tokens = Tokens {
            access: "a1".into(),
            refresh: "r1".into(),
            expires_at_ms: 1000,
            scopes: Some(vec!["user:inference".into()]),
            subscription_type: None,
        };

        write_credentials(&path, &tokens).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let blob = read_json(&path).unwrap();
        assert_eq!(blob["claudeAiOauth"]["accessToken"], "a1");
        assert_eq!(blob["claudeAiOauth"]["scopes"][0], "user:inference");
        assert!(usable_at(&path));

        // A rewrite preserves fields it doesn't know about, at both levels.
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"x","refreshToken":"y","expiresAt":1,
                "subscriptionType":"max"},"mcpOAuth":{"keep":true}}"#,
        )
        .unwrap();
        write_credentials(
            &path,
            &Tokens {
                access: "a2".into(),
                refresh: "r2".into(),
                expires_at_ms: 2000,
                scopes: None,
                subscription_type: None,
            },
        )
        .unwrap();
        let blob = read_json(&path).unwrap();
        assert_eq!(blob["claudeAiOauth"]["refreshToken"], "r2");
        assert_eq!(blob["claudeAiOauth"]["expiresAt"], 2000);
        assert_eq!(blob["claudeAiOauth"]["subscriptionType"], "max");
        assert_eq!(blob["mcpOAuth"]["keep"], true);
    }

    #[test]
    fn scrub_rescues_newer_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let creds = dir.path().join(".credentials.json");
        std::fs::write(
            &creds,
            r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"dead","expiresAt":1000,"scopes":["user:inference"]}}"#,
        )
        .unwrap();
        std::fs::write(
            &auth,
            r#"{"anthropic":{"type":"oauth","access":"new","refresh":"live","expires":2000},"other":{"type":"api","key":"k"}}"#,
        )
        .unwrap();

        scrub_anthropic_auth(&auth, &creds).unwrap();

        let blob = read_json(&creds).unwrap();
        assert_eq!(blob["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(blob["claudeAiOauth"]["refreshToken"], "live");
        assert_eq!(blob["claudeAiOauth"]["expiresAt"], 2000);
        // Fields the rescue doesn't know about survive the rewrite.
        assert_eq!(blob["claudeAiOauth"]["scopes"][0], "user:inference");
        let auth_json = read_json(&auth).unwrap();
        assert!(auth_json.get("anthropic").is_none());
        assert_eq!(auth_json["other"]["key"], "k");
    }

    #[test]
    fn scrub_leaves_credentials_alone_for_older_or_keyless_entries() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let creds = dir.path().join(".credentials.json");
        let original =
            r#"{"claudeAiOauth":{"accessToken":"cur","refreshToken":"cur","expiresAt":5000}}"#;

        // An older rotation, then a plain API key: both drop the entry
        // without touching the credentials file.
        for entry in [
            r#"{"anthropic":{"type":"oauth","access":"a","refresh":"r","expires":1000}}"#,
            r#"{"anthropic":{"type":"api","key":"sk-x"}}"#,
        ] {
            std::fs::write(&creds, original).unwrap();
            std::fs::write(&auth, entry).unwrap();
            scrub_anthropic_auth(&auth, &creds).unwrap();
            assert_eq!(std::fs::read_to_string(&creds).unwrap(), original);
            let auth_json = read_json(&auth).unwrap();
            assert!(auth_json.get("anthropic").is_none());
        }
    }

    #[test]
    fn scrub_keeps_entry_when_rescue_fails() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let creds = dir.path().join(".credentials.json");
        std::fs::write(&creds, "not json").unwrap();
        let entry = r#"{"anthropic":{"type":"oauth","access":"a","refresh":"r","expires":9000}}"#;
        std::fs::write(&auth, entry).unwrap();

        scrub_anthropic_auth(&auth, &creds).unwrap();

        // The unparseable credentials file blocked the rescue, so the only
        // live pair stays parked in auth.json instead of being destroyed.
        assert_eq!(std::fs::read_to_string(&auth).unwrap(), entry);
        assert_eq!(std::fs::read_to_string(&creds).unwrap(), "not json");
    }

    #[test]
    fn reconcile_rescues_without_scrubbing() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let creds = dir.path().join(".credentials.json");
        std::fs::write(
            &creds,
            r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"dead","expiresAt":1000}}"#,
        )
        .unwrap();
        let entry =
            r#"{"anthropic":{"type":"oauth","access":"new","refresh":"live","expires":2000}}"#;
        std::fs::write(&auth, entry).unwrap();

        reconcile(&auth, &creds).unwrap();

        let blob = read_json(&creds).unwrap();
        assert_eq!(blob["claudeAiOauth"]["refreshToken"], "live");
        // The plugin owns the mirror while the runner is live.
        assert_eq!(std::fs::read_to_string(&auth).unwrap(), entry);

        // Missing auth.json is a no-op, not an error.
        std::fs::remove_file(&auth).unwrap();
        reconcile(&auth, &creds).unwrap();
    }

    #[test]
    fn refresh_digest_takes_the_last_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("claude-auth-debug.log");

        assert_eq!(refresh_digest(&log), Refresh::NoData);
        std::fs::write(&log, "").unwrap();
        assert_eq!(refresh_digest(&log), Refresh::NoData);

        std::fs::write(
            &log,
            [
                r#"{"event":"plugin_init","accountCount":1}"#,
                "not json at all",
                r#"{"event":"refresh_failed","kind":"transient","error":"HTTP 529"}"#,
                r#"{"event":"refresh_success","source":"oauth"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        assert_eq!(refresh_digest(&log), Refresh::Ok);

        // A terminal failure trails follow-up events; the worst verdict
        // since the last success must win, not the last one.
        std::fs::write(
            &log,
            [
                r#"{"event":"refresh_success"}"#,
                r#"{"event":"refresh_failed","kind":"terminal","oauthError":"invalid_grant"}"#,
                // A kindless CLI failure must not overwrite the verdict.
                r#"{"event":"refresh_failed","source":"cli","error":"spawn failed"}"#,
                r#"{"event":"refresh_exhausted","source":"file"}"#,
                r#"{"event":"credentials_unavailable"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        assert_eq!(
            refresh_digest(&log),
            Refresh::Terminal {
                reason: Some("invalid_grant".into())
            }
        );

        std::fs::write(&log, r#"{"event":"refresh_exhausted","source":"file"}"#).unwrap();
        assert_eq!(refresh_digest(&log), Refresh::Transient { reason: None });

        std::fs::write(&log, r#"{"event":"credentials_unavailable"}"#).unwrap();
        assert_eq!(refresh_digest(&log), Refresh::Unavailable);
    }

    #[test]
    fn health_reflects_the_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join(".credentials.json");
        let log = dir.path().join("log");

        let empty = health_at(&creds, &log);
        assert!(!empty.connected);
        assert_eq!(empty.expires_at, None);
        assert_eq!(empty.refresh, Refresh::NoData);

        std::fs::write(
            &creds,
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":123}}"#,
        )
        .unwrap();
        let connected = health_at(&creds, &log);
        assert!(connected.connected);
        assert_eq!(connected.expires_at, Some(123));
    }
}
