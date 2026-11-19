//! Thresholds (docs/design.md §6.4). They come from two incidents; M5 calibrates them.

use crate::model::{Bytes, GIB, MIB, Pressure};

#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    /// How often a sample is taken.
    pub sample_every_secs: u64,
    /// The series window: samples with `taken_at ≥ now − window_secs`.
    pub window_secs: u64,
    /// (a) The window must span at least this...
    pub min_span_secs: u64,
    /// (a) ...and have at least this many jumps.
    pub min_jumps: usize,
    /// (b) Minimum growth between the oldest and the newest sample.
    pub min_growth: Bytes,
    /// (c) Minimum percentage of strictly positive jumps.
    pub min_positive_percent: usize,
    /// Minimum footprint for the verdict to be `Leak` (otherwise, `Growing`).
    pub leak_floor: Bytes,
    /// Re-alert a leak if it grew this much since the last alert...
    pub realert_growth: Bytes,
    /// ...or if this much time passed.
    pub realert_secs: u64,
    /// Time in a row without `Leak` to go back to Normal.
    pub back_to_normal_secs: u64,
    /// The pressure level from which to notify; below it, it only goes to the log.
    /// CRITICAL because on this Mac WARN comes and goes many times a day with
    /// nothing happening (17 alerts in 27 h, almost all without swapout).
    pub pressure_notify_from: Pressure,
    /// The same pressure level is not alerted twice within this time.
    pub pressure_cooldown_secs: u64,
    /// Processes listed in the pressure alert.
    pub pressure_alert_processes: usize,
    /// Failed cycles in a row before alerting that sampling is not possible.
    pub failed_cycles_alert: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            sample_every_secs: 30,
            window_secs: 600,
            min_span_secs: 540,
            min_jumps: 10,
            min_growth: Bytes(512 * MIB),
            min_positive_percent: 80,
            leak_floor: Bytes(3 * GIB),
            realert_growth: Bytes(2 * GIB),
            realert_secs: 1800,
            back_to_normal_secs: 600,
            pressure_notify_from: Pressure::Critical,
            pressure_cooldown_secs: 900,
            pressure_alert_processes: 5,
            failed_cycles_alert: 10,
        }
    }
}
