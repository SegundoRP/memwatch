//! Text source: runs `top` and parses its output (see docs/design.md §7).
//!
//! The parsers (`parse_mem`, `parse_top`) are pure functions: they take text and
//! return data, with no I/O. That is why they are tested with the real fixtures in
//! `tests/fixtures/` without running `top`. Only `read_top` touches the system.

use std::fmt;
use std::num::{ParseFloatError, ParseIntError};
use std::time::{Duration, Instant};

use super::{Source, SourceError};
use crate::command::run_with_timeout;
use crate::model::{
    Bytes, GIB, KIB, MIB, Pressure, Process, ProcessKey, Sample, SystemState, Timestamp,
};

/// Deadline for `top`, `vm_stat` and `sysctl` (`top -l 1` takes ~1.3 s).
const TIMEOUT: Duration = Duration::from_secs(10);

const VM_STAT: &str = "/usr/bin/vm_stat";
const SYSCTL: &str = "/usr/sbin/sysctl";

const TOP: &str = "/usr/bin/top";
const TOP_ARGS: [&str; 8] = [
    "-l",
    "1",
    "-o",
    "mem",
    "-n",
    "50",
    "-stats",
    "pid,command,mem",
];

/// What one run of `top` yields.
#[derive(Debug, Clone, PartialEq)]
pub struct TopOutput {
    pub processes: Vec<Process>,
    /// Rows that could not be read and were ignored.
    pub skipped: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// There is no line starting with "PID".
    MissingHeader,
    /// There is a "PID" header but no rows below it.
    NoRows,
    /// More than half of the rows could not be read: the cycle counts as failed.
    TooManyBadRows {
        bad: usize,
        total: usize,
    },
    /// A MEM value with no known unit, or so big it does not fit in a u64.
    InvalidMem(String),
    /// A field expected in the output is missing (for example "Swapouts").
    MissingField(String),
    /// A calculation that does not fit in a u64.
    TooLarge(String),
    Int(ParseIntError),
    Float(ParseFloatError),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::MissingHeader => write!(f, "missing PID header line"),
            ParseError::NoRows => write!(f, "no process rows"),
            ParseError::TooManyBadRows { bad, total } => {
                write!(f, "{bad} of {total} rows could not be read")
            }
            ParseError::InvalidMem(token) => write!(f, "invalid memory value: {token:?}"),
            ParseError::MissingField(field) => write!(f, "missing field {field:?}"),
            ParseError::TooLarge(what) => write!(f, "{what} does not fit in 64 bits"),
            ParseError::Int(error) => write!(f, "invalid number: {error}"),
            ParseError::Float(error) => write!(f, "invalid decimal number: {error}"),
        }
    }
}

impl std::error::Error for ParseError {}

// With these conversions, `text.parse::<u64>()?` inside a function that
// returns `Result<_, ParseError>` converts the error by itself.
impl From<ParseIntError> for ParseError {
    fn from(error: ParseIntError) -> Self {
        ParseError::Int(error)
    }
}

impl From<ParseFloatError> for ParseError {
    fn from(error: ParseFloatError) -> Self {
        ParseError::Float(error)
    }
}

/// The text source: each sample runs `vm_stat`, `sysctl` and `top`.
pub struct TextSource {
    started: Instant,
}

impl TextSource {
    pub fn new() -> TextSource {
        TextSource {
            started: Instant::now(),
        }
    }
}

impl Default for TextSource {
    fn default() -> Self {
        TextSource::new()
    }
}

impl Source for TextSource {
    fn sample(&mut self) -> Result<Sample, SourceError> {
        let system = read_system()?;
        let top = read_top()?;
        Ok(Sample {
            // Instant is monotonic: it does not jump if you change the system time.
            taken_at: Timestamp(self.started.elapsed().as_secs()),
            processes: top.processes,
            // The top rows that could not be read (at most 50).
            unreadable: top.skipped as u32,
            system,
        })
    }
}

