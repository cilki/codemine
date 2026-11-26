//! The turn receipt: what a completed turn cost, measured by the runner and
//! left as a comment on each PR and issue the turn opened.
//!
//! The agent can't see its own clock or token count, so what it wrote about
//! them in a description was an estimate at best. The runner measures both
//! exactly (`Turn::elapsed`, and the token sum `usage` reads off opencode's
//! storage), and the forge is where the person deciding on the change is
//! looking — the web UI's turn list is gone with every restart.

use crate::deliveries::format_rfc3339;
use crate::status::{Savings, TokenUsage};

/// Everything a receipt reports: which turn, which model, and what it spent.
pub struct Receipt<'a> {
    pub task: &'a str,
    pub repo: &'a str,
    pub model: &'a str,
    /// Epoch seconds.
    pub started: u64,
    pub duration_secs: u64,
    /// None when opencode's session storage couldn't be read.
    pub tokens: Option<&'a TokenUsage>,
    pub rtk: Option<&'a Savings>,
}

impl Receipt<'_> {
    /// The comment body, as Markdown every forge renders the same way. Counts
    /// only — the model, the slug and the figures — never any of the log, so
    /// there is nothing in it to redact.
    pub fn render(&self) -> String {
        let started = format_rfc3339(self.started);
        // `2026-11-25T14:02:09Z` reads better as `2026-11-25 14:02 UTC`.
        let started = format!("{} {} UTC", &started[..10], &started[11..16]);
        let mut lines = vec![
            format!("**codemine turn receipt** — {} on {}", self.task, self.repo),
            String::new(),
            format!("- model: `{}`", self.model),
            format!("- ran {}, started {started}", duration(self.duration_secs)),
        ];
        lines.push(match self.tokens {
            Some(t) => format!(
                "- tokens: {} (input {} · output {} · reasoning {} · cache read {} · cache write {})",
                thousands(t.input + t.output + t.reasoning + t.cache_read + t.cache_write),
                thousands(t.input),
                thousands(t.output),
                thousands(t.reasoning),
                thousands(t.cache_read),
                thousands(t.cache_write),
            ),
            None => "- tokens: not recorded (opencode's session storage couldn't be read)".into(),
        });
        if let Some(rtk) = self.rtk.filter(|rtk| rtk.commands > 0) {
            lines.push(format!(
                "- rtk saved ~{} tokens over {} {}",
                thousands(rtk.input.saturating_sub(rtk.output)),
                rtk.commands,
                if rtk.commands == 1 {
                    "command"
                } else {
                    "commands"
                }
            ));
        }
        lines.push(String::new());
        lines.push("<sub>Measured by the runner, not reported by the agent.</sub>".into());
        lines.join("\n")
    }
}

/// A count with thousands separators: `1284310` as `1,284,310`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The two largest units that carry information, the way the web UI's
/// `fmtDuration` spells a turn's length: `23m 41s`, `2h 5m`, `1d 3h`.
fn duration(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        secs % 86400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    match () {
        _ if d > 0 => format!("{d}d {h}h"),
        _ if h > 0 => format!("{h}h {m}m"),
        _ if m > 0 => format!("{m}m {s}s"),
        _ => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(41220), "41,220");
        assert_eq!(thousands(1_284_310), "1,284,310");
    }

    #[test]
    fn duration_keeps_the_two_largest_units() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(23 * 60 + 41), "23m 41s");
        assert_eq!(duration(2 * 3600 + 5 * 60 + 9), "2h 5m");
        assert_eq!(duration(86400 + 3 * 3600 + 59), "1d 3h");
    }

    #[test]
    fn a_receipt_names_the_turn_and_everything_it_spent() {
        let tokens = TokenUsage {
            input: 41_220,
            output: 18_904,
            reasoning: 6_112,
            cache_read: 1_201_880,
            cache_write: 16_194,
        };
        let rtk = Savings {
            commands: 37,
            input: 250_000,
            output: 38_000,
        };
        let receipt = Receipt {
            task: "simplify",
            repo: "cilki/codemine",
            model: "anthropic/claude-opus-5-5",
            // 2026-11-25T20:02:09Z
            started: 1_795_636_929,
            duration_secs: 23 * 60 + 41,
            tokens: Some(&tokens),
            rtk: Some(&rtk),
        };
        assert_eq!(
            receipt.render(),
            "**codemine turn receipt** — simplify on cilki/codemine\n\
             \n\
             - model: `anthropic/claude-opus-5-5`\n\
             - ran 23m 41s, started 2026-11-25 20:02 UTC\n\
             - tokens: 1,284,310 (input 41,220 · output 18,904 · reasoning 6,112 · cache read 1,201,880 · cache write 16,194)\n\
             - rtk saved ~212,000 tokens over 37 commands\n\
             \n\
             <sub>Measured by the runner, not reported by the agent.</sub>"
        );
    }

    /// A figure the runner couldn't read is said to be missing rather than
    /// shown as zero, and rtk is left out when it recorded nothing.
    #[test]
    fn a_receipt_says_what_it_could_not_measure() {
        let idle = Savings::default();
        for rtk in [None, Some(&idle)] {
            let body = Receipt {
                task: "docs",
                repo: "me/repo",
                model: "m",
                started: 0,
                duration_secs: 5,
                tokens: None,
                rtk,
            }
            .render();
            assert!(body.contains("- tokens: not recorded"), "{body}");
            assert!(!body.contains("rtk"), "{body}");
            assert!(body.contains("started 1970-01-01 00:00 UTC"), "{body}");
        }
    }
}
