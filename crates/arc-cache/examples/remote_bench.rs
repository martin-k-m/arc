//! Remote cache benchmark.
//!
//! Runs a real client against a real server over loopback, with optional
//! injected per-request latency, and reports the phases separately: a total is
//! not enough to tell a request-count problem from a bandwidth problem.
//!
//!   cargo run --release -p arc-cache --example remote_bench

use arc_core::hash::hash_bytes;
use arc_core::remote::{protocol::RemoteExecution, Remote, RemoteConfig};
use arc_core::store::Store;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    println!("arc remote cache bench\n");
    for delay_ms in [0, 10, 50] {
        for &(label, count, size) in &[
            ("1 object x 1 MB", 1usize, 1 << 20),
            ("100 objects x 16 KB", 100, 16 << 10),
            ("1000 objects x 1 KB", 1000, 1 << 10),
        ] {
            bench(label, count, size, delay_ms)?;
        }
        println!();
    }
    Ok(())
}

fn bench(label: &str, count: usize, size: usize, delay_ms: u64) -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let server = arc_cache::Server::start(arc_cache::Options {
        data: tmp.path().join("server"),
        addr: "127.0.0.1:0".into(),
        threads: 8,
        faults: arc_cache::Faults {
            delay_ms,
            ..Default::default()
        },
        ..Default::default()
    })?;

    let up = Store::open(&tmp.path().join("up"))?;
    let down = Store::open(&tmp.path().join("down"))?;
    let mut digests = Vec::with_capacity(count);
    let mut total = 0u64;
    for i in 0..count {
        // Source-like, so compression has something to work with.
        let body = format!("fn item{i}() -> u32 {{ 0 }}\n").repeat(size / 24 + 1);
        digests.push(up.put_bytes(body.as_bytes())?.hex());
        total += body.len() as u64;
    }

    let cfg = RemoteConfig {
        url: server.url(),
        namespace: "bench".into(),
        ..Default::default()
    };
    let client = Remote::open(&cfg).map_err(|d| anyhow::anyhow!(d.reason()))?;

    let t = Instant::now();
    client.upload(&up, &digests)?;
    let upload_ms = t.elapsed().as_millis();

    let rec = record(&digests);
    let t = Instant::now();
    client.publish(&rec)?;
    let publish_ms = t.elapsed().as_millis();

    let t = Instant::now();
    let fetched = client.lookup(&rec.execution_key)?.expect("published");
    let lookup_ms = t.elapsed().as_millis();

    let t = Instant::now();
    client.download(&down, &fetched.digests())?;
    let download_ms = t.elapsed().as_millis();

    let t = Instant::now();
    client.download(&down, &fetched.digests())?;
    let warm_ms = t.elapsed().as_millis();

    let m = client.metrics();
    println!(
        "{delay_ms:>3}ms rtt | {label:<20} | {} raw {} wire | upload {upload_ms:>5}ms publish {publish_ms:>4}ms lookup {lookup_ms:>4}ms download {download_ms:>5}ms warm {warm_ms:>3}ms | {} requests | verify {}ms",
        bytes(total),
        bytes(m.wire_bytes_uploaded),
        m.requests,
        m.verify_ms,
    );
    Ok(())
}

fn record(digests: &[String]) -> RemoteExecution {
    use arc_core::remote::protocol::{PathEncoding, WireOutput, WirePath, PROTOCOL_VERSION};
    RemoteExecution {
        protocol: PROTOCOL_VERSION,
        key_semantics: arc_core::SCHEMA_VERSION,
        execution_key: hash_bytes(digests.join(",").as_bytes()).hex(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        program: "bench".into(),
        args: vec![],
        rel_cwd: String::new(),
        family_key: "bench".into(),
        exit_code: 0,
        duration_ms: 0,
        outputs: digests
            .iter()
            .enumerate()
            .map(|(i, d)| WireOutput {
                path: WirePath {
                    enc: PathEncoding::Utf8,
                    v: format!("out/{i}.txt"),
                },
                digest: d.clone(),
                size: 0,
                exec: false,
            })
            .collect(),
        stdout: None,
        stderr: None,
        arc_version: arc_core::VERSION.into(),
    }
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1}MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1}KB", n as f64 / (1 << 10) as f64),
        n => format!("{n}B"),
    }
}
