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
        /// Observe the execution and report what Arc learned about it
        #[arg(long)]
        trace: bool,
        /// Show observed paths and processes individually, not just counts
        #[arg(short, long)]
        verbose: bool,
        /// Emit a machine-readable result on stderr
        #[arg(long)]
        json: bool,
        /// The command to run
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Show the dependency graph Arc has learned
    Graph {
        /// Show every known path, not just a summary
        #[arg(short, long)]
        verbose: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show which known executions the working tree's changes may affect
    Affected {
        #[arg(long)]
        json: bool,
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
            trace,
            verbose,
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
                trace,
            },
            Display {
                explain,
                trace,
                verbose,
                json,
            },
        ),
        Cmd::Graph { verbose, json } => cmd_graph(&home, &cwd, verbose, json).map(|_| 0),
        Cmd::Affected { json } => cmd_affected(&home, &cwd, json).map(|_| 0),
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

/// How much of a run to show. Kept separate from `RunOptions` so presentation
/// choices never leak into what the engine actually does.
struct Display {
    explain: bool,
    trace: bool,
    verbose: bool,
    json: bool,
}

fn cmd_run(
    home: &Path,
    cwd: &Path,
    command: Vec<String>,
    opts: RunOptions,
    show: Display,
) -> Result<i32> {
    let (program, args) = command.split_first().expect("clap requires at least one");
    let project = Project::discover(cwd)?;

    let report = engine::run(&project, cwd, program, args, &opts, home)?;
    let e = &report.explain;

    if show.explain {
        render_explain(&project, home, &report)?;
    }
    if show.trace {
        render_trace(&report, show.verbose);
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
                " {} {} ({})",
                ui::dim("·"),
                ui::plural(report.restored_files, "file", "files"),
                ui::bytes(report.restored_bytes)
            ));
        }
        eprintln!(
            "\n{} {}  {}",
            ui::brand(ui::MARK),
            ui::badge("CACHE HIT"),
            ui::dim(&report.record.command_line())
        );
        eprintln!("  {detail}");
    }

    if show.json {
        let r = &report.record;
        let out = serde_json::json!({
            "schema": arc_core::SCHEMA_VERSION,
            "id": r.id,
            "key": r.key,
            "family_key": r.family_key,
            "command": r.command_line(),
            "cache_status": r.cache_status.label(),
            "exit_code": r.exit_code,
            "duration_ms": r.duration_ms,
            "saved_ms": report.saved_ms,
            "restored_files": report.restored_files,
            "input_digest": r.input_digest,
            "trace": r.trace,
            "explain": e,
        });
        eprintln!("{}", serde_json::to_string(&out)?);
    }

    Ok(report.record.exit_code)
}

