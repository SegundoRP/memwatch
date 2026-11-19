//! ALL the state between cycles (docs/design.md §6.2–§6.3). Decides WHEN to alert.
//!
//! It is pure: it takes samples and returns events. It does not notify, does not
//! write the log and does not read the clock; time arrives inside each `Sample`.

use std::collections::HashMap;

use crate::detect::{self, Verdict};
use crate::history::History;
use crate::model::{Bytes, Pressure, ProcessKey, Sample, SystemState, Timestamp};
use crate::report::top_n;
use crate::source::SourceError;
use crate::thresholds::Thresholds;

/// Something worth interrupting you with a notification.
#[derive(Debug, Clone, PartialEq)]
pub enum Alert {
    Leak {
        pid: u32,
        name: String,
        footprint: Bytes,
        /// How much it grew within the window (newest − oldest).
        growth: Bytes,
        /// How many seconds that growth spans.
        span_secs: u64,
    },
    Pressure {
        system: SystemState,
        swapout_rate: Option<Bytes>,
        heaviest: Vec<(String, Bytes)>,
    },
    SamplingFailed {
        cycles: u32,
        last_error: String,
    },
}

impl Alert {
    pub fn title(&self) -> String {
        match self {
            Alert::Leak { name, .. } => format!("memwatch: possible leak in {name}"),
            Alert::Pressure { .. } => "memwatch: memory is running out".to_string(),
            Alert::SamplingFailed { .. } => "memwatch: cannot read memory".to_string(),
        }
    }

    /// The notification text: first what is happening, then what to do.
    pub fn body(&self) -> String {
        match self {
            Alert::Leak {
                pid,
                name,
                footprint,
                growth,
                span_secs,
            } => format!(
                "{name} grew {growth} in {} min and now uses {footprint}.\n\
                 If you don't need it, quit it or run: kill -9 {pid}",
                (span_secs + 30) / 60
            ),
            Alert::Pressure {
                system,
                swapout_rate,
                heaviest,
            } => {
                let disk = match swapout_rate {
                    Some(rate) if rate.0 > 0 => format!("is writing to disk at {rate}/s"),
                    _ => "is using the disk as RAM".to_string(),
                };
                let mut processes = Vec::new();
                for (name, footprint) in heaviest {
                    processes.push(format!("{} {footprint}", friendly_name(name)));
                }
                format!(
                    "The Mac {disk} (swap {}). Using the most: {}.",
                    system.swap_used,
                    processes.join(", ")
                )
            }
            Alert::SamplingFailed { cycles, last_error } => format!(
                "{cycles} attempts in a row failed to read memory. Last error: {last_error}"
            ),
        }
    }
}

/// Clearer names for the processes that show up most on this Mac.
fn friendly_name(name: &str) -> &str {
    if name.starts_with("com.apple.Virtualization") {
        "virtual machine"
    } else if name == "plugin-container" {
        "Firefox tab"
    } else {
        name
    }
}

/// What `alerts` decides in one cycle.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Notify (and also write it to the log).
    Notify(Alert),
    /// Just a line in the log.
    Log(String),
}

/// The alert state of one process (§6.3).
#[derive(Debug, Clone, Copy, PartialEq)]
enum LeakState {
    Normal,
    Leaking {
        last_alert_at: Timestamp,
        last_alert_footprint: Bytes,
        /// Since when the verdict has not been `Leak` (`None` if it is `Leak` this cycle).
        quiet_since: Option<Timestamp>,
    },
}

/// What to do after a `LeakState` transition.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LeakStep {
    Nothing,
    Notify,
    BackToNormal,
}

