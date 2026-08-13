use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "arc-cache", version, about = "Arc remote cache server")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the remote cache protocol over HTTP.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7890")]
        listen: String,
        #[arg(long, default_value = "arc-cache-data")]
        data: PathBuf,
        /// Name of the environment variable holding the bearer token clients
        /// must present. The token itself is never a command-line argument.
        #[arg(long)]
        token_env: Option<String>,
        #[arg(long, default_value_t = 8)]
        threads: usize,
        /// Log one line per request. Never includes credentials.
        #[arg(long)]
        log: bool,
    },
    /// Report what a cache directory holds.
    Stats {
        #[arg(long, default_value = "arc-cache-data")]
        data: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Serve {
            listen,
            data,
            token_env,
            threads,
            log,
        } => {
            let token = match &token_env {
                Some(name) => match std::env::var(name) {
                    Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
                    _ => anyhow::bail!("{name} is not set; refusing to start unauthenticated"),
                },
                None => None,
            };
            let server = arc_cache::Server::start(arc_cache::Options {
                data: data.clone(),
                addr: listen,
                token,
                threads,
                faults: Default::default(),
                log,
            })?;
            println!("arc-cache serving {} on {}", data.display(), server.url());
            println!(
                "  auth {}",
                if token_env.is_some() {
                    "token required"
                } else {
                    "open"
                }
            );
            wait_for_signal();
            println!("\nshutting down");
            server.shutdown();
            Ok(())
        }
        Cmd::Stats { data } => {
            let s = arc_cache::storage::Storage::open(&data)?;
            let (objects, bytes, records, tasks) = s.stats()?;
            println!("objects    {objects}");
            println!("bytes      {bytes}");
            println!("executions {records}");
            println!("tasks      {tasks}");
            Ok(())
        }
    }
}

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Block until Ctrl-C. In-flight uploads finish because each worker owns its
/// request, and nothing is left half-written because every commit is a rename.
fn wait_for_signal() {
    install_hook();
    while !STOP.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(unix)]
fn install_hook() {
    extern "C" fn on_sigint(_: i32) {
        STOP.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // SAFETY: the handler only stores to a static atomic, which is
    // async-signal-safe. It allocates nothing and takes no lock.
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
}

/// On Windows the console delivers Ctrl-C to the process directly; there is no
/// cleanup to perform that a rename has not already made unnecessary.
#[cfg(not(unix))]
fn install_hook() {}
