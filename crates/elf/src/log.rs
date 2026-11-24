//! The daemon log: one line per event, never the API key, never audio.

use std::fmt::Arguments;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

enum Sink {
    File(File),
    Stderr,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

pub fn init_file(path: &Path) {
    let sink = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Sink::File(f),
        Err(_) => Sink::Stderr,
    };
    let _ = SINK.set(Mutex::new(sink));
}

pub fn init_stderr() {
    let _ = SINK.set(Mutex::new(Sink::Stderr));
}

pub fn write(args: Arguments) {
    let Some(sink) = SINK.get() else { return };
    let now = crate::clock::unix_ms();
    let line = format!("{}.{:03} {args}\n", &crate::clock::iso_local(now)[11..19], now % 1000);
    match &mut *sink.lock().unwrap() {
        Sink::File(f) => {
            let _ = f.write_all(line.as_bytes());
        }
        Sink::Stderr => eprint!("{line}"),
    }
}

#[macro_export]
macro_rules! elog {
    ($($arg:tt)*) => { $crate::log::write(format_args!($($arg)*)) };
}
