//! The `memwatch watch` loop: sample → record → detect → alert → sleep.
//!
//! `Watcher::step` is pure (no I/O). `cycle` adds the source and the notifier
//! behind traits, so the tests run it with fakes.

use std::io::{self, IsTerminal};
use std::thread;
use std::time::{Duration, Instant};

use crate::alerts::{Alerts, Event};
use crate::detect::{self, Verdict};
use crate::history::History;
use crate::log::{Logger, now_secs, utc};
use crate::model::Sample;
use crate::notify::Notifier;
use crate::report::top_n;
use crate::source::{Source, SourceError};
use crate::thresholds::Thresholds;

pub struct Watcher {
    thresholds: Thresholds,
    history: History,
    alerts: Alerts,
}

/// The result of one step: which events happened and the status line for the terminal.
pub struct Step {
    pub events: Vec<Event>,
    pub status: Option<String>,
}

impl Watcher {
    pub fn new(thresholds: Thresholds) -> Watcher {
        Watcher {
            history: History::new(thresholds.window_secs),
            alerts: Alerts::new(thresholds.clone()),
            thresholds,
        }
    }

    /// A cycle without I/O: takes what the source read and decides.
    pub fn step(&mut self, reading: Result<Sample, SourceError>) -> Step {
        match reading {
            Ok(sample) => {
                self.history.record(&sample);
                let events = self.alerts.observe(&sample, &self.history);
                Step {
                    events,
                    status: Some(self.status_line(&sample)),
                }
            }
            Err(error) => Step {
                events: self.alerts.sampling_failed(&error),
                status: None,
            },
        }
    }

    /// "pressure WARN · swap 17.9 GB · compressor 10.7 GB · swapout 0 B/s · heaviest: …"
    fn status_line(&self, sample: &Sample) -> String {
        let system = &sample.system;
        let rate = match self.alerts.last_swapout_rate() {
            Some(rate) => format!("{rate}/s"),
            None => "?".to_string(),
        };
        let mut line = format!(
            "pressure {} · swap {} · compressor {} · swapout {rate}",
            system.pressure, system.swap_used, system.compressor
        );
        if let Some(heaviest) = top_n(&sample.processes, 1).first() {
            line.push_str(&format!(
                " · heaviest: {} {}",
                heaviest.name, heaviest.footprint
            ));
        }
        for process in &sample.processes {
            let Some(series) = self.history.get(&process.key) else {
                continue;
            };
            match detect::verdict(series, &self.thresholds) {
                Verdict::Stable => {}
                Verdict::Growing => line.push_str(&format!(
                    " · growing: {} {}",
                    process.name, process.footprint
                )),
                Verdict::Leak => {
                    line.push_str(&format!(" · LEAK: {} {}", process.name, process.footprint))
                }
            }
        }
        if sample.unreadable > 0 {
            line.push_str(&format!(" · {} unreadable", sample.unreadable));
        }
        line
    }
}

/// What a full cycle leaves behind.
pub struct CycleOutput {
    /// Lines for the log (alerts go here too).
    pub log: Vec<String>,
    /// Status line for the terminal (`None` if the cycle failed).
    pub status: Option<String>,
}

/// A full cycle: read from the source, decide and notify.
///
/// `&mut dyn Source` is a "trait object": any type that implements `Source`
/// works, and which one is resolved at runtime.
pub fn cycle(
    watcher: &mut Watcher,
    source: &mut dyn Source,
    notifier: &mut dyn Notifier,
) -> CycleOutput {
    let step = watcher.step(source.sample());
    let mut log = Vec::new();
    for event in step.events {
        match event {
            Event::Log(line) => log.push(line),
            Event::Notify(alert) => {
                let title = alert.title();
                let body = alert.body();
                // osascript finishing fine does not guarantee the banner was seen,
                // so every alert also goes to the log.
                log.push(format!("ALERT {title}: {}", body.replace('\n', " / ")));
                if let Err(error) = notifier.notify(&title, &body) {
                    log.push(format!("could not notify: {error}"));
                }
            }
        }
    }
    CycleOutput {
        log,
        status: step.status,
    }
}

/// The `memwatch watch` loop. It never ends: the `!` ("never") type says so.
///
/// Each cycle's status line goes to stdout ONLY if stdout is a terminal;
/// under launchd nothing is written per cycle.
pub fn run(
    source: &mut dyn Source,
    notifier: &mut dyn Notifier,
    logger: &mut Logger,
    thresholds: Thresholds,
) -> ! {
    let interval = Duration::from_secs(thresholds.sample_every_secs);
    let show_status = io::stdout().is_terminal();
    let mut watcher = Watcher::new(thresholds);
    logger.line(&format!("start: one sample every {} s", interval.as_secs()));

    loop {
        let started = Instant::now();
        let output = cycle(&mut watcher, source, notifier);
        for line in &output.log {
            logger.line(line);
        }
        if show_status && let Some(status) = output.status {
            // utc() gives "2026-11-17T21:40:03Z"; for the terminal the time is enough.
            println!("{} {status}", &utc(now_secs())[11..19]);
        }
        // Sleep for what is left to complete the interval (never negative).
        thread::sleep(interval.saturating_sub(started.elapsed()));
    }
}
