mod ui;

use anyhow::{Context, Result};
use arc_core::db::Db;
use arc_core::engine::{self, RunOptions};
use arc_core::maintenance;
use arc_core::project::Project;
use arc_core::record::CacheStatus;
use arc_core::store::Store;
use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "arc",
    version,
    about = "Arc - never repeat work that is already done",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command, reusing a cached result when the inputs are unchanged
    Run {
        /// Print how the cache decision was made
        #[arg(long)]
        explain: bool,
        /// Neither read nor write the cache
        #[arg(long)]
        no_cache: bool,
        /// Execute even on a hit, and replace the cached result
        #[arg(long)]
        refresh: bool,
        /// Give the child the terminal directly (nothing is cached)
        #[arg(long)]
        no_capture: bool,
        /// Also cache executions that exit non-zero
        #[arg(long)]
        cache_failures: bool,
        /// Emit a machine-readable result on stderr
        #[arg(long)]
        json: bool,
        /// The command to run
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Show recent executions
    History {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show everything Arc recorded about one execution
    Inspect {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Inspect and maintain the local cache
    Cache {
        #[command(subcommand)]
        command: CacheCmd,
    },
    /// Check the local Arc installation
    Doctor,
    /// Show the effective configuration
    Config {
        #[command(subcommand)]
        command: ConfigCmd,
    },
    /// Remove cached results (see also `arc cache prune`)
    Clean {
        /// Remove the entire Arc home directory
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Summarise cache size, reuse and time saved
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// List cache entries, most recently used first
    List {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Show the execution behind a cache key
    Inspect { key: String },
    /// Evict least-recently-used entries until the cache fits its size limit
    Prune {
        /// Override the configured maximum size, e.g. 5GB
        #[arg(long)]
        max_size: Option<String>,
    },
    /// Delete unreferenced objects
    Gc,
    /// Re-hash every stored object and quarantine anything corrupt
    Verify,
    /// Delete every cache entry and stored object
    Clear,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the configuration Arc is using, and where it came from
    Show,
}

fn main() {
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            // Errors are multi-line and written for a human: mark the first
            // line, indent the rest.
            let text = format!("{e:#}");
            let mut lines = text.lines();
            eprintln!(
                "\n{} {}",
                ui::red(ui::MARK),
                lines.next().unwrap_or("unknown error")
            );
            for line in lines {
                eprintln!("{line}");
            }
            eprintln!();
            std::process::exit(1);
        }
    }
}

fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    let home = arc_core::arc_home()?;
    let cwd = std::env::current_dir().context("reading the current directory")?;

    match cli.command {
        Cmd::Run {
            explain,
            no_cache,
            refresh,
            no_capture,
            cache_failures,
            json,
            command,
        } => cmd_run(
            &home,
            &cwd,
            command,
            RunOptions {
                no_cache,
                refresh,
                no_capture,
                cache_failures,
            },
            explain,
            json,
        ),
        Cmd::History { limit, json } => cmd_history(&home, limit, json).map(|_| 0),
        Cmd::Inspect { id, json } => cmd_inspect(&home, &id, json).map(|_| 0),
        Cmd::Cache { command } => cmd_cache(&home, &cwd, command).map(|_| 0),
        Cmd::Doctor => cmd_doctor(&home, &cwd).map(|_| 0),
        Cmd::Config { command } => match command {
            ConfigCmd::Show => cmd_config_show(&cwd).map(|_| 0),
        },
        Cmd::Clean { all } => cmd_clean(&home, all).map(|_| 0),
    }
}

fn cmd_run(
    home: &Path,
    cwd: &Path,
    command: Vec<String>,
    opts: RunOptions,
    explain: bool,
    json: bool,
) -> Result<i32> {
    let (program, args) = command.split_first().expect("clap requires at least one");
    let project = Project::discover(cwd)?;

    let report = engine::run(&project, cwd, program, args, &opts, home)?;
    let e = &report.explain;

    if explain {
        eprintln!("\n{} {}\n", ui::cyan(ui::MARK), ui::bold("arc explain"));
        eprintln!("{}", ui::row("command", e.command.trim_end()));
        eprintln!("{}", ui::row("directory", &e.cwd));
        eprintln!(
            "{}",
            ui::row(
                "inputs",
                &format!(
                    "{} {} {} {} {} {} reused",
                    e.input_files,
                    if e.input_files == 1 { "file" } else { "files" },
                    ui::dim("·"),
                    ui::duration(e.fingerprint_ms),
                    ui::dim("·"),
                    e.reused_fingerprints
                )
            )
        );
        eprintln!(
            "{}",
            ui::row("input digest", &ui::dim(&short(&e.input_digest)))
        );
        eprintln!(
            "{}",
            ui::row("environment", &ui::dim(&short(&e.env_digest)))
        );
        eprintln!(
            "{}",
            ui::row("toolchain", &ui::dim(&short(&e.toolchain_digest)))
        );
        eprintln!(
            "{}",
            ui::row("execution key", &ui::dim(&short(&e.execution_key)))
        );
        let result = if e.result == "cache hit" {
            ui::green(&e.result)
        } else {
            ui::yellow(&e.result)
        };
        eprintln!("{}", ui::row("result", &result));
        eprintln!("{}", ui::row("reason", &e.reason));
        if !e.changed.is_empty() {
            eprintln!("\n{}", ui::dim("  changed"));
            for c in e.changed.iter().take(10) {
                eprintln!("    {c}");
            }
            if e.changed.len() > 10 {
                eprintln!(
                    "{}",
                    ui::dim(&format!("    and {} more", e.changed.len() - 10))
                );
            }
        }
        eprintln!();
    }

    if report.record.cache_status == CacheStatus::Hit {
        let mut detail = format!(
            "restored in {} {} saved {}",
            ui::duration(report.restore_ms),
            ui::dim("·"),
            ui::duration(report.saved_ms)
        );
        if report.restored_files > 0 {
            detail.push_str(&format!(
                " {} {} files ({})",
                ui::dim("·"),
                report.restored_files,
                ui::bytes(report.restored_bytes)
            ));
        }
        eprintln!(
            "\n{} {}  {}",
            ui::cyan(ui::MARK),
            ui::badge("CACHE HIT"),
            ui::dim(&report.record.command_line())
        );
        eprintln!("  {detail}");
    }

    if json {
        let r = &report.record;
        let out = serde_json::json!({
            "id": r.id,
            "key": r.key,
            "command": r.command_line(),
            "cache_status": r.cache_status.label(),
            "exit_code": r.exit_code,
            "duration_ms": r.duration_ms,
            "saved_ms": report.saved_ms,
            "restored_files": report.restored_files,
            "input_digest": r.input_digest,
            "explain": report.explain,
        });
        eprintln!("{}", serde_json::to_string(&out)?);
    }

    Ok(report.record.exit_code)
}

fn cmd_history(home: &Path, limit: usize, json: bool) -> Result<()> {
    let db = Db::open(home)?;
    let rows = db.history(limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No executions recorded yet.");
        return Ok(());
    }
    let now = arc_core::scan::now_millis();
    println!(
        "{}",
        ui::dim(&format!(
            "{:<8} {:<34} {:>6} {:>6} {:>10}  {}",
            "ID", "COMMAND", "EXIT", "CACHE", "DURATION", "WHEN"
        ))
    );
    for r in rows {
        let exit = if r.exit_code == 0 {
            ui::green(&format!("{:>6}", r.exit_code))
        } else {
            ui::red(&format!("{:>6}", r.exit_code))
        };
        println!(
            "{} {:<34} {} {} {:>10}  {}",
            ui::bold(&format!("{:<8}", engine::short(&r.id))),
            truncate(&r.command_line(), 34),
            exit,
            ui::status_color(r.cache_status.label()),
            ui::duration(r.duration_ms),
            ui::dim(&ui::relative_time(r.started_at, now))
        );
    }
    Ok(())
}

fn cmd_inspect(home: &Path, id: &str, json: bool) -> Result<()> {
    let db = Db::open(home)?;
    let rec = db
        .find_execution(id)?
        .with_context(|| format!("no execution matches `{id}`. Try `arc history`."))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rec)?);
        return Ok(());
    }
    ui::field(
        "Execution",
        &format!("{} ({})", rec.id, rec.cache_status.label()),
    );
    ui::field("Command", &rec.command_line());
    ui::field(
        "Project",
        &format!(
            "{} (cwd: {})",
            rec.project_root,
            if rec.rel_cwd.is_empty() {
                "."
            } else {
                &rec.rel_cwd
            }
        ),
    );
    ui::field("Exit code", &rec.exit_code.to_string());
    ui::field("Duration", &ui::duration(rec.duration_ms));
    if !rec.key.is_empty() {
        ui::field("Execution key", &rec.key);
        ui::field(
            "Input digest",
            &format!("{} ({} files)", rec.input_digest, rec.input_file_count),
        );
        ui::field("Environment digest", &rec.env.digest);
        ui::field(
            "Toolchain",
            &format!(
                "{} ({})",
                rec.toolchain
                    .resolved_path
                    .as_deref()
                    .unwrap_or("unresolved"),
                short(&rec.toolchain.digest)
            ),
        );
    }
    if let Some(from) = &rec.replayed_from {
        ui::field("Replayed from", from);
    }
    if !rec.outputs.is_empty() {
        println!("Outputs");
        for o in rec.outputs.iter().take(50) {
            println!("  {:<50} {}", o.rel, ui::bytes(o.size));
        }
        if rec.outputs.len() > 50 {
            println!("  ... and {} more", rec.outputs.len() - 50);
        }
        println!();
    }
    println!("Environment (values are hashed, never stored)");
    for v in rec.env.vars.iter().filter(|v| v.present) {
        let shown = if v.redacted {
            "<redacted>".to_string()
        } else {
            short(&v.value_digest)
        };
        println!("  {:<24} {}", v.name, shown);
    }
    Ok(())
}

