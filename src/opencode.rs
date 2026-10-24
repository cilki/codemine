//! Which opencode the runner is driving, and the flags that version takes.
//!
//! The 2.x CLI was rewritten, and the runner has to speak to both sides of
//! that: a host's opencode comes from whatever nixpkgs it is built against,
//! so a runner built for 2.x can land next to a 1.x opencode (and did — every
//! turn exited 1 on the usage text for two days, because 1.x rejects the two
//! flags below). The version is read off `opencode --version` and settles
//! every flag that differs, so there is one place the difference lives.
//!
//! An opencode that can't say its version is an incomplete install: the
//! runner can't know which flags would work, and guessing wrong either fails
//! every turn or — on 2.x without `--standalone` — runs the agent outside the
//! sandbox. So no turn runs until it can.

use std::process::Command;
use std::sync::OnceLock;

use tracing::info;

/// The opencode on `PATH`, as `opencode --version` reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// The version as printed, for the log and error messages.
    pub text: String,
    major: u64,
}

impl Version {
    /// Whether this is the rewritten 2.x CLI.
    fn v2(&self) -> bool {
        self.major >= 2
    }

    /// The flags `opencode run` takes, ahead of `--model` and the prompt.
    ///
    /// 2.x hands a session to a shared background service unless told
    /// otherwise, and that service's tools run outside the Landlock ruleset
    /// and see none of the forge auth or git identity the turn sets up, so
    /// `--standalone` asks for a private server started inside the sandbox.
    /// `--auto` backs up `OPENCODE_PERMISSION`: approve whatever it doesn't
    /// explicitly deny, should this opencode ignore the variable.
    ///
    /// 1.x takes neither — 1.15 doesn't know `--auto` and nothing before 2.0
    /// knows `--standalone`, and either one fails the run on the usage text.
    /// It needs neither, too: its `run` always started a private server, and
    /// honors `OPENCODE_PERMISSION` on its own.
    pub fn run_flags(&self) -> &'static [&'static str] {
        match self.v2() {
            true => &["--standalone", "--auto"],
            false => &[],
        }
    }

    /// The flags `opencode models` takes: on 2.x, answer from a private
    /// server rather than starting or leaning on the shared background
    /// service. 1.x always answered on its own, and rejects the flag.
    pub fn models_flags(&self) -> &'static [&'static str] {
        match self.v2() {
            true => &["--standalone"],
            false => &[],
        }
    }
}

/// The opencode on `PATH`, or why the install is incomplete.
///
/// Only an answer is cached: a failure is asked again next time, so an
/// install fixed in place (a container whose `PATH` was wrong, say) is picked
/// up without a restart. An answer stands for as long as the process runs,
/// since replacing opencode under a NixOS service restarts the service too.
pub fn detect() -> Result<Version, String> {
    static DETECTED: OnceLock<Version> = OnceLock::new();
    if let Some(version) = DETECTED.get() {
        return Ok(version.clone());
    }
    let stdout = crate::child::output(Command::new("opencode").arg("--version"))
        .map_err(|err| format!("{err:#}"))?;
    let stdout = String::from_utf8_lossy(&stdout);
    let version = parse(&stdout)
        .ok_or_else(|| format!("opencode --version printed no version: {:?}", stdout.trim()))?;
    // Two callers can race to the first answer (the main loop and the model
    // listing); both asked the same opencode, so whichever lands is right,
    // and only that one is logged.
    if DETECTED.set(version.clone()).is_ok() {
        info!(
            "opencode {}: speaking its {} command line",
            version.text,
            if version.v2() { "2.x" } else { "1.x" }
        );
    }
    Ok(version)
}

/// The version in `opencode --version`'s output: the first word shaped like
/// `MAJOR.MINOR[...]`, with or without a leading `v`. Read as a word rather
/// than as the whole output, so startup chatter (a migration notice, a
/// plugin warning) or a name in front doesn't hide it.
fn parse(stdout: &str) -> Option<Version> {
    stdout.split_whitespace().find_map(|word| {
        let text = word.strip_prefix('v').unwrap_or(word);
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        // At least a minor, so a lone number in the chatter isn't taken for
        // a version.
        let minor = parts.next()?;
        if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Version {
            text: text.to_owned(),
            major,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn major(stdout: &str) -> Option<u64> {
        parse(stdout).map(|version| version.major)
    }

    /// What the versions this has run against actually print.
    #[test]
    fn reads_the_real_outputs() {
        assert_eq!(major("1.15.10\n"), Some(1));
        assert_eq!(major("1.19.0\n"), Some(1));
        assert_eq!(major("2.1.1\n"), Some(2));
        assert_eq!(parse("2.1.1").unwrap().text, "2.1.1");
    }

    #[test]
    fn looks_past_chatter_and_names() {
        assert_eq!(
            major("Performing one time database migration...\n2.1.1\n"),
            Some(2)
        );
        assert_eq!(major("opencode v2.1.1\n"), Some(2));
        assert_eq!(parse("opencode v2.1.1").unwrap().text, "2.1.1");
        assert_eq!(major("2.2.0-beta.3\n"), Some(2));
    }

    #[test]
    fn no_version_is_no_answer() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("\n"), None);
        assert_eq!(parse("command not found"), None);
        // A lone number, or a dotted word that isn't numeric.
        assert_eq!(parse("exit 127"), None);
        assert_eq!(parse("1."), None);
        assert_eq!(parse("v.1"), None);
        assert_eq!(parse("a.b.c"), None);
    }

    #[test]
    fn flags_follow_the_major_version() {
        let v1 = parse("1.15.10").unwrap();
        assert!(v1.run_flags().is_empty());
        assert!(v1.models_flags().is_empty());

        let v2 = parse("2.1.1").unwrap();
        assert_eq!(v2.run_flags(), ["--standalone", "--auto"]);
        assert_eq!(v2.models_flags(), ["--standalone"]);
        // Anything newer keeps the 2.x command line until it says otherwise.
        assert_eq!(parse("3.0.0").unwrap().run_flags(), v2.run_flags());
    }
}
