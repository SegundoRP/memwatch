//! Pure, STATELESS functions (see docs/design.md §4 and §6): same inputs,
//! same output. They do not read the clock or do I/O.

use crate::history::Series;
use crate::model::{Bytes, Sample};
use crate::thresholds::Thresholds;

/// What a process's memory seems to be doing (§6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Everything that is not sustained growth, including insufficient history.
    Stable,
    /// Grows like a leak, but is still below the floor (3 GiB).
    Growing,
    /// Grows like a leak and is already big.
    Leak,
}

/// The verdict for a series that `History` already trimmed to the window.
///
/// The three conditions of §6.1 are written as comparisons, never as `u64`
/// subtractions: a process that shrinks cannot cause a panic.
pub fn verdict(series: &Series, thresholds: &Thresholds) -> Verdict {
    let (Some(oldest), Some(newest)) = (series.oldest(), series.newest()) else {
        return Verdict::Stable;
    };
    let points = series.points();
    let jumps = points.len() - 1;

    // (a) Enough history: ≥ 9 min of window and ≥ 10 jumps.
    let enough_history = newest.taken_at.0 >= oldest.taken_at.0 + thresholds.min_span_secs
        && jumps >= thresholds.min_jumps;

    // (b) Recent growth: the newest ≥ the oldest + 512 MiB.
    let grew = newest.footprint.0 >= oldest.footprint.0.saturating_add(thresholds.min_growth.0);

    // (c) Sustained growth: ≥ 80 % of the jumps strictly positive.
    // `zip` with `skip(1)` walks the (previous, next) pairs.
    let mut positive = 0;
    for (previous, next) in points.iter().zip(points.iter().skip(1)) {
        if next.footprint > previous.footprint {
            positive += 1;
        }
    }
    // positive / jumps ≥ 80 / 100, without decimals: positive × 100 ≥ 80 × jumps.
    let sustained = positive * 100 >= thresholds.min_positive_percent * jumps;

    if !(enough_history && grew && sustained) {
        Verdict::Stable
    } else if newest.footprint >= thresholds.leak_floor {
        Verdict::Leak
    } else {
        Verdict::Growing
    }
}

