//! A scripted source for the tests: returns the samples you gave it, in order.

use std::collections::VecDeque;

use super::{Source, SourceError};
use crate::model::Sample;

pub struct FakeSource {
    /// `Some(sample)` = good cycle; `None` = failed cycle.
    script: VecDeque<Option<Sample>>,
}

impl FakeSource {
    pub fn new(script: Vec<Option<Sample>>) -> FakeSource {
        FakeSource {
            script: VecDeque::from(script),
        }
    }

    pub fn remaining(&self) -> usize {
        self.script.len()
    }
}

impl Source for FakeSource {
    fn sample(&mut self) -> Result<Sample, SourceError> {
        match self.script.pop_front() {
            Some(Some(sample)) => Ok(sample),
            Some(None) => Err(SourceError::Unavailable(
                "scripted failed cycle".to_string(),
            )),
            None => Err(SourceError::Unavailable("script ran out".to_string())),
        }
    }
}
