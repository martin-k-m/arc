mod ui;

use anyhow::{Context, Result};
use arc_core::db::Db;
use arc_core::engine::{self, RunOptions};
use arc_core::maintenance;
use arc_core::project::Project;
use arc_core::record::{CacheSource, CacheStatus};
use arc_core::remote::Remote;
use arc_core::store::Store;
use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
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
        /// Ignore any configured remote cache
        #[arg(long)]
        no_remote: bool,
        /// Run a cache miss on a remote worker, if one can run it
        #[arg(long, conflicts_with = "no_remote_execution")]
        remote_execution: bool,
        /// Never send this command to a remote worker
        #[arg(long)]
        no_remote_execution: bool,
        /// Pin the tracing backend: auto, fast, ptrace, snapshot, or off
        #[arg(long, value_name = "NAME", default_value = "auto")]
        trace_backend: String,
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
    /// Show the task graph Arc has learned
    Graph {
        /// Show only this task and everything connected to it
        #[arg(long, value_name = "NAME")]
        task: Option<String>,
        /// Show only tasks the working tree's changes reach
        #[arg(long)]
        affected: bool,
        /// Also list each task's outputs and declared inputs
        #[arg(short, long)]
        verbose: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show which tasks the working tree's changes reach, and optionally run them
    Affected {
        /// Execute the affected tasks, in dependency order
        #[arg(long)]
        run: bool,
        /// With --run, print the plan instead of executing it
        #[arg(long)]
        dry_run: bool,
        /// Tasks to run at once (default: available parallelism, capped at 16)
        #[arg(short = 'j', long, value_name = "N")]
        jobs: Option<usize>,
        /// Stop starting new tasks after the first failure
        #[arg(long)]
        fail_fast: bool,
        /// Show why each task is in its bucket
        #[arg(long)]
        explain: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run the work a branch's changes require, and report what was reused
    Ci {
        /// Revision to compare against; overrides anything the CI provider says
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        /// Revision to compare; without it, the working tree is compared
        #[arg(long, value_name = "REV")]
        head: Option<String>,
        /// Analyse and plan, but execute nothing
        #[arg(long)]
        dry_run: bool,
        /// Say why each task was selected or skipped
        #[arg(long)]
        explain: bool,
        /// Limit the run to these CI task names or tags
        #[arg(long, value_name = "NAME")]
        task: Vec<String>,
        /// Tasks to run at once (default: available parallelism, capped at 16)
        #[arg(short = 'j', long, value_name = "N")]
        jobs: Option<usize>,
        /// Stop starting new tasks after the first failure
        #[arg(long)]
        fail_fast: bool,
        /// Allow Arc to deepen the repository to obtain the base commit
        #[arg(long)]
        fetch: bool,
        /// Ignore any configured remote cache
        #[arg(long)]
        no_remote: bool,
        /// Publish results even when the event is not one Arc considers trusted
        #[arg(long)]
        remote_write: bool,
        /// Do not write a GitHub job summary or step outputs
        #[arg(long)]
        no_summary: bool,
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
    /// Capture and inspect execution environments
    Env {
        #[command(subcommand)]
        command: EnvCmd,
    },
    /// Inspect the configured remote cache
    Remote {
        #[command(subcommand)]
        command: RemoteCmd,
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
enum EnvCmd {
    /// Capture the tools an [environment.<alias>] block names, and pin the id
    Capture {
        alias: String,
        /// Also upload the environment to the remote cache, so a worker or
        /// another machine can materialise it without this one
        #[arg(long)]
        publish: bool,
        #[arg(long)]
        json: bool,
    },
    /// List environments this machine has materialised
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show what an environment contains
    Inspect {
        /// Alias from arc-env.lock, or an environment id
        name: String,
        /// List every captured file rather than a summary
        #[arg(long)]
        files: bool,
        #[arg(long)]
        json: bool,
    },
    /// Re-hash every object an environment names and prove it materialises
    Verify { name: String },
    /// Compare each pinned alias against what capturing it here would produce
    Status,
    /// Remove materialised environments no longer pinned by this project
    Gc,
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Show the remote cache configuration and whether it can be reached
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Contact the remote cache and report the round trip
    Ping,
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
            no_remote,
            remote_execution,
            no_remote_execution,
            trace_backend,
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
                no_remote,
                remote_execution: match (remote_execution, no_remote_execution) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                },
                backend: arc_core::trace::Selection::parse(&trace_backend).with_context(|| {
                    format!(
                        "unknown --trace-backend `{trace_backend}`; use {}",
                        arc_core::trace::Selection::NAMES
                    )
                })?,
            },
            Display {
                explain,
                trace,
                verbose,
                json,
            },
        ),
        Cmd::Graph {
            task,
            affected,
            verbose,
            json,
        } => cmd_graph(&home, &cwd, task, affected, verbose, json).map(|_| 0),
        Cmd::Affected {
            run,
            dry_run,
            jobs,
            fail_fast,
            explain,
            json,
        } => cmd_affected(&home, &cwd, run, dry_run, jobs, fail_fast, explain, json),
        Cmd::Ci {
            base,
            head,
            dry_run,
            explain,
            task,
            jobs,
            fail_fast,
            fetch,
            no_remote,
            remote_write,
            no_summary,
            json,
        } => cmd_ci(
            &home,
            &cwd,
            arc_core::ci::analysis::CiOptions {
                base,
                head,
                tasks: task,
                fetch,
                no_remote,
                force_remote_write: remote_write,
            },
            CiDisplay {
                dry_run,
                explain,
                json,
                summary: !no_summary,
                jobs,
                fail_fast,
            },
        ),
        Cmd::History { limit, json } => cmd_history(&home, limit, json).map(|_| 0),
        Cmd::Inspect { id, json } => cmd_inspect(&home, &id, json).map(|_| 0),
        Cmd::Cache { command } => cmd_cache(&home, &cwd, command).map(|_| 0),
        Cmd::Env { command } => cmd_env(&home, &cwd, command),
        Cmd::Remote { command } => cmd_remote(&cwd, command),
        Cmd::Doctor => cmd_doctor(&home, &cwd).map(|_| 0),
        Cmd::Config { command } => match command {
            ConfigCmd::Show => cmd_config_show(&cwd).map(|_| 0),
        },
        Cmd::Clean { all } => cmd_clean(&home, all).map(|_| 0),
    }
}

/// Drives the spinner from the engine's stage reports. Once the command itself
/// starts, the spinner stops for good: the child owns the terminal from there.
struct SpinnerProgress {
    spinner: std::sync::Mutex<ui::Spinner>,
}

impl engine::Progress for SpinnerProgress {
    /// A worker's output goes to stderr as it arrives, so a long remote command
    /// is not silent. The run's own stdout and stderr are still replayed
    /// faithfully when it finishes.
    fn log(&self, text: &str) {
        if let Ok(mut s) = self.spinner.lock() {
            s.stop();
        }
        eprint!("{text}");
        use std::io::Write;
        let _ = std::io::stderr().flush();
    }

    fn stage(&self, label: &str) {
        let Ok(mut s) = self.spinner.lock() else {
            return;
        };
        if label == "executing" {
            s.stop();
        } else {
            s.set(label);
        }
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

    // The spinner erases itself the moment the child is about to run, so its
    // output and Arc's never share a line.
    let progress = SpinnerProgress {
        spinner: std::sync::Mutex::new(ui::Spinner::start("starting")),
    };
    let report = engine::run(&project, cwd, program, args, &opts, home, &progress);
    drop(progress);
    let report = report?;
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
        eprintln!();
        ui::flourish();
        eprintln!(
            "{} {}  {}",
            ui::brand(ui::MARK),
            ui::badge(if report.record.cache_source == CacheSource::Remote {
                "REMOTE HIT"
            } else {
                "CACHE HIT"
            }),
            ui::dim(&report.record.command_line())
        );
        eprintln!("  {detail}");
        if let Some(r) = &report.remote {
            if report.record.cache_source == CacheSource::Remote {
                eprintln!(
                    "  {}",
                    ui::dim(&format!(
                        "fetched {} from {} in {}",
                        ui::bytes(r.metrics.bytes_downloaded),
                        r.endpoint,
                        ui::duration(r.metrics.lookup_ms + r.metrics.transfer_ms)
                    ))
                );
            }
        }
    }

