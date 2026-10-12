//! What the tail of a turn's log says about how the turn went. opencode exits
//! 0 even when the turn died on an error (unknown command, missing auth, bad
//! model), so its exit status settles almost nothing and the log is the
//! witness: it is scanned for opencode's error reports, the agent's own task
//! markers, usage-limit notices, and proxy authentication failures.
//!
//! opencode colors its output, so the tail has to be read through ANSI
//! escapes. That stripping — and the slice of closing lines the markers are
//! looked for in — happens once, in [`Verdict::read`], and every answer below
//! comes off that one copy. Each answer used to strip the text for itself,
//! and the only caller wants all four: two strips of the closing lines and
//! two of the whole tail, to say four things about one piece of text.

/// What the turn's log says about how it went: the four things the log is the
/// only witness to, read together because they are read from one text.
pub struct Verdict {
    /// The agent reported real forge changes with a `TASK COMPLETED` marker;
    /// only these turns count toward the hourly limit.
    pub completed: bool,
    /// The session ended on an opencode error report rather than on the
    /// agent's own marker, which it does while still exiting 0.
    pub died: bool,
    /// The epoch an exhausted usage window reopens at, if the log named one.
    pub usage_limit: Option<u64>,
    /// The turn died on proxy authentication.
    pub auth_error: bool,
}

/// How much of the tail counts as the agent's last word. A `TASK COMPLETED`
/// left further back than this — by a turn that reported and then ran on for
/// pages — is not how the session ended.
const CLOSING_LINES: usize = 50;

impl Verdict {
    /// Read the log tail once and answer all four questions off it, rather
    /// than scanning it again wherever one of the answers is wanted.
    pub fn read(tail: &str) -> Verdict {
        let tail = strip_ansi(tail);
        // The markers and how the session ended are the agent's last word, so
        // they are looked for in the closing lines; the usage notice and the
        // auth failure are whatever went wrong mid-turn and can sit anywhere
        // in the tail.
        let closing = last_lines(&tail, CLOSING_LINES);
        Verdict {
            completed: reported_completed(closing),
            died: died_on_error(closing),
            usage_limit: usage_limit_epoch(&tail),
            auth_error: auth_error(&tail),
        }
    }
}

/// The last `count` lines of `text`, counted the way `tail` counts them: a
/// trailing newline ends the last line rather than opening an empty one.
pub fn last_lines(text: &str, count: usize) -> &str {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    match trimmed.rmatch_indices('\n').nth(count.saturating_sub(1)) {
        Some((at, _)) => &text[at + 1..],
        None => text,
    }
}

/// Whether the session ended on an opencode error report instead of on the
/// agent's own sign-off.
///
/// opencode prints a report for every tool call that fails, and a turn is
/// full of calls the agent recovers from: a path that isn't there, a CLI
/// invoked with a flag it doesn't take, a command that exits nonzero on
/// purpose. So an error report says nothing on its own about how the turn
/// went — what tells a session that died from one that carried on is whether
/// the agent reached its closing marker afterwards. Reports and markers are
/// therefore read in order and the last one wins: an error with a marker
/// after it was recovered from, an error with nothing after it is what the
/// turn died of.
fn died_on_error(closing: &str) -> bool {
    let mut died = false;
    for line in closing.lines() {
        if is_marker(line) {
            died = false;
        } else if is_error_report(line) {
            died = true;
        }
    }
    died
}

/// Whether one line is an opencode error report: a line starting with
/// "Error:" or carrying a `"name": "<Something>Error"` JSON field.
fn is_error_report(line: &str) -> bool {
    line.starts_with("Error:") || has_error_name(line)
}

fn has_error_name(line: &str) -> bool {
    line.match_indices("\"name\": \"").any(|(at, prefix)| {
        let rest = &line[at + prefix.len()..];
        let name: &str = &rest[..rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len())];
        rest[name.len()..].starts_with('"') && name.ends_with("Error") && name.len() > "Error".len()
    })
}

/// The two markers the sweep command has the agent end its final message
/// with. One of them is the last thing a session that reached a conclusion of
/// its own printed, whichever conclusion that was.
const COMPLETED: &str = "TASK COMPLETED";
const SKIPPED: &str = "TASK SKIPPED";

/// Whether the agent reported real forge changes: a `TASK COMPLETED` marker
/// line. Only this marker counts as completed, so a turn that skipped, forgot
/// the marker, or died halfway can't eat into the hourly limit.
fn reported_completed(closing: &str) -> bool {
    closing
        .lines()
        .any(|line| line.trim_start().starts_with(COMPLETED))
}

/// Whether one line is either marker — the agent signing off, having either
/// changed something or decided there was nothing to change.
fn is_marker(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with(COMPLETED) || line.starts_with(SKIPPED)
}

