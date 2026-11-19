//! The bounded window of samples for each process (docs/design.md §6.1).
//!
//! `History` keeps, for each `ProcessKey`, the samples from the last
//! `window_secs`. It uses the key only as a key: it never does `match` on it.

use std::collections::{HashMap, VecDeque};

use crate::model::{Bytes, ProcessKey, Sample, Timestamp};

/// One sample of a process: when and how much.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub taken_at: Timestamp,
    pub footprint: Bytes,
}

/// A process's samples within the window, from oldest to newest.
///
/// `VecDeque` is a double-ended queue: pushing at the back and popping from the
/// front cost the same, which is exactly what a sliding window does.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Series {
    points: VecDeque<Point>,
}

impl Series {
    pub fn push(&mut self, point: Point) {
        self.points.push_back(point);
    }

    pub fn points(&self) -> &VecDeque<Point> {
        &self.points
    }

    pub fn oldest(&self) -> Option<Point> {
        self.points.front().copied()
    }

    pub fn newest(&self) -> Option<Point> {
        self.points.back().copied()
    }

    /// Removes the samples that fell outside the window.
    fn drop_older_than(&mut self, now: Timestamp, window_secs: u64) {
        // The condition to stay is `taken_at ≥ now − window`, written as an
        // addition (`taken_at + window ≥ now`) to avoid subtracting u64.
        while let Some(oldest) = self.points.front() {
            if oldest.taken_at.0.saturating_add(window_secs) >= now.0 {
                break;
            }
            self.points.pop_front();
        }
    }
}

#[derive(Debug)]
pub struct History {
    window_secs: u64,
    series: HashMap<ProcessKey, Series>,
}

impl History {
    pub fn new(window_secs: u64) -> History {
        History {
            window_secs,
            series: HashMap::new(),
        }
    }

    /// Adds a sample: appends a point to each process's series, trims every
    /// window and deletes the series left empty (processes that have not shown
    /// up for more than `window_secs`).
    pub fn record(&mut self, sample: &Sample) {
        for process in &sample.processes {
            // entry().or_default(): that process's series, or a new empty one.
            let series = self.series.entry(process.key.clone()).or_default();
            series.push(Point {
                taken_at: sample.taken_at,
                footprint: process.footprint,
            });
        }

        for series in self.series.values_mut() {
            series.drop_older_than(sample.taken_at, self.window_secs);
        }
        self.series.retain(|_, series| !series.points.is_empty());
    }

    pub fn get(&self, key: &ProcessKey) -> Option<&Series> {
        self.series.get(key)
    }

    pub fn contains(&self, key: &ProcessKey) -> bool {
        self.series.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MIB, Pressure, Process, SystemState};

    fn process(pid: u32, mib: u64) -> Process {
        Process {
            key: ProcessKey::PidName(pid, "p".to_string()),
            pid,
            name: "p".to_string(),
            footprint: Bytes(mib * MIB),
        }
    }

    fn sample(taken_at: u64, processes: Vec<Process>) -> Sample {
        Sample {
            taken_at: Timestamp(taken_at),
            processes,
            unreadable: 0,
            system: SystemState {
                pressure: Pressure::Normal,
                swap_used: Bytes(0),
                compressor: Bytes(0),
                swapouts_pages: 0,
                page_size: 16384,
            },
        }
    }

    fn key(pid: u32) -> ProcessKey {
        ProcessKey::PidName(pid, "p".to_string())
    }

    #[test]
    fn records_one_point_per_process() {
        let mut history = History::new(600);
        history.record(&sample(0, vec![process(1, 100), process(2, 200)]));
        history.record(&sample(30, vec![process(1, 110), process(2, 200)]));
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(&key(1)).unwrap().points().len(), 2);
        assert_eq!(
            history.get(&key(1)).unwrap().newest().unwrap().footprint,
            Bytes(110 * MIB)
        );
    }

    #[test]
    fn a_point_exactly_at_the_window_edge_stays() {
        let mut history = History::new(600);
        history.record(&sample(0, vec![process(1, 100)]));
        history.record(&sample(600, vec![process(1, 100)]));
        assert_eq!(history.get(&key(1)).unwrap().points().len(), 2);
    }

    #[test]
    fn a_point_one_second_past_the_edge_goes() {
        let mut history = History::new(600);
        history.record(&sample(0, vec![process(1, 100)]));
        history.record(&sample(601, vec![process(1, 100)]));
        let series = history.get(&key(1)).unwrap();
        assert_eq!(series.points().len(), 1);
        assert_eq!(series.oldest().unwrap().taken_at, Timestamp(601));
    }

    #[test]
    fn missing_from_one_sample_keeps_the_series() {
        let mut history = History::new(600);
        history.record(&sample(0, vec![process(1, 100)]));
        history.record(&sample(30, vec![process(2, 100)]));
        history.record(&sample(60, vec![process(1, 120)]));
        assert_eq!(history.get(&key(1)).unwrap().points().len(), 2);
    }

    #[test]
    fn a_process_gone_for_the_whole_window_is_deleted() {
        let mut history = History::new(600);
        history.record(&sample(0, vec![process(1, 100), process(2, 100)]));
        history.record(&sample(600, vec![process(2, 100)]));
        assert!(history.contains(&key(1)));
        history.record(&sample(601, vec![process(2, 100)]));
        assert!(!history.contains(&key(1)));
        assert!(history.contains(&key(2)));
    }

    #[test]
    fn a_gap_longer_than_the_window_starts_over() {
        let mut history = History::new(600);
        for t in (0..=300).step_by(30) {
            history.record(&sample(t, vec![process(1, 100 + t)]));
        }
        history.record(&sample(1000, vec![process(1, 5000)]));
        let series = history.get(&key(1)).unwrap();
        assert_eq!(series.points().len(), 1);
        assert_eq!(series.oldest().unwrap().taken_at, Timestamp(1000));
    }
}