    if report.record.cache_status != CacheStatus::Hit {
        if let Some(x) = report.remote_execution.as_ref().filter(|x| x.used) {
            eprintln!();
            eprintln!(
                "{} {}  {}",
                ui::brand(ui::MARK),
                ui::badge("REMOTE EXEC"),
                ui::dim(&report.record.command_line())
            );
            let t = x.timings.clone().unwrap_or_default();
            eprintln!(
                "  ran in {} {} queued {} {} {} in, {} out {} {}",
                ui::duration(report.record.duration_ms),
                ui::dim("·"),
                ui::duration(x.queued_ms),
                ui::dim("·"),
                ui::bytes(t.input_bytes),
                ui::bytes(t.output_bytes),
                ui::dim("·"),
                ui::dim(&x.endpoint)
            );
            if !x.published {
                eprintln!("  {}", ui::dim("result not published to the shared cache"));
            }
        }
    }

    if let Some(env) = &report.environment {
        eprintln!();
        eprintln!(
            "{} {}  {} {} {}",
            ui::brand(ui::MARK),
            ui::badge("ENVIRONMENT"),
            ui::accent(&env.alias),
            ui::dim("·"),
            ui::dim(&env.id[..12])
        );
        if !env.reused {
            eprintln!(
                "  {}",
                ui::dim(&format!(
                    "materialised {}",
                    ui::bytes(env.materialised_bytes)
                ))
            );
        }
        match env.hermeticity.as_str() {
            "" => {}
            "hermetic" => eprintln!(
                "  {}",
                ui::green("hermetic: nothing outside the environment")
            ),
            "unknown" => eprintln!(
                "  {}",
                ui::dim("hermeticity unknown: the execution was not completely observed")
            ),
            _ => {
                eprintln!(
                    "  {}",
                    ui::yellow("host-dependent: this run read host state")
                );
                for l in env.leaks.iter().take(5) {
                    eprintln!("    {}", ui::dim(l));
                }
                if env.leaks.len() > 5 {
                    eprintln!(
                        "    {}",
                        ui::dim(&format!("and {} more", env.leaks.len() - 5))
                    );
                }
            }
        }
    }

    // A remote problem is worth one line and no more: the command itself has
    // already been served correctly either way.
    if let Some(e) = report.remote.as_ref().and_then(|r| r.error.as_ref()) {
        eprintln!("  {}", ui::yellow(&format!("remote cache: {e}")));
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
            "cache": {
                "status": r.cache_status.label().to_lowercase(),
                "source": if r.cache_status == CacheStatus::Hit {
                    r.cache_source.label()
                } else {
                    "none"
                },
            },
            "execution": {
                "source": e.execution_source,
                "reason": e.remote_execution,
                "worker": report.remote_execution.as_ref().filter(|x| x.used).map(|x| x.endpoint.clone()),
                "job": report.remote_execution.as_ref().and_then(|x| x.job_id.clone()),
                "published": report.remote_execution.as_ref().map(|x| x.published).unwrap_or(false),
                "queued_ms": report.remote_execution.as_ref().map(|x| x.queued_ms).unwrap_or(0),
                "timings": report.remote_execution.as_ref().and_then(|x| x.timings.clone()),
            },
            "environment": report.environment.as_ref().map(|env| serde_json::json!({
                "alias": env.alias,
                "id": env.id,
                "completeness": env.completeness,
                "hermeticity": env.hermeticity,
                "leaks": env.leaks,
                "materialised_bytes": env.materialised_bytes,
                "reused": env.reused,
            })),
            "remote": report.remote.as_ref().map(|rr| serde_json::json!({
                "endpoint": rr.endpoint,
                "namespace": rr.namespace,
                "read": rr.read,
                "write": rr.write,
                "error": rr.error,
                "metrics": rr.metrics,
            })),
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
                    ui::green("inputs narrowed")
                } else {
                    ui::dim(&e.narrow_reason)
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
    if e.execution_source != "none" {
        eprintln!(
            "{}",
            ui::row(
                "executed",
                &if e.execution_source == "remote" {
                    ui::accent("remotely")
                } else {
                    "locally".to_string()
                }
            )
        );
    }
    if !e.environment.is_empty() {
        eprintln!("{}", ui::row("environment", &ui::accent(&e.environment)));
        eprintln!("{}", ui::row("environment id", &ui::dim(&e.environment_id)));
        if !e.hermeticity.is_empty() {
            eprintln!(
                "{}",
                ui::row("hermeticity", &ui::completeness_color(&e.hermeticity))
            );
        }
    }
    if !e.remote_execution.is_empty() && e.remote_execution != "not enabled" {
        eprintln!(
            "{}",
            ui::row("remote execution", &ui::dim(&e.remote_execution))
        );
    }

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
                &ui::yellow("not observed (cache hit, or tracing off)")
            )
        );
        eprintln!();
        return;
    };
    let deps = report.dependencies.as_ref();
    let complete =
        deps.is_some_and(|d| d.completeness == arc_core::dependency::Completeness::Complete);

    eprint!(
        "{}",
        ui::banner(if complete {
            "TRACE COMPLETE"
        } else {
            "TRACE PARTIAL"
        })
    );
    eprintln!("{}", ui::row("command", &report.record.command_line()));
    eprintln!(
        "{}",
        // What actually ran, not what this platform prefers: `--trace-backend`
        // and a fallback both make those differ.
        ui::row(
            "backend",
            &ui::accent(
                report
                    .record
                    .trace
                    .as_ref()
                    .map(|t| t.backend.clone())
                    .unwrap_or_else(|| arc_core::trace::platform_backend_name().to_string())
                    .as_str()
            )
        )
    );
    eprintln!("{}", ui::row("processes", &obs.processes.len().to_string()));
    eprintln!("{}", ui::row("events", &obs.files.len().to_string()));
    if let Some(d) = deps {
        for (label, n) in [
            ("files read", d.inputs.len() + d.external.len()),
            (
                "directories",
                d.directories.len() + d.external_directories.len(),
            ),
            ("existence checks", d.existence.len()),
            ("executables", d.executables.len()),
            ("outputs", d.outputs.len()),
        ] {
            eprintln!("{}", ui::row(label, &n.to_string()));
        }
        eprintln!(
            "{}",
            ui::row(
                "dependency model",
                &ui::completeness_color(d.completeness.label())
            )
        );
        // A partial model must never read as a complete one, so every reason it
        // fell short is printed rather than summarised away.
        for reason in &d.downgrades {
            eprintln!(
                "{}",
                ui::row("not complete", &ui::yellow(&reason.describe()))
            );
        }
        eprintln!(
            "{}",
            ui::row(
                "next run narrows",
                &check(report.explain.inputs_narrowed || complete)
            )
        );
    }
    for note in &obs.notes {
        eprintln!("{}", ui::row("note", &ui::dim(note)));
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
            eprintln!("\n  {}", ui::dim("observations, in order"));
            for f in obs.files.iter().take(300) {
                eprintln!(
                    "    {:<8} {}",
                    f.op.label(),
                    f.rel.as_deref().unwrap_or(&f.path)
                );
            }
            if obs.files.len() > 300 {
                eprintln!(
                    "{}",
                    ui::dim(&format!("    and {} more", obs.files.len() - 300))
                );
            }
        }
    }
    eprintln!(
        "
  {}",
        ui::dim(&format!(
            "{} total",
            ui::duration(report.record.duration_ms)
        ))
    );
    eprintln!();
}

