//! Sample sources: something that, every cycle, produces a `Sample`.
//!
//! - `ffi`: asks the kernel directly with `libc` (the one `watch` uses).
//! - `text`: runs `top`, `vm_stat` and `sysctl` and parses their output.
//! - `fake`: scripted samples for the tests.

pub mod fake;
pub mod ffi;
pub mod text;

use std::fmt;

use crate::command::CommandError;
use crate::model::Sample;
use text::ParseError;

/// What every source knows how to do. `watch` only knows this trait, so
/// switching sources does not touch the core (detect, history, alerts).
pub trait Source {
    /// Reads a sample. An `Err` is a failed cycle: there is no `Sample` and nothing changes.
    fn sample(&mut self) -> Result<Sample, SourceError>;
}

/// Everything that can fail while reading a sample.
#[derive(Debug)]
pub enum SourceError {
    /// A command could not be run (or did not finish in time).
    Command(CommandError),
    /// The command ran, but its output cannot be understood.
    Parse(ParseError),
    /// Any other failure, with its description.
    Unavailable(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Command(error) => write!(f, "{error}"),
            SourceError::Parse(error) => write!(f, "unrecognized output: {error}"),
            SourceError::Unavailable(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for SourceError {}

// These two conversions are what allow using `?` in `text::read_top`:
// `?` converts the error with `From` before returning it.
impl From<CommandError> for SourceError {
    fn from(error: CommandError) -> Self {
        SourceError::Command(error)
    }
}

impl From<ParseError> for SourceError {
    fn from(error: ParseError) -> Self {
        SourceError::Parse(error)
    }
}
