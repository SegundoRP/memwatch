# memwatch: design document

- **Status:** implemented (M0–M6) and running. §12 lists what changed during implementation.
- **Platform:** macOS 26 (Tahoe) on Apple Silicon. Nothing else.
- **Related notes:**
  - [`notes/m5-launchd.md`](notes/m5-launchd.md): the LaunchAgent.
  - [`notes/m6-ffi.md`](notes/m6-ffi.md): the FFI source.
  - [`notes/verified-facts.md`](notes/verified-facts.md): the evidence behind each technical fact.
- **Units:** across the whole project, "KB/MB/GB" mean KiB/MiB/GiB (base 1024), the same as `top` and Activity Monitor.

---

## 1. Problem

This Mac (M5, 24 GB) has already had both problems that `memwatch` must detect:

1. **A process leak (2026-07-29).** ChatGPT.app's `codex app-server` reached a `phys_footprint_peak` of 31 GB. The compressor rose to 12 GB, swap thrashing reached ~400 MB/s and `kernel_task` hit 81 % CPU. It was noticed late, when the machine was already slow and hot.
2. **Death by a thousand cuts (2026-10-28).** No process was leaking (codex was at 1.1 GB), but swap was at 24.3 of 25.6 GB, pressure was at WARN and 372 MB were free. It was the sum of several processes: the Docker VM (4.1 GB), two Firefox instances with their `plugin-container` processes (~7 GB), Claude, Codex and Chrome.

Today nothing warns about either one.

## 2. Goals and non-goals

**Goals**

- Detect a fast process leak in time: in a case like the July one, before 6 GB and not at 31.
- Warn when the system enters memory pressure and say who is causing it.
- Warn with a macOS notification plus a log line, always running in the background (LaunchAgent).

**Non-goals**

- **It never kills processes.** It only warns; the alert includes the `kill` command so that you decide.
- **Slow leaks** (less than ~50 MB/min): the per-process path does not detect them (§6.5). The system pressure path covers them once they affect the machine.
- **A separate swap-thrashing alert:** out of scope for v1. The swapout rate is shown in `watch` and inside the pressure alert. It will be reconsidered with the M5 data, only if thrashing shows up while pressure is Normal.
- No root daemon and no `sudo`.
- No configuration file in v1: the thresholds live in `Thresholds::default()`.
- No GUI and no menu bar icon. No support for Linux, Intel or Rosetta.
- **No external dependencies until M6** (only `std`). M6 adds only `libc`.

## 3. The metric: `phys_footprint`, not RSS

This is the finding that weighed most in the design:

- **RSS** counts the pages resident in RAM. It is what `ps` reports and also what `Process::memory()` from the `sysinfo` crate returns on macOS. When a leaking process starts to be compressed or to go to swap, **its RSS drops**. Measured on this Mac: codex had between 56 and 118 MB of RSS versus ~1 GB of footprint.
- **`phys_footprint`** counts the anonymous memory of the process, **including compressed and swapped memory**, at its uncompressed size. jetsam uses it, and according to `man footprint` it is what the MEM column of `top`, the Memory column of Activity Monitor and the Xcode memory gauge show.
- **Consequence:** the footprint **can exceed RAM**. It reached 31 GB on a 24 GB Mac, and the sum of the footprints of your processes came to 72.6 GB. The thresholds in this document measure footprint, not "used RAM".

That is why the `sysinfo` crate is ruled out as the source of per-process memory.

## 4. Architecture

```
memwatch/
├── src/main.rs        ← thin: reads the args, wires up the real pieces and calls the lib
├── src/lib.rs
├── src/model.rs       ← Bytes, Timestamp, ProcessKey, Process, Pressure, SystemState, Sample
├── src/command.rs     ← run (M1) → run_with_timeout (M4b): runs top / sysctl / vm_stat / osascript
├── src/report.rs      ← top_n and the `memwatch report` table (pure)
├── src/source/        ← trait Source: produces one Sample per cycle
│   ├── text.rs        ← runs top / sysctl / vm_stat and parses (the parsers are pure functions)
│   ├── fake.rs        ← scripted samples for the tests
│   └── ffi.rs         ← M6: libc straight to the kernel
├── src/history.rs     ← bounded window of samples per process
├── src/detect.rs      ← pure and stateless functions: verdict of a series, swapout rate
├── src/alerts.rs      ← ALL the state: per process, system and cooldowns. Decides WHEN to warn
├── src/notify.rs      ← trait Notifier: Osascript, Log, Fake
└── src/watch.rs       ← the loop: sample → record → detect → warn → sleep
```