fn cmd_graph(
    home: &Path,
    cwd: &Path,
    task: Option<String>,
    affected_only: bool,
    verbose: bool,
    json: bool,
) -> Result<()> {
    let project = Project::discover(cwd)?;
    let db = Db::open(home)?;
    let graph = arc_core::graph::build(&project, &db)?;

    let keep: Option<BTreeSet<String>> = match (&task, affected_only) {
        (Some(needle), _) => {
            let matches = graph.find(needle);
            if matches.is_empty() {
                anyhow::bail!("no task matches `{needle}`. Try `arc graph` to list them.");
            }
            let mut keep: BTreeSet<String> = BTreeSet::new();
            for m in matches {
                keep.insert(m.family_key.clone());
                keep.extend(reachable(&graph, &m.family_key, true));
                keep.extend(reachable(&graph, &m.family_key, false));
            }
            Some(keep)
        }
        (None, true) => {
            let report = arc_core::affected::compute(&project, &db, cwd)?;
            Some(
                report
                    .selected()
                    .iter()
                    .map(|t| t.family_key.clone())
                    .collect(),
            )
        }
        (None, false) => None,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&filter_graph(&graph, keep.as_ref()))?
        );
        return Ok(());
    }

    if graph.nodes.is_empty() {
        print!("{}", ui::banner("TASK GRAPH"));
        println!(
            "  {}\n",
            ui::dim("No executions recorded for this project yet.")
        );
        println!("  Run:\n    arc run <command>");
        return Ok(());
    }

    print!("{}", ui::banner("TASK GRAPH"));
    println!("{}\n", ui::dim(&graph.project_root));

    let shown: Vec<&arc_core::graph::TaskNode> = graph
        .nodes
        .iter()
        .filter(|n| keep.as_ref().map_or(true, |k| k.contains(&n.family_key)))
        .collect();
    let visible: BTreeSet<&str> = shown.iter().map(|n| n.family_key.as_str()).collect();

    let mut printed: BTreeSet<&str> = BTreeSet::new();
    for node in shown.iter().filter(|n| {
        graph
            .incoming(&n.family_key)
            .all(|e| !visible.contains(e.from.as_str()))
    }) {
        print_subtree(
            &graph,
            node,
            &visible,
            &mut printed,
            "",
            true,
            true,
            verbose,
        );
    }
    // Anything reachable only through a cycle has no root to hang from.
    let orphans: Vec<&arc_core::graph::TaskNode> = shown
        .iter()
        .copied()
        .filter(|n| !printed.contains(n.family_key.as_str()))
        .collect();
    for node in orphans {
        print_subtree(
            &graph,
            node,
            &visible,
            &mut printed,
            "",
            true,
            true,
            verbose,
        );
    }

    println!();
    let complete = graph
        .nodes
        .iter()
        .filter(|n| n.completeness == arc_core::dependency::Completeness::Complete)
        .count();
    println!(
        "  {}",
        ui::dim(&format!(
            "{} · {} · {} complete · {} partial",
            ui::plural(graph.nodes.len(), "task", "tasks"),
            ui::plural(graph.edges.len(), "edge", "edges"),
            complete,
            graph.nodes.len() - complete
        ))
    );
    if !graph.ambiguities.is_empty() {
        println!("\n  {}", ui::yellow("ambiguous producers"));
        for a in graph.ambiguities.iter().take(10) {
            println!(
                "    {} {}",
                a.path,
                ui::dim(&format!("({} producers)", a.producers.len()))
            );
        }
    }
    if !graph.cycles.is_empty() {
        println!("\n  {}", ui::yellow("cycles"));
        for c in &graph.cycles {
            let names: Vec<String> = c
                .members
                .iter()
                .filter_map(|m| graph.node(m).map(|n| n.label.clone()))
                .collect();
            println!("    {}", names.join(" <-> "));
        }
    }
    for u in &graph.unresolved {
        println!(
            "\n  {}",
            ui::yellow(&format!(
                "{}: `after` names unknown task `{}`",
                u.task, u.after
            ))
        );
    }
    println!();
    Ok(())
}

fn reachable(graph: &arc_core::graph::TaskGraph, from: &str, downstream: bool) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![from.to_string()];
    while let Some(key) = queue.pop() {
        let next: Vec<String> = if downstream {
            graph.outgoing(&key).map(|e| e.to.clone()).collect()
        } else {
            graph.incoming(&key).map(|e| e.from.clone()).collect()
        };
        for n in next {
            if seen.insert(n.clone()) {
                queue.push(n);
            }
        }
    }
    seen
}

fn filter_graph(
    graph: &arc_core::graph::TaskGraph,
    keep: Option<&BTreeSet<String>>,
) -> arc_core::graph::TaskGraph {
    let Some(keep) = keep else {
        return graph.clone();
    };
    let mut g = graph.clone();
    g.nodes.retain(|n| keep.contains(&n.family_key));
    g.edges
        .retain(|e| keep.contains(&e.from) && keep.contains(&e.to));
    g
}

#[allow(clippy::too_many_arguments)]
fn print_subtree<'a>(
    graph: &'a arc_core::graph::TaskGraph,
    node: &'a arc_core::graph::TaskNode,
    visible: &BTreeSet<&str>,
    printed: &mut BTreeSet<&'a str>,
    prefix: &str,
    root: bool,
    last: bool,
    verbose: bool,
) {
    let repeat = !printed.insert(node.family_key.as_str());
    let connector = if root {
        String::new()
    } else {
        ui::branch(last)
    };
    let mut tags: Vec<String> = Vec::new();
    if node.completeness != arc_core::dependency::Completeness::Complete {
        tags.push(ui::yellow(node.completeness.label()));
    }
    if graph.cycle_of(&node.family_key).is_some() {
        tags.push(ui::yellow("cycle"));
    }
    if repeat {
        tags.push(ui::dim("(shown above)"));
    }
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!("  {}", tags.join(" "))
    };
    println!("{prefix}{connector}{}{suffix}", ui::bold(&node.label));

    let detail_prefix = if root {
        "  ".to_string()
    } else if last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}{}   ", ui::dim("│"))
    };
    if verbose {
        for p in node.produces.iter().take(8) {
            println!("{detail_prefix}{} {p}", ui::dim("produces"));
        }
        for p in node.declared_inputs.iter().take(8) {
            println!("{detail_prefix}{} {p}", ui::dim("declared"));
        }
    }
    if repeat {
        return;
    }

    let mut children: Vec<&arc_core::graph::TaskNode> = graph
        .outgoing(&node.family_key)
        .filter(|e| visible.contains(e.to.as_str()))
        .filter_map(|e| graph.node(&e.to))
        .collect();
    children.sort_by(|a, b| a.label.cmp(&b.label));
    children.dedup_by(|a, b| a.family_key == b.family_key);

    let child_prefix = if root { String::new() } else { detail_prefix };
    for (i, c) in children.iter().enumerate() {
        print_subtree(
            graph,
            c,
            visible,
            printed,
            &child_prefix,
            false,
            i + 1 == children.len(),
            verbose,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_affected(
    home: &Path,
    cwd: &Path,
    run: bool,
    dry_run: bool,
    jobs: Option<usize>,
    fail_fast: bool,
    explain: bool,
    json: bool,
) -> Result<i32> {
    let project = Project::discover(cwd)?;
    let db = Db::open(home)?;
    let report = arc_core::affected::compute(&project, &db, cwd)?;
    let graph = arc_core::graph::build(&project, &db)?;
    let plan = arc_core::plan::build(&graph, &report);

    if !run {
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            render_affected(&report, &graph, explain);
        }
        return Ok(0);
    }

    if dry_run {
        if json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            render_plan(&plan);
        }
        return Ok(0);
    }

    if plan.is_empty() {
        if json {
            // A caller asking for `--run --json` wants a run summary, whether or
            // not anything needed running.
            println!(
                "{}",
                serde_json::to_string_pretty(&arc_core::plan::RunSummary::default())?
            );
        } else {
            print!("{}", ui::banner("NOTHING TO DO"));
            println!(
                "  {}\n",
                ui::dim("no known task is affected by these changes")
            );
        }
        return Ok(0);
    }

    let opts = arc_core::plan::SchedulerOptions {
        jobs: jobs.unwrap_or_else(arc_core::plan::default_jobs),
        fail_fast,
        ..Default::default()
    };
    db.release();
    if !json {
        print!("{}", ui::banner("RUNNING"));
        println!(
            "  {}\n",
            ui::dim(&format!(
                "{} of {} tasks · {} at a time",
                plan.tasks.len(),
                plan.total_known_tasks,
                opts.jobs
            ))
        );
    }
    let observer = TaskObserver { json };
    let summary = arc_core::plan::execute(&plan, &project.root, home, &opts, &observer)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        render_summary(&summary);
    }
    Ok(summary.exit_code())
}

struct TaskObserver {
    json: bool,
}