/// Runs `top` and returns its processes.
pub fn read_top() -> Result<TopOutput, SourceError> {
    let output = run_with_timeout(TOP, &TOP_ARGS, TIMEOUT)?; // CommandError → SourceError
    let top = parse_top(&output)?; // ParseError → SourceError
    Ok(top)
}

/// Runs `vm_stat` and `sysctl` and builds the system state.
pub fn read_system() -> Result<SystemState, SourceError> {
    let vm_stat_output = run_with_timeout(VM_STAT, &[], TIMEOUT)?;
    let swap_output = run_with_timeout(SYSCTL, &["-n", "vm.swapusage"], TIMEOUT)?;
    let pressure_output = run_with_timeout(
        SYSCTL,
        &["-n", "kern.memorystatus_vm_pressure_level"],
        TIMEOUT,
    )?;

    let vm = parse_vm_stat(&vm_stat_output)?;
    Ok(SystemState {
        pressure: parse_pressure_level(&pressure_output)?,
        swap_used: parse_swapusage(&swap_output)?,
        compressor: vm.compressor,
        swapouts_pages: vm.swapouts_pages,
        page_size: vm.page_size,
    })
}

/// Parses the full output of `top -stats pid,command,mem`.
///
/// Only the rows after the LAST "PID" line count: that way, with `top -l 2`,
/// the second sample is used. Unparseable rows are ignored and counted, unless
/// they are more than half.
pub fn parse_top(output: &str) -> Result<TopOutput, ParseError> {
    let lines: Vec<&str> = output.lines().collect();
    let header = lines
        .iter()
        .rposition(|line| line.starts_with("PID"))
        .ok_or(ParseError::MissingHeader)?;

    let mut processes = Vec::new();
    let mut skipped = 0;
    for line in &lines[header + 1..] {
        if line.trim().is_empty() {
            continue;
        }
        match parse_row(line) {
            Some(process) => processes.push(process),
            None => skipped += 1,
        }
    }

    let total = processes.len() + skipped;
    if total == 0 {
        return Err(ParseError::NoRows);
    }
    if skipped * 2 > total {
        return Err(ParseError::TooManyBadRows {
            bad: skipped,
            total,
        });
    }
    Ok(TopOutput { processes, skipped })
}

/// One row: "21854  Claude Helper (R 1241M-".
///
/// The first token is the pid and the last one is MEM; what is in between is the
/// name, which may contain spaces. Returns `None` if the row does not have that shape.
fn parse_row(line: &str) -> Option<Process> {
    let (pid, rest) = line.trim().split_once(char::is_whitespace)?;
    let (name, mem) = rest.trim().rsplit_once(char::is_whitespace)?;

    let pid: u32 = pid.parse().ok()?;
    let name = name.trim().to_string();
    let footprint = parse_mem(mem).ok()?;

    Some(Process {
        key: ProcessKey::PidName(pid, name.clone()),
        pid,
        name,
        footprint,
    })
}

/// Parses a value from the MEM column: "4112M", "1271M+", "512K", "31G", "0B".
///
/// - The `+`/`-` suffix (went up/down) appears from the 2nd sample of `top -l 2` on.
/// - A decimal ("9.8G") is accepted even though we never saw one in `top`: if it
///   existed and we did not accept it, we would lose exactly the row of the
///   fastest-growing process.
pub fn parse_mem(token: &str) -> Result<Bytes, ParseError> {
    let token = token.trim().trim_end_matches(['+', '-']);
    let invalid = || ParseError::InvalidMem(token.to_string());

    let unit_size = match token.chars().last() {
        Some('B') => 1,
        Some('K') => KIB,
        Some('M') => MIB,
        Some('G') => GIB,
        _ => return Err(invalid()),
    };
    // The unit is an ASCII letter (1 byte), so removing the last byte is safe.
    let number = &token[..token.len() - 1];

    if number.contains('.') {
        let value: f64 = number.parse()?;
        let bytes = (value * unit_size as f64).round();
        // `as u64` never fails: it SATURATES. A huge value would silently become
        // u64::MAX (a false leak), so whatever does not fit is rejected first.
        if !(bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64) {
            return Err(invalid());
        }
        Ok(Bytes(bytes as u64))
    } else {
        let value: u64 = number.parse()?;
        // checked_mul returns None if the multiplication goes past u64::MAX.
        match value.checked_mul(unit_size) {
            Some(bytes) => Ok(Bytes(bytes)),
            None => Err(invalid()),
        }
    }
}