```
Source ──Sample──▶ History ──Series──▶ detect ──Verdict──▶ alerts ──Alert──▶ Notifier
 (I/O)            └──────── pure core: no I/O, testable without a Mac ──────┘     (I/O)
```

**Decisions that define the shape**

1. **Pure core.** Nothing between `Source` and `Notifier` does I/O or reads the clock. Inside the core, `detect` has no state (same inputs, same output) and `alerts` is the only module that remembers things between cycles.
2. **Time travels inside the `Sample`** as a `Timestamp`: monotonic seconds since memwatch started. The real source computes it with `std::time::Instant` and the fakes make it up. That makes the tests deterministic: you can simulate 40 minutes of leaking in microseconds. Also, launchd coalesces timers, so the code never assumes that exactly 30 s have passed: it always uses the timestamps.
3. **The `Source` decides what "the same process" is** and stores it in `Process::key` (a `ProcessKey`, see §5). `History` and `alerts` use **only** `key` as the key and **never** `match` on it. That way, when the FFI source in M6 changes the identity from `(pid, name)` to `(pid, start time)`, the core is not touched.
4. **Degrade, don't die.** A watchdog that crashes exactly when there is memory pressure is useless (see §9).
5. **Three subcommands**, read with `std::env::args` (no `clap`):
   - `memwatch report`: a snapshot of the moment (heaviest processes and system state).
   - `memwatch watch`: the loop. It prints one line per cycle **only if stdout is a terminal** (`std::io::IsTerminal`); under launchd it writes nothing per cycle.
   - `memwatch notify-test`: sends a notification exactly like the one for a real alert.

## 5. Data model

**Core types** (`src/model.rs`):

```rust
/// Display in base 1024, no decimals for B and 1 decimal from KB up.
/// The unit is chosen AFTER rounding, and GB is the largest unit.
/// E.g.: 1023 → "1023 B", 1024 → "1.0 KB", 1_048_575 → "1.0 MB" (not "1024.0 KB"),
///      1_181_116_006 → "1.1 GB".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub u64);

/// Monotonic seconds since memwatch started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub u64);

/// Identity of a process. ONLY the Source builds it.
/// M6 adds the PidStart(u32, u64) variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProcessKey { PidName(u32, String) }

#[derive(Debug, Clone, PartialEq)]
pub struct Process {
    pub key: ProcessKey,
    pub pid: u32,
    pub name: String,      // in the text source, top truncates it to 16 characters
    pub footprint: Bytes,
}

/// Value of kern.memorystatus_vm_pressure_level: 1, 2, 4 or anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure { Normal, Warn, Critical, Unknown(u32) }

#[derive(Debug, Clone, PartialEq)]
pub struct SystemState {
    pub pressure: Pressure,
    pub swap_used: Bytes,
    pub compressor: Bytes,     // "Pages occupied by compressor" × page_size
    pub swapouts_pages: u64,   // cumulative counter since boot
    pub page_size: u64,        // from the vm_stat header (16384 on Apple Silicon)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub taken_at: Timestamp,
    pub processes: Vec<Process>,
    pub unreadable: u32,       // processes the source could not read (always 0 in the text source; M6 uses it)
    pub system: SystemState,
}
```

**Also:**

- `ParseError`, with `Display`, `Error`, `From<ParseIntError>` and `From<ParseFloatError>`, so that `?` works in the parsers.
- A `run_with_timeout(program, args, timeout) -> Result<String, CommandError>` helper that runs a command and returns its output (§9).

