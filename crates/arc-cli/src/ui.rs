//! Terminal presentation: colour, symbols, and units.
//!
//! Styling is applied only when stderr is a terminal and `NO_COLOR` is unset,
//! so piped or CI output stays plain text.

use std::io::IsTerminal;
use std::sync::OnceLock;

fn colored() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
            && std::io::stderr().is_terminal()
    })
}

fn paint(code: &str, s: &str) -> String {
    if colored() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}

/// Arc's mark. Kept to one glyph so it never wraps a narrow terminal.
pub const MARK: &str = "◆";

/// A filled label. Falls back to plain text when colour is off, so scripts and
/// CI logs still read naturally.
pub fn badge(text: &str) -> String {
    if colored() {
        format!("\x1b[42;30;1m {text} \x1b[0m")
    } else {
        text.to_string()
    }
}

/// Colour a cache status, padding before styling so columns stay aligned.
pub fn status_color(label: &str) -> String {
    let padded = format!("{label:>6}");
    match label {
        "HIT" => green(&padded),
        "MISS" => yellow(&padded),
        _ => dim(&padded),
    }
}

/// `label  value`, aligned, with the label dimmed.
pub fn row(label: &str, value: &str) -> String {
    format!("  {:<18} {}", dim(label), value)
}

pub fn duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let s = ms / 1000;
        format!("{}m {}s", s / 60, s % 60)
    }
}

pub fn long_duration(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        duration(ms)
    }
}

pub fn bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn relative_time(ts_ms: i64, now_ms: i64) -> String {
    let s = (now_ms - ts_ms).max(0) / 1000;
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// A section of `arc inspect`: dimmed heading, indented value.
pub fn field(name: &str, value: &str) {
    println!("{}\n  {value}\n", dim(name));
}

pub fn heading(text: &str) {
    println!("{}\n", bold(text));
}