fn render_explain(project: &Project, home: &Path, report: &engine::RunReport) -> Result<()> {
    let e = &report.explain;
    eprint!("{}", ui::banner("EXPLAIN"));
    eprintln!("{}", ui::row("command", e.command.trim_end()));
    eprintln!("{}", ui::row("directory", &e.cwd));
    eprintln!("{}", ui::row("family", &ui::dim(&short(&e.family_key))));
    eprintln!(
        "{}",
        ui::row(
            "inputs",
            &format!(
                "{} {} {} {} {} reused",
                ui::plural(e.input_files, "file", "files"),
                ui::dim("·"),
                ui::duration(e.fingerprint_ms),
                ui::dim("·"),
                e.reused_fingerprints
            )
        )
    );
    eprintln!(
        "{}",
        ui::row(
            "dependencies",
            &format!(
                "{} {} {}",
                ui::completeness_color(&e.dependency_state),
                ui::dim("·"),
                if e.inputs_narrowed {
                    "inputs narrowed"
                } else {
                    "whole project"
                }
            )
        )
    );
    for (label, value) in [
        ("input digest", &e.input_digest),
        ("environment", &e.env_digest),
        ("toolchain", &e.toolchain_digest),
        ("execution key", &e.execution_key),
    ] {
        eprintln!("{}", ui::row(label, &ui::dim(&short(value))));
    }
    let result = if e.result == "cache hit" {
        ui::green(&e.result)
    } else {
        ui::yellow(&e.result)
    };
    eprintln!("{}", ui::row("result", &result));
    eprintln!("{}", ui::row("reason", &e.reason));

    if !e.changed.is_empty() {
        eprintln!("\n  {}", ui::dim("changed"));
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

    // Only worth computing when Arc can actually prove something irrelevant,
    // and only because the user asked for an explanation.
    if e.inputs_narrowed {
        let ignored = engine::ignored_changes(project, home, &e.command, &e.family_key)?;
        if !ignored.is_empty() {
            eprintln!("\n  {}", ui::dim("outside this execution's inputs"));
            for p in ignored.iter().take(10) {
                eprintln!("    {p}");
            }
            if ignored.len() > 10 {
                eprintln!(
                    "{}",
                    ui::dim(&format!("    and {} more", ignored.len() - 10))
                );
            }
        }
    }
    eprintln!();
    Ok(())
}

fn render_trace(report: &engine::RunReport, verbose: bool) {
    let Some(obs) = &report.observations else {
        eprint!("{}", ui::banner("TRACE"));
        eprintln!("{}", ui::row("backend", &ui::dim("none")));
        eprintln!(
            "{}",
            ui::row(
                "status",
                &ui::yellow("not observed (cache hit or tracing off)")
            )
        );
        eprintln!();
        return;
    };
    let deps = report.dependencies.as_ref();
    let caps = arc_core::trace::platform_capabilities();

    eprint!("{}", ui::banner("TRACE"));
    eprintln!("{}", ui::row("command", &report.record.command_line()));
    eprintln!(
        "{}",
        ui::row(
            "backend",
            &ui::accent(arc_core::trace::platform_backend_name())
        )
    );
    eprintln!(
        "{}",
        ui::row(
            "processes",
            &ui::plural(obs.processes.len(), "observed", "observed")
        )
    );
    eprintln!(
        "{}",
        ui::row(
            "files written",
            &ui::plural(obs.files.len(), "observed", "observed")
        )
    );
    if let Some(d) = deps {
        eprintln!(
            "{}",
            ui::row(
                "executables",
                &ui::plural(d.executables.len(), "known", "known")
            )
        );
        eprintln!(
            "{}",
            ui::row(
                "dependency model",
                &ui::completeness_color(d.completeness.label())
            )
        );
    }
    // Never let a partial observation read as a full one.
    if !caps.file_reads {
        eprintln!(
            "{}",
            ui::row(
                "not observed",
                &ui::dim("file reads, directory reads, existence checks")
            )
        );
    }
    for note in &obs.notes {
        eprintln!("{}", ui::row("note", &ui::yellow(note)));
    }

    if verbose {
        if !obs.processes.is_empty() {
            eprintln!("\n  {}", ui::dim("processes"));
            for p in &obs.processes {
                eprintln!(
                    "    {:<8} {}",
                    p.pid,
                    p.image
                        .as_deref()
                        .unwrap_or("<exited before identification>")
                );
            }
        }
        if !obs.files.is_empty() {
            eprintln!("\n  {}", ui::dim("files"));
            for f in obs.files.iter().take(200) {
                eprintln!(
                    "    {:<8} {}",
                    f.op.label(),
                    f.rel.as_deref().unwrap_or(&f.path)
                );
            }
            if obs.files.len() > 200 {
                eprintln!(
                    "{}",
                    ui::dim(&format!("    and {} more", obs.files.len() - 200))
                );
            }
        }
    }
    eprintln!(
        "\n  {}",
        ui::dim(&format!(
            "{} total",
            ui::duration(report.record.duration_ms)
        ))
    );
    eprintln!();
}

fn cmd_graph(home: &Path, cwd: &Path, verbose: bool, json: bool) -> Result<()> {
    let project = Project::discover(cwd)?;
    let db = Db::open(home)?;
    let graph = arc_core::graph::build(&project, &db)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&graph)?);
        return Ok(());
    }
    if graph.nodes.is_empty() {
        println!(
            "{}",
            ui::banner("DEPENDENCY GRAPH").trim_start_matches('\n')
        );
        println!("  No executions recorded for this project yet.\n");
        println!("  Run:\n    arc run --trace <command>");
        return Ok(());
    }

    print!("{}", ui::banner("DEPENDENCY GRAPH"));
    println!("{}\n", ui::dim(&graph.project_root));
    for node in &graph.nodes {
        println!(
            "{}  {}",
            ui::bold(&node.command),
            ui::dim(&format!(
                "{} · {} · {}",
                ui::plural(node.runs as usize, "run", "runs"),
                node.backend,
                node.completeness
            ))
        );
        let mut lines: Vec<(String, String)> = Vec::new();
        for g in &node.declared_inputs {
            lines.push(("declared".into(), g.clone()));
        }
        let limit = if verbose { usize::MAX } else { 8 };
        for p in node.inputs.iter().take(limit) {
            lines.push(("input".into(), p.clone()));
        }
        for p in node.outputs.iter().take(limit) {
            lines.push(("output".into(), p.clone()));
        }
        for p in node.executables.iter().take(limit) {
            lines.push(("exec".into(), p.clone()));
        }
        if lines.is_empty() {
            println!("{}{}\n", ui::branch(true), ui::dim("nothing observed yet"));
            continue;
        }
        for (i, (kind, path)) in lines.iter().enumerate() {
            println!(
                "{}{} {}",
                ui::branch(i + 1 == lines.len()),
                ui::dim(&format!("{kind:<8}")),
                path
            );
        }
        if !node.inputs_narrowed {
            println!(
                "  {}",
                ui::dim("inputs are not narrowed; this execution depends on the whole project")
            );
        }
        println!();
    }
    Ok(())
}

