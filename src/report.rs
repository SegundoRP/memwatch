//! `memwatch report`: a snapshot of the heaviest processes.
//!
//! Everything here is pure (no I/O): `main.rs` reads `top` and prints what
//! `render_processes` returns.

use std::cmp::Reverse;

use crate::model::{Process, SystemState};

/// The system status line: "pressure WARN · swap 18.8 GB · compressor 10.0 GB".
pub fn render_system(system: &SystemState) -> String {
    format!(
        "pressure {} · swap {} · compressor {}",
        system.pressure, system.swap_used, system.compressor
    )
}

/// The `n` processes with the largest footprint, from largest to smallest.
///
/// Returns references (`&Process`) to the processes in `processes`, not copies:
/// sorting 50 pointers is cheaper than cloning 50 `String`s, and the caller
/// stays the owner of the data.
pub fn top_n(processes: &[Process], n: usize) -> Vec<&Process> {
    let mut heaviest: Vec<&Process> = processes.iter().collect();
    // sort_by_key is stable: equal footprints keep the input order, so the
    // output is always the same for the same input.
    heaviest.sort_by_key(|process| Reverse(process.footprint));
    heaviest.truncate(n);
    heaviest
}

/// The table that `memwatch report` prints.
pub fn render_processes(processes: &[&Process]) -> String {
    let mut out = String::from("   PID     MEMORY  PROCESS\n");
    for process in processes {
        // `Bytes` implements Display but ignores the width (`{:>9}`), so it is
        // first converted to a String and the String is aligned. 9 is the width
        // of the longest value seen in practice: "1017.0 MB".
        let footprint = process.footprint.to_string();
        out.push_str(&format!(
            "{:>6}  {:>9}  {}\n",
            process.pid, footprint, process.name
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Bytes, MIB, ProcessKey};
    use crate::source::text::parse_top;

    fn process(pid: u32, name: &str, mib: u64) -> Process {
        Process {
            key: ProcessKey::PidName(pid, name.to_string()),
            pid,
            name: name.to_string(),
            footprint: Bytes(mib * MIB),
        }
    }

    fn pids(processes: &[&Process]) -> Vec<u32> {
        processes.iter().map(|process| process.pid).collect()
    }

    #[test]
    fn heaviest_first() {
        let processes = vec![
            process(1, "a", 100),
            process(2, "b", 300),
            process(3, "c", 200),
        ];
        assert_eq!(pids(&top_n(&processes, 3)), vec![2, 3, 1]);
    }

    #[test]
    fn keeps_only_n() {
        let processes = vec![
            process(1, "a", 100),
            process(2, "b", 300),
            process(3, "c", 200),
        ];
        assert_eq!(pids(&top_n(&processes, 2)), vec![2, 3]);
    }

    #[test]
    fn n_larger_than_the_list_returns_everything() {
        let processes = vec![process(1, "a", 100), process(2, "b", 300)];
        assert_eq!(pids(&top_n(&processes, 10)), vec![2, 1]);
    }

    #[test]
    fn n_zero_returns_nothing() {
        let processes = vec![process(1, "a", 100)];
        assert!(top_n(&processes, 0).is_empty());
    }

    #[test]
    fn ties_keep_the_input_order() {
        let processes = vec![
            process(1, "a", 462),
            process(2, "b", 500),
            process(3, "c", 462),
        ];
        assert_eq!(pids(&top_n(&processes, 3)), vec![2, 1, 3]);
    }

    #[test]
    fn returns_references_into_the_input_not_copies() {
        let processes = vec![process(1, "a", 100), process(2, "b", 300)];
        let heaviest = top_n(&processes, 2);
        // ptr::eq compares memory addresses: it is the same Process, not an equal one.
        assert!(std::ptr::eq(heaviest[0], &processes[1]));
        assert!(std::ptr::eq(heaviest[1], &processes[0]));
    }

    #[test]
    fn top_ten_of_the_real_fixture() {
        let top = parse_top(include_str!("../tests/fixtures/top_l1.txt")).unwrap();
        let heaviest = top_n(&top.processes, 10);
        assert_eq!(heaviest.len(), 10);
        assert_eq!(heaviest[0].name, "com.apple.Virtua");
        for pair in heaviest.windows(2) {
            assert!(pair[0].footprint >= pair[1].footprint);
        }
    }

    #[test]
    fn renders_the_system_line() {
        let system = SystemState {
            pressure: crate::model::Pressure::Warn,
            swap_used: Bytes(19_300 * MIB),
            compressor: Bytes(10_240 * MIB),
            swapouts_pages: 0,
            page_size: 16384,
        };
        assert_eq!(
            render_system(&system),
            "pressure WARN · swap 18.8 GB · compressor 10.0 GB"
        );
    }

    #[test]
    fn renders_a_table() {
        let vm = process(70062, "com.apple.Virtua", 4112);
        let plugin = process(32856, "plugin-container", 1017);
        let claude = process(63855, "claude", 241);
        let expected = [
            "   PID     MEMORY  PROCESS",
            " 70062     4.0 GB  com.apple.Virtua",
            " 32856  1017.0 MB  plugin-container",
            " 63855   241.0 MB  claude",
        ];
        assert_eq!(
            render_processes(&[&vm, &plugin, &claude]),
            expected.join("\n") + "\n"
        );
    }
}
