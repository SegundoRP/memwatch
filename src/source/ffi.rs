//! FFI source (M6): asks the kernel directly with `libc`, without running `top`
//! (see docs/notes/m6-ffi.md).
//!
//! All of the project's `unsafe` lives in this file. Every block carries a
//! `SAFETY:` comment that explains why the call is correct: the compiler
//! cannot check it, so we check it ourselves.
//!
//! Compared with `TextSource`:
//! - it gains 1 MiB resolution at any size (top switches to GiB above ~10 GB), a key
//!   that survives pid reuse, and names of up to 32 characters;
//! - it loses the processes of other users and of root (EPERM): they are counted in
//!   `Sample::unreadable`, never reported as 0.

use std::ffi::{CStr, c_void};
use std::io;
use std::mem;
use std::ptr;
use std::time::Instant;

use super::{Source, SourceError};
use crate::model::{Bytes, MIB, Pressure, Process, ProcessKey, Sample, SystemState, Timestamp};

// libc's `mach_host_self` is marked #[deprecated], and with `-D warnings` that
// does not compile. It is declared by hand; in the 2024 edition a hand-written
// `extern` block has to be `unsafe extern`.
unsafe extern "C" {
    fn mach_host_self() -> libc::mach_port_t;
}

/// How many 4-byte integers of `vm_statistics64` are requested: revision 2 of
/// the struct (everything before `total_tag_storage_pages`), which gives 40.
/// NOT `libc::HOST_VM_INFO64_COUNT`: it is 104 and the macOS 26.6 kernel expects 62.
const VM_INFO64_REV2_COUNT: libc::mach_msg_type_number_t =
    (mem::offset_of!(libc::vm_statistics64, total_tag_storage_pages) / 4) as u32;

/// With fewer integers than this, `swapouts` and `compressor_page_count` are missing.
const VM_INFO64_MIN_COUNT: libc::mach_msg_type_number_t = 38;

pub struct FfiSource {
    started: Instant,
    host: libc::mach_port_t,
}

impl FfiSource {
    pub fn new() -> FfiSource {
        // SAFETY: it takes nothing and returns the host port of this task.
        // It is requested only once and stored.
        let host = unsafe { mach_host_self() };
        FfiSource {
            started: Instant::now(),
            host,
        }
    }
}

impl Default for FfiSource {
    fn default() -> Self {
        FfiSource::new()
    }
}

impl Source for FfiSource {
    fn sample(&mut self) -> Result<Sample, SourceError> {
        let system = read_system(self.host)?;
        let (processes, unreadable) = read_processes()?;
        Ok(Sample {
            taken_at: Timestamp(self.started.elapsed().as_secs()),
            processes,
            unreadable,
            system,
        })
    }
}

// --- processes ---

/// All the processes that can be read, plus how many returned EPERM.
fn read_processes() -> Result<(Vec<Process>, u32), SourceError> {
    let mut processes = Vec::new();
    let mut unreadable = 0;
    for pid in list_pids()? {
        match read_process(pid) {
            Ok(process) => processes.push(process),
            // Another user's or root's: it exists, but we cannot read it.
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => unreadable += 1,
            // ESRCH: it died between the listing and the read. Skipped.
            Err(_) => {}
        }
    }
    Ok((processes, unreadable))
}

fn list_pids() -> Result<Vec<libc::pid_t>, SourceError> {
    // SAFETY: with a null buffer of size 0 it only returns how many pids there are.
    let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(os_error("proc_listallpids"));
    }

    // The count changes between one call and the next: leave some headroom and,
    // if the buffer filled up completely, retry with twice the size.
    let mut capacity = count as usize + 64;
    loop {
        let mut pids: Vec<libc::pid_t> = vec![0; capacity];
        let bytes = (capacity * mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: `pids` has exactly `bytes` writable bytes and the function
        // does not write past the size we pass it.
        let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
        if found <= 0 {
            return Err(os_error("proc_listallpids"));
        }
        let found = found as usize;
        if found < capacity {
            pids.truncate(found);
            return Ok(pids);
        }
        capacity *= 2;
    }
}

fn read_process(pid: libc::pid_t) -> io::Result<Process> {
    let mut info = libc::rusage_info_v4::default();
    // SAFETY: the flavor (V4) matches the buffer type (rusage_info_v4): the
    // kernel writes sizeof(rusage_info_v4) = 296 bytes WITHOUT looking at the
    // buffer size, so a newer flavor would overflow it. The parameter is
    // declared as `*mut rusage_info_t` due to a quirk of the C header; we pass
    // the pointer to the struct, cast, not a pointer to a pointer.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            ptr::from_mut(&mut info).cast::<libc::rusage_info_t>(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let pid = pid as u32; // pids are never negative
    Ok(Process {
        // Pid + start time: if the pid is reused, the key changes.
        key: ProcessKey::PidStart(pid, info.ri_proc_start_abstime),
        pid,
        name: process_name(pid as libc::c_int),
        footprint: whole_mib(info.ri_phys_footprint),
    })
}

/// Rounds down to whole MiB, the same resolution as `top`.
///
/// The §6.1 rules were designed on values in MiB: a "positive" jump means
/// growing at least 1 MiB. With exact bytes, a process that jumps up at once and
/// then grows a few KiB per cycle would have 100 % positive jumps and would give
/// a false leak. Rounding here keeps those semantics without touching the core.
fn whole_mib(bytes: u64) -> Bytes {
    Bytes(bytes / MIB * MIB)
}