**Unsigned arithmetic:** `Bytes` wraps a `u64`, which is unsigned. `a - b` with `b > a` **panics in `cargo test`** and in `--release` wraps around to a huge number, which would produce a false leak. That is why the rules in §6 are written as **comparisons** (`b ≥ a + 512 MiB`) and not as subtractions. Where a subtraction is needed, the code uses `checked_sub` or `saturating_sub`.

**Other types** (the history and its series, `Thresholds`, the `alerts` state, the `Source` and `Notifier` traits, `Alert` and the source errors) live in their modules. The design fixes only two things:

- `enum Verdict { Stable, Growing, Leak }`, with the semantics of §6.1.
- The per-process alert state is an `enum`. A suggested shape, not a required one: `Normal | Leaking { … }`.

## 6. Detection rules

There are two independent detection paths and one layer on top that decides when to interrupt you.

### 6.1 Process leak (`detect`, pure)

Each cycle (30 s) adds a sample `(taken_at, footprint)` to the process's series. The series keeps only the **window** of the last 10 min: the samples with `taken_at ≥ now − 600 s`, where `now` is the `taken_at` of the current cycle's `Sample` (not the newest sample in the series, which can be old if the process stopped showing up).

Over the window, looking at the **jumps** between pairs of consecutive samples:

- **(a) Enough history:** the window spans at least 9 min (`newest.taken_at ≥ oldest.taken_at + 540 s`) **and** has at least 10 jumps. The 1 min margin absorbs timer drift; the minimum number of jumps avoids judging sparse samples (after failed cycles).
- **(b) Recent growth:** `newest.footprint ≥ oldest.footprint + 512 MiB`.
- **(c) Sustained growth:** at least **80 %** of the jumps are **strictly positive** (`next > previous`). A jump of 0 does **not** count as growth.

| Verdict | Condition |
|---|---|
| `Leak` | (a) **and** (b) **and** (c) **and** `current footprint ≥ 3 GiB` |
| `Growing` | (a) **and** (b) **and** (c), but `current footprint < 3 GiB` |
| `Stable` | everything else, including insufficient history |

`Growing` is only visible in `watch`, because `report` is a snapshot without history. It never produces a notification.

**Why these three conditions together.** Every series shape that is not a leak fails at least one of them:

| Series shape | Verdict | Condition that fails |
|---|---|---|
| The July leak, from the moment memwatch starts (+190 MiB per sample) | `Leak` at minute 9, at ~4.3 GB | none |
| The July leak after 30 min stable with noise | `Leak` at ~3.0–3.2 GB, barely crosses the floor | none |
| The Docker VM boots (rises to 4.1 GB in 2 min) and stays flat | `Stable` | (c): the jumps are 0 |
| A single step from 1 to 4 GB, then flat | `Stable` | (c) |
| Large stable process with ±20 MB of noise | `Stable` | (b) and (c) |
| Sawtooth: 3 rises and 1 drop of 600 MB | `Stable` | (c): 75 % < 80 % |
| A leak that stops | stops being `Leak` in ~2.5 min | (c) |
| Process that shrinks from 5 GB to 3.2 GB | `Stable` | (b); and since it is a comparison, there is no panic |

**A series is deleted when its window becomes empty**, that is, when its process has not shown up for 10 min. If a process is missing from a single sample (it briefly dropped out of the top 50 or a row could not be parsed), its series and its alert state survive. A failed cycle produces no `Sample` and changes nothing.

### 6.2 System pressure

- **Level:** it comes from `kern.memorystatus_vm_pressure_level` and is converted with `Pressure::from_level`. The severity is Normal < Warn < Critical; `Unknown` has no severity.
- **Confirmed level:** when the **last 2 samples** agree on a known level, that level is confirmed. If either of the two is `Unknown`, nothing is confirmed and the value is written to the log. The kernel applies hysteresis and the level is "sticky"; this debounce avoids noise.
- **At startup there is no confirmed level.** The first confirmation **does not notify**: it only logs "started with pressure WARN". That way, restarting memwatch (reinstall, `kickstart`, `KeepAlive`) does not send you a banner every time.
- **When the confirmed level changes:**
  - If it **worsens** (Normal→Warn, Warn→Critical or Normal→Critical), it notifies, unless that same level was already notified less than 15 min ago (cooldown per level, which expires at `≥ 900 s`).
  - If it **improves**, it is only logged.
  - In both cases, the confirmed level is updated.