impl arc_core::plan::Observer for TaskObserver {
    fn finished(&self, r: &arc_core::plan::TaskResult) {
        if self.json {
            return;
        }
        use arc_core::plan::TaskOutcome;
        let (mark, style): (&str, fn(&str) -> String) = match r.outcome {
            TaskOutcome::Hit => ("HIT", ui::green),
            TaskOutcome::Ran => ("RAN", ui::brand),
            TaskOutcome::Failed => ("FAIL", ui::red),
            TaskOutcome::Blocked => ("BLOCKED", ui::yellow),
            TaskOutcome::Cancelled => ("SKIPPED", ui::dim),
        };
        eprintln!(
            "  {:>8} {:<40} {}",
            style(mark),
            truncate(&r.label, 40),
            ui::dim(&ui::duration(r.duration_ms))
        );
        let body = format!("{}{}", r.stdout, r.stderr);
        if !body.trim().is_empty() {
            for line in body.lines() {
                eprintln!("    {} {line}", ui::dim("|"));
            }
        }
        if let Some(cause) = &r.blocked_by {
            eprintln!(
                "    {} prerequisite {} did not succeed",
                ui::dim("|"),
                ui::dim(&short(cause))
            );
        }
    }
}

fn render_summary(s: &arc_core::plan::RunSummary) {
    type Part = (usize, &'static str, fn(&str) -> String);
    let parts: [Part; 4] = [
        (s.ran, "ran", ui::brand),
        (s.hits, "cached", ui::green),
        (s.failed, "failed", ui::red),
        (s.blocked, "blocked", ui::yellow),
    ];
    let body: Vec<String> = parts
        .iter()
        .filter(|(n, _, _)| *n > 0)
        .map(|(n, label, style)| style(&format!("{n} {label}")))
        .collect();
    eprintln!(
        "\n{} {}  {}",
        ui::brand(ui::MARK),
        ui::bold(&body.join(" · ")),
        ui::dim(&ui::duration(s.duration_ms))
    );
}

fn render_plan(plan: &arc_core::plan::ExecutionPlan) {
    print!("{}", ui::banner("EXECUTION PLAN"));
    if plan.is_empty() {
        println!("  {}\n", ui::dim("nothing to run"));
        return;
    }
    for (i, wave) in plan.waves.iter().enumerate() {
        let together = if wave.len() > 1 {
            ui::dim("  (concurrent)")
        } else {
            String::new()
        };
        println!("  {}{together}", ui::dim(&format!("step {}", i + 1)));
        for label in wave {
            println!("    {}", ui::bold(label));
        }
    }
    println!(
        "\n  {}\n",
        ui::dim(&format!(
            "{} of {} tasks would run; {} skipped",
            plan.tasks.len(),
            plan.total_known_tasks,
            plan.skipped
        ))
    );
}

fn render_affected(
    report: &arc_core::affected::Report,
    graph: &arc_core::graph::TaskGraph,
    explain: bool,
) {
    use arc_core::affected::Verdict;
    print!("{}", ui::banner("AFFECTED"));
    if let Some(err) = &report.git_error {
        println!(
            "  {}\n",
            ui::yellow(&format!("no change information: {err}"))
        );
        return;
    }
    if report.tasks.is_empty() {
        println!(
            "  {}\n\n  Run:\n    arc run <command>",
            ui::dim("No tasks recorded for this project.")
        );
        return;
    }

    println!("  {}", ui::dim("changed"));
    if report.changes.is_empty() {
        println!("    {}", ui::dim("working tree is clean"));
    }
    for c in report.changes.iter().take(15) {
        println!("    {:<10} {}", ui::dim(c.kind.label()), c.path);
    }
    if report.changes.len() > 15 {
        println!(
            "    {}",
            ui::dim(&format!("and {} more", report.changes.len() - 15))
        );
    }

    for (title, verdict, paint) in [
        (
            "affected",
            Verdict::Affected,
            ui::yellow as fn(&str) -> String,
        ),
        ("unknown", Verdict::Unknown, ui::dim),
        ("unaffected", Verdict::Unaffected, ui::green),
    ] {
        let rows: Vec<_> = report.of(verdict).collect();
        if rows.is_empty() {
            continue;
        }
        println!("\n  {}", ui::dim(title));
        for t in rows {
            println!("    {}", paint(&t.label));
            if explain {
                for line in explain_cause(report, graph, t, 0).iter().take(6) {
                    println!("{line}");
                }
            }
        }
    }
    if report.of(Verdict::Unknown).next().is_some() {
        println!(
            "\n  {}",
            ui::dim("Arc cannot rule these out, so they run. `arc graph -v` shows why.")
        );
    }
    println!();
}

fn explain_cause(
    report: &arc_core::affected::Report,
    graph: &arc_core::graph::TaskGraph,
    task: &arc_core::affected::TaskVerdict,
    depth: usize,
) -> Vec<String> {
    use arc_core::affected::Cause;
    let pad = "      ".to_string() + &"  ".repeat(depth);
    let label_of = |k: &str| {
        graph
            .node(k)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| short(k))
    };
    let mut out = Vec::new();
    for cause in task.causes.iter().take(2) {
        match cause {
            Cause::Changed { path } => out.push(format!(
                "{pad}{}",
                ui::dim(&format!("because {path} changed"))
            )),
            Cause::Upstream { task: up, via, .. } => {
                out.push(format!(
                    "{pad}{}",
                    ui::dim(&format!("because {} is affected, via {via}", label_of(up)))
                ));
                if depth < 3 {
                    if let Some(upstream) = report.task(up) {
                        out.extend(explain_cause(report, graph, upstream, depth + 1));
                    }
                }
            }
            Cause::UpstreamUnknown { task: up } => out.push(format!(
                "{pad}{}",
                ui::dim(&format!("because {} is unknown", label_of(up)))
            )),
            Cause::NotProvable { reason } => {
                out.push(format!("{pad}{}", ui::dim(&format!("because {reason}"))))
            }
            Cause::Cycle => out.push(format!("{pad}{}", ui::dim("part of a dependency cycle"))),
        }
    }
    out
}

struct CiDisplay {
    dry_run: bool,
    explain: bool,
    json: bool,
    summary: bool,
    jobs: Option<usize>,
    fail_fast: bool,
}

fn cmd_ci(
    home: &Path,
    cwd: &Path,
    opts: arc_core::ci::analysis::CiOptions,
    show: CiDisplay,
) -> Result<i32> {
    use arc_core::ci::{self, context::Environment, CiRunSummary};

    let project = Project::discover(cwd)?;
    let db = Db::open(home)?;
    let env = Environment::process();
    let analysis = ci::analysis::analyse(&project, &db, cwd, &env, &opts)?;
    let plan = analysis.plan.clone().unwrap_or_else(|| {
        unreachable!("analysis always produces a plan");
    });

    // Arc does not invent work. Without declared tasks there is nothing CI can
    // select from, and guessing `npm test` would be a different product.
    if analysis.known_tasks == 0 && !show.json {
        render_ci_header(&analysis);
        println!(
            "\n  {}\n\n  {}\n\n    [[command]]\n    name = \"test\"\n    command = \"cargo\"\n    args = [\"test\"]\n\n    [ci]\n    tasks = [\"test\"]\n",
            ui::yellow("no CI tasks are declared"),
            ui::dim("declare what CI should run in arc.toml:")
        );
        return Ok(0);
    }

    let jobs = show.jobs.unwrap_or_else(arc_core::plan::default_jobs);
    let run = if show.dry_run || plan.is_empty() {
        None
    } else {
        if !show.json {
            render_ci_header(&analysis);
            eprintln!(
                "\n  {}\n",
                ui::dim(&format!(
                    "running {} of {} tasks · {jobs} at a time",
                    plan.tasks.len(),
                    analysis.known_tasks
                ))
            );
        }
        db.release();
        // The scheduler runs `arc run` per task, so the remote policy has to
        // travel to the children: a fork's build must not be able to publish
        // merely because it was started by a trusted workflow.
        let mut child_env = Vec::new();
        if !analysis.remote_write {
            child_env.push(("ARC_REMOTE_WRITE".to_string(), "0".to_string()));
        }
        if opts.no_remote {
            child_env.push(("ARC_REMOTE_ENABLED".to_string(), "0".to_string()));
        }
        Some(arc_core::plan::execute(
            &plan,
            &project.root,
            home,
            &arc_core::plan::SchedulerOptions {
                jobs,
                fail_fast: show.fail_fast,
                child_env,
                ..Default::default()
            },
            &TaskObserver { json: show.json },
        )?)
    };

    let summary = CiRunSummary::build(&analysis, run.as_ref());
    if show.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": arc_core::SCHEMA_VERSION,
                "dry_run": show.dry_run,
                "analysis": analysis,
                "plan": plan,
                "summary": summary,
            }))?
        );
    } else {
        if run.is_none() {
            render_ci_header(&analysis);
        }
        render_ci_summary(&analysis, &summary, show.dry_run, show.explain);
    }

    if show.summary {
        publish_ci_summary(&env, &analysis, &summary, show.dry_run);
    }
    Ok(summary.exit_code)
}

