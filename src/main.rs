//! Entry point: reads the arguments, assembles the real pieces and calls the lib.
//! No logic here.

use std::env;
use std::path::PathBuf;
use std::process::{self, ExitCode};

use memwatch::alerts::Alert;
use memwatch::log::Logger;
use memwatch::model::{Bytes, MIB};
use memwatch::notify::{Notifier, OsascriptNotifier};
use memwatch::report::{render_processes, render_system, top_n};
use memwatch::source::Source;
use memwatch::source::ffi::FfiSource;
use memwatch::source::text::TextSource;
use memwatch::thresholds::Thresholds;
use memwatch::watch;

const USAGE: &str = "usage:
  memwatch report [--source ffi|text]
  memwatch watch  [--source ffi|text] [--log FILE]
  memwatch notify-test

  --source ffi    (default) reads the kernel directly: whole MiB, only your processes
  --source text   uses top/vm_stat/sysctl: also sees root processes
  --log FILE      writes the log to FILE (rotated at 10 MB) instead of stderr";
const REPORT_PROCESSES: usize = 10;
const LOG_LIMIT_BYTES: u64 = 10 * 1024 * 1024;

enum SourceKind {
    Ffi,
    Text,
}

struct Options {
    source: SourceKind,
    log: Option<PathBuf>,
}

fn main() -> ExitCode {
    // env::args() gives Strings; matching against "report" needs &str.
    let owned: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();

    // `rest @ ..` captures "everything that follows" in a slice.
    match args.as_slice() {
        ["report", rest @ ..] => match parse_options(rest) {
            Some(options) if options.log.is_none() => run_report(&options),
            _ => usage(),
        },
        ["watch", rest @ ..] => match parse_options(rest) {
            Some(options) => run_watch(&options),
            None => usage(),
        },
        ["notify-test"] => run_notify_test(),
        _ => usage(),
    }
}

fn parse_options(mut rest: &[&str]) -> Option<Options> {
    let mut options = Options {
        source: SourceKind::Ffi,
        log: None,
    };
    loop {
        match rest {
            [] => return Some(options),
            ["--source", "ffi", tail @ ..] => {
                options.source = SourceKind::Ffi;
                rest = tail;
            }
            ["--source", "text", tail @ ..] => {
                options.source = SourceKind::Text;
                rest = tail;
            }
            ["--log", path, tail @ ..] => {
                options.log = Some(PathBuf::from(path));
                rest = tail;
            }
            _ => return None,
        }
    }
}

/// `Box<dyn Source>`: any source, chosen at runtime.
fn make_source(kind: &SourceKind) -> Box<dyn Source> {
    match kind {
        SourceKind::Ffi => Box::new(FfiSource::new()),
        SourceKind::Text => Box::new(TextSource::new()),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn fail(message: &str) -> ExitCode {
    eprintln!("memwatch: {message}");
    ExitCode::FAILURE
}

fn run_report(options: &Options) -> ExitCode {
    let sample = match make_source(&options.source).sample() {
        Ok(sample) => sample,
        Err(error) => return fail(&error.to_string()),
    };

    println!("{}", render_system(&sample.system));
    println!();
    print!(
        "{}",
        render_processes(&top_n(&sample.processes, REPORT_PROCESSES))
    );
    if sample.unreadable > 0 {
        println!();
        println!(
            "({} processes unreadable: owned by root or other users with --source ffi, unparseable rows with --source text)",
            sample.unreadable
        );
    }
    ExitCode::SUCCESS
}

fn run_watch(options: &Options) -> ExitCode {
    let mut logger = match &options.log {
        None => Logger::stderr(),
        Some(path) => match Logger::file(path, LOG_LIMIT_BYTES) {
            Ok(logger) => logger,
            Err(error) => {
                return fail(&format!("could not open {}: {error}", path.display()));
            }
        },
    };
    let mut source = make_source(&options.source);
    // `watch::run` returns `!` (it never ends), which fits any type.
    watch::run(
        source.as_mut(),
        &mut OsascriptNotifier,
        &mut logger,
        Thresholds::default(),
    )
}

/// A notification with the same shape as a real leak alert. The pid is this
/// very process's, which has already exited by the time you see it: the
/// `kill -9` on the last line does nothing to anyone.
fn run_notify_test() -> ExitCode {
    let alert = Alert::Leak {
        pid: process::id(),
        name: "memwatch-test".to_string(),
        footprint: Bytes(9_800 * MIB),
        growth: Bytes(3_700 * MIB),
        span_secs: 600,
    };
    let title = format!("{} (test)", alert.title());
    match OsascriptNotifier.notify(&title, &alert.body()) {
        Ok(()) => {
            println!("Notification sent. If you don't see the banner, see §8.1 of docs/design.md.");
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error.to_string()),
    }
}