/// What is used from `vm_stat`.
#[derive(Debug, Clone, PartialEq)]
pub struct VmStat {
    pub page_size: u64,
    pub compressor: Bytes,
    pub swapouts_pages: u64,
}

/// Parses the output of `vm_stat` (no arguments: never `vm_stat N`).
///
/// Each line is "Label:   value." Only two labels matter; the rest, including
/// this hardware's MTE rows ("Pages tagged…"), are ignored.
pub fn parse_vm_stat(output: &str) -> Result<VmStat, ParseError> {
    let mut page_size = None;
    let mut compressor_pages = None;
    let mut swapouts_pages = None;

    for line in output.lines() {
        // Header: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
        if let Some((_, rest)) = line.split_once("page size of ") {
            let number = rest.split_whitespace().next().unwrap_or("");
            page_size = Some(number.parse::<u64>()?);
            continue;
        }
        // `let ... else`: if the line has no ":", it is skipped.
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_end_matches('.');
        // Some labels come in quotes: "Translation faults".
        match label.trim().trim_matches('"') {
            // NOT "Pages stored in compressor": that is the logical size (~70 GB here).
            "Pages occupied by compressor" => compressor_pages = Some(value.parse::<u64>()?),
            "Swapouts" => swapouts_pages = Some(value.parse::<u64>()?),
            _ => {}
        }
    }

    let page_size = page_size.ok_or_else(|| missing("page size"))?;
    let compressor_pages =
        compressor_pages.ok_or_else(|| missing("Pages occupied by compressor"))?;
    let swapouts_pages = swapouts_pages.ok_or_else(|| missing("Swapouts"))?;
    let compressor = compressor_pages
        .checked_mul(page_size)
        .ok_or_else(|| ParseError::TooLarge("the compressor".to_string()))?;

    Ok(VmStat {
        page_size,
        compressor: Bytes(compressor),
        swapouts_pages,
    })
}

/// Parses `sysctl -n vm.swapusage` and returns the USED swap:
/// "total = 20480.00M  used = 19300.50M  free = 1179.50M  (encrypted)".
///
/// The total is useless: macOS creates and deletes swap files on demand.
pub fn parse_swapusage(output: &str) -> Result<Bytes, ParseError> {
    let (_, rest) = output
        .split_once("used = ")
        .ok_or_else(|| missing("used"))?;
    let value = rest
        .split_whitespace()
        .next()
        .ok_or_else(|| missing("used"))?;
    // It is the same format as top's MEM column (number + unit).
    parse_mem(value)
}

/// Parses `sysctl -n kern.memorystatus_vm_pressure_level`: a single integer.
pub fn parse_pressure_level(output: &str) -> Result<Pressure, ParseError> {
    let level: u32 = output.trim().parse()?;
    Ok(Pressure::from_level(level))
}

