//! memwatch domain types (see docs/design.md §5).

use std::fmt;

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

/// An amount of memory in bytes.
///
/// It is printed in base 1024, like `top` and Activity Monitor:
/// - under 1 KB: no decimals ("1023 B")
/// - from KB up: 1 decimal ("1.1 GB")
/// - the unit is chosen AFTER rounding: 1_048_575 → "1.0 MB", not "1024.0 KB"
/// - GB is the largest unit
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub u64);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 3] = ["KB", "MB", "GB"];

        if self.0 < 1024 {
            return write!(f, "{} B", self.0);
        }

        let mut value = self.0 as f64 / 1024.0;
        let mut unit = 0;
        // Move up a unit by looking at the ALREADY rounded value: 1_048_575 B is
        // 1023.999 KB, which would show as "1024.0 KB", so it becomes "1.0 MB".
        while round_to_tenth(value) >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        write!(f, "{:.1} {}", round_to_tenth(value), UNITS[unit])
    }
}

fn round_to_tenth(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// A process's identity: what counts as "the same process" across cycles.
///
/// ONLY the source builds it. The rest of the program uses it as an opaque key
/// and never does `match` on it: that is why adding `PidStart` in M6 changed
/// nothing outside `source/` and this file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProcessKey {
    /// Text source: pid + name from `top`. It does not detect a pid reused
    /// by a process with the same name (rare).
    PidName(u32, String),
    /// FFI source: pid + the process start time (`ri_proc_start_abstime`).
    PidStart(u32, u64),
}

/// A process in a sample.
#[derive(Debug, Clone, PartialEq)]
pub struct Process {
    pub key: ProcessKey,
    pub pid: u32,
    /// As `top` shows it: truncated to 16 characters ("Claude Helper (R").
    pub name: String,
    pub footprint: Bytes,
}

/// Monotonic seconds since memwatch started.
///
/// The real source takes them from `std::time::Instant`; the tests make them up.
/// That way 40 minutes of leak can be simulated in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub u64);

/// Value of `kern.memorystatus_vm_pressure_level`: 1, 2, 4 or anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pressure {
    Normal,
    Warn,
    Critical,
    Unknown(u32),
}

impl Pressure {
    pub fn from_level(level: u32) -> Pressure {
        match level {
            1 => Pressure::Normal,
            2 => Pressure::Warn,
            4 => Pressure::Critical,
            other => Pressure::Unknown(other),
        }
    }

    /// Normal < Warn < Critical. `Unknown` has no severity (`None`).
    pub fn severity(self) -> Option<u8> {
        match self {
            Pressure::Normal => Some(0),
            Pressure::Warn => Some(1),
            Pressure::Critical => Some(2),
            Pressure::Unknown(_) => None,
        }
    }
}

impl fmt::Display for Pressure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pressure::Normal => write!(f, "NORMAL"),
            Pressure::Warn => write!(f, "WARN"),
            Pressure::Critical => write!(f, "CRITICAL"),
            Pressure::Unknown(level) => write!(f, "unknown ({level})"),
        }
    }
}

/// The memory state of the whole system at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemState {
    pub pressure: Pressure,
    pub swap_used: Bytes,
    /// "Pages occupied by compressor" × page_size: what the compressor takes in RAM.
    pub compressor: Bytes,
    /// Cumulative counter since boot.
    pub swapouts_pages: u64,
    /// The kernel page size (16384 on Apple Silicon), from the `vm_stat` header.
    pub page_size: u64,
}

/// Everything read in one cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub taken_at: Timestamp,
    pub processes: Vec<Process>,
    /// Processes the source could not read: unparseable `top` rows in the text
    /// source; other users' processes (EPERM) in the FFI one.
    pub unreadable: u32,
    pub system: SystemState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_levels_from_the_kernel() {
        assert_eq!(Pressure::from_level(1), Pressure::Normal);
        assert_eq!(Pressure::from_level(2), Pressure::Warn);
        assert_eq!(Pressure::from_level(4), Pressure::Critical);
        assert_eq!(Pressure::from_level(3), Pressure::Unknown(3));
        assert_eq!(Pressure::from_level(0), Pressure::Unknown(0));
    }

    #[test]
    fn severity_orders_the_known_levels() {
        assert!(Pressure::Normal.severity() < Pressure::Warn.severity());
        assert!(Pressure::Warn.severity() < Pressure::Critical.severity());
        assert_eq!(Pressure::Unknown(7).severity(), None);
    }

    #[test]
    fn pressure_display() {
        assert_eq!(Pressure::Warn.to_string(), "WARN");
        assert_eq!(Pressure::Unknown(7).to_string(), "unknown (7)");
    }

    #[test]
    fn zero_bytes() {
        assert_eq!(Bytes(0).to_string(), "0 B");
    }

    #[test]
    fn below_one_kb_has_no_decimals() {
        assert_eq!(Bytes(1023).to_string(), "1023 B");
    }

    #[test]
    fn exactly_one_kb() {
        assert_eq!(Bytes(KIB).to_string(), "1.0 KB");
    }

    #[test]
    fn unit_is_chosen_after_rounding() {
        assert_eq!(Bytes(MIB - 1).to_string(), "1.0 MB");
    }

    #[test]
    fn codex_size_from_the_design_doc() {
        assert_eq!(Bytes(1_181_116_006).to_string(), "1.1 GB");
    }

    #[test]
    fn vm_mem_from_top_fixture() {
        // "70062  com.apple.Virtua 4112M" in tests/fixtures/top_l1.txt
        assert_eq!(Bytes(4112 * MIB).to_string(), "4.0 GB");
    }

    #[test]
    fn july_incident_peak() {
        assert_eq!(Bytes(31 * GIB).to_string(), "31.0 GB");
    }

    #[test]
    fn gb_is_the_largest_unit() {
        assert_eq!(Bytes(2048 * GIB).to_string(), "2048.0 GB");
    }
}