/// The §6.3 transition for one process: a pure function, easy to test on its own.
fn next_leak_state(
    state: LeakState,
    verdict: Verdict,
    now: Timestamp,
    footprint: Bytes,
    thresholds: &Thresholds,
) -> (LeakState, LeakStep) {
    let alerted_now = LeakState::Leaking {
        last_alert_at: now,
        last_alert_footprint: footprint,
        quiet_since: None,
    };

    match (state, verdict) {
        (LeakState::Normal, Verdict::Leak) => (alerted_now, LeakStep::Notify),
        (LeakState::Normal, _) => (LeakState::Normal, LeakStep::Nothing),
        (
            LeakState::Leaking {
                last_alert_at,
                last_alert_footprint,
                ..
            },
            Verdict::Leak,
        ) => {
            let grew = footprint.0
                >= last_alert_footprint
                    .0
                    .saturating_add(thresholds.realert_growth.0);
            let waited = now.0 >= last_alert_at.0.saturating_add(thresholds.realert_secs);
            if grew || waited {
                (alerted_now, LeakStep::Notify)
            } else {
                // A `Leak` in between restarts the countdown back to Normal.
                let still = LeakState::Leaking {
                    last_alert_at,
                    last_alert_footprint,
                    quiet_since: None,
                };
                (still, LeakStep::Nothing)
            }
        }
        (
            LeakState::Leaking {
                last_alert_at,
                last_alert_footprint,
                quiet_since,
            },
            _,
        ) => {
            let since = quiet_since.unwrap_or(now);
            if now.0 >= since.0.saturating_add(thresholds.back_to_normal_secs) {
                (LeakState::Normal, LeakStep::BackToNormal)
            } else {
                let still = LeakState::Leaking {
                    last_alert_at,
                    last_alert_footprint,
                    quiet_since: Some(since),
                };
                (still, LeakStep::Nothing)
            }
        }
    }
}

pub struct Alerts {
    thresholds: Thresholds,
    leaks: HashMap<ProcessKey, LeakState>,
    /// Last pressure level seen (confirmed or not).
    last_level: Option<Pressure>,
    /// Confirmed level: two samples in a row that match. `None` at startup.
    confirmed: Option<Pressure>,
    /// When each level was last alerted (for the cooldown).
    notified_at: HashMap<Pressure, Timestamp>,
    previous: Option<Sample>,
    last_swapout_rate: Option<Bytes>,
    failed_cycles: u32,
}

impl Alerts {
    pub fn new(thresholds: Thresholds) -> Alerts {
        Alerts {
            thresholds,
            leaks: HashMap::new(),
            last_level: None,
            confirmed: None,
            notified_at: HashMap::new(),
            previous: None,
            last_swapout_rate: None,
            failed_cycles: 0,
        }
    }

    /// The swapout rate computed in the last good cycle (`None` = unknown).
    pub fn last_swapout_rate(&self) -> Option<Bytes> {
        self.last_swapout_rate
    }

    /// A good cycle. `history` must already include `sample`.
    pub fn observe(&mut self, sample: &Sample, history: &History) -> Vec<Event> {
        let mut events = Vec::new();

        if self.failed_cycles > 0 {
            events.push(Event::Log(format!(
                "sampling recovered after {} failed cycles",
                self.failed_cycles
            )));
            self.failed_cycles = 0;
        }

        self.last_swapout_rate = match &self.previous {
            Some(previous) => detect::swapout_rate(previous, sample),
            None => None,
        };

        self.observe_leaks(sample, history, &mut events);
        self.observe_pressure(sample, &mut events);
        self.previous = Some(sample.clone());
        events
    }

    /// A failed cycle: there is no sample, so nothing else changes.
    pub fn sampling_failed(&mut self, error: &SourceError) -> Vec<Event> {
        self.failed_cycles += 1;
        let mut events = vec![Event::Log(format!(
            "failed cycle ({} in a row): {error}",
            self.failed_cycles
        ))];
        // `==` and not `>=`: alert ONCE per streak of failures.
        if self.failed_cycles == self.thresholds.failed_cycles_alert {
            events.push(Event::Notify(Alert::SamplingFailed {
                cycles: self.failed_cycles,
                last_error: error.to_string(),
            }));
        }
        events
    }

    fn observe_leaks(&mut self, sample: &Sample, history: &History, events: &mut Vec<Event>) {
        for process in &sample.processes {
            let Some(series) = history.get(&process.key) else {
                continue;
            };
            let verdict = detect::verdict(series, &self.thresholds);
            let state = self
                .leaks
                .entry(process.key.clone())
                .or_insert(LeakState::Normal);
            let (next, step) = next_leak_state(
                *state,
                verdict,
                sample.taken_at,
                process.footprint,
                &self.thresholds,
            );
            *state = next;

            match step {
                LeakStep::Nothing => {}
                LeakStep::Notify => {
                    // The alert's "↑": from the oldest sample in the window to now.
                    let (growth, span_secs) = match series.oldest() {
                        Some(oldest) => (
                            Bytes(process.footprint.0.saturating_sub(oldest.footprint.0)),
                            sample.taken_at.0.saturating_sub(oldest.taken_at.0),
                        ),
                        None => (Bytes(0), 0),
                    };
                    events.push(Event::Notify(Alert::Leak {
                        pid: process.pid,
                        name: process.name.clone(),
                        footprint: process.footprint,
                        growth,
                        span_secs,
                    }));
                }
                LeakStep::BackToNormal => events.push(Event::Log(format!(
                    "{} (pid {}) back to Normal: 10 min without growing like a leak",
                    process.name, process.pid
                ))),
            }
        }

        // If a process's series was deleted (it has not shown up for 10 min),
        // its state is deleted too.
        self.leaks.retain(|key, _| history.contains(key));
    }