/// Bytes per second the system sent to swap between two samples.
///
/// `None` = unknown: if no time passed or if some counter went backwards.
/// Everything uses `checked_*`: subtracting two `u64` the wrong way panics (in
/// debug) or wraps around to a huge number (in release).
pub fn swapout_rate(previous: &Sample, current: &Sample) -> Option<Bytes> {
    let seconds = current.taken_at.0.checked_sub(previous.taken_at.0)?;
    if seconds == 0 {
        return None;
    }
    let pages = current
        .system
        .swapouts_pages
        .checked_sub(previous.system.swapouts_pages)?;
    let bytes = pages.checked_mul(current.system.page_size)?;
    Some(Bytes(bytes / seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MIB, Pressure, SystemState, Timestamp};

    fn sample(taken_at: u64, swapouts_pages: u64) -> Sample {
        Sample {
            taken_at: Timestamp(taken_at),
            processes: Vec::new(),
            unreadable: 0,
            system: SystemState {
                pressure: Pressure::Normal,
                swap_used: Bytes(0),
                compressor: Bytes(0),
                swapouts_pages,
                page_size: 16384,
            },
        }
    }

    #[test]
    fn swapout_rate_of_the_design_example() {
        // 72 MiB/s for 30 s = 2160 MiB = 138_240 pages of 16 KiB.
        let rate = swapout_rate(&sample(0, 1_000), &sample(30, 1_000 + 138_240));
        assert_eq!(rate, Some(Bytes(72 * MIB)));
        assert_eq!(rate.unwrap().to_string(), "72.0 MB");
    }

    #[test]
    fn no_swapouts_is_zero() {
        assert_eq!(swapout_rate(&sample(0, 5), &sample(30, 5)), Some(Bytes(0)));
    }

    #[test]
    fn unknown_when_no_time_passed() {
        assert_eq!(swapout_rate(&sample(30, 5), &sample(30, 9)), None);
    }

    #[test]
    fn unknown_when_the_counter_goes_backwards() {
        assert_eq!(swapout_rate(&sample(0, 9), &sample(30, 5)), None);
    }

    #[test]
    fn unknown_when_time_goes_backwards() {
        assert_eq!(swapout_rate(&sample(60, 5), &sample(30, 9)), None);
    }

    // --- verdict: each row of the §6.1 table is a test ---

    use crate::history::Point;

    /// A series with one sample every 30 s, with these footprints in MiB.
    fn series(footprints_mib: &[u64]) -> Series {
        let mut series = Series::default();
        for (i, mib) in footprints_mib.iter().enumerate() {
            series.push(Point {
                taken_at: Timestamp(i as u64 * 30),
                footprint: Bytes(mib * MIB),
            });
        }
        series
    }

    /// `count` samples that start at `start` and rise by `step` each.
    fn ramp(start: u64, step: u64, count: u64) -> Vec<u64> {
        (0..count).map(|i| start + i * step).collect()
    }

    fn judge(footprints_mib: &[u64]) -> Verdict {
        verdict(&series(footprints_mib), &Thresholds::default())
    }

    #[test]
    fn july_leak_is_a_leak_at_minute_nine() {
        // 1 GB + 190 MiB every 30 s. 19 samples = 540 s and 18 jumps.
        let leak = ramp(1024, 190, 19);
        assert_eq!(*leak.last().unwrap(), 4444); // ~4.3 GB
        assert_eq!(judge(&leak), Verdict::Leak);
    }

    #[test]
    fn july_leak_before_minute_nine_is_not_judged_yet() {
        // 18 samples = 510 s: fails (a).
        assert_eq!(judge(&ramp(1024, 190, 18)), Verdict::Stable);
    }

    #[test]
    fn the_same_shape_below_three_gib_is_growing() {
        assert_eq!(judge(&ramp(100, 100, 19)), Verdict::Growing);
    }

    #[test]
    fn docker_vm_boots_and_stays_flat() {
        // Rises to 4.1 GB in 2 min and stays flat: fails (c).
        let mut vm = vec![0, 1000, 2000, 3000, 4112];
        vm.extend([4112; 16]);
        assert_eq!(judge(&vm), Verdict::Stable);
    }

    #[test]
    fn single_step_then_flat() {
        let mut step = vec![1024];
        step.extend([4096; 20]);
        assert_eq!(judge(&step), Verdict::Stable);
    }

    #[test]
    fn big_stable_process_with_noise() {
        let noisy: Vec<u64> = (0..21)
            .map(|i| if i % 2 == 0 { 3500 } else { 3520 })
            .collect();
        assert_eq!(judge(&noisy), Verdict::Stable);
    }

    #[test]
    fn sawtooth_three_up_one_down() {
        // +300, +300, +300, −600: grows 1500 MiB in the window, but only 75 %
        // of the jumps are positive: fails (c).
        let mut sawtooth = vec![4000];
        for i in 0..20 {
            let last = *sawtooth.last().unwrap();
            sawtooth.push(if i % 4 == 3 { last - 600 } else { last + 300 });
        }
        assert_eq!(judge(&sawtooth), Verdict::Stable);
    }

    #[test]
    fn exactly_eighty_percent_positive_is_still_a_leak() {
        // 16 jumps of +190 and 4 of 0: 16 × 100 = 80 × 20.
        let mut window = ramp(4000, 190, 17);
        window.extend([*window.last().unwrap(); 4]);
        assert_eq!(judge(&window), Verdict::Leak);
    }

    #[test]
    fn a_leak_that_stops_is_not_a_leak_after_five_flat_samples() {
        // 15 positive jumps and 5 at zero (2.5 min flat): 75 % < 80 %.
        let mut window = ramp(4000, 190, 16);
        window.extend([*window.last().unwrap(); 5]);
        assert_eq!(judge(&window), Verdict::Stable);
    }

    #[test]
    fn a_shrinking_process_is_stable_and_does_not_panic() {
        assert_eq!(judge(&ramp(5120, 0, 1)), Verdict::Stable);
        let shrinking: Vec<u64> = (0..21).map(|i| 5120 - i * 90).collect();
        assert_eq!(judge(&shrinking), Verdict::Stable);
    }

    #[test]
    fn two_samples_600_seconds_apart_are_not_enough() {
        let mut two = Series::default();
        two.push(Point {
            taken_at: Timestamp(0),
            footprint: Bytes(1024 * MIB),
        });
        two.push(Point {
            taken_at: Timestamp(600),
            footprint: Bytes(8192 * MIB),
        });
        assert_eq!(verdict(&two, &Thresholds::default()), Verdict::Stable);
    }

    #[test]
    fn an_empty_series_is_stable() {
        assert_eq!(judge(&[]), Verdict::Stable);
    }
}
