//! The one way the runner makes an HTTP request of its own. Both callers
//! (the Gitea API, which `tea` has no generic subcommand for, and
//! CLIProxyAPI's management API) send a secret in a header, so the request is
//! built with a typed client rather than assembled as text for a child
//! process: a `HeaderValue` cannot be anything but a header value, and a
//! `Url` cannot be read as an option.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use reqwest::header::HeaderValue;

/// GET `url`, optionally with one header, and return the HTTP status and the
/// body. `Err` means no HTTP conversation happened at all — an error response
/// is a status, not an error, so callers can tell "the forge said 404" from
/// "the forge never answered".
pub fn get(url: &str, header: Option<(&str, &str)>, timeout_secs: u64) -> Result<(u16, String)> {
    let mut request = client()?
        .get(url)
        .timeout(Duration::from_secs(timeout_secs));
    if let Some((name, value)) = header {
        request = request.header(name, secret(name, value)?);
    }
    let response = request
        .send()
        .with_context(|| format!("failed to GET {url}"))?;
    let status = response.status().as_u16();
    // Read as bytes rather than as text: both bodies are JSON, which is
    // UTF-8 by definition, so there is no charset to negotiate and a body
    // that isn't JSON at all is the caller's error to report.
    let body = response
        .bytes()
        .with_context(|| format!("failed to read the answer to {url}"))?;
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// The shared client. Built once: a blocking client owns a connection pool
/// and the thread the runtime lives on, and the proxy probe runs on the
/// event stream's tick.
fn client() -> Result<&'static Client> {
    static CLIENT: OnceLock<std::result::Result<Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            // reqwest is built without a rustls provider (see `Cargo.toml`),
            // so one has to be in place before the first handshake. An `Err`
            // here only means something else installed one first, which is
            // just as good.
            let _ = rustls::crypto::ring::default_provider().install_default();
            Client::builder()
                // No redirect following, which is also curl's default and
                // what the previous implementation therefore did. Both
                // callers address an API they configured; a 3xx from one is
                // a misconfiguration to report, not a hop to take with the
                // secret header still attached.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                // reqwest's own message is only "builder error"; the reason
                // (no CA certificates, a malformed proxy variable) is in the
                // source chain, which `{:#}` spells out.
                .map_err(|err| format!("{:#}", anyhow::Error::from(err)))
        })
        .as_ref()
        .map_err(|err| anyhow!("failed to build the http client: {err}"))
}

/// One header value holding a secret the runner was handed — a forge token,
/// the proxy's management key.
///
/// It is untrusted text: it arrives over the settings API as arbitrary JSON,
/// or straight out of a hand-edited `config.json`, which `SettingsStore::load`
/// deserializes without going through the UI's validation at all. `HeaderValue`
/// is what makes that safe to send: it accepts only visible ASCII plus tab, so
/// a newline or a NUL is refused here instead of becoming part of the request.
fn secret(name: &str, value: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(value)
        .with_context(|| format!("the {name} value is not a usable header value"))?;
    // Keeps the secret out of `Debug` output and out of any header the client
    // would otherwise carry across a redirect.
    value.set_sensitive(true);
    Ok(value)
}