- **Change on 2026-11-18:** it only notifies on reaching CRITICAL (`Thresholds::pressure_notify_from`); worsening to WARN only goes to the log. On the first day installed there were 17 WARN alerts in 27 h, almost all with swapout 0: on this Mac, WARN is the normal state. The alert texts were also rewritten so that they say first what is happening and then what to do.
- **Swapout rate** (informational): `Δswapouts_pages × page_size / Δt`, in MiB/s. It is **unknown** if there is no previous sample, if `Δt = 0` or if the counter goes backwards (this should not happen, because a reboot restarts memwatch; it is only defensive). It is shown in `watch` and in the pressure alert, as `swapout ?` when it is unknown.
- **Contents of the pressure alert:** whether the Mac is writing to disk right now (the swapout rate, when it is known and above 0), the used swap, and **the 5 processes with the largest footprint**. The `watch` status line also shows the level and the compressor, and prints `swapout ?` when the rate is unknown.
- **What is NOT used:** the used swap / total swap ratio. macOS creates and deletes 1 GiB swap files on demand, so the total changes and the ratio lies.

### 6.3 When to warn (`alerts`, stateful)

**Per process:**

- **Normal → Leaking** with the first `Leak`: it notifies.
- **In Leaking, with a `Leak` verdict in this cycle**, it notifies again if `footprint ≥ last_alert_footprint + 2 GiB` **or** `now ≥ last_alert + 1800 s`.
- **In Leaking, with a verdict other than `Leak`:** nothing is notified. If **10 min in a row** pass without `Leak` (`now ≥ first_cycle_without_leak + 600 s`), it goes back to Normal and this is logged. A `Leak` in between restarts that count.
- When the series is deleted (§6.1), the state is deleted too. In v1 this is not notified.

**System:** the confirmed level, the previous sample (for the rate) and the per-level cooldown from §6.2.

**Source:** the counter of consecutive failed cycles (§9).

### 6.4 Default thresholds (`Thresholds::default()`)

| Threshold | Value |
|---|---|
| Sampling interval | 30 s |
| Series window | 10 min (`≥ now − 600 s`, with `now` = `taken_at` of the current cycle) |
| Enough history | spans ≥ 540 s **and** ≥ 10 jumps |
| Minimum recent growth | 512 MiB in the window |
| Strictly positive jumps | ≥ 80 % |
| Minimum footprint for `Leak` | 3 GiB |
| Re-alert on growth | +2 GiB |
| Re-alert on time | 1800 s |
| Back to Normal | 600 s without `Leak` |
| Pressure debounce | 2 matching samples |
| Cooldown per pressure level | 900 s |
| Processes in the pressure alert | 5 |
| Processes read from `top` | 50 |
| Timeout for `top` / `sysctl` / `vm_stat` | 10 s (`top -l 1` takes ~1.3 s) |
| Timeout for `osascript` | 5 s |
| Consecutive failed cycles before alerting | 10 |

These values come from two incidents and are an initial estimate. In M5 they are calibrated with a week of real data and this table is updated.

### 6.5 Known limits of the rules

- **Leaks slower than ~50 MB/min** (512 MiB in 10 min) do not give `Leak`. They are left to the pressure path.
- **Some slow sawtooth patterns**, with at least 80 % rises and more than 512 MiB of amplitude within 10 min, cannot be told apart from a short-term leak and can trigger an alert. The spaced-out re-alerts limit the noise. To be reviewed with the M5 data.
- **A "staircase" leak** (grows, pauses for several minutes, grows again) may not reach 80 %. Same as above: the pressure path remains, and the M5 review.
- **Above ~10 GB, `TextSource` sees the footprint in 1 GiB steps** (§7). A leak of 190 MiB every 30 s would show almost all jumps as 0, so it does not reach the 80 % of rule (c) and the verdict stops being `Leak`. The first alert is not affected (it fires between 3 and 4.3 GB), but the +2 GiB re-alert and the return to Normal are: **review in M3/M4a**. With the M6 FFI source (exact bytes) the problem goes away. **Resolved:** since M6, `watch` uses the FFI source by default.