/// Whether the turn died on authentication somewhere between opencode and
/// Anthropic, as CLIProxyAPI reports it (bodies captured from 7.3.2 and
/// 8.0.4): a rejected client key answers `{"error":"Invalid API key"}`, a
/// proxy whose login can no longer refresh answers 503 `auth_unavailable: no
/// auth available (providers=claude, ...)`, a 7.x proxy holding no Claude
/// credential knows no claude models and answers `unknown provider for model
/// ...`, and a dead login upstream passes Anthropic's own
/// `authentication_error` through.
fn auth_error(tail: &str) -> bool {
    [
        "authentication_error",
        "Invalid API key",
        "unknown provider for model",
        "auth_unavailable",
        "no auth available",
    ]
    .iter()
    .any(|needle| tail.contains(needle))
}

/// The epoch at which an exhausted usage window reopens, parsed from a
/// `usage limit reached|<epoch>` notice.
///
/// The runner holds turns until whatever this says, and what it reads is the
/// agent's own output — which carries, verbatim, whatever the repository
/// under the turn printed into it. So the digits are only taken as an epoch
/// when they are the length of one: nine or ten. Fewer is the noise the lower
/// bound always rejected; more is a number that reached the log some other
/// way, and eleven digits already name a date past the year 5000.
fn usage_limit_epoch(tail: &str) -> Option<u64> {
    tail.split("usage limit reached|").skip(1).find_map(|rest| {
        let digits = &rest[..rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len())];
        (9..=10)
            .contains(&digits.len())
            .then(|| digits.parse().ok())
            .flatten()
    })
}