fn process_name(pid: libc::c_int) -> String {
    let mut buffer = [0u8; 64];
    // SAFETY: we pass it the real buffer size; it writes at most that.
    let length = unsafe {
        libc::proc_name(
            pid,
            buffer.as_mut_ptr().cast::<c_void>(),
            buffer.len() as u32,
        )
    };
    if length <= 0 {
        return format!("pid {pid}");
    }
    let length = (length as usize).min(buffer.len());
    String::from_utf8_lossy(&buffer[..length]).into_owned()
}

// --- system ---

fn read_system(host: libc::mach_port_t) -> Result<SystemState, SourceError> {
    let level: u32 = sysctl_by_name(c"kern.memorystatus_vm_pressure_level")?;
    let swap: libc::xsw_usage = sysctl_by_name(c"vm.swapusage")?;
    // vm.pagesize is the kernel page size (16384). NEVER hw.pagesize, which is
    // the process's and is 4096 under Rosetta.
    let page_size: u32 = sysctl_by_name(c"vm.pagesize")?;
    let vm = host_vm_statistics(host)?;

    let page_size = u64::from(page_size);
    let compressor = u64::from(vm.compressor_page_count)
        .checked_mul(page_size)
        .ok_or_else(|| {
            SourceError::Unavailable("the compressor does not fit in 64 bits".to_string())
        })?;

    Ok(SystemState {
        pressure: Pressure::from_level(level),
        swap_used: Bytes(swap.xsu_used),
        compressor: Bytes(compressor),
        swapouts_pages: vm.swapouts,
        page_size,
    })
}

/// Reads a sysctl by name and requires the kernel to write exactly
/// `size_of::<T>()` bytes. Only for types made of integers (u32, xsw_usage),
/// where any combination of bytes is a valid value.
fn sysctl_by_name<T: Default>(name: &CStr) -> Result<T, SourceError> {
    let mut value = T::default();
    let mut length = mem::size_of::<T>();
    // SAFETY: `oldp` points to a T of `length` bytes, and the kernel does not
    // write more than `length`. We write nothing (`newp` is null).
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            ptr::from_mut(&mut value).cast::<c_void>(),
            &mut length,
            ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(os_error(&format!("sysctl {name:?}")));
    }
    if length != mem::size_of::<T>() {
        return Err(SourceError::Unavailable(format!(
            "sysctl {name:?}: the kernel returned {length} bytes, expected {}",
            mem::size_of::<T>()
        )));
    }
    Ok(value)
}

fn host_vm_statistics(host: libc::mach_port_t) -> Result<libc::vm_statistics64, SourceError> {
    let mut stats = libc::vm_statistics64::default();
    let mut count = VM_INFO64_REV2_COUNT;
    // SAFETY: `count` (40) says how many 4-byte integers fit in the buffer,
    // which is smaller than the whole `stats`; the kernel writes at most that
    // and returns in `count` how many it wrote.
    let result = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            ptr::from_mut(&mut stats).cast::<libc::integer_t>(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS {
        return Err(SourceError::Unavailable(format!(
            "host_statistics64 returned {result}"
        )));
    }
    if count < VM_INFO64_MIN_COUNT {
        return Err(SourceError::Unavailable(format!(
            "host_statistics64 wrote only {count} integers"
        )));
    }
    Ok(stats)
}

fn os_error(what: &str) -> SourceError {
    SourceError::Unavailable(format!("{what}: {}", io::Error::last_os_error()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::detect::{Verdict, verdict};
    use crate::history::{Point, Series};
    use crate::thresholds::Thresholds;

    /// A step from 1 GiB to 4 GiB and then +16 KiB per sample for 10 min.
    fn step_then_creep(round: fn(u64) -> Bytes) -> Verdict {
        let mut series = Series::default();
        for i in 0..21u64 {
            let bytes = if i == 0 {
                1024 * MIB
            } else {
                4096 * MIB + i * 16 * 1024
            };
            series.push(Point {
                taken_at: Timestamp(i * 30),
                footprint: round(bytes),
            });
        }
        verdict(&series, &Thresholds::default())
    }

    #[test]
    fn with_exact_bytes_a_creep_after_a_step_would_look_like_a_leak() {
        assert_eq!(step_then_creep(Bytes), Verdict::Leak);
    }

    #[test]
    fn rounded_to_whole_mib_it_is_stable_as_the_design_says() {
        assert_eq!(step_then_creep(whole_mib), Verdict::Stable);
    }

    #[test]
    fn whole_mib_rounds_down() {
        assert_eq!(whole_mib(4096 * MIB + 1023 * 1024), Bytes(4096 * MIB));
    }

    #[test]
    fn asks_for_revision_2_of_vm_statistics64() {
        assert_eq!(VM_INFO64_REV2_COUNT, 40);
    }

    #[test]
    fn reads_this_very_process() {
        let sample = FfiSource::new().sample().unwrap();
        let me = std::process::id();
        let this = sample
            .processes
            .iter()
            .find(|process| process.pid == me)
            .expect("this very process can always be read");
        assert!(this.footprint > Bytes(0));
        assert_eq!(this.footprint.0 % MIB, 0);
        assert!(matches!(this.key, ProcessKey::PidStart(pid, _) if pid == me));
        // launchd, WindowServer, kernel_task… belong to root or another user.
        assert!(sample.unreadable > 0);
        assert_eq!(sample.system.page_size, 16384);
    }
}
