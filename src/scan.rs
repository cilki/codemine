//! Heuristics over the opencode log. opencode exits 0 even when the turn died
//! on an error (unknown command, missing auth, bad model), so the log tail is
//! scanned for its error reports and usage-limit notices.

/// Whether the tail contains an opencode error report: a line starting with
/// "Error:" or a `"name": "<Something>Error"` JSON field.
pub fn has_error_report(tail: &str) -> bool {
    tail.lines()
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

/// Whether the agent reported the turn as skipped: a `TASK SKIPPED` marker
/// line. Anything else counts as completed, so a turn that forgets the marker
/// can't dodge the daily limit.
pub fn reported_skipped(tail: &str) -> bool {
    tail.lines()
        .any(|line| line.trim_start().starts_with("TASK SKIPPED"))
}

/// The epoch at which an exhausted usage window reopens, parsed from a
/// `usage limit reached|<epoch>` notice. Epochs under nine digits are noise.
pub fn usage_limit_epoch(tail: &str) -> Option<u64> {
    tail.split("usage limit reached|").skip(1).find_map(|rest| {
        let digits = &rest[..rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len())];
        (digits.len() >= 9).then(|| digits.parse().ok())?
    })
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
    fn skip_marker() {
        assert!(reported_skipped("nothing to do here\nTASK SKIPPED\n"));
        assert!(reported_skipped("TASK SKIPPED — no open PRs\nmore output"));
        assert!(!reported_skipped("the marker is TASK SKIPPED on its own line"));
        assert!(!reported_skipped("TASK COMPLETED\n"));
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