/// The text with ANSI escape sequences removed: CSI (colors, cursor moves),
/// OSC (titles), and the two- and three-byte escapes.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameter and intermediate bytes, then one final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs to a BEL or an `ESC \` terminator.
            Some(']') => loop {
                match chars.next() {
                    Some('\x1b') => {
                        chars.next();
                        break;
                    }
                    Some('\x07') | None => break,
                    Some(_) => {}
                }
            },
            // Charset designation carries one more byte.
            Some('(' | ')') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_line_at_start_only() {
        assert!(died_on_error("some output\nError: unknown command\nmore"));
        assert!(!died_on_error("some output\n  Error: indented\nmore"));
        assert!(!died_on_error("an Error: mid-line mention"));
    }

    #[test]
    fn error_name_json() {
        assert!(died_on_error(r#"{"name": "AuthError", "data": {}}"#));
        assert!(!died_on_error(r#"{"name": "Error"}"#)); // no leading letters
        assert!(!died_on_error(r#"{"name": "AuthErrorFoo"}"#)); // not terminal
        assert!(!died_on_error(r#"{"name": "notanerror"}"#));
        assert!(!died_on_error(r#"{"label": "AuthError"}"#));
    }

    /// Errors and markers are weighed by position, because opencode reports a
    /// tool call that failed exactly the way it reports a session that died.
    /// A turn is full of the former — a path that isn't there, a flag a CLI
    /// doesn't take — and the agent carries on past them to its own sign-off,
    /// which is what settles how the turn went.
    #[test]
    fn the_last_of_the_error_and_the_marker_wins() {
        let recovered = "Error: NotFound: FileSystem.access (/w/gitea/me/rep)\n\
                         I'll try that another way.\n";
        assert!(died_on_error(recovered));
        assert!(!died_on_error(&format!("{recovered}TASK COMPLETED\n")));
        assert!(!died_on_error(&format!("{recovered}TASK SKIPPED\n")));
        // An error after the sign-off is the session dying on its way out,
        // which is a failure however the agent meant to end.
        assert!(died_on_error(&format!(
            "{recovered}TASK COMPLETED\nError: {{\"type\":\"api_error\"}}\n"
        )));
        // Several recovered calls in a row still end up recovered from.
        assert!(!died_on_error(&format!(
            "{recovered}{recovered}TASK SKIPPED\n"
        )));
        // The markers are read through opencode's styling, like everything
        // else here, and are allowed the indentation the renderer gives them.
        assert!(
            !Verdict::read(&format!(
                "{recovered}\x1b[0m  \x1b[1mTASK COMPLETED\x1b[0m — opened PR #5\n"
            ))
            .died
        );
    }

    #[test]
    fn completed_marker() {
        assert!(reported_completed("all done\nTASK COMPLETED\n"));
        assert!(reported_completed("TASK COMPLETED — opened PR #5\nmore"));
        assert!(!reported_completed(
            "the marker is TASK COMPLETED on its own line"
        ));
        // Skipping, or forgetting the marker entirely, is not completion.
        assert!(!reported_completed("TASK SKIPPED\n"));
        assert!(!reported_completed("opened a PR and wandered off\n"));
    }

    /// Every answer is read through opencode's styling, off the one stripped
    /// copy `Verdict::read` makes: an escape sequence between the words of a
    /// marker, or in the middle of a usage-limit epoch, must not hide it.
    #[test]
    fn markers_survive_ansi_styling() {
        assert!(Verdict::read("\x1b[0m \x1b[1mTASK COMPLETED\x1b[0m — opened PR #5\n").completed);
        assert!(Verdict::read("\x1b[91m\x1b[1mError:\x1b[0m the user rejected permission\n").died);
        assert_eq!(
            Verdict::read("\x1b[33musage limit reached|\x1b[1m1757000000\x1b[0m").usage_limit,
            Some(1_757_000_000)
        );
        assert!(Verdict::read("\x1b[91mauthentication_error\x1b[0m").auth_error);
    }

    /// The log is read once, and where each thing is looked for matters. The
    /// agent's markers are its last word, so a `TASK COMPLETED` left far
    /// above the end — by a turn that reported and then ran on for pages — is
    /// not its answer. A usage notice or an auth failure is whatever went
    /// wrong mid-turn, so those are looked for in the whole tail.
    #[test]
    fn markers_are_read_at_the_end_and_failures_anywhere() {
        let filler = "filler\n".repeat(CLOSING_LINES + 10);

        let verdict = Verdict::read(&format!(
            "TASK COMPLETED\nusage limit reached|1757000000\nauthentication_error\n{filler}"
        ));
        assert_eq!(verdict.usage_limit, Some(1_757_000_000));
        assert!(verdict.auth_error);
        assert!(!verdict.completed, "a buried marker is not the answer");

        let verdict = Verdict::read(&format!("{filler}TASK COMPLETED\n"));
        assert!(verdict.completed);
        assert!(!verdict.died);
        assert_eq!(verdict.usage_limit, None);
        assert!(!verdict.auth_error);

        let verdict = Verdict::read(&format!("{filler}Error: unknown command\n"));
        assert!(verdict.died);
        assert!(!verdict.completed);
    }

    #[test]
    fn last_lines_counts_like_tail() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a\nb\nc\n", 1), "c\n");
        assert_eq!(last_lines("a\nb", 50), "a\nb");
        assert_eq!(last_lines("", 50), "");
    }

    #[test]
    fn ansi_stripping() {
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi("\x1b[38;5;208morange\x1b[0m"), "orange");
        assert_eq!(
            strip_ansi("\x1b]0;title\x07left\x1b]2;t\x1b\\right"),
            "leftright"
        );
        assert_eq!(strip_ansi("a\x1b(Bb\x1b[2Kc\x1b"), "abc");
    }

    #[test]
    fn auth_failures() {
        // A dead login upstream, passed through the proxy verbatim.
        assert!(auth_error(
            r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid bearer token"}}"#
        ));
        // The proxy's own rejection of a wrong client key.
        assert!(auth_error(r#"{"error":"Invalid API key"}"#));
        // A proxy with no Claude credential knows no claude models.
        assert!(auth_error(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"unknown provider for model claude-sonnet-5"}}"#
        ));
        // A proxy whose stored login can no longer refresh (8.0.4 answers 503
        // with the upstream refresh failure redacted).
        assert!(auth_error(
            r#"{"type":"error","error":{"type":"api_error","message":"auth_unavailable: no auth available (providers=claude, model=claude-sonnet-4-6; last upstream error: token: [REDACTED]); check Claude auth/key session and cooldown state via /v0/management/auth-files"}}"#
        ));
        assert!(!auth_error("all quiet"));
        assert!(!auth_error("Error: something else entirely"));
    }

    #[test]
    fn usage_limit() {
        assert_eq!(
            usage_limit_epoch("blah usage limit reached|1757000000 blah"),
            Some(1757000000)
        );
        assert_eq!(usage_limit_epoch("usage limit reached|123"), None); // too short
        assert_eq!(usage_limit_epoch("usage limit reached soon"), None);
        assert_eq!(usage_limit_epoch("all quiet"), None);

        // Ten digits is still a date a usage window could carry; the notice
        // is scanned for in the agent's output, so anything longer is a
        // number from the repository under the turn rather than an epoch.
        // Taken at face value these park the runner for centuries, and the
        // widest of them overflows the Instant the wait is built from.
        assert_eq!(
            usage_limit_epoch("usage limit reached|9999999999"),
            Some(9_999_999_999)
        );
        assert_eq!(usage_limit_epoch("usage limit reached|99999999999"), None);
        assert_eq!(
            usage_limit_epoch("usage limit reached|18446744073709551615"),
            None
        );
        // One good notice still wins over a decoy alongside it.
        assert_eq!(
            usage_limit_epoch("usage limit reached|99999999999\nusage limit reached|1757000000"),
            Some(1757000000)
        );
    }
}