    fn observe_pressure(&mut self, sample: &Sample, events: &mut Vec<Event>) {
        let level = sample.system.pressure;
        if let Pressure::Unknown(value) = level {
            events.push(Event::Log(format!("unknown pressure level: {value}")));
        }

        // `replace` stores the new level and returns the previous one.
        let previous = self.last_level.replace(level);
        let confirmed_now = match previous {
            Some(previous) if previous == level && level.severity().is_some() => level,
            _ => return, // there are not yet two equal, known samples
        };

        let Some(before) = self.confirmed.replace(confirmed_now) else {
            // The first confirmation does not notify: restarting memwatch must
            // not send you a banner every time.
            events.push(Event::Log(format!("started with pressure {confirmed_now}")));
            return;
        };
        if before == confirmed_now {
            return;
        }

        if confirmed_now.severity() > before.severity() {
            let notify_from = self.thresholds.pressure_notify_from;
            if confirmed_now.severity() < notify_from.severity() {
                events.push(Event::Log(format!(
                    "pressure worsened: {before} → {confirmed_now} (no alert: alerts start at {notify_from})"
                )));
                return;
            }
            let now = sample.taken_at;
            let in_cooldown = match self.notified_at.get(&confirmed_now) {
                Some(at) => now.0 < at.0.saturating_add(self.thresholds.pressure_cooldown_secs),
                None => false,
            };
            if in_cooldown {
                events.push(Event::Log(format!(
                    "pressure {before} → {confirmed_now}, no alert: already alerted less than 15 min ago"
                )));
            } else {
                self.notified_at.insert(confirmed_now, now);
                events.push(Event::Notify(self.pressure_alert(sample)));
            }
        } else {
            events.push(Event::Log(format!(
                "pressure improved: {before} → {confirmed_now}"
            )));
        }
    }