## 7. Text source (`TextSource`, M1–M5)

Always with absolute paths, because launchd runs with `PATH=/usr/bin:/bin:/usr/sbin:/sbin`:

| Data | Command |
|---|---|
| Processes | `/usr/bin/top -l 1 -o mem -n 50 -stats pid,command,mem` |
| Compressor, swapouts, page size | `/usr/bin/vm_stat` (snapshot mode only; **never** `vm_stat N`) |
| Used swap | `/usr/sbin/sysctl -n vm.swapusage` |
| Pressure level | `/usr/sbin/sysctl -n kern.memorystatus_vm_pressure_level` |

**Parsing rules** (taken from the real fixtures, Appendix A):

- **`top`:**
  - Only the rows that come **after the last** line starting with `PID` are used; everything before is ignored. That way `top_l2.txt`, which has two samples, gives 50 processes with the values of the second one.
  - Empty lines are skipped.
  - In each row, the first token is the pid and the last one is MEM. Everything in between is the name: it can contain spaces and is truncated to 16 characters (`"Claude Helper (R"`, `"com.apple.Virtua"`). It is trimmed.
  - A row that cannot be parsed is ignored and counted. If more than half of the rows fail, the cycle counts as failed.
- **MEM:** a number plus the unit `K`/`M`/`G` (or `B`), with an optional `+`/`-` suffix that appears from the second sample of `top -l 2` on. Rows have trailing padding spaces. The fixtures only have `M`; the `K`, `G` and `B` cases are covered by synthetic tests. A decimal (`9.8G`) is also accepted: we never saw one, but if `top` used it near 10 GB and we did not accept it, we would lose exactly the row of the leaking process.
- **`vm_stat`:**
  - The page size comes from the header `(page size of 16384 bytes)`.
  - Each line has the form `Label:   value.`: the key is the text before `:` (some are quoted, like `"Translation faults"`) and the trailing period is removed from the value.
  - **`Pages occupied by compressor`** is used (not `Pages stored in compressor`, which is the logical size, ~70 GB here), and so is **`Swapouts`**.
  - Unknown lines are ignored. On this hardware there are MTE rows (`Pages tagged…`) that do not exist on other Macs.
- **`vm.swapusage`:** the format is `total = 20480.00M  used = 19300.50M  free = 1179.50M  (encrypted)`, in MiB with 2 decimals and two spaces between fields.
- **Pressure:** a single integer.
- **Page size:** taken from the `vm_stat` header, which is the kernel page. **Never** from `hw.pagesize`, which is the process page and is 4096 under Rosetta.

**Known limitations** (accepted for v1):

- Names are truncated to 16 characters.
- **MEM resolution:** `top` shows up to 4 digits in `M` (`9123M`) and from ~10 GB on it switches to whole `G` (`10G`), so above 10 GB the resolution is 1 GiB. This is an **inference** drawn from the header fields (`PhysMem`, `VM`), not from a process row. See §6.5.
- The `PidName` key does not detect a PID being reused for a process with the same name (this is rare).
- Only the 50 heaviest processes are read. One that starts leaking from very small enters the history late, which is acceptable with a 3 GiB floor.
- `codex app-server` cannot be told apart from `codex exec-server`: they have the same name.
- The text format can change with a macOS update. The fixture tests catch it; if it happens, the fixtures are captured again.
- Advantage over the FFI source: `top` also sees the processes of other users and of root, because it has a private Apple entitlement (`com.apple.system-task-ports.read`).

The M6 FFI source is in [`notes/m6-ffi.md`](notes/m6-ffi.md).

## 8. Notification and log

### 8.1 `OsascriptNotifier`

```rust
Command::new("/usr/bin/osascript")
    .args(["-e", "on run argv",
           "-e", "display notification (item 2 of argv) with title (item 1 of argv)",
           "-e", "end run", "--"])
    .arg(title)
    .arg(body)
```