/// GitHub's own channels: a Markdown job summary, step outputs for later steps,
/// and an annotation for anything that made the analysis less certain. None of
/// them is load-bearing, so none of them can fail the build.
fn publish_ci_summary(
    env: &arc_core::ci::context::Environment,
    analysis: &arc_core::ci::analysis::CiAnalysis,
    summary: &arc_core::ci::CiRunSummary,
    dry_run: bool,
) {
    use arc_core::ci::{self, github};
    if let Some(path) = env.get("GITHUB_STEP_SUMMARY") {
        let _ = github::write_summary(
            Path::new(path),
            &ci::summary_markdown(analysis, summary, dry_run),
        );
    }
    if let Some(path) = env.get("GITHUB_OUTPUT") {
        let _ = github::write_outputs(Path::new(path), &ci::outputs(summary));
    }
    if analysis.provider == "github-actions" && !analysis.diff_available {
        // stderr, so `--json` on stdout stays a single parseable document.
        for note in analysis.notes.iter().take(3) {
            eprintln!("{}", github::warning(&format!("arc: {note}")));
        }
    }
}

fn render_ci_header(a: &arc_core::ci::analysis::CiAnalysis) {
    use arc_core::ci::sanitize;
    print!("{}", ui::banner("ARC CI"));
    println!(
        "{}",
        ui::row(
            "provider",
            &format!(
                "{} {} {}",
                ui::accent(&sanitize(&a.provider)),
                ui::dim("·"),
                sanitize(&a.event)
            )
        )
    );
    let rev = |v: &Option<String>| v.as_deref().map(short).unwrap_or_else(|| "unknown".into());
    println!("{}", ui::row("base", &rev(&a.base)));
    println!("{}", ui::row("head", &rev(&a.head)));
    println!(
        "{}",
        ui::row(
            "changed",
            &if a.diff_available {
                ui::plural(a.changed_files, "file", "files")
            } else {
                ui::yellow("cannot establish a complete diff")
            }
        )
    );
    for note in a.notes.iter().take(4) {
        println!("{}", ui::row("note", &ui::yellow(&sanitize(note))));
    }
}

