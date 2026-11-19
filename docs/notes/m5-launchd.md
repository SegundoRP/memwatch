# Notes for M5: LaunchAgent

Verified facts and pitfalls for running memwatch as a LaunchAgent.

## Plist

- **Path:** `~/Library/LaunchAgents/com.segundorp.memwatch.plist`. The file must be owned by you and not writable by the group or by other users. A `bootstrap` with error 122 points to a permissions or ownership problem.
- **Keys:**
  - `Label`: the same as the file name, without `.plist`.
  - `ProgramArguments`: `[~/.local/bin/memwatch, watch]` (expanded in the plist: `/Users/<you>/.local/bin/memwatch`), always with absolute paths.
  - `KeepAlive = true`: this is the unambiguous option for a watchdog, because it restarts even after a `kill -9`. It is not documented whether the dictionary `{SuccessfulExit=false, Crashed=true}` covers a SIGKILL. With `KeepAlive` on, `RunAtLoad` is redundant.
  - `ThrottleInterval = 30`: the minimum pause between restarts.
  - `StandardOutPath` and `StandardErrorPath`: absolute paths in `~/Library/Logs/`. launchd creates the file, but not any missing folders, and it **does not rotate it**.
  - `ProcessType`: **leave it undefined.** The man page says that undefined means "light resource limits" and that `Background` is for not disturbing the user. That `Background` limits more is an inference from that wording; nobody measured it. Either way, memwatch must run exactly when the machine is out of resources. `Interactive` is not justified.
- **Do not use** folders protected by TCC (Desktop, Documents, Downloads, iCloud), neither for the binary nor for the logs.

## Installing and updating the binary

1. `cargo build --release`.
2. Copy to a temporary file **inside** `~/.local/bin`.
3. `mv` (rename) over the previous binary.
4. `launchctl kickstart -k gui/$(id -u)/com.segundorp.memwatch`.

**Never `cp` over the running binary.** Apple documents code-signing crashes caused by this: the kernel caches the signature information per file. Also, do not modify the binary after linking it, because cargo leaves it with an ad-hoc signature (`linker-signed`) and you would break it. If the plist points inside `target/`, a rebuild changes the file, but the process keeps running the old code until the `kickstart`.

## Commands

- `plutil -lint <plist>`: validates the plist.
- `launchctl bootstrap gui/$(id -u) <plist>`: loads the agent the first time. Error 5 almost always means it was already loaded; run `bootout` first.
- `launchctl print gui/$(id -u)/com.segundorp.memwatch`: shows the state, the runs and the last exit code. The output says "NOT API": memwatch must not parse it.
- `launchctl kickstart -k gui/$(id -u)/com.segundorp.memwatch`: restarts it.
- **Uninstall:** `launchctl bootout gui/$(id -u)/com.segundorp.memwatch` **and** delete the plist. `bootout` only lasts until the next login, because at login launchd loads everything in `~/Library/LaunchAgents` again. Another option is `launchctl disable`, which does persist.

## Diagnostics

- Use `launchctl print` plus the memwatch log.
- `/var/log/com.apple.xpc.launchd/launchd.log` is readable, but it rotates at ~7 MB and on this Mac it keeps only ~18 min of history (almost all `mdworker` spam). It is useless for looking back.
- macOS can list the agent in System Settings → General → Login Items. If someone turns it off there, it stops running without warning. This is not verified for hand-installed plists.

## Log

- Under launchd, stdout stays quiet (§4 of the design), so only stderr grows, with alerts and events, at a few KB per day.
- Requirement: at startup, if the log is over 10 MB, it is truncated. You choose the mechanism.
- `println!` flushes on each newline, but `print!` without `\n` does not reach the file. `eprintln!` is unbuffered.

## Timers and latency

- launchd coalesces the timers of its jobs (`LegacyTimers` only has an effect with `ProcessType = Interactive`), so the 30 s interval will drift. That is why the design uses the timestamps and does not assume the period.
- You need to measure how long it takes to detect a leak under real pressure: `top` itself gets slower when the machine is out of memory.

## Plan B for notifications

If `notify-test` does not show the banner from the LaunchAgent:

1. **A minimal `Memwatch.app` bundle:** an `Info.plist` with `CFBundleIdentifier`, the binary in `Contents/MacOS` and an ad-hoc signature (`codesign -s -`), notifying through UNUserNotificationCenter. `terminal-notifier` 3.x is an app of this kind, ad-hoc signed and not notarized, and it works; this is practical evidence that an ad-hoc signature is enough for local use, although it was not tested on this Mac.
2. **`terminal-notifier` 3.x** from Homebrew, as an external helper.
3. Avoid `mac-notification-sys` and `notify-rust`: they borrow another app's identity through NSUserNotificationCenter, which is deprecated since macOS 11, and they have known hangs on macOS 26. Also avoid `user-notify`, which without a bundle silently falls back to a mock that only writes to the log.

## M5 acceptance criteria

- It survives a reboot and a `kill -9`: `launchctl print` shows that it ran again.
- Under launchd, stdout does not grow and stderr stays below 10 MB.
- One week live. Then the thresholds in §6.4 and the limits in §6.5 are reviewed with the real data, and the design is updated.