- The text travels **only as argv**. It is never interpolated into the script, because that would allow AppleScript injection.
- The `--` is **required**: if the title starts with `-`, `osascript` takes it as an option and exits with code 2 without running the script.
- `osascript` exiting with code 0 does **not** mean that the banner was shown. That is why every alert is also written to the log.
- It runs with the timeout from §9.

**Manual setup, only once** (only you can do it; memwatch cannot request the permission):

1. Open Script Editor.app, run `display notification "memwatch test"` and click **Allow**.
2. Go to System Settings → Notifications → Script Editor and choose the **Persistent** style. On Tahoe, with the Temporary style the notification often only reaches Notification Center, without a banner.
3. Run `memwatch notify-test` from the terminal. The channel only counts as working when you see the banner.

**Known limits:** the banner appears as if it came from Script Editor, clicking it opens Script Editor, and a Focus mode can hide it.

**Plan B** if `osascript` does not work on this Mac: see [`notes/m5-launchd.md`](notes/m5-launchd.md).

**Examples.** Names are shown as the source gives them, except two aliases: `com.apple.Virtualization…` is shown as "virtual machine" and `plugin-container` as "Firefox tab". The growth in the leak alert ("grew 3.7 GB in 10 min") is the difference between the newest and the oldest sample in the window. The format was rewritten in plain language on 2026-11-18: each alert says first what is happening and then what to do.

```
memwatch: possible leak in codex
codex grew 3.7 GB in 10 min and now uses 9.6 GB.
If you don't need it, quit it or run: kill -9 48213
```

```
memwatch: memory is running out
The Mac is writing to disk at 72.0 MB/s (swap 18.8 GB). Using the most: virtual machine 4.0 GB, node 1.7 GB, Firefox tab 1.7 GB.
```

When the swapout rate is unknown or 0, the pressure body starts with `The Mac is using the disk as RAM (swap …)` instead.

### 8.2 Log

- Each alert, each degraded cycle and each relevant state change (startup, recovery, return to Normal) leaves a line on stderr. `eprintln!` is used because stderr is unbuffered.
- Under launchd, stderr goes to a file that launchd does not rotate. That is why, at startup, if that log is over 10 MB, it is truncated. With stdout quiet under launchd (§4), this is a few KB per day.

## 9. Error handling

| Failure | Behavior |
|---|---|
| `top`, `sysctl` or `vm_stat` fail or exceed the timeout | It is logged and the cycle is skipped (no `Sample`, nothing changes) |
| 10 consecutive failed cycles | **One** alert: "memwatch: cannot read memory". The counter resets with the first successful cycle, which also logs "recovered" and re-arms the alert |
| More than half of the `top` rows cannot be parsed | Failed cycle |
| `osascript` fails or exceeds the timeout | It is logged and the loop continues |
| Panic | No `unwrap`/`expect` in the loop, except documented invariants. If there is a panic anyway, launchd restarts the process (`KeepAlive`) |

**Command timeout (M4b):** a shared helper, `run_with_timeout`, used by the text source and by `osascript`. It works like this:

1. It launches the command with `spawn` and stdout through a pipe.
2. It polls `try_wait` until the deadline.
3. If the deadline passes, it calls `kill()` **and then `wait()`**, so that no zombie processes are left in an agent that runs 24/7.
4. The output is read when the process ends. This is safe because the outputs are under 5 KB, well below the pipe buffer.

**Error types:** they are custom, with `Display` and `std::error::Error` implemented by hand, without `anyhow` or `thiserror`.

## 10. Testing strategy

- **Unit tests** inside each module (`#[cfg(test)] mod tests` with `use super::*;`).
- **Real fixtures** in `tests/fixtures/`, loaded with `include_str!` (Appendix A).
- **Synthetic cases** for what is not in the fixtures: MEM with `K`, `G` and `B`, empty output, garbage rows and a missing `PID` line.
- **Verdict (§6.1):** each row of the "why these three conditions" table is a test, plus these edge cases:
  - a series of 2 samples 600 s apart;
  - a gap of more than 10 min before the last sample;
  - a sample exactly on the edge of the window (600 s).
