//! How an alert reaches you (docs/design.md §8.1).

use std::time::Duration;

use crate::command::{CommandError, run_with_timeout};

/// What every notification channel knows how to do.
pub trait Notifier {
    fn notify(&mut self, title: &str, body: &str) -> Result<(), CommandError>;
}

/// macOS banner via `osascript`. It needs the manual setup from §8.1
/// (allow Script Editor and pick the Persistent style).
pub struct OsascriptNotifier;

impl Notifier for OsascriptNotifier {
    fn notify(&mut self, title: &str, body: &str) -> Result<(), CommandError> {
        // The text travels ONLY as arguments (argv). If it were pasted inside the
        // script, a process name with quotes could inject AppleScript.
        // The "--" is mandatory: without it, a title starting with "-" is taken
        // as an osascript option.
        run_with_timeout(
            "/usr/bin/osascript",
            &[
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                "--",
                title,
                body,
            ],
            Duration::from_secs(5),
        )?;
        Ok(())
    }
}

/// For the tests: stores what it was asked to notify.
#[derive(Debug, Default)]
pub struct FakeNotifier {
    pub sent: Vec<(String, String)>,
}

impl Notifier for FakeNotifier {
    fn notify(&mut self, title: &str, body: &str) -> Result<(), CommandError> {
        self.sent.push((title.to_string(), body.to_string()));
        Ok(())
    }
}