    fn pressure_alert(&self, sample: &Sample) -> Alert {
        let mut heaviest = Vec::new();
        for process in top_n(&sample.processes, self.thresholds.pressure_alert_processes) {
            heaviest.push((process.name.clone(), process.footprint));
        }
        Alert::Pressure {
            system: sample.system.clone(),
            swapout_rate: self.last_swapout_rate,
            heaviest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GIB, MIB, Process};

    fn system(pressure: Pressure) -> SystemState {
        SystemState {
            pressure,
            swap_used: Bytes(2 * GIB),
            compressor: Bytes(6 * GIB),
            swapouts_pages: 0,
            page_size: 16384,
        }
    }

    fn sample_at(taken_at: u64, pressure: Pressure) -> Sample {
        Sample {
            taken_at: Timestamp(taken_at),
            processes: vec![Process {
                key: ProcessKey::PidName(1, "vm".to_string()),
                pid: 1,
                name: "vm".to_string(),
                footprint: Bytes(4 * GIB),
            }],
            unreadable: 0,
            system: system(pressure),
        }
    }

    /// Runs a sequence of levels (one every 30 s) and returns the positions
    /// where a notification happened.
    fn notified_at_positions(levels: &[Pressure]) -> Vec<usize> {
        let mut alerts = Alerts::new(Thresholds::default());
        let history = History::new(600);
        let mut positions = Vec::new();
        for (i, level) in levels.iter().enumerate() {
            let events = alerts.observe(&sample_at(i as u64 * 30, *level), &history);
            if events.iter().any(|event| matches!(event, Event::Notify(_))) {
                positions.push(i);
            }
        }
        positions
    }

    use Pressure::{Critical, Normal, Unknown, Warn};

    #[test]
    fn starting_in_warn_only_logs_then_critical_notifies() {
        assert_eq!(
            notified_at_positions(&[Warn, Warn, Critical, Critical]),
            vec![3]
        );
    }

    #[test]
    fn the_first_confirmation_logs_where_it_started() {
        let mut alerts = Alerts::new(Thresholds::default());
        let history = History::new(600);
        alerts.observe(&sample_at(0, Warn), &history);
        let events = alerts.observe(&sample_at(30, Warn), &history);
        assert_eq!(
            events,
            vec![Event::Log("started with pressure WARN".to_string())]
        );
    }

    #[test]
    fn a_single_warn_sample_is_not_confirmed() {
        assert!(notified_at_positions(&[Normal, Normal, Warn, Normal]).is_empty());
    }

    #[test]
    fn warn_only_goes_to_the_log() {
        assert!(notified_at_positions(&[Normal, Normal, Warn, Warn]).is_empty());

        let mut alerts = Alerts::new(Thresholds::default());
        let history = History::new(600);
        for (i, level) in [Normal, Normal, Warn].iter().enumerate() {
            alerts.observe(&sample_at(i as u64 * 30, *level), &history);
        }
        assert_eq!(
            alerts.observe(&sample_at(90, Warn), &history),
            vec![Event::Log(
                "pressure worsened: NORMAL → WARN (no alert: alerts start at CRITICAL)".to_string()
            )]
        );
    }

    #[test]
    fn two_critical_samples_after_normal_notify() {
        assert_eq!(
            notified_at_positions(&[Normal, Normal, Critical, Critical]),
            vec![3]
        );
    }

    #[test]
    fn warn_then_critical_notifies_at_critical() {
        let levels = [Normal, Normal, Warn, Warn, Critical, Critical];
        assert_eq!(notified_at_positions(&levels), vec![5]);
    }

    #[test]
    fn improving_does_not_notify() {
        assert!(notified_at_positions(&[Warn, Warn, Normal, Normal]).is_empty());
    }

    #[test]
    fn warn_again_within_the_cooldown_does_not_notify() {
        // Critical alert at t=90; back to Normal and Critical again at t=210 (< 900 s).
        let levels = [
            Normal, Normal, Critical, Critical, Normal, Normal, Critical, Critical,
        ];
        assert_eq!(notified_at_positions(&levels), vec![3]);
    }

    #[test]
    fn warn_again_after_the_cooldown_notifies() {
        let mut levels = vec![Normal, Normal, Critical, Critical, Normal, Normal];
        levels.extend([Normal; 30]); // 15 more min
        levels.extend([Critical, Critical]);
        assert_eq!(notified_at_positions(&levels), vec![3, levels.len() - 1]);
    }

    #[test]
    fn unknown_levels_confirm_nothing() {
        // The design (§10) writes it as "Normal,Unknown,Unknown,Warn,Warn"; one
        // more Normal is needed so there is a confirmed level to worsen from.
        let levels = [Normal, Normal, Unknown(9), Unknown(9), Critical, Critical];
        assert_eq!(notified_at_positions(&levels), vec![5]);
    }

    #[test]
    fn ten_failed_cycles_notify_once() {
        let mut alerts = Alerts::new(Thresholds::default());
        let error = SourceError::Unavailable("top did not respond".to_string());
        let mut notifications = 0;
        for _ in 0..25 {
            for event in alerts.sampling_failed(&error) {
                if matches!(
                    event,
                    Event::Notify(Alert::SamplingFailed { cycles: 10, .. })
                ) {
                    notifications += 1;
                }
            }
        }
        assert_eq!(notifications, 1);
    }

    #[test]
    fn recovering_logs_and_rearms_the_alert() {
        let mut alerts = Alerts::new(Thresholds::default());
        let history = History::new(600);
        let error = SourceError::Unavailable("x".to_string());
        for _ in 0..10 {
            alerts.sampling_failed(&error);
        }
        let events = alerts.observe(&sample_at(0, Normal), &history);
        assert!(events.contains(&Event::Log(
            "sampling recovered after 10 failed cycles".to_string()
        )));
        let mut again = Vec::new();
        for _ in 0..10 {
            again.extend(alerts.sampling_failed(&error));
        }
        assert!(again.iter().any(|event| matches!(event, Event::Notify(_))));
    }

    // --- leak transitions ---

    fn leaking(at: u64, mib: u64) -> LeakState {
        LeakState::Leaking {
            last_alert_at: Timestamp(at),
            last_alert_footprint: Bytes(mib * MIB),
            quiet_since: None,
        }
    }

    fn step(state: LeakState, verdict: Verdict, now: u64, mib: u64) -> (LeakState, LeakStep) {
        next_leak_state(
            state,
            verdict,
            Timestamp(now),
            Bytes(mib * MIB),
            &Thresholds::default(),
        )
    }

    #[test]
    fn first_leak_notifies() {
        assert_eq!(
            step(LeakState::Normal, Verdict::Leak, 540, 4444),
            (leaking(540, 4444), LeakStep::Notify)
        );
    }

    #[test]
    fn growing_never_notifies() {
        assert_eq!(
            step(LeakState::Normal, Verdict::Growing, 540, 2000).1,
            LeakStep::Nothing
        );
    }

    #[test]
    fn still_leaking_without_enough_growth_or_time_is_quiet() {
        assert_eq!(
            step(leaking(540, 4444), Verdict::Leak, 840, 4444 + 2047).1,
            LeakStep::Nothing
        );
    }

    #[test]
    fn two_more_gib_notifies_again() {
        assert_eq!(
            step(leaking(540, 4444), Verdict::Leak, 840, 4444 + 2048).1,
            LeakStep::Notify
        );
    }

    #[test]
    fn thirty_minutes_later_notifies_again() {
        assert_eq!(
            step(leaking(540, 4444), Verdict::Leak, 540 + 1800, 4500).1,
            LeakStep::Notify
        );
    }

    #[test]
    fn ten_quiet_minutes_go_back_to_normal() {
        let (state, first) = step(leaking(540, 4444), Verdict::Stable, 600, 4444);
        assert_eq!(first, LeakStep::Nothing);
        let (state, _) = step(state, Verdict::Stable, 1170, 4444);
        assert!(matches!(state, LeakState::Leaking { .. }));
        assert_eq!(
            step(state, Verdict::Stable, 1200, 4444),
            (LeakState::Normal, LeakStep::BackToNormal)
        );
    }

    #[test]
    fn a_leak_in_between_restarts_the_quiet_count() {
        let (state, _) = step(leaking(540, 4444), Verdict::Stable, 600, 4444);
        let (state, _) = step(state, Verdict::Leak, 900, 4500);
        let (state, back) = step(state, Verdict::Stable, 1200, 4500);
        assert_eq!(back, LeakStep::Nothing);
        assert!(matches!(
            state,
            LeakState::Leaking {
                quiet_since: Some(Timestamp(1200)),
                ..
            }
        ));
    }

    // --- texts ---

    #[test]
    fn leak_alert_text() {
        let alert = Alert::Leak {
            pid: 48213,
            name: "codex".to_string(),
            footprint: Bytes(9_800 * MIB + MIB / 2),
            growth: Bytes(3_790 * MIB),
            span_secs: 600,
        };
        assert_eq!(alert.title(), "memwatch: possible leak in codex");
        assert_eq!(
            alert.body(),
            "codex grew 3.7 GB in 10 min and now uses 9.6 GB.\n\
             If you don't need it, quit it or run: kill -9 48213"
        );
    }

    #[test]
    fn pressure_alert_text_without_swapout() {
        let alert = Alert::Pressure {
            system: system(Critical),
            swapout_rate: None,
            heaviest: vec![
                (
                    "com.apple.Virtualization.Virtua".to_string(),
                    Bytes(4112 * MIB),
                ),
                ("plugin-container".to_string(), Bytes(1700 * MIB)),
                ("firefox".to_string(), Bytes(1271 * MIB)),
            ],
        };
        assert_eq!(alert.title(), "memwatch: memory is running out");
        assert_eq!(
            alert.body(),
            "The Mac is using the disk as RAM (swap 2.0 GB). Using the most: \
             virtual machine 4.0 GB, Firefox tab 1.7 GB, firefox 1.2 GB."
        );
    }

    #[test]
    fn pressure_alert_text_with_swapout() {
        let alert = Alert::Pressure {
            system: system(Critical),
            swapout_rate: Some(Bytes(72 * MIB)),
            heaviest: vec![("codex".to_string(), Bytes(9 * GIB))],
        };
        assert_eq!(
            alert.body(),
            "The Mac is writing to disk at 72.0 MB/s (swap 2.0 GB). Using the most: codex 9.0 GB."
        );
    }
}