fn cmd_cache(home: &Path, cwd: &Path, command: CacheCmd) -> Result<()> {
    let store = Store::open(home)?;
    let db = Db::open(home)?;
    match command {
        CacheCmd::Stats { json } => {
            let s = maintenance::stats(&store, &db)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "cache_entries": s.cache_entries,
                        "executions": s.executions,
                        "objects": s.blobs,
                        "stored_bytes": s.stored_bytes,
                        "logical_bytes": s.logical_bytes,
                        "hits": s.hits,
                        "misses": s.misses,
                        "ms_saved": s.ms_saved,
                    }))?
                );
                return Ok(());
            }
            ui::heading(&format!("{} arc cache", ui::cyan(ui::MARK)));
            println!("{}", ui::row("entries", &s.cache_entries.to_string()));
            println!("{}", ui::row("executions", &s.executions.to_string()));
            println!("{}", ui::row("objects", &s.blobs.to_string()));
            println!(
                "{}",
                ui::row("stored", &ui::bold(&ui::bytes(s.stored_bytes)))
            );
            println!("{}", ui::row("logical size", &ui::bytes(s.logical_bytes)));
            println!(
                "{}",
                ui::row("deduplicated", &ui::bytes(s.deduplicated_bytes()))
            );
            let hit_rate = match s.hit_rate() {
                Some(r) => format!(
                    "{} {}",
                    ui::green(&format!("{r:.1}%")),
                    ui::dim(&format!("({} hits, {} misses)", s.hits, s.misses))
                ),
                None => ui::dim("n/a"),
            };
            println!("{}", ui::row("hit rate", &hit_rate));
            println!(
                "{}",
                ui::row("time saved", &ui::green(&ui::long_duration(s.ms_saved)))
            );
        }
        CacheCmd::List { limit } => {
            let mut entries = db.cache_entries()?;
            entries.sort_by_key(|(_, e)| std::cmp::Reverse(e.last_accessed));
            let execs = db.all_executions()?;
            let now = arc_core::scan::now_millis();
            println!(
                "{}",
                ui::dim(&format!(
                    "{:<14} {:<38} {:>5} {:>10}  {}",
                    "KEY", "COMMAND", "HITS", "ORIGINAL", "LAST USED"
                ))
            );
            for (key, entry) in entries.into_iter().take(limit) {
                let cmd = execs
                    .iter()
                    .find(|e| e.id == entry.execution_id)
                    .map(|e| e.command_line())
                    .unwrap_or_else(|| "<execution pruned>".into());
                println!(
                    "{:<14} {:<38} {:>5} {:>10}  {}",
                    &key[..12],
                    truncate(&cmd, 38),
                    entry.hits,
                    ui::duration(entry.original_duration_ms),
                    ui::relative_time(entry.last_accessed, now)
                );
            }
        }
        CacheCmd::Inspect { key } => {
            let entries = db.cache_entries()?;
            let (_, entry) = entries
                .iter()
                .find(|(k, _)| k.starts_with(&key))
                .with_context(|| format!("no cache entry matches `{key}`"))?;
            return cmd_inspect(home, &entry.execution_id, false);
        }
        CacheCmd::Prune { max_size } => {
            let max = match max_size {
                Some(s) => arc_core::project::parse_size(&s)?,
                None => Project::discover(cwd)?.max_size_bytes()?,
            };
            let r = maintenance::prune(&store, &db, max)?;
            println!(
                "Removed {} cache entries, {} executions, {} objects ({} freed).\nCache is now {}.",
                r.entries_removed,
                r.executions_removed,
                r.blobs_removed,
                ui::bytes(r.bytes_freed),
                ui::bytes(r.final_bytes)
            );
        }
        CacheCmd::Gc => {
            let (n, freed) = maintenance::gc(&store, &db)?;
            println!(
                "Removed {n} unreferenced objects ({} freed).",
                ui::bytes(freed)
            );
        }
        CacheCmd::Verify => {
            let (checked, bad) = maintenance::verify(&store, &db)?;
            if bad.is_empty() {
                println!("Verified {checked} objects. No corruption found.");
            } else {
                for c in &bad {
                    println!(
                        "Arc detected a corrupted cache object.\n\nObject:\n  b3:{}\n\nExpected:\n  {}\n\nActual:\n  {}\n\nThe object has been quarantined and will not be reused.\n  {}\n",
                        c.expected.short(),
                        c.expected.short(),
                        c.actual.short(),
                        c.quarantined
                    );
                }
                println!("Verified {checked} objects, {} corrupt.", bad.len());
            }
        }
        CacheCmd::Clear => {
            std::fs::remove_dir_all(store.root.parent().unwrap().join("store")).ok();
            db.clear_all()?;
            println!("Cache cleared.");
        }
    }
    Ok(())
}

