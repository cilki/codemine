//! The one way the runner reads a child process's output.
//!
//! Most of the tools the runner drives are asked a question rather than told
//! to do work: `tea` and `gh`/`glab` list what the token can reach, `git`
//! says what a clone is checked out at, `opencode` lists the models it can
//! resolve, `codegraph` says whether it is installed at all. Each of those
//! carried its own copy of the same three steps — close stdin, run, turn a
//! nonzero exit into an error — and the copies disagreed about the one part
//! that matters, which is what the error says. `git` reported a bare exit
//! status and threw away everything git had said about it, `codegraph`
//! discarded both streams, and two of the forge calls appended an empty
//! stderr as a dangling colon.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Run `command` to completion and hand back its standard output, failing
/// with whatever it complained about.
///
/// The error carries the program's own stderr, because that is the only
/// place the reason lives: these errors reach the web UI as what blocks
/// turns from running, and "tea exited with exit status: 1" says nothing
/// about the mistyped token behind it. The status alone is all there is to
/// report when the program itself said nothing.
///
/// Stdin is closed rather than inherited: every one of these runs
/// unattended, so a tool that decides to prompt would otherwise read the
/// runner's own stdin and wait on it forever — holding up the draw, the
/// turn, or the whole main loop, depending on who asked.
pub fn output(command: &mut Command) -> Result<Vec<u8>> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    match String::from_utf8_lossy(&output.stderr).trim() {
        "" => bail!("{program} exited with {}", output.status),
        said => bail!("{program} exited with {}: {said}", output.status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Result<Vec<u8>> {
        output(Command::new("sh").args(["-c", script]))
    }

    #[test]
    fn stdout_comes_back_on_success() {
        assert_eq!(shell("echo hello").unwrap(), b"hello\n");
    }

    /// What the program complained about is the whole value of the error:
    /// every caller's failure is reported to a human — in the log, or on the
    /// settings page as the reason turns can't run — and the exit status on
    /// its own never says which token, path, or flag was wrong.
    #[test]
    fn a_failure_carries_what_the_program_said() {
        let err = shell("echo 'the token is bad' >&2; exit 3").unwrap_err();
        let said = format!("{err:#}");
        assert!(said.contains("the token is bad"), "{said}");
        assert!(said.contains("sh"), "{said}");
        assert!(said.contains('3'), "{said}");
    }

    /// A program that failed silently has nothing to append, so the message
    /// ends at the status rather than trailing a colon with nothing after it.
    #[test]
    fn a_silent_failure_is_not_reported_with_a_dangling_colon() {
        let err = shell("exit 4").unwrap_err();
        let said = format!("{err:#}");
        assert!(said.ends_with('4'), "{said}");
    }

    /// A tool that isn't installed is told apart from one that ran and
    /// refused: `codegraph` and `rtk` are optional, and the runner decides
    /// what to do about them from this.
    #[test]
    fn a_missing_program_is_an_error_naming_it() {
        let err = output(&mut Command::new("codemine-no-such-program")).unwrap_err();
        let said = format!("{err:#}");
        assert!(
            said.contains("failed to run codemine-no-such-program"),
            "{said}"
        );
    }

    /// Nothing here is attended, so a child that reads stdin has to see it
    /// closed: inherited, `cat` would sit on the runner's own stdin and the
    /// caller would never come back.
    #[test]
    fn a_child_that_reads_stdin_sees_it_closed() {
        assert!(shell("cat").unwrap().is_empty());
    }
}
