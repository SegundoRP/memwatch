# Notes for M6: `FfiSource`

Verified facts and pitfalls behind the FFI source (`src/source/ffi.rs`).

## Goal

Replace `TextSource` with direct calls to the kernel, without touching `detect`, `alerts` or `history`. Only `source/` and `model.rs` change (the `ProcessKey::PidStart` variant and the use of `Sample::unreadable`).

## Dependency

- `libc = "0.2.191"`. Written like this, Cargo reads it as `^0.2.191`. You need at least 0.2.190, because from that version on `rusage_info_v4` implements `Default`.
- Do **not** use `1.0.0-alpha`.
- The 2024 edition requires hand-written `extern` blocks to be `unsafe extern "C" { … }`. `libc` already declares almost everything.

## Processes

- **Enumerate:** `libc::proc_listallpids`.
  - It returns the **number** of PIDs (not bytes) and **0 on error** (not −1).
  - The list includes pid 0, which is `kernel_task`.
  - The number varies between calls (1373 versus 1354 in two consecutive calls), so reserve some headroom and query again.
- **Footprint:** `libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, buf)` with a `libc::rusage_info_v4` buffer. From it you get:
  - `ri_phys_footprint`: the current value, in bytes.
  - `ri_lifetime_max_phys_footprint`: the peak, the same kind of data as the `phys_footprint_peak` from the incident.
  - `ri_proc_start_abstime`: goes into the key (`ProcessKey::PidStart`).
- **The buffer parameter is `*mut rusage_info_t`**, and `rusage_info_t = *mut c_void`. It looks like a double pointer, but it is a quirk of the C header: pass the pointer to your struct, cast, for example with `.cast::<libc::rusage_info_t>()`. Do **not** pass `&mut ptr`.
- **The flavor and the buffer type must match.** The kernel writes `sizeof(rusage_info_vN)` bytes **without checking the size of your buffer**. Bytes measured per flavor: v0 = 96, v1 = 144, v2 = 160, v3 = 232, v4 = 296, v5 = 304, v6 = 464. Passing `RUSAGE_INFO_CURRENT` (6) with a v4 buffer overflows it by 168 bytes. `libc` only has v0 to v4: use v4 with v4.
- **Initialize the buffer** with `MaybeUninit::zeroed()` or with `Default`. An all-zero buffer is valid because the struct is only integers.
- **Do not use `ri_interval_max_phys_footprint`:** other processes can reset it. Peaks and growth come from your own samples plus `ri_lifetime_max_phys_footprint`.
- **Zombies:** `proc_pid_rusage` also works on zombies, but it returns the snapshot saved at exit, not live data. That is why the key includes the start time.

## Permissions

- The check compares the caller's **effective** uid with the process's uid. Your own processes read fine; processes of another user and of root return **`EPERM`**. That includes `launchd`, `WindowServer` (uid 88), `coreaudiod`, `mDNSResponder` and pid 0 (`kernel_task`). Measured: 1210 OK, 191 EPERM, 0 mismatches.
- Setuid programs that you launch are also invisible, for the same reason.
- `EPERM` → it is counted in `Sample::unreadable` and the process is excluded from `processes`. **It is never reported as 0.**
- `ESRCH` → the process died between enumeration and reading; skip it.
- `top` does see all of them because it has a private Apple entitlement, `com.apple.system-task-ports.read`, and reads through task ports. memwatch cannot have it. For a leak in a root process, the system pressure path remains.
- `task_name_for_pid` + `task_info(TASK_VM_INFO)` (from the `mach2` crate) gives the same footprint, but with the **same** reach (only your own processes) and more code. It adds nothing.

## System state

- **`libc::sysctlbyname`:** always check that the returned length is the expected one. Verified widths:

  | sysctl | Width | How to read it |
  |---|---|---|
  | `vm.pagesize` | 4 bytes | `u32`/`c_int`, then convert to `u64` |
  | `kern.memorystatus_vm_pressure_level` | 4 bytes | `u32` |
  | `vm.swapusage` | 32 bytes | `libc::xsw_usage` (fields `xsu_used` and `xsu_total` in exact bytes) |
  | `hw.pagesize` | 8 bytes | **do not use**: it is the process page |

- `kern.memorystatus_vm_pressure_level` **does not appear** in `sysctl -a` (`CTLFLAG_MASKED`), but it reads fine by name.
- **`libc::host_statistics64(host, HOST_VM_INFO64, …)`** gives `compressor_page_count` and `swapouts`:
  - You must pass an **explicit** count of 40, which is REV2: `(offset_of!(libc::vm_statistics64, total_tag_storage_pages) / 4) as u32`. Do **not** use `libc::HOST_VM_INFO64_COUNT`: it is 104 in libc, the macOS 26.6 kernel expects 62, and with the oversized value the kernel logs an error on every call.
  - Check that the result is `KERN_SUCCESS` and that the returned count is at least 38 before trusting the fields.
- **`mach_host_self()`:** the libc version is `#[deprecated]` ("Use the mach2 crate instead"), so with `-D warnings` it does not compile as is. There are two ways out that keep the "only libc" rule:
  1. `#[allow(deprecated)]` on the single call, with a comment that explains why.
  2. Declare it yourself in an `unsafe extern "C" { fn mach_host_self() -> libc::mach_port_t; }`. memwatch does this.
  
  In both cases, call it once at startup and cache it. Calling it every cycle breaks nothing (the references saturate at 65535), but it is sloppy.
- `mach2` 0.7.0 does **not** have `host_statistics64`. Mixing crates is confusing; `libc` alone is enough.

## Oracle test

- An integration test with `#[ignore]` that you run by hand on this Mac: it compares `FfiSource` against `TextSource` within a tolerance.
- `top` rounds to MiB (it matches `ri_phys_footprint / 1048576` within ±1 MiB), and the values move between one reading and the next.

## What you gain and what you lose

- **You gain:**
  - exact bytes;
  - a key that is correct when PIDs are reused;
  - the lifetime peak of the process;
  - the option to read argv (`KERN_PROCARGS2`) to tell `codex app-server` apart from `codex exec-server`.
- **You lose:** processes of other users and of root (see Permissions).
