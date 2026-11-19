//! M6 oracle test: the FFI source has to match `top` (within a tolerance,
//! because the two reads are not simultaneous).
//!
//! It reads the real system, so it does not run with `cargo test`. To run it:
//!     cargo test --test oracle -- --ignored

use memwatch::model::{Bytes, MIB, Sample};
use memwatch::source::Source;
use memwatch::source::ffi::FfiSource;
use memwatch::source::text::TextSource;

/// Equal within a margin of `max(floor, 5 %)`: memory moves between reads.
fn close(a: Bytes, b: Bytes, floor: u64) -> bool {
    let (small, big) = if a <= b { (a.0, b.0) } else { (b.0, a.0) };
    let margin = floor.max(big / 20);
    big - small <= margin
}

fn read_both() -> (Sample, Sample) {
    let from_ffi = FfiSource::new().sample().unwrap();
    let from_text = TextSource::new().sample().unwrap();
    (from_ffi, from_text)
}

#[test]
#[ignore = "reads this Mac; run by hand with -- --ignored"]
fn ffi_system_state_matches_vm_stat_and_sysctl() {
    let (ffi, text) = read_both();
    assert_eq!(ffi.system.page_size, text.system.page_size);
    assert!(
        close(ffi.system.swap_used, text.system.swap_used, 256 * MIB),
        "swap: ffi {} vs text {}",
        ffi.system.swap_used,
        text.system.swap_used
    );
    assert!(
        close(ffi.system.compressor, text.system.compressor, 256 * MIB),
        "compressor: ffi {} vs text {}",
        ffi.system.compressor,
        text.system.compressor
    );
    // The FFI is read first and the counter only goes up.
    assert!(ffi.system.swapouts_pages <= text.system.swapouts_pages);
}

#[test]
#[ignore = "reads this Mac; run by hand with -- --ignored"]
fn ffi_footprints_match_top() {
    let (ffi, text) = read_both();
    let mut compared = 0;
    let mut mismatches = Vec::new();

    for from_top in &text.processes {
        let Some(from_kernel) = ffi.processes.iter().find(|p| p.pid == from_top.pid) else {
            continue; // root's or another user's: the FFI does not see it, and that is fine
        };
        compared += 1;
        if !close(from_kernel.footprint, from_top.footprint, 32 * MIB) {
            mismatches.push(format!(
                "{} (pid {}): ffi {} vs top {}",
                from_top.name, from_top.pid, from_kernel.footprint, from_top.footprint
            ));
        }
    }

    println!("compared {compared} processes; {} differ", mismatches.len());
    assert!(compared >= 10, "only {compared} processes in common");
    // Some may have really changed between the two reads.
    assert!(mismatches.len() <= compared / 10, "{mismatches:#?}");
}
