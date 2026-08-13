//! `arc-worker` — the reference remote execution worker.

use anyhow::{Context, Result};
use arc_worker::{Options, Worker};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "arc-worker",
    version,
    about = "Arc remote execution worker",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Accept execution jobs and run them in isolated workspaces
    Serve {
        #[arg(long, default_value = "127.0.0.1:7891")]
        listen: String,
        /// Where the worker keeps its object store and workspaces
        #[arg(long, default_value = "arc-worker-data")]
        data: PathBuf,
        /// The shared cache to read inputs from and publish results to
        #[arg(long, value_name = "URL")]
        cache_url: String,
        /// Name of the environment variable holding the cache token
        #[arg(long, value_name = "NAME")]
        cache_token_env: Option<String>,
        /// Name of the environment variable holding the token clients must
        /// present to execute. Without one, anybody who can reach the port can
        /// run commands on this machine.
        #[arg(long, value_name = "NAME")]
        token_env: Option<String>,
        /// Name of the environment variable holding a token that may query but
        /// not execute
        #[arg(long, value_name = "NAME")]
        read_token_env: Option<String>,
        /// Commands to run at once
        #[arg(long, default_value_t = 4)]
        max_jobs: usize,
        /// Jobs to hold while workers are busy
        #[arg(long, default_value_t = 64)]
        queue_limit: usize,
        /// Log each request path
        #[arg(long)]
        log: bool,
    },
    /// Summarise the worker's object store
    Stats {
        #[arg(long, default_value = "arc-worker-data")]
        data: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Serve {
            listen,
            data,
            cache_url,
            cache_token_env,
            token_env,
            read_token_env,
            max_jobs,
            queue_limit,
            log,
        } => {
            let execute_token = token(token_env.as_deref())?;
            let read_token = token(read_token_env.as_deref())?;
            let cache = arc_core::remote::RemoteConfig {
                url: cache_url.clone(),
                // Set per job from the request's namespace.
                namespace: "placeholder".into(),
                token_env: cache_token_env.unwrap_or_default(),
                ..Default::default()
            };
            let worker = Worker::start(Options {
                data: data.clone(),
                addr: listen,
                execute_token,
                read_token,
                max_jobs,
                queue_limit,
                cache,
                log,
            })?;
            println!("arc-worker {} on {}", arc_core::VERSION, worker.url());
            println!("  data       {}", data.display());
            println!("  cache      {cache_url}");
            println!("  max jobs   {max_jobs}");
            println!(
                "  auth       {}",
                if token_env.is_some() {
                    "execute token required"
                } else {
                    "open — only safe on a trusted network"
                }
            );
            println!("  network    unrestricted (commands reach what this host reaches)");
            wait_for_signal();
            println!("\nstopping");
            drop(worker);
            Ok(())
        }
        Cmd::Stats { data } => {
            let store = arc_core::store::Store::open(&data)?;
            let blobs = store.iter_blobs()?;
            println!("objects    {}", blobs.len());
            println!("bytes      {}", blobs.iter().map(|(_, s)| s).sum::<u64>());
            Ok(())
        }
    }
}

fn token(var: Option<&str>) -> Result<Option<String>> {
    let Some(name) = var else { return Ok(None) };
    anyhow::ensure!(
        name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid environment variable name `{name}`"
    );
    let value = std::env::var(name)
        .with_context(|| format!("{name} is not set, so no token could be read"))?;
    anyhow::ensure!(!value.trim().is_empty(), "{name} is empty");
    Ok(Some(value.trim().to_string()))
}

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn wait_for_signal() {
    install();
    while !STOP.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

#[cfg(unix)]
fn install() {
    // SAFETY: the handler only stores into a static `AtomicBool`, which is
    // async-signal-safe. It allocates nothing and takes no locks.
    unsafe {
        libc::signal(libc::SIGINT, handle as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle as *const () as libc::sighandler_t);
    }
}

#[cfg(unix)]
extern "C" fn handle(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(unix))]
fn install() {}
