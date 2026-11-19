# Verified facts

Researched on 2026-10-28. Each fact went through one researcher and then through a second agent that tried to refute it. What remains here are the facts that survived, or their corrected version. Facts that are an inference, and not something measured or documented, say so.

| Fact | Evidence |
|---|---|
| `sysinfo` 0.40.0: on macOS, `Process::memory()` reads `pti_resident_size` (RSS) through `proc_pidinfo(PROC_PIDTASKINFO)`, not `phys_footprint`. If the call fails, it ignores the error and returns 0 | sysinfo code at tag v0.40.0 (`src/unix/apple/macos/process.rs`); SDK `proc_info.h` |
| `sysinfo` 0.40.0: `System::new()`/`new_all()` return `Result` (a change documented in `migration_guide.md`, not in the CHANGELOG) | `src/common/system.rs` L47–92 in v0.40.0; issue #1686 |
| The MEM column of `top`, the Memory column of Activity Monitor and the Xcode memory gauge report the process footprint ledger | `man footprint` L276–278; `man top`; local measurement (MEM matches `ri_phys_footprint` within ±1 MiB) |
| RSS versus footprint for codex: 56–118 MB versus ~1 GB. The ratio is not fixed (from ~1× to ~60×), and in small processes it is sometimes inverted, because RSS counts shared and file-backed pages that the footprint excludes | Local measurement, two independent runs; XNU `task.c` L1258–1275 |
| The footprint counts compressed and swapped memory at its uncompressed size, so it can exceed RAM (the sum of your processes came to 72.6 GiB on 24 GiB) | XNU `pmap.c` (arm64) and `task.c`; local measurement |
| `kern.memorystatus_vm_pressure_level` is 1/2/4 (Normal/Warn/Critical) and is hidden in `sysctl -a` (`CTLFLAG_MASKED`), but it can be read by name. The levels have hysteresis | XNU `kern_memorystatus_notify.c`, `vm_pageout.c`; equivalent to `DISPATCH_MEMORYPRESSURE_*` in `<dispatch/source.h>` L255–257 |
| Activity Monitor probably uses that same level for its pressure graph. **Inference:** the binary contains the string `kern.memorystatus_vm_pressure_level`, but the exact mapping is not documented | `strings` on Activity Monitor |
| `kern.memorystatus_level` is the % of available memory, **not** the pressure level | XNU `vm_pressure_response()`; matches the output of `memory_pressure` without flags |
| Compressor in RAM = `Pages occupied by compressor` × page size (matches `top`). `Pages stored in compressor` is the logical size. `vm.compressor_bytes_used` gives ~10 % less because it does not count fragmentation | Local measurement; XNU `vm_compressor_backing_store.c` |
| Total swap grows and shrinks on demand (1 GiB files, up to 100), so used/total is useless as a signal | XNU `vm_compressor_backing_store.c`; it went from 24 to 28 GiB during the research |
| Swapins and swapouts are cumulative counters since boot. The interval mode of `vm_stat` (`vm_stat N`) is not reliable: the first row is cumulative, the header resets the baseline and large values are truncated | XNU; `vm_stat` code |
| `hw.pagesize` is the process page (4096 under Rosetta); `vm.pagesize` and the `vm_stat` header are the kernel page (16384) | Tested with `arch -x86_64` |
| `memory_pressure` is read-only **only without flags**. `-l`, `-p`, `-S` and the undocumented `-w` allocate memory or simulate pressure | `memory_pressure` code |
| `libc` 0.2.191 exposes `proc_pid_rusage`, `rusage_info_v4.ri_phys_footprint`, `RUSAGE_INFO_V4`, `proc_listallpids`, `host_statistics64` and `xsw_usage`. Its `mach_host_self` is deprecated | `src/unix/bsd/apple/mod.rs` in rust-lang/libc |
| `libc::HOST_VM_INFO64_COUNT` is 104, while the macOS 26.6 kernel uses 62; you must pass 40 (REV2) explicitly | libc code; XNU; local test |
| `proc_pid_rusage` returns `EPERM` for processes of another euid and works for the same uid; with the v4 flavor the kernel writes 296 bytes | XNU `proc_info.c`; local test (1210 OK, 191 EPERM, 0 mismatches) |
| `top` sees other users' processes thanks to the private entitlement `com.apple.system-task-ports.read`, not only because it is setuid | `codesign -d --entitlements - /usr/bin/top` |
| `osascript`: you need `--` before the arguments (tested: a title that starts with `-` gives exit code 2). On Tahoe you need the Persistent style to see the banner (community reports on 26.0). The notification is attributed to Script Editor | Local test of `--`; macscripter forums |
| launchd: the `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin`; `KeepAlive = true` is the unambiguous option; `cp` over a signed binary can cause code-signing crashes; `bootout` only lasts until the next login. That `ProcessType = Background` limits more than leaving it undefined is an **inference** from the wording of the man page | `man launchd.plist`, `man launchctl`; Apple documentation "Updating Mac software" |
| MEM format in `top` (2026-11-17): integers without decimals, up to 4 digits in `M` (`4111M`, `9123M`, `6144M`); from ~10 GB it switches to `G` (`10G`, `23G`). The `G` part is an **inference** from the header fields, because no process was above 10 GB at measurement time | Local `top -l 1`: process rows and `PhysMem`/`VM` headers |
| `cargo-watch` is archived; `bacon` (3.26) is maintained | GitHub (archived=true); crates.io |
| Rust 1.99.1 is the current stable; the 2024 edition requires `unsafe extern` | static.rust-lang.org; edition guide |