fn cmd_doctor(home: &Path, cwd: &Path) -> Result<()> {
    ui::heading(&format!("{} arc {}", ui::cyan(ui::MARK), arc_core::VERSION));
    println!(
        "{}",
        ui::row(
            "platform",
            &format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH)
        )
    );
    println!(
        "{}",
        ui::row("cache directory", &home.display().to_string())
    );

    let writable = std::fs::create_dir_all(home)
        .and_then(|_| std::fs::write(home.join(".arc-write-test"), b"1"))
        .map(|_| {
            let _ = std::fs::remove_file(home.join(".arc-write-test"));
            true
        })
        .unwrap_or(false);
    println!("{}", ui::row("cache writable", &check(writable)));

    match Db::open(home).and_then(|db| db.counters()) {
        Ok(_) => println!("{}", ui::row("metadata database", &check(true))),
        Err(e) => println!(
            "{}",
            ui::row("metadata database", &ui::red(&format!("{e:#}")))
        ),
    }
    match Store::open(home).and_then(|s| s.total_size()) {
        Ok(size) => println!("{}", ui::row("objects on disk", &ui::bytes(size))),
        Err(e) => println!(
            "{}",
            ui::row("objects on disk", &ui::red(&format!("{e:#}")))
        ),
    }

    let project = Project::discover(cwd)?;
    println!(
        "{}",
        ui::row("project root", &project.root.display().to_string())
    );
    println!("{}", ui::row("git repository", &check(project.git)));
    println!(
        "{}",
        ui::row(
            "configuration",
            &project
                .config_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "defaults".into())
        )
    );
    println!(
        "{}",
        ui::row(
            "git on PATH",
            &check(arc_core::key::which("git", cwd).is_some())
        )
    );

    println!("\n{}\n", ui::bold("capabilities"));
    println!("{}", ui::row("command caching", &ui::green("supported")));
    println!("{}", ui::row("output capture", "declared globs only"));
    println!(
        "{}",
        ui::row("filesystem tracing", &ui::dim("not implemented"))
    );
    println!("{}", ui::row("remote cache", &ui::dim("not implemented")));
    Ok(())
}

fn cmd_config_show(cwd: &Path) -> Result<()> {
    let project = Project::discover(cwd)?;
    match &project.config_path {
        Some(p) => println!("# effective configuration, from {}\n", p.display()),
        None => println!("# effective configuration (no arc.toml found; showing defaults)\n"),
    }
    print!("{}", toml::to_string_pretty(&project.config)?);
    Ok(())
}

fn cmd_clean(home: &Path, all: bool) -> Result<()> {
    if all {
        std::fs::remove_dir_all(home).ok();
        println!("Removed {}.", home.display());
        return Ok(());
    }
    let store = Store::open(home)?;
    let db = Db::open(home)?;
    let (n, freed) = maintenance::gc(&store, &db)?;
    println!(
        "Removed {n} unreferenced objects ({} freed).",
        ui::bytes(freed)
    );
    println!(
        "Use `arc cache clear` to drop every cached result, or `arc clean --all` to remove {}.",
        home.display()
    );
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n - 3).collect::<String>())
    }
}

fn short(hex: &str) -> String {
    hex.chars().take(12).collect()
}

fn check(ok: bool) -> String {
    if ok {
        ui::green("yes")
    } else {
        ui::yellow("no")
    }
}
