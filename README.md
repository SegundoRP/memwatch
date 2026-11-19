# memwatch

Memory watchdog for macOS, written in Rust. It runs in the background and sends you a notification when:

- **a process has a memory leak**: it grows nonstop for 10 minutes and is already above 3 GB;
- **the Mac runs out of memory**: the system memory pressure reaches CRITICAL.

It never quits or kills anything; it only warns you and tells you which process it is.

> **Why it exists:** a process of a desktop app grew to 31 GB on a 24 GB Mac. The Mac was slow and hot for a long while before macOS said anything. memwatch warns you at 3–4 GB and names the culprit.

## Example alert

> **memwatch: possible leak in codex**
> codex grew 3.7 GB in 10 min and now uses 9.6 GB.
> If you don't need it, quit it or run: kill -9 48213

## Requirements

- macOS on Apple Silicon (tested on macOS 26).
- Stable Rust with the 2024 edition (`rustup`).

## Usage

```bash
cargo run -- report                  # snapshot: pressure, swap, compressor and the 10 heaviest processes
cargo run -- watch                   # watches every 30 s in the terminal (Ctrl+C to quit)
cargo run -- notify-test             # sends a test notification
```

`--source text` uses `top`, `vm_stat` and `sysctl` instead of reading the kernel directly. It is slower, but it also sees root processes.

## Install as a service

First, enable notifications: open Script Editor, run `display notification "x"` and click **Allow**. Then, in System Settings → Notifications → Script Editor, choose the **Persistent** style. Check that `cargo run -- notify-test` shows the banner.

```bash
./scripts/install.sh     # builds, installs to ~/.local/bin and registers it as a LaunchAgent
./scripts/uninstall.sh   # removes it
```

The log goes to `~/Library/Logs/memwatch.log`. memwatch uses about 2 MB of memory and almost no CPU.

## How it decides there is a leak

Every 30 s it reads the *footprint* of each process, the same value as the Memory column in Activity Monitor (not RSS). It looks at the last 10 minutes of each process and flags it as a leak if all three conditions hold:

1. there are at least 9 minutes of history;
2. it grew at least 512 MB in that time;
3. at least 80 % of the measurements went up.

Together, the three rule out what is not a leak: a virtual machine that boots and then stays flat, a large process with noise, or sawtooth memory. The details are in [`docs/design.md`](docs/design.md) §6.

## Documentation

- [`docs/design.md`](docs/design.md): the full design (metric, architecture, rules and milestones).
- [`docs/notes/`](docs/notes/): verified facts about macOS, launchd and the FFI.

## License

[MIT](LICENSE)