- **Incident simulation with `FakeSource` and `FakeNotifier`** (the star test of M4a):
  - codex starts at 1 GB and grows 190 MiB every 30 s: the first alert fires **before 6 GB**.
  - The same leak, but after 30 min stable at ~1 GB with alternating +20/−20 MB noise (deterministic): the alert also fires before 6 GB.
  - After the first alert, there is no other one until +2 GiB or 1800 s.
  - A leak that stops no longer gives `Leak` and produces no time-based re-alerts.
  - The VM that rises and stays flat, and the sawtooth, never alert.
  - A process missing from a single sample does not lose its series.
- **Pressure tests (§6.2):**
  - start at WARN,WARN → no alert (log only); then CRITICAL,CRITICAL → alert;
  - Normal,Warn (a single sample at Warn) → no alert;
  - Normal,Normal,Warn,Warn → alert;
  - Warn→Normal → no alert;
  - back to Warn within 900 s of the previous Warn alert → no alert;
  - Normal,Normal,Unknown,Unknown,Warn,Warn → the `Unknown`s confirm nothing and the alert fires with the Warn,Warn pair (the second Normal is needed: without a confirmed level, the first confirmed pair does not notify);
  - unknown swapout rate with `Δt = 0` and with the counter going backwards.
- **Gate for each milestone:** `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.

## 11. Milestones

All milestones are implemented. Each one left something usable.

| # | Scope | Acceptance criteria |
|---|---|---|
| **M0** | Toolchain (§13), `cargo init`, `src/lib.rs`; `Display` for `Bytes` | `cargo test` green with: 0 → `0 B`, 1023 → `1023 B`, 1024 → `1.0 KB`, 1_048_575 → `1.0 MB`, 1_181_116_006 → `1.1 GB`, 31 GiB → `31.0 GB` |
| **M1** | `memwatch report` (processes): MEM parser, `top` parser, `top_n` | `report` prints the 10 processes with the largest footprint using the real `top`; the tests with `top_l1.txt` (50 processes) and `top_l2.txt` (50 processes from the 2nd sample, without suffixes) pass; `top_n` returns `Vec<&Process>` without cloning |
| **M2** | System state: `vm_stat` and `sysctl` parsers, `Pressure::from_level`, severity, swapout rate, `Sample` | `report` shows the header with pressure, swap and compressor; the rate is unknown with `Δt = 0` and with the counter going backwards |
| **M3** | `History` + the §6.1 verdict | The §10 verdict tests pass |
| **M4a** | `alerts` (§6.2–§6.3), `Source`/`Notifier` traits, `FakeSource`, `FakeNotifier`. All pure | The incident simulation and the §10 pressure tests pass |
| **M4b** | I/O: real `TextSource`, `run_with_timeout`, `watch` loop, degraded cycles, `OsascriptNotifier`, `notify-test` | `watch` runs for an hour in the terminal without failing; `notify-test` shows the banner (after the §8.1 setup) |
| **M5** | LaunchAgent and bounded log | See [`notes/m5-launchd.md`](notes/m5-launchd.md): it survives a reboot and a `kill -9`; one week live; §6.4 thresholds recalibrated |
| **M6** | `FfiSource` with `libc` | It passes the oracle test against `TextSource`; **`git diff` shows no changes in `detect`, `alerts` or `history`**, only in `source/` and `model.rs`. That proves the architecture withstands the change of source |

## 12. Changes during implementation

- **`watch` and `report` use the FFI source by default.** That solves the `top` resolution problem above ~10 GB (§6.5), because the FFI gives exact MiB at any size, and it avoids running `top` every 30 s. It rounds to MiB on purpose: with exact bytes, a process that jumps up and then grows a few KiB per cycle would look like a leak (the tests in `ffi.rs` prove it). In exchange, it does not see root processes; that is what `--source text` is for.
- **Pressure only notifies at CRITICAL** (2026-11-18). WARN comes and goes many times a day on this Mac and caused too many alerts; now it stays in the log. The alert texts say first what is happening and then what to do.
- **`Sample::unreadable` in the text source** counts the `top` rows that could not be read. The design said it was always 0.
- **The log** is written by memwatch to its own file (`--log`). At startup, if it is over 10 MB, it is renamed to `.old`. This avoids truncating a file that launchd has open. Times are in UTC.
- **The pressure test `Normal,Unknown,Unknown,Warn,Warn` in §10** needs one more `Normal` at the start. Without a previously confirmed level, the first `Warn,Warn` pair is the first confirmation, which does not notify.

## 13. Toolchain

- **Installation:** the method recommended by rust-lang.org is `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` with the default profile. **This Mac already had `rustup` 1.29.2 from Homebrew** (installed on 2026-11-03, with stable 1.99.1), so that one is used. Its only pitfall is that Homebrew only links `rustup` into `/opt/homebrew/bin`: `rustc` and `cargo` live in `/opt/homebrew/opt/rustup/bin`, which you have to add to the PATH by hand. Also, `rustup self update` does not work: rustup is updated with `brew upgrade rustup` and the toolchains with `rustup update`. Do **not** use `brew install rust`: it is a fixed toolchain, without docs, and it clashes with rustup.
- **Versions verified as of 2026-10-28:** Rust stable 1.99.1, 2024 edition, rustup 1.29.2.
- **After installing,** in a new login shell: `rustc --version`, `cargo --version` and `rustup component add rust-src` (rust-analyzer needs it to autocomplete `std`).
- **PATH:** `~/.zprofile` gets `export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"`, after `brew shellenv` if you have it. The first path gives `rustc` and `cargo`; the second, the tools you install with `cargo install` (like `bacon`). To check it: `echo $PATH | tr ':' '\n'`. VS Code takes the PATH from your shell when it starts, so you have to quit it completely (Cmd+Q) and open it again.
- **Editor:** VS Code, updated to 1.144 or later, with the `rust-lang.rust-analyzer` extension. If the old `rust-lang.rust` ever gets installed, remove it because it clashes.
- **Feedback loop:** `cargo install --locked bacon`. `cargo-watch` is archived.
- **Project:** `cargo init` inside this repo, which already has git, `docs/` and `tests/fixtures/`. It creates `src/main.rs`; `src/lib.rs` is added by hand, because a crate that only has a binary cannot be used from `tests/`.

