//! Heuristics over the opencode log. opencode exits 0 even when the turn died
//! on an error (unknown command, missing auth, bad model), so the log tail is
//! scanned for its error reports, task markers, and usage-limit notices.
//! opencode colors its output, so every scan strips ANSI escapes first.

/// Whether the tail contains an opencode error report: a line starting with
/// "Error:" or a `"name": "<Something>Error"` JSON field.
pub fn has_error_report(tail: &str) -> bool {
    strip_ansi(tail)
        .lines()
        .any(|line| line.starts_with("Error:") || has_error_name(line))
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

/// Whether the agent reported real forge changes: a `TASK COMPLETED` marker
/// line. Only this marker counts as completed, so a turn that skipped, forgot
/// the marker, or died halfway can't eat into the daily limit.
pub fn reported_completed(tail: &str) -> bool {
    strip_ansi(tail)
        .lines()
        .any(|line| line.trim_start().starts_with("TASK COMPLETED"))
}

/// The epoch at which an exhausted usage window reopens, parsed from a
/// `usage limit reached|<epoch>` notice. Epochs under nine digits are noise.
pub fn usage_limit_epoch(tail: &str) -> Option<u64> {
    let tail = strip_ansi(tail);
    tail.split("usage limit reached|").skip(1).find_map(|rest| {
        let digits = &rest[..rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len())];
        (digits.len() >= 9).then(|| digits.parse().ok())?
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
        assert!(has_error_report(
            "some output\nError: unknown command\nmore"
        ));
        assert!(!has_error_report("some output\n  Error: indented\nmore"));
        assert!(!has_error_report("an Error: mid-line mention"));
    }

    #[test]
    fn error_name_json() {
        assert!(has_error_report(r#"{"name": "AuthError", "data": {}}"#));
        assert!(!has_error_report(r#"{"name": "Error"}"#)); // no leading letters
        assert!(!has_error_report(r#"{"name": "AuthErrorFoo"}"#)); // not terminal
        assert!(!has_error_report(r#"{"name": "notanerror"}"#));
        assert!(!has_error_report(r#"{"label": "AuthError"}"#));
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

    #[test]
    fn markers_survive_ansi_styling() {
        assert!(reported_completed(
            "\x1b[0m \x1b[1mTASK COMPLETED\x1b[0m — opened PR #5\n"
        ));
        assert!(has_error_report(
            "\x1b[91m\x1b[1mError:\x1b[0m the user rejected permission\n"
        ));
        assert_eq!(
            usage_limit_epoch("\x1b[33musage limit reached|\x1b[1m1757000000\x1b[0m"),
            Some(1757000000)
        );
    }

    #[test]
    fn ansi_stripping() {
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi("\x1b[38;5;208morange\x1b[0m"), "orange");
        assert_eq!(strip_ansi("\x1b]0;title\x07left\x1b]2;t\x1b\\right"), "leftright");
        assert_eq!(strip_ansi("a\x1b(Bb\x1b[2Kc\x1b"), "abc");
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
    }
}