fn render_ci_summary(
    a: &arc_core::ci::analysis::CiAnalysis,
    s: &arc_core::ci::CiRunSummary,
    dry_run: bool,
    explain: bool,
) {
    use arc_core::ci::{human_ms, sanitize, TaskOutcome};
    let c = &s.counts;
    println!("\n  {}", ui::dim("plan"));
    println!("    {} affected", c.affected);
    println!("    {} unknown", c.unknown);
    println!("    {} unaffected", c.skipped);

    // Explanations come from the analysis, not the run: a dry run has verdicts
    // and reasons but no outcomes, and "why" is a question about the plan.
    if explain {
        println!("\n  {}", ui::dim("why"));
        for t in &a.selected {
            println!(
                "    {} {}",
                ui::bold(&sanitize(&t.name)),
                ui::dim(&format!("· {}", t.verdict.label()))
            );
            for reason in t.reasons.iter().take(2) {
                println!("      {}", ui::dim(&sanitize(reason)));
            }
        }
        for name in &a.skipped {
            println!(
                "    {} {}",
                ui::bold(&sanitize(name)),
                ui::dim("· skipped, no known dependency intersects the change")
            );
        }
    }

    if dry_run {
        println!(
            "\n  {}\n",
            ui::bold(&format!("would run {} of {} tasks", c.selected, c.known))
        );
        return;
    }

    println!("\n  {}", ui::dim("cache"));
    println!("    {} local", c.local_hits);
    println!("    {} remote", c.remote_hits);
    println!("    {} executed", c.executed);

    println!("\n  {}", ui::dim("time"));
    println!("    {:<10} {}", "arc", ui::duration(s.arc_ms));
    println!("    {:<10} {}", "work", ui::duration(s.work_ms));
    println!(
        "    {:<10} {}",
        "saved",
        if s.estimated_avoided_ms > 0 {
            ui::green(&format!("~{} estimated", human_ms(s.estimated_avoided_ms)))
        } else {
            ui::dim("unknown, no execution history yet")
        }
    );
    if s.avoided_without_history > 0 && s.estimated_avoided_ms > 0 {
        println!(
            "    {:<10} {}",
            "",
            ui::dim(&format!(
                "{} reused tasks have no recorded duration",
                s.avoided_without_history
            ))
        );
    }

    let failed = c.failed + c.blocked;
    let passed = c.selected.saturating_sub(failed);
    eprintln!(
        "\n{} {}\n",
        if failed == 0 {
            ui::green(ui::MARK)
        } else {
            ui::red(ui::MARK)
        },
        ui::bold(&format!(
            "{passed}/{} tasks passed{}",
            c.selected,
            if failed > 0 {
                format!(", {failed} did not")
            } else {
                String::new()
            }
        ))
    );
    for t in s.tasks.iter().filter(|t| t.outcome == TaskOutcome::Blocked) {
        eprintln!(
            "  {} {}",
            ui::yellow("blocked"),
            ui::dim(&sanitize(&t.name))
        );
    }
    if !a.remote_write {
        eprintln!(
            "  {}",
            ui::dim(&format!(
                "remote cache read-only: {}",
                sanitize(&a.remote_write_reason)
            ))
        );
    }
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
    if let Some(env) = &rec.environment {
        ui::field(
            "Environment",
            &format!("{} ({})", env.alias, short(&env.id)),
        );
        if !env.hermeticity.is_empty() {
            ui::field("Hermeticity", &env.hermeticity);
        }
    }
    if let Some(from) = &rec.replayed_from {
        ui::field("Replayed from", from);
        ui::field("Cache source", rec.cache_source.label());
    }
    if !rec.outputs.is_empty() {
        ui::field(
            "Objects",
            &format!(
                "{} ({})",
                rec.outputs.len(),
                ui::bytes(rec.outputs.iter().map(|o| o.size).sum())
            ),
        );
    }
    if !rec.family_key.is_empty() {
        ui::field("Family", &rec.family_key);
        if let Ok(project) = Project::discover(Path::new(&rec.project_root)) {
            if let Ok(graph) = arc_core::graph::build(&project, &db) {
                if let Some(node) = graph.node(&rec.family_key) {
                    println!("{}", ui::dim("Task"));
                    println!("{}", ui::row("label", &node.label));
                    println!("{}", ui::row("produces", &node.produces.len().to_string()));
                    let up = graph.upstream_of(&rec.family_key);
                    let down = graph.downstream_of(&rec.family_key);
                    println!(
                        "{}",
                        ui::row(
                            "upstream",
                            &if up.is_empty() {
                                ui::dim("none")
                            } else {
                                up.iter()
                                    .map(|n| n.label.clone())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        )
                    );
                    println!(
                        "{}",
                        ui::row(
                            "downstream",
                            &if down.is_empty() {
                                ui::dim("none")
                            } else {
                                down.iter()
                                    .map(|n| n.label.clone())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        )
                    );
                    if graph.cycle_of(&rec.family_key).is_some() {
                        println!("{}", ui::row("cycle", &ui::yellow("yes")));
                    }
                    println!();
                }
            }
        }
        let deps = db.dependency_set(&rec.family_key)?;
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
                    "{} {} {}",
                    ui::meter(r / 100.0, 12),
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

/// The remote as configured for this project, with the reason it is not in use
/// when it is not. Never returns a token.
fn remote_for(cwd: &Path) -> Result<(arc_core::remote::RemoteConfig, Result<Remote, String>)> {
    let project = Project::discover(cwd)?;
    let cfg = arc_core::remote::effective_config(&project.config.remote);
    let opened = Remote::open(&project.config.remote).map_err(|d| d.reason());
    Ok((cfg, opened))
}

fn executor_for(cwd: &Path) -> Result<Result<arc_core::remote::Executor, String>> {
    let project = Project::discover(cwd)?;
    Ok(arc_core::remote::Executor::open(&project.config.remote).map_err(|d| d.reason()))
}

/// Shared by `arc doctor` and `arc remote status`: what the worker says about
/// itself, and whether this machine could use it.
fn render_execution(cwd: &Path) {
    match executor_for(cwd) {
        Ok(Ok(x)) => {
            println!("{}", ui::row("configured", &check(true)));
            println!("{}", ui::row("endpoint", &ui::accent(x.endpoint())));
            println!(
                "{}",
                ui::row(
                    "auth",
                    &if x.has_token() {
                        ui::green("token configured")
                    } else {
                        ui::dim("none")
                    }
                )
            );
            match x.capabilities() {
                Ok(c) => {
                    println!("{}", ui::row("reachable", &check(true)));
                    println!("{}", ui::row("worker", &arc_core::ci::sanitize(&c.worker)));
                    println!("{}", ui::row("platform", &format!("{} / {}", c.os, c.arch)));
                    let compatible =
                        c.os == std::env::consts::OS && c.arch == std::env::consts::ARCH;
                    println!(
                        "{}",
                        ui::row(
                            "compatible",
                            &if compatible {
                                ui::green("yes")
                            } else {
                                ui::yellow("no — the worker is a different platform")
                            }
                        )
                    );
                    println!(
                        "{}",
                        ui::row(
                            "capacity",
                            &format!("{} jobs, {} queued", c.max_jobs, c.queued)
                        )
                    );
                    println!("{}", ui::row("environment", &ui::dim(&c.environment_id)));
                    // Stated plainly because it is a limitation, not a feature.
                    println!("{}", ui::row("network", &ui::yellow(c.network.label())));
                }
                Err(e) => {
                    println!("{}", ui::row("reachable", &ui::yellow(&format!("{e:#}"))));
                    println!(
                        "{}",
                        ui::row("impact", &ui::dim("commands run locally instead"))
                    );
                }
            }
        }
        Ok(Err(reason)) => {
            println!("{}", ui::row("configured", &ui::dim(&reason)));
            println!("{}", ui::row("execution", &ui::green("local only")));
        }
        Err(e) => println!("{}", ui::row("configured", &ui::red(&format!("{e:#}")))),
    }
}

// ------------------------------------------------------------ environments --

fn cmd_env(home: &Path, cwd: &Path, command: EnvCmd) -> Result<i32> {
    use arc_core::environment as env;
    let project = Project::discover(cwd)?;
    let store = Store::open(home)?;

    match command {
        EnvCmd::Capture {
            alias,
            publish,
            json,
        } => {
            let cfg = project.config.environment.get(&alias).with_context(|| {
                format!(
                    "no [environment.{alias}] block in {}",
                    project
                        .config_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "arc.toml".into())
                )
            })?;
            let mut spinner = ui::Spinner::start("capturing");
            let captured = env::capture::capture(&env::capture::Spec::from_config(cfg), &store);
            spinner.stop();
            let captured = captured?;
            let id = env::store_manifest(&captured.manifest, &store)?;

            let mut lock = env::lock::Lock::load(&project.root)?;
            let previous = lock.get(&alias).map(str::to_string);
            lock.set(&alias, &id)?;
            let lock_path = lock.save(&project.root)?;

            let published = if publish {
                let r = Remote::open(&project.config.remote)
                    .map_err(|d| anyhow::anyhow!("cannot publish: {}", d.reason()))?;
                let mut digests = captured.manifest.digests();
                digests.push(id.clone());
                r.upload(&store, &digests)?;
                true
            } else {
                false
            };

            if json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "alias": alias,
                        "id": id,
                        "previous": previous,
                        "files": captured.files,
                        "bytes": captured.bytes,
                        "tools": captured.manifest.tools.len(),
                        "completeness": captured.manifest.completeness.label(),
                        "gaps": captured.manifest.gaps,
                        "capture_ms": captured.elapsed_ms,
                        "published": published,
                    }))?
                );
                return Ok(0);
            }

            print!("{}", ui::banner("ENVIRONMENT CAPTURED"));
            println!("{}", ui::row("alias", &ui::accent(&alias)));
            println!("{}", ui::row("id", &captured.id));
            println!("{}", ui::row("files", &captured.files.to_string()));
            println!("{}", ui::row("bytes", &ui::bytes(captured.bytes)));
            println!(
                "{}",
                ui::row("tools", &captured.manifest.tools.len().to_string())
            );
            println!(
                "{}",
                ui::row(
                    "completeness",
                    &ui::completeness_color(captured.manifest.completeness.label())
                )
            );
            for gap in &captured.manifest.gaps {
                println!("{}", ui::row("gap", &ui::yellow(gap)));
            }
            println!(
                "{}",
                ui::row("captured in", &ui::duration(captured.elapsed_ms))
            );
            println!("{}", ui::row("pinned in", &lock_path.display().to_string()));
            if published {
                println!("{}", ui::row("published", &ui::green("yes")));
            }
            if previous.as_deref() == Some(id.as_str()) {
                println!("\n{}", ui::dim("unchanged: nothing on this machine moved"));
            } else if previous.is_some() {
                println!(
                    "\n{}",
                    ui::yellow(
                        "the id changed, so results built under the old one will not be reused"
                    )
                );
            }
            println!();
            Ok(0)
        }

        EnvCmd::List { json } => {
            let envs = env::Environments::open(home)?;
            let materialised = envs.list()?;
            let lock = env::lock::Lock::load(&project.root)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "pinned": lock.environments,
                        "materialised": materialised
                            .iter()
                            .map(|(id, bytes)| serde_json::json!({"id": id, "bytes": bytes}))
                            .collect::<Vec<_>>(),
                    }))?
                );
                return Ok(0);
            }
            print!("{}", ui::banner("ENVIRONMENTS"));
            if lock.environments.is_empty() {
                println!(
                    "{}",
                    ui::dim("  no environments are pinned by this project")
                );
            }
            for (alias, id) in &lock.environments {
                let state = if envs.ready(id) {
                    ui::green("materialised")
                } else {
                    ui::dim("not materialised here")
                };
                println!("  {}  {}  {}", ui::accent(alias), &id[..12], state);
            }
            let unpinned: Vec<&(String, u64)> = materialised
                .iter()
                .filter(|(id, _)| !lock.environments.values().any(|v| v == id))
                .collect();
            if !unpinned.is_empty() {
                println!("\n{}", ui::bold("materialised but not pinned here"));
                for (id, bytes) in unpinned {
                    println!("  {}  {}", &id[..12], ui::bytes(*bytes));
                }
            }
            println!();
            Ok(0)
        }

        EnvCmd::Inspect { name, files, json } => {
            let id = resolve_env_name(&project, &name)?;
            let m = env::load_manifest(&id, &store)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "id": id,
                        "os": m.os,
                        "arch": m.arch,
                        "tools": m.tools,
                        "files": m.files.len(),
                        "bytes": m.total_bytes(),
                        "objects": m.digests().len(),
                        "path": m.path_entries,
                        "library_path": m.library_path,
                        "env": m.env,
                        "host": m.host,
                        "completeness": m.completeness.label(),
                        "gaps": m.gaps,
                    }))?
                );
                return Ok(0);
            }
            print!("{}", ui::banner("ENVIRONMENT"));
            println!("{}", ui::row("id", &id));
            println!("{}", ui::row("platform", &format!("{} / {}", m.os, m.arch)));
            println!("{}", ui::row("files", &m.files.len().to_string()));
            println!("{}", ui::row("objects", &m.digests().len().to_string()));
            println!("{}", ui::row("bytes", &ui::bytes(m.total_bytes())));
            println!(
                "{}",
                ui::row(
                    "completeness",
                    &ui::completeness_color(m.completeness.label())
                )
            );
            println!("\n{}\n", ui::bold("tools"));
            for t in &m.tools {
                println!("  {}  {}  {}", ui::accent(&t.name), t.rel, &t.digest[..12]);
            }
            println!("\n{}\n", ui::bold("execution"));
            println!("{}", ui::row("PATH", &m.path_entries.join(":")));
            if !m.library_path.is_empty() {
                println!("{}", ui::row("library path", &m.library_path.join(":")));
            }
            for (k, v) in &m.env {
                println!("{}", ui::row(k, v));
            }
            println!("\n{}\n", ui::bold("host requirements"));
            println!("{}", ui::row("libc", &m.host.libc));
            for i in &m.host.interpreters {
                println!("{}", ui::row("loader", i));
            }
            if !m.host.libraries.is_empty() {
                println!(
                    "{}",
                    ui::row("system libraries", &m.host.libraries.join(", "))
                );
            }
            for gap in &m.gaps {
                println!("{}", ui::row("gap", &ui::yellow(gap)));
            }
            if files {
                println!("\n{}\n", ui::bold("files"));
                for f in &m.files {
                    let what = match &f.link {
                        Some(t) => format!("-> {t}"),
                        None => format!("{}  {}", &f.digest[..12], ui::bytes(f.size)),
                    };
                    println!("  {}  {}", f.path.v, ui::dim(&what));
                }
            }
            println!();
            Ok(0)
        }

        EnvCmd::Verify { name } => {
            let id = resolve_env_name(&project, &name)?;
            let m = env::load_manifest(&id, &store)?;
            print!("{}", ui::banner("ENVIRONMENT VERIFY"));
            println!("{}", ui::row("id", &id));
            let mut missing = Vec::new();
            let mut corrupt = Vec::new();
            for d in m.digests() {
                let parsed = arc_core::hash::Digest::parse(&d)?;
                if !store.exists(&parsed) {
                    missing.push(d);
                } else if let Ok(Some(actual)) = store.verify(&parsed) {
                    corrupt.push(format!("{} is {}", &d[..12], actual.short()));
                }
            }
            println!("{}", ui::row("objects", &m.digests().len().to_string()));
            println!("{}", ui::row("missing", &missing.len().to_string()));
            println!("{}", ui::row("corrupt", &corrupt.len().to_string()));
            for c in &corrupt {
                println!("{}", ui::row("", &ui::red(c)));
            }
            if !missing.is_empty() || !corrupt.is_empty() {
                println!("\n{} environment cannot be trusted\n", ui::red(ui::MARK));
                return Ok(1);
            }
            match env::Environments::open(home)?.materialise(&m, &store) {
                Ok(e) => {
                    println!("{}", ui::row("materialises", &ui::green("yes")));
                    println!("{}", ui::row("root", &e.root.display().to_string()));
                }
                Err(e) => {
                    println!("{}", ui::row("materialises", &ui::red(&format!("{e:#}"))));
                    return Ok(1);
                }
            }
            println!();
            Ok(0)
        }

        EnvCmd::Status => {
            let lock = env::lock::Lock::load(&project.root)?;
            print!("{}", ui::banner("ENVIRONMENT STATUS"));
            if lock.environments.is_empty() {
                println!("{}\n", ui::dim("  nothing pinned"));
                return Ok(0);
            }
            let mut stale = 0;
            for (alias, id) in &lock.environments {
                let Some(cfg) = project.config.environment.get(alias) else {
                    println!(
                        "  {}  {}",
                        ui::accent(alias),
                        ui::yellow("pinned but no [environment] block defines it")
                    );
                    continue;
                };
                // Re-capturing is the only honest comparison: an environment is
                // its content, and nothing cheaper can tell whether this machine
                // still produces the same content.
                match env::capture::capture(&env::capture::Spec::from_config(cfg), &store) {
                    Ok(now) if now.id == *id => {
                        println!("  {}  {}", ui::accent(alias), ui::green("current"))
                    }
                    Ok(now) => {
                        stale += 1;
                        println!(
                            "  {}  {} (this machine would capture {})",
                            ui::accent(alias),
                            ui::yellow("differs"),
                            &now.id[..12]
                        );
                    }
                    Err(e) => println!("  {}  {}", ui::accent(alias), ui::red(&format!("{e:#}"))),
                }
            }
            if stale > 0 {
                println!(
                    "\n{}",
                    ui::dim(
                        "`arc env capture <alias>` re-pins; results under the old id stay cached"
                    )
                );
            }
            println!();
            Ok(0)
        }

        EnvCmd::Gc => {
            let lock = env::lock::Lock::load(&project.root)?;
            let keep: BTreeSet<String> = lock.environments.values().cloned().collect();
            let removed = env::Environments::open(home)?.gc(&keep)?;
            print!("{}", ui::banner("ENVIRONMENT GC"));
            println!("{}", ui::row("kept", &keep.len().to_string()));
            println!("{}", ui::row("removed", &removed.len().to_string()));
            for id in &removed {
                println!("{}", ui::row("", &ui::dim(&id[..12])));
            }
            println!();
            Ok(0)
        }
    }
}

