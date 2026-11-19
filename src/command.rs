//! Running external programs (`top`, `vm_stat`, `sysctl`, `osascript`) with a
//! deadline (docs/design.md §9).

use std::fmt;
use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum CommandError {
    /// The program did not start (missing, no permissions…) or could not be waited on.
    Spawn { program: String, error: io::Error },
    /// The program ran but exited with a code other than 0.
    Failed {
        program: String,
        status: ExitStatus,
        stderr: String,
    },
    /// It did not finish within the deadline: it was killed.
    TimedOut { program: String, after: Duration },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::Spawn { program, error } => {
                write!(f, "could not run {program}: {error}")
            }
            CommandError::Failed {
                program,
                status,
                stderr,
            } => write!(f, "{program} failed ({status}): {stderr}"),
            CommandError::TimedOut { program, after } => {
                write!(f, "{program} did not finish within {} s", after.as_secs())
            }
        }
    }
}

impl std::error::Error for CommandError {}

/// Runs `program` with `args` and returns what it wrote to stdout. If it does
/// not finish within `timeout`, kills it and returns `TimedOut`.
///
/// `program` always has an absolute path: under launchd the PATH is minimal.
pub fn run_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, CommandError> {
    let spawn_error = |error| CommandError::Spawn {
        program: program.to_string(),
        error,
    };

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;

    let deadline = Instant::now() + timeout;
    // try_wait does not block: it says whether it already finished. Checked every 20 ms.
    while child.try_wait().map_err(spawn_error)?.is_none() {
        if Instant::now() >= deadline {
            // kill() and THEN wait(): without the wait a zombie process would be
            // left behind, and in an agent that runs 24/7 they would pile up.
            let _ = child.kill();
            let _ = child.wait();
            return Err(CommandError::TimedOut {
                program: program.to_string(),
                after: timeout,
            });
        }
        thread::sleep(Duration::from_millis(20));
    }

    // It already finished, so this does not block. Reading the output only at
    // the end is safe because it is < 5 KB, well below what the pipe can hold.
    let output = child.wait_with_output().map_err(spawn_error)?;

    if !output.status.success() {
        return Err(CommandError::Failed {
            program: program.to_string(),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    // `from_utf8_lossy` and not `from_utf8`: if top cuts a name in the middle of a
    // multibyte character, that name comes out with a "�", but the sample survives.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLENTY: Duration = Duration::from_secs(5);

    #[test]
    fn returns_stdout() {
        assert_eq!(
            run_with_timeout("/bin/echo", &["hello"], PLENTY).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn nonzero_exit_is_an_error() {
        let error = run_with_timeout("/usr/bin/false", &[], PLENTY).unwrap_err();
        assert!(matches!(error, CommandError::Failed { .. }));
    }

    #[test]
    fn missing_program_is_an_error() {
        let error = run_with_timeout("/does/not/exist", &[], PLENTY).unwrap_err();
        assert!(matches!(error, CommandError::Spawn { .. }));
    }

    #[test]
    fn a_slow_program_is_killed_at_the_deadline() {
        let started = Instant::now();
        let error =
            run_with_timeout("/bin/sleep", &["10"], Duration::from_millis(200)).unwrap_err();
        assert!(matches!(error, CommandError::TimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
