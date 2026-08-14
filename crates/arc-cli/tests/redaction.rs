//! Secret values must not survive anywhere Arc writes.
//!
//! Arc's execution key depends on environment variables, and some of them are
//! credentials. The rule is that a secret-shaped variable is *used* — its value
//! contributes to the key — but never *recorded*. This drives the real binary
//! with adversarial values and searches every surface a human or another
//! machine can read.
//!
//! The values below are deliberately awkward: one contains regex
//! metacharacters, one contains a path separator, one is long enough to be
//! truncated by a naive formatter, and one looks like a digest, so a leak
//! cannot hide by being mistaken for something else.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ARC: &str = env!("CARGO_BIN_EXE_arc");

/// Each is `(variable name, value)`. Every name must be caught by Arc's
/// secret-shape rule; if one is not, that is the finding.
fn secrets() -> Vec<(&'static str, String)> {
    vec![
        ("AWS_SECRET_ACCESS_KEY", "sk-AKIA/leak+me=0".to_string()),
        (
            "GITHUB_TOKEN",
            "ghp_LEAKCANARY0000000000000000000000".to_string(),
        ),
        ("ARC_REMOTE_PASSWORD", "p@ss.*word?[a]".to_string()),
        ("MY_API_KEY", "x".repeat(300)),
        (
            "DATABASE_CREDENTIAL",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        ),
    ]
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let home = tmp.path().join("archome");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("input.txt"), "one").unwrap();
        // Every secret is named in the key, so each one is genuinely read,
        // hashed and carried through the whole pipeline rather than ignored.
        let names: Vec<String> = secrets().iter().map(|(n, _)| format!("\"{n}\"")).collect();
        std::fs::write(
            root.join("arc.toml"),
            format!(
                "[env]\ninclude = [{}]\n\n[outputs]\ninclude = [\"out/**\"]\n",
                names.join(", ")
            ),
        )
        .unwrap();
        Sandbox {
            _tmp: tmp,
            root,
            home,
        }
    }

    fn arc(&self, args: &[&str]) -> Output {
        let mut c = Command::new(ARC);
        c.args(args)
            .current_dir(&self.root)
            .env("ARC_HOME", &self.home)
            .env("ARC_NO_ANIM", "1");
        for (name, value) in secrets() {
            c.env(name, value);
        }
        c.output().expect("running arc")
    }
}

/// The surfaces a secret could reach, as text.
fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[track_caller]
fn clean(what: &str, body: &str) {
    for (name, value) in secrets() {
        if let Some(at) = body.find(&value) {
            let from = at.saturating_sub(120);
            let to = (at + value.len() + 120).min(body.len());
            panic!(
                "{what} contains the value of {name}\n  ...{}...",
                &body[from..to]
            );
        }
    }
}

/// Every regular file under a directory, as lossy text. Binary formats are read
/// as bytes and searched the same way: a secret embedded in a database page is
/// still a leak.
fn tree_text(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() => {
                    out.push_str(&p.to_string_lossy());
                    out.push('\n');
                    if let Ok(bytes) = std::fs::read(&p) {
                        out.push_str(&String::from_utf8_lossy(&bytes));
                        out.push('\n');
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn run_cmd() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["cmd", "/c", "echo built"]
    } else {
        vec!["sh", "-c", "echo built"]
    }
}

#[test]
fn no_command_output_carries_a_secret() {
    let s = Sandbox::new();
    let mut argv = vec!["run", "--"];
    argv.extend(run_cmd());
    clean("arc run", &text(&s.arc(&argv)));

    // A second run hits, and takes a different code path to the same surfaces.
    clean("arc run (hit)", &text(&s.arc(&argv)));

    let mut verbose = vec!["run", "--explain", "--verbose", "--trace", "--"];
    verbose.extend(run_cmd());
    clean(
        "arc run --explain --verbose --trace",
        &text(&s.arc(&verbose)),
    );

    let mut json = vec!["run", "--json", "--"];
    json.extend(run_cmd());
    clean("arc run --json", &text(&s.arc(&json)));

    for args in [
        vec!["history"],
        vec!["history", "--json"],
        vec!["inspect"],
        vec!["inspect", "--json"],
        vec!["doctor"],
        vec!["cache", "stats"],
        vec!["graph"],
        vec!["graph", "--json"],
        vec!["env", "list"],
        vec!["config", "show"],
    ] {
        let o = s.arc(&args);
        clean(&format!("arc {}", args.join(" ")), &text(&o));
    }
}

#[test]
fn nothing_arc_stores_carries_a_secret() {
    let s = Sandbox::new();
    let mut argv = vec!["run", "--"];
    argv.extend(run_cmd());
    s.arc(&argv);
    s.arc(&argv);
    clean("the Arc home", &tree_text(&s.home));
}

#[test]
fn a_captured_environment_carries_no_secret() {
    let s = Sandbox::new();
    let tool = if cfg!(windows) { "cmd" } else { "sh" };
    let cfg = std::fs::read_to_string(s.root.join("arc.toml")).unwrap();
    std::fs::write(
        s.root.join("arc.toml"),
        format!(
            "{cfg}
[environment.tools]
programs = [\"{tool}\"]
"
        ),
    )
    .unwrap();
    let o = s.arc(&["env", "capture", "tools"]);
    clean("arc env capture", &text(&o));
    clean("the Arc home after capture", &tree_text(&s.home));
    clean("the project after capture", &tree_text(&s.root));
}

#[test]
fn doctor_output_is_safe_to_paste_into_a_bug_report() {
    let s = Sandbox::new();
    let o = s.arc(&["doctor"]);
    let body = text(&o);
    clean("arc doctor", &body);
    // Not just the values: a bug report should not enumerate which credentials
    // exist on the reporter's machine either.
    for (name, _) in secrets() {
        assert!(
            !body.contains(name),
            "arc doctor names the credential {name}"
        );
    }
}

/// The hash of a secret is what the key depends on, so it must still change
/// when the secret does — redaction must not have turned into ignoring.
#[test]
fn changing_a_secret_still_misses() {
    let s = Sandbox::new();
    let mut argv = vec!["run", "--"];
    argv.extend(run_cmd());
    s.arc(&argv);

    let mut c = Command::new(ARC);
    c.args(&argv)
        .current_dir(&s.root)
        .env("ARC_HOME", &s.home)
        .env("ARC_NO_ANIM", "1");
    for (name, value) in secrets() {
        c.env(name, value);
    }
    c.env("GITHUB_TOKEN", "ghp_A_COMPLETELY_DIFFERENT_VALUE_0000");
    let o = c.output().expect("running arc");
    let body = text(&o);
    assert!(
        !body.contains("CACHE HIT") && !body.contains("cache hit"),
        "a changed credential still hit:\n{body}"
    );
}