## 14. Risks and open questions

1. **`osascript` delivery from a LaunchAgent on Tahoe 26.6:** not verified on this Mac. The `notify-test` gate in M4b covers it, with plan B in the M5 notes.
2. **Detection latency under pressure:** timers are coalesced, and `top` itself is slow and takes memory. It is measured in M5.
3. **Thresholds and the limits in §6.5:** they come from two incidents. They are calibrated in M5.
4. **Leaks in root processes:** `TextSource` sees them, but `FfiSource` does not. In that case the system pressure path remains.
5. **Restarts:** the state lives only in memory. If memwatch restarts in the middle of a leak, it warns again after ~9 min (once the history has been collected). This is accepted: it is a useful alert, not noise.
6. **Privacy:** the fixtures contain the list of your processes. Review them before making the repo public.

---

## Appendix A: Fixtures

Captured on 2026-11-04 22:30 on this Mac, under real pressure (level 2 = WARN, 101 MB free, 10 GB in the compressor).

| File | Command | What is interesting about it |
|---|---|---|
| `top_l1.txt` | `top -l 1 -o mem -n 50 -stats pid,command,mem` | Full header and 50 rows; names with spaces, truncated (`Claude Helper (R`); MEM without a suffix and with trailing padding spaces |
| `top_l2.txt` | `top -l 2 -s 1 -o mem -n 50 -stats pid,command,mem` | Two samples with two `PID` headers; in the second one, MEM with a `+`/`-` suffix (13 with `+`, 10 with `-`, 27 without a suffix) |
| `vm_stat.txt` | `vm_stat` | Header with the page size of 16384; includes the MTE rows of this hardware |
| `sysctl_swapusage.txt` | `sysctl -n vm.swapusage` | `total = 20480.00M  used = 19300.50M  free = 1179.50M  (encrypted)` |
| `sysctl_pressure_level.txt` | `sysctl -n kern.memorystatus_vm_pressure_level` | `2` (WARN) |
| `sysctl_pagesize.txt` | `sysctl -n vm.pagesize` | `16384` |
