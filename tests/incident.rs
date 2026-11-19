//! Simulation of the July incident with `FakeSource` and `FakeNotifier`
//! (docs/design.md §10). Runs 60 minutes of cycles in microseconds.
//!
//! It is an integration test: it lives in `tests/` and only uses the crate's
//! public API, as `main.rs` would.

use memwatch::model::{Bytes, MIB, Pressure, Process, ProcessKey, Sample, SystemState, Timestamp};
use memwatch::notify::FakeNotifier;
use memwatch::source::fake::FakeSource;
use memwatch::thresholds::Thresholds;
use memwatch::watch::{Watcher, cycle};

const CODEX: u32 = 48213;
const VM: u32 = 70062;
const SIX_GIB_IN_MIB: u64 = 6 * 1024;

fn process(pid: u32, name: &str, mib: u64) -> Process {
    Process {
        key: ProcessKey::PidName(pid, name.to_string()),
        pid,
        name: name.to_string(),
        footprint: Bytes(mib * MIB),
    }
}

/// One cycle every 30 s, with the Docker VM always at 4112 MiB and codex with the
/// given footprint (`None` = codex does not show up in that sample).
fn script(codex_mib: &[Option<u64>]) -> Vec<Option<Sample>> {
    let mut script = Vec::new();
    for (i, codex) in codex_mib.iter().enumerate() {
        let mut processes = vec![process(VM, "com.apple.Virtua", 4112)];
        if let Some(mib) = codex {
            processes.push(process(CODEX, "codex", *mib));
        }
        script.push(Some(Sample {
            taken_at: Timestamp(i as u64 * 30),
            processes,
            unreadable: 0,
            system: SystemState {
                pressure: Pressure::Normal,
                swap_used: Bytes(0),
                compressor: Bytes(0),
                swapouts_pages: 0,
                page_size: 16384,
            },
        }));
    }
    script
}

struct Outcome {
    /// Which cycles had a notification, with its title and its text.
    notifications: Vec<(usize, String, String)>,
    log: Vec<String>,
}

fn run(script: Vec<Option<Sample>>) -> Outcome {
    let mut source = FakeSource::new(script);
    let mut notifier = FakeNotifier::default();
    let mut watcher = Watcher::new(Thresholds::default());
    let mut notifications = Vec::new();
    let mut log = Vec::new();

    let mut i = 0;
    while source.remaining() > 0 {
        let before = notifier.sent.len();
        let output = cycle(&mut watcher, &mut source, &mut notifier);
        log.extend(output.log);
        for (title, body) in &notifier.sent[before..] {
            notifications.push((i, title.clone(), body.clone()));
        }
        i += 1;
    }
    Outcome { notifications, log }
}

/// codex at 1 GB, growing 190 MiB every 30 s.
fn july_leak(cycles: u64) -> Vec<Option<u64>> {
    (0..cycles).map(|i| Some(1024 + 190 * i)).collect()
}

fn cycles_with_alerts(outcome: &Outcome) -> Vec<usize> {
    outcome.notifications.iter().map(|(i, _, _)| *i).collect()
}

#[test]
fn cold_start_leak_alerts_before_six_gib() {
    let codex = july_leak(120);
    let outcome = run(script(&codex));

    let (first, title, body) = &outcome.notifications[0];
    assert_eq!(*first, 18); // minute 9
    assert_eq!(codex[*first], Some(4444)); // ~4.3 GB
    assert!(codex[*first].unwrap() < SIX_GIB_IN_MIB);
    assert_eq!(title, "memwatch: possible leak in codex");
    assert!(body.starts_with("codex grew 3.3 GB in 9 min and now uses 4.3 GB."));
    assert!(body.ends_with("kill -9 48213"));
}

#[test]
fn leak_after_thirty_noisy_minutes_alerts_before_six_gib() {
    // 30 min stable at ~1 GB with alternating +20/−20 MiB noise, and then the leak.
    let mut codex: Vec<Option<u64>> = (0..60)
        .map(|i| Some(if i % 2 == 0 { 1024 } else { 1044 }))
        .collect();
    codex.extend((1..=60).map(|k| Some(1024 + 190 * k)));

    let outcome = run(script(&codex));
    let first = cycles_with_alerts(&outcome)[0];
    let footprint = codex[first].unwrap();
    assert!(
        footprint < SIX_GIB_IN_MIB,
        "alerted only at {footprint} MiB"
    );
    assert!(
        footprint >= 3 * 1024,
        "alerted below the floor: {footprint} MiB"
    );
}

#[test]
fn realerts_need_two_more_gib_or_thirty_minutes() {
    let codex = july_leak(120);
    let alerts = cycles_with_alerts(&run(script(&codex)));
    assert!(alerts.len() >= 2);

    let mib = |i: usize| codex[i].unwrap();
    for pair in alerts.windows(2) {
        let (previous, next) = (pair[0], pair[1]);
        let enough = |i: usize| mib(i) >= mib(previous) + 2048 || (i - previous) * 30 >= 1800;
        assert!(enough(next), "early re-alert at cycle {next}");
        assert!(!enough(next - 1), "the re-alert at cycle {next} came late");
    }
}

#[test]
fn a_leak_that_stops_alerts_once_and_goes_back_to_normal() {
    let mut codex = july_leak(25); // up to 5584 MiB
    codex.extend([Some(1024 + 190 * 24); 90]); // 45 min flat

    let outcome = run(script(&codex));
    assert_eq!(cycles_with_alerts(&outcome), vec![18]);
    assert!(
        outcome
            .log
            .iter()
            .any(|line| line.contains("codex (pid 48213) back to Normal"))
    );
}

#[test]
fn docker_vm_boot_and_sawtooth_never_alert() {
    let mut vm: Vec<Option<u64>> = vec![Some(0), Some(1000), Some(2000), Some(3000)];
    vm.extend([Some(4112); 116]);
    assert!(run(script(&vm)).notifications.is_empty());

    let mut sawtooth = vec![Some(4000)];
    for i in 0..120 {
        let last = sawtooth.last().unwrap().unwrap();
        sawtooth.push(Some(if i % 4 == 3 { last - 600 } else { last + 300 }));
    }
    assert!(run(script(&sawtooth)).notifications.is_empty());
}

#[test]
fn a_process_missing_from_one_sample_keeps_its_series_and_state() {
    let mut codex = july_leak(40);
    codex[10] = None; // before the first alert
    codex[20] = None; // after the first alert
    // Without the series, the alert would come late; without the state, it would repeat at 21.
    assert_eq!(cycles_with_alerts(&run(script(&codex))), vec![18, 29]);
}

#[test]
fn ten_failed_cycles_alert_once_and_recovery_is_logged() {
    let mut script = script(&july_leak(5));
    script.extend(std::iter::repeat_n(None, 15));
    script.push(script[4].clone());

    let outcome = run(script);
    let titles: Vec<&str> = outcome
        .notifications
        .iter()
        .map(|(_, title, _)| title.as_str())
        .collect();
    assert_eq!(titles, vec!["memwatch: cannot read memory"]);
    assert!(
        outcome
            .log
            .contains(&"sampling recovered after 15 failed cycles".to_string())
    );
}