fn cmd_affected(home: &Path, cwd: &Path, json: bool) -> Result<()> {
    let project = Project::discover(cwd)?;
    let db = Db::open(home)?;
    let report = arc_core::affected::compute(&project, &db, cwd)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print!("{}", ui::banner("AFFECTED"));
    if let Some(err) = &report.git_error {
        println!(
            "  {}\n",
            ui::yellow(&format!("no change information: {err}"))
        );
        return Ok(());
    }
    if report.changes.is_empty() {
        println!("  {}\n", ui::dim("working tree is clean"));
        return Ok(());
    }
    if report.families.is_empty() {
        println!(
            "  {}\n\n  Run:\n    arc run --trace <command>",
            ui::dim("No dependency information available for this project.")
        );
        return Ok(());
    }

    println!("  {}", ui::dim("changed"));
    for c in report.changes.iter().take(20) {
        println!("    {:<10} {}", ui::dim(c.kind.label()), c.path);
    }
    if report.changes.len() > 20 {
        println!(
            "{}",
            ui::dim(&format!("    and {} more", report.changes.len() - 20))
        );
    }

    use arc_core::affected::Verdict;
    for (title, verdict, paint) in [
        (
            "affected executions",
            Verdict::Affected,
            ui::yellow as fn(&str) -> String,
        ),
        ("unaffected", Verdict::Unaffected, ui::green),
        ("unknown (inputs not narrowed)", Verdict::Unknown, ui::dim),
    ] {
        let rows: Vec<_> = report.of(verdict).collect();
        if rows.is_empty() {
            continue;
        }
        println!("\n  {}", ui::dim(title));
        for f in rows {
            println!("    {}", paint(&f.command));
            if verdict == Verdict::Affected {
                for m in f.matched.iter().take(3) {
                    println!("      {}", ui::dim(m));
                }
            }
        }
    }
    if report.of(Verdict::Unknown).next().is_some() {
        println!(
            "\n  {}",
            ui::dim("Arc cannot rule these out: scope them with [[command]] inputs in arc.toml.")
        );
    }
    println!();
    Ok(())
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
            "{:<8} {:<32} {:>6} {:>6} {:>7} {:>10}  {}",
            "ID", "COMMAND", "EXIT", "CACHE", "INPUTS", "DURATION", "WHEN"
        ))
    );
    for r in rows {
        let exit = if r.exit_code == 0 {
            ui::green(&format!("{:>6}", r.exit_code))
        } else {
            ui::red(&format!("{:>6}", r.exit_code))
        };
        // A traced run is marked so a reader can tell which executions
        // contributed dependency knowledge.
        let inputs = match r.input_file_count {
            0 => ui::dim(&format!("{:>7}", "-")),
            n if r.trace.is_some() => ui::brand(&format!("{n:>6}*")),
            n => format!("{n:>7}"),
        };
        println!(
            "{} {:<32} {} {} {} {:>10}  {}",
            ui::bold(&format!("{:<8}", engine::short(&r.id))),
            truncate(&r.command_line(), 32),
            exit,
            ui::status_color(r.cache_status.label()),
            inputs,
            ui::duration(r.duration_ms),
            ui::dim(&ui::relative_time(r.started_at, now))
        );
    }
    println!("{}", ui::dim("\n* execution was traced"));
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
    if !rec.family_key.is_empty() {
        ui::field("Family", &rec.family_key);
        let deps = Db::open(home)?.dependency_set(&rec.family_key)?;
        if let Some(d) = &deps {
            println!("{}", ui::dim("Dependencies"));
            println!("{}", ui::row("model", d.completeness.label()));
            println!("{}", ui::row("backend", &d.backend));
            println!("{}", ui::row("observations", &d.observations.to_string()));
            println!(
                "{}",
                ui::row("declared inputs", &d.declared_inputs.len().to_string())
            );
            println!(
                "{}",
                ui::row("observed inputs", &d.inputs.len().to_string())
            );
            println!(
                "{}",
                ui::row("observed outputs", &d.outputs.len().to_string())
            );
            println!(
                "{}",
                ui::row("executables", &d.executables.len().to_string())
            );
            println!(
                "{}\n",
                ui::row("inputs narrowed", &check(d.inputs_are_narrowed()))
            );
        }
    }
    if let Some(t) = &rec.trace {
        println!("{}", ui::dim("Trace"));
        println!("{}", ui::row("backend", &t.backend));
        println!(
            "{}",
            ui::row(
                "completeness",
                &ui::completeness_color(t.completeness.label())
            )
        );
        println!("{}", ui::row("processes", &t.processes.to_string()));
        println!(
            "{}\n",
            ui::row("files observed", &t.files_observed.to_string())
        );
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
            print!("{}", ui::banner("CACHE"));
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
    print!("{}", ui::banner(&format!("DOCTOR  {}", arc_core::VERSION)));
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

    println!("\n{}\n", ui::bold("tracing"));
    let caps = arc_core::trace::platform_capabilities();
    println!(
        "{}",
        ui::row(
            "backend",
            &ui::accent(arc_core::trace::platform_backend_name())
        )
    );
    for (label, ok) in caps.rows() {
        println!("{}", ui::row(label, &supported(ok)));
    }
    println!(
        "{}",
        ui::row(
            "completeness",
            &ui::completeness_color(arc_core::dependency::Completeness::of(&caps, false).label())
        )
    );
    println!(
        "{}",
        ui::row(
            "input narrowing",
            &ui::dim("configuration only (see arc.toml [[command]])")
        )
    );

    println!("\n{}\n", ui::bold("capabilities"));
    println!("{}", ui::row("command caching", &ui::green("supported")));
    println!("{}", ui::row("output capture", "declared globs only"));
    println!("{}", ui::row("dependency graph", &ui::green("supported")));
    println!(
        "{}",
        ui::row(
            "affected executions",
            &if arc_core::git::available(cwd) {
                ui::green("supported")
            } else {
                ui::yellow("needs git on PATH")
            }
        )
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

/// Unsupported capabilities are dimmed, not red: they are honest gaps in what
/// the platform allows, not failures of the installation.
fn supported(ok: bool) -> String {
    if ok {
        ui::green("supported")
    } else {
        ui::dim("unsupported")
    }
}