fn missing(field: &str) -> ParseError {
    ParseError::MissingField(field.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOP_L1: &str = include_str!("../../tests/fixtures/top_l1.txt");
    const TOP_L2: &str = include_str!("../../tests/fixtures/top_l2.txt");

    fn find(top: &TopOutput, pid: u32) -> &Process {
        top.processes
            .iter()
            .find(|process| process.pid == pid)
            .expect("that pid is in the fixture")
    }

    // --- parse_mem ---

    #[test]
    fn mem_in_megabytes() {
        assert_eq!(parse_mem("4112M"), Ok(Bytes(4112 * MIB)));
    }

    #[test]
    fn mem_ignores_trailing_padding() {
        assert_eq!(parse_mem("241M  "), Ok(Bytes(241 * MIB)));
    }

    #[test]
    fn mem_ignores_growth_suffix() {
        assert_eq!(parse_mem("1271M+"), Ok(Bytes(1271 * MIB)));
        assert_eq!(parse_mem("1241M-"), Ok(Bytes(1241 * MIB)));
    }

    #[test]
    fn mem_in_kilobytes() {
        assert_eq!(parse_mem("512K"), Ok(Bytes(512 * KIB)));
    }

    #[test]
    fn mem_in_gigabytes() {
        assert_eq!(parse_mem("31G"), Ok(Bytes(31 * GIB)));
    }

    #[test]
    fn mem_in_bytes() {
        assert_eq!(parse_mem("0B"), Ok(Bytes(0)));
    }

    #[test]
    fn mem_with_one_decimal() {
        assert_eq!(parse_mem("1.5G"), Ok(Bytes(1536 * MIB)));
    }

    #[test]
    fn mem_without_unit_is_invalid() {
        assert_eq!(
            parse_mem("4112"),
            Err(ParseError::InvalidMem("4112".into()))
        );
    }

    #[test]
    fn mem_with_unknown_unit_is_invalid() {
        assert_eq!(parse_mem("12X"), Err(ParseError::InvalidMem("12X".into())));
    }

    #[test]
    fn empty_mem_is_invalid() {
        assert_eq!(parse_mem(""), Err(ParseError::InvalidMem("".into())));
    }

    #[test]
    fn unit_without_number_is_invalid() {
        assert!(matches!(parse_mem("M"), Err(ParseError::Int(_))));
    }

    #[test]
    fn mem_that_overflows_u64_is_invalid() {
        // 2^34 GiB = 2^64 bytes: one more than u64::MAX.
        let token = "17179869184G";
        assert_eq!(parse_mem(token), Err(ParseError::InvalidMem(token.into())));
    }

    #[test]
    fn decimal_mem_that_overflows_u64_is_invalid() {
        let token = "99999999999999999999.5G";
        assert_eq!(parse_mem(token), Err(ParseError::InvalidMem(token.into())));
    }

    // --- parse_top with the real fixtures ---

    #[test]
    fn top_l1_has_50_processes() {
        let top = parse_top(TOP_L1).unwrap();
        assert_eq!(top.processes.len(), 50);
        assert_eq!(top.skipped, 0);
    }

    #[test]
    fn top_l1_first_row() {
        let top = parse_top(TOP_L1).unwrap();
        let vm = &top.processes[0];
        assert_eq!(vm.pid, 70062);
        assert_eq!(vm.name, "com.apple.Virtua");
        assert_eq!(vm.footprint, Bytes(4112 * MIB));
        assert_eq!(
            vm.key,
            ProcessKey::PidName(70062, "com.apple.Virtua".into())
        );
    }

    #[test]
    fn top_l1_name_with_spaces() {
        let top = parse_top(TOP_L1).unwrap();
        assert_eq!(find(&top, 21854).name, "Claude Helper (R");
    }

    #[test]
    fn top_l1_padded_name_is_trimmed() {
        let top = parse_top(TOP_L1).unwrap();
        let claude = find(&top, 63855);
        assert_eq!(claude.name, "claude");
        assert_eq!(claude.footprint, Bytes(241 * MIB));
    }

    #[test]
    fn top_l2_uses_the_second_sample() {
        let top = parse_top(TOP_L2).unwrap();
        assert_eq!(top.processes.len(), 50);
        // In the 1st sample they were 1095M and 336M.
        assert_eq!(find(&top, 12487).footprint, Bytes(1175 * MIB));
        assert_eq!(find(&top, 571).footprint, Bytes(255 * MIB));
    }

    #[test]
    fn top_l2_values_have_no_suffix() {
        let top = parse_top(TOP_L2).unwrap();
        assert_eq!(top.skipped, 0);
        assert_eq!(find(&top, 12450).footprint, Bytes(1271 * MIB)); // "1271M+"
    }

    // --- parse_top with synthetic cases ---

    #[test]
    fn empty_output_has_no_header() {
        assert_eq!(parse_top(""), Err(ParseError::MissingHeader));
    }

    #[test]
    fn output_without_pid_line_has_no_header() {
        assert_eq!(
            parse_top("Processes: 971 total\n70062  vm 4112M\n"),
            Err(ParseError::MissingHeader)
        );
    }

    #[test]
    fn header_without_rows() {
        assert_eq!(
            parse_top("PID    COMMAND          MEM\n\n"),
            Err(ParseError::NoRows)
        );
    }

    #[test]
    fn a_few_bad_rows_are_skipped() {
        let output = "PID COMMAND MEM\n1 a 1M\n2 b 2M\n3 c 3M\ngarbage\n";
        let top = parse_top(output).unwrap();
        assert_eq!(top.processes.len(), 3);
        assert_eq!(top.skipped, 1);
    }

    #[test]
    fn exactly_half_bad_rows_is_still_ok() {
        let output = "PID COMMAND MEM\n1 a 1M\n2 b 2M\ngarbage\nx y z\n";
        assert_eq!(parse_top(output).unwrap().skipped, 2);
    }

    #[test]
    fn more_than_half_bad_rows_fails() {
        let output = "PID COMMAND MEM\n1 a 1M\ngarbage\nx y z\n";
        assert_eq!(
            parse_top(output),
            Err(ParseError::TooManyBadRows { bad: 2, total: 3 })
        );
    }

    // --- vm_stat and sysctl ---

    const VM_STAT_FIXTURE: &str = include_str!("../../tests/fixtures/vm_stat.txt");

    #[test]
    fn vm_stat_fixture() {
        let vm = parse_vm_stat(VM_STAT_FIXTURE).unwrap();
        assert_eq!(vm.page_size, 16384);
        assert_eq!(vm.compressor, Bytes(655_404 * 16384));
        assert_eq!(vm.compressor.to_string(), "10.0 GB"); // top said "10G compressor"
        assert_eq!(vm.swapouts_pages, 136_742_912); // top said "136742912(0) swapouts"
    }

    #[test]
    fn vm_stat_uses_the_page_size_of_the_header() {
        let output = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\n\
                      Pages occupied by compressor:   10.\n\
                      Swapouts:   5.\n";
        let vm = parse_vm_stat(output).unwrap();
        assert_eq!(vm.page_size, 4096);
        assert_eq!(vm.compressor, Bytes(40960));
    }

    #[test]
    fn vm_stat_without_header_fails() {
        let output = "Pages occupied by compressor:   10.\nSwapouts:   5.\n";
        assert_eq!(
            parse_vm_stat(output),
            Err(ParseError::MissingField("page size".into()))
        );
    }

    #[test]
    fn vm_stat_without_swapouts_fails() {
        let output = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                      Pages occupied by compressor:   10.\n";
        assert_eq!(
            parse_vm_stat(output),
            Err(ParseError::MissingField("Swapouts".into()))
        );
    }

    #[test]
    fn swapusage_fixture() {
        let used = parse_swapusage(include_str!("../../tests/fixtures/sysctl_swapusage.txt"));
        // Exactly 19300.50 MiB.
        assert_eq!(used, Ok(Bytes(19_300 * MIB + MIB / 2)));
        assert_eq!(used.unwrap().to_string(), "18.8 GB");
    }

    #[test]
    fn swapusage_without_swap() {
        let output = "total = 0.00M  used = 0.00M  free = 0.00M  (encrypted)";
        assert_eq!(parse_swapusage(output), Ok(Bytes(0)));
    }

    #[test]
    fn swapusage_garbage_fails() {
        assert_eq!(
            parse_swapusage("garbage"),
            Err(ParseError::MissingField("used".into()))
        );
    }

    #[test]
    fn pressure_level_fixture() {
        let level = include_str!("../../tests/fixtures/sysctl_pressure_level.txt");
        assert_eq!(parse_pressure_level(level), Ok(Pressure::Warn));
    }

    #[test]
    fn unexpected_pressure_level_is_unknown() {
        assert_eq!(parse_pressure_level("7\n"), Ok(Pressure::Unknown(7)));
    }

    #[test]
    fn pressure_level_that_is_not_a_number_fails() {
        assert!(matches!(
            parse_pressure_level("abc"),
            Err(ParseError::Int(_))
        ));
    }
}