/// An alias from the lock file, or an id given directly.
fn resolve_env_name(project: &Project, name: &str) -> Result<String> {
    if arc_core::environment::materialise::valid_id(name) {
        return Ok(name.to_string());
    }
    let lock = arc_core::environment::lock::Lock::load(&project.root)?;
    lock.get(name).map(str::to_string).with_context(|| {
        format!("`{name}` is not an environment id and is not pinned in arc-env.lock")
    })
}

fn cmd_remote(cwd: &Path, command: RemoteCmd) -> Result<i32> {
    let (cfg, opened) = remote_for(cwd)?;
    match command {
        RemoteCmd::Status { json } => {
            let reachable = opened.as_ref().ok().map(|r| r.info());
            if json {
                let out = serde_json::json!({
                    "configured": opened.is_ok(),
                    "reason": opened.as_ref().err(),
                    "endpoint": opened.as_ref().ok().map(|r| r.endpoint()),
                    "namespace": cfg.namespace,
                    "read": cfg.read,
                    "write": cfg.write,
                    "auth": opened.as_ref().ok().map(|r| r.has_token()).unwrap_or(false),
                    "reachable": reachable.as_ref().map(|i| i.is_ok()),
                    "protocol": reachable.as_ref().and_then(|i| i.as_ref().ok()).map(|i| i.protocol),
                    "error": reachable.as_ref().and_then(|i| i.as_ref().err()).map(|e| format!("{e:#}")),
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(0);
            }
            print!("{}", ui::banner("REMOTE CACHE"));
            let Ok(remote) = &opened else {
                let reason = opened.as_ref().err().cloned().unwrap_or_default();
                println!("{}", ui::row("configured", &ui::dim(&reason)));
                println!("{}", ui::row("cache", &ui::green("local only")));
                println!();
                return Ok(0);
            };
            println!("{}", ui::row("configured", &check(true)));
            println!("{}", ui::row("endpoint", &ui::accent(remote.endpoint())));
            println!("{}", ui::row("namespace", remote.namespace()));
            println!("{}", ui::row("read", &enabled(remote.read)));
            println!("{}", ui::row("write", &enabled(remote.write)));
            // The presence of a credential, never the credential.
            println!(
                "{}",
                ui::row(
                    "auth",
                    &if remote.has_token() {
                        ui::green("token configured")
                    } else {
                        ui::dim("none")
                    }
                )
            );
            match remote.info() {
                Ok(info) => {
                    println!("{}", ui::row("reachable", &check(true)));
                    println!("{}", ui::row("protocol", &format!("v{}", info.protocol)));
                    println!(
                        "{}",
                        ui::row("server", &format!("{} {}", info.server, info.version))
                    );
                }
                Err(e) => {
                    println!("{}", ui::row("reachable", &ui::yellow(&format!("{e:#}"))));
                    println!(
                        "{}",
                        ui::row("cache", &ui::green("local cache still works"))
                    );
                }
            }
            println!(
                "
{}
",
                ui::bold("execution")
            );
            render_execution(cwd);
            println!();
            Ok(0)
        }
        RemoteCmd::Ping => {
            let remote = opened.map_err(|e| anyhow::anyhow!("no remote cache: {e}"))?;
            let start = std::time::Instant::now();
            let info = remote.info()?;
            println!(
                "{} {} {} protocol v{} in {}",
                ui::green(ui::MARK),
                remote.endpoint(),
                ui::dim("·"),
                info.protocol,
                ui::duration(start.elapsed().as_millis() as u64)
            );
            Ok(0)
        }
    }
}

fn enabled(v: bool) -> String {
    if v {
        ui::green("enabled")
    } else {
        ui::dim("disabled")
    }
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

    let envs = arc_core::environment::Environments::open(home)
        .and_then(|e| e.list())
        .unwrap_or_default();
    let lock = arc_core::environment::lock::Lock::load(&project.root).unwrap_or_default();
    println!("\n{}\n", ui::bold("environments"));
    println!(
        "{}",
        ui::row("configured", &project.config.environment.len().to_string())
    );
    println!(
        "{}",
        ui::row("pinned", &lock.environments.len().to_string())
    );
    // Deliberately from stored state: proving an environment is intact means
    // re-hashing every object in it, which is `arc env verify`, not `doctor`.
    println!("{}", ui::row("materialised here", &envs.len().to_string()));
    println!(
        "{}",
        ui::row(
            "on disk",
            &ui::bytes(envs.iter().map(|(_, b)| b).sum::<u64>())
        )
    );
    let host = arc_core::environment::host_capability();
    println!("{}", ui::row("host userspace", &host.libc));
    println!("{}", ui::row("sandbox", &host.sandbox.join(", ")));
    for (alias, id) in &lock.environments {
        let state = if envs.iter().any(|(m, _)| m == id) {
            ui::green("ready")
        } else {
            ui::dim("not materialised")
        };
        println!("{}", ui::row(alias, &format!("{}  {}", &id[..12], state)));
    }

    println!("\n{}\n", ui::bold("tracing"));
    let probe = arc_core::trace::probe();
    println!("{}", ui::row("preferred", &ui::accent(probe.name)));
    println!("{}", ui::row("available", &check(probe.available)));
    for b in &probe.backends {
        let state = match (&b.available, &b.reason) {
            (true, _) => ui::green("available"),
            (false, Some(r)) => ui::dim(r),
            (false, None) => ui::dim("unavailable"),
        };
        println!("{}", ui::row(b.name, &state));
    }
    if !probe.backends.is_empty() {
        println!("{}", ui::row("snapshot", &ui::green("available")));
    }
    if let Some(reason) = &probe.reason {
        // Naming the actual obstacle is the difference between a message a user
        // can act on and one they can only shrug at.
        println!("{}", ui::row("reason", &ui::yellow(reason)));
        println!("{}", ui::row("fallback", &ui::dim(probe.fallback)));
    }
    let caps = probe.capabilities;
    for (label, ok) in caps.rows() {
        println!("{}", ui::row(label, &supported(ok)));
    }
    let narrows = caps.observes_everything();
    println!(
        "{}",
        ui::row(
            "best completeness",
            &ui::completeness_color(if narrows { "complete" } else { "partial" })
        )
    );
    println!(
        "{}",
        ui::row(
            "automatic narrowing",
            &if narrows {
                ui::green("supported")
            } else {
                ui::dim("configuration only (arc.toml [[command]])")
            }
        )
    );

    println!("\n{}\n", ui::bold("remote cache"));
    match remote_for(cwd) {
        Ok((cfg, Ok(remote))) => {
            println!("{}", ui::row("configured", &check(true)));
            println!("{}", ui::row("endpoint", &ui::accent(remote.endpoint())));
            println!("{}", ui::row("namespace", remote.namespace()));
            println!(
                "{}",
                ui::row(
                    "auth",
                    &if remote.has_token() {
                        ui::green("configured")
                    } else {
                        ui::dim("none")
                    }
                )
            );
            println!("{}", ui::row("read", &enabled(cfg.read)));
            println!("{}", ui::row("write", &enabled(cfg.write)));
            match remote.info() {
                Ok(info) => {
                    println!("{}", ui::row("reachable", &check(true)));
                    println!("{}", ui::row("protocol", &format!("v{}", info.protocol)));
                }
                // An unreachable remote is a degraded optimisation, not a
                // broken installation, so doctor says so and moves on.
                Err(e) => {
                    println!("{}", ui::row("reachable", &ui::yellow(&format!("{e:#}"))));
                    println!(
                        "{}",
                        ui::row("impact", &ui::dim("local cache still functional"))
                    );
                }
            }
        }
        Ok((_, Err(reason))) => {
            println!("{}", ui::row("configured", &ui::dim(&reason)));
            println!("{}", ui::row("cache", &ui::green("local only")));
        }
        Err(e) => println!("{}", ui::row("configured", &ui::red(&format!("{e:#}")))),
    }

    println!(
        "
{}
",
        ui::bold("remote execution")
    );
    render_execution(cwd);

    println!("\n{}\n", ui::bold("continuous integration"));
    {
        use arc_core::ci::context::{CiContext, Environment};
        let env = Environment::process();
        let ctx = CiContext::detect(&env);
        println!("{}", ui::row("provider", &ui::accent(ctx.provider.label())));
        println!("{}", ui::row("event", ctx.event.label()));
        println!("{}", ui::row("trust", &ui::dim(ctx.trust.label())));
        let resolution = arc_core::ci::revisions::resolve(cwd, &ctx, None, None, false);
        println!(
            "{}",
            ui::row(
                "diff available",
                &match &resolution.comparison {
                    arc_core::ci::revisions::Comparison::Unknown { reason } =>
                        ui::yellow(&arc_core::ci::sanitize(reason)),
                    c => ui::green(c.label()),
                }
            )
        );
        println!("{}", ui::row("shallow clone", &check(!resolution.shallow)));
        match arc_core::ci::tasks::canonical(&project, &[]) {
            Ok(t) if t.is_empty() => println!(
                "{}",
                ui::row(
                    "ci tasks",
                    &ui::dim("none declared ([[command]] + [ci] tasks)")
                )
            ),
            Ok(t) => println!(
                "{}",
                ui::row(
                    "ci tasks",
                    &t.iter()
                        .map(|c| arc_core::ci::sanitize(&c.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            ),
            Err(e) => println!("{}", ui::row("ci tasks", &ui::red(&format!("{e:#}")))),
        }
        println!(
            "{}",
            ui::row(
                "remote write",
                &format!("policy {}", project.config.ci.remote_write.label())
            )
        );
        for w in arc_core::ci::volatile_env_warnings(&project.config) {
            println!("{}", ui::row("warning", &ui::yellow(&w)));
        }
    }

    println!("\n{}\n", ui::bold("task graph"));
    match Db::open(home).and_then(|db| arc_core::graph::build(&project, &db)) {
        Ok(g) => {
            use arc_core::dependency::Completeness;
            let complete = g
                .nodes
                .iter()
                .filter(|n| n.completeness == Completeness::Complete)
                .count();
            println!("{}", ui::row("tasks", &g.nodes.len().to_string()));
            println!("{}", ui::row("edges", &g.edges.len().to_string()));
            println!("{}", ui::row("complete", &complete.to_string()));
            println!(
                "{}",
                ui::row("partial", &(g.nodes.len() - complete).to_string())
            );
            println!(
                "{}",
                ui::row("ambiguous outputs", &count_style(g.ambiguities.len()))
            );
            println!("{}", ui::row("cycles", &count_style(g.cycles.len())));
            if !g.unresolved.is_empty() {
                println!(
                    "{}",
                    ui::row("unresolved `after`", &count_style(g.unresolved.len()))
                );
            }
        }
        Err(e) => println!("{}", ui::row("task graph", &ui::red(&format!("{e:#}")))),
    }

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
    println!(
        "{}",
        ui::row("selective execution", &ui::green("supported"))
    );
    println!("{}", ui::row("remote cache", &ui::green("supported")));
    println!(
        "{}",
        ui::row("shared task knowledge", &ui::green("supported"))
    );
    println!(
        "{}",
        ui::row(
            "ci integration",
            &if arc_core::git::available(cwd) {
                ui::green("github actions, generic")
            } else {
                ui::yellow("needs git on PATH")
            }
        )
    );
    println!("{}", ui::row("remote execution", &ui::green("supported")));
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

/// Zero is the healthy answer for ambiguities and cycles, so it should not draw
/// the eye the way a non-zero count should.
fn count_style(n: usize) -> String {
    if n == 0 {
        ui::green("0")
    } else {
        ui::yellow(&n.to_string())
    }
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
