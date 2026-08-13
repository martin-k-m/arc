//! Reference implementation of the Arc remote cache protocol.
//!
//! Scope is deliberately small: content-addressed objects, namespace-scoped
//! execution records, a bearer token, and nothing else. It is enough to run a
//! shared cache for a team from a single directory, and enough to validate the
//! protocol against a real client.
//!
//! The server is not trusted by clients and does not trust them either. It
//! hashes every uploaded object itself, validates every record, and never
//! interpolates a client-supplied string into a filesystem path without first
//! checking its syntax.

pub mod storage;

use anyhow::{Context, Result};
use arc_core::remote::protocol::{
    self, ErrorBody, Info, MissingRequest, MissingResponse, RemoteExecution,
};
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use storage::{Publish, Storage};
use tiny_http::{Header, Method, Request, Response, StatusCode};

/// Deliberate misbehaviour, for tests that need to prove a client survives a
/// broken or hostile server. Never settable from the command line.
#[derive(Debug, Default, Clone)]
pub struct Faults {
    pub delay_ms: u64,
    /// Serve object bytes that do not hash to the requested digest.
    pub corrupt_objects: bool,
    /// Close the body early.
    pub truncate_objects: bool,
    /// Fail this many requests with 503 before behaving normally.
    pub fail_first: usize,
    /// Accept the request and never answer.
    pub hang: bool,
}

pub struct Options {
    pub data: PathBuf,
    pub addr: String,
    pub token: Option<String>,
    pub threads: usize,
    pub faults: Faults,
    pub log: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            data: PathBuf::from("arc-cache-data"),
            addr: "127.0.0.1:7890".into(),
            token: None,
            threads: 8,
            faults: Faults::default(),
            log: false,
        }
    }
}

pub struct Server {
    http: Arc<tiny_http::Server>,
    addr: SocketAddr,
    stopping: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

struct State {
    storage: Storage,
    token: Option<String>,
    faults: Faults,
    remaining_failures: AtomicUsize,
    stopping: Arc<AtomicBool>,
    log: bool,
}

impl Server {
    pub fn start(opts: Options) -> Result<Server> {
        let storage = Storage::open(&opts.data)
            .with_context(|| format!("opening cache data at {}", opts.data.display()))?;
        let http = tiny_http::Server::http(&opts.addr)
            .map_err(|e| anyhow::anyhow!("listening on {}: {e}", opts.addr))?;
        let addr = http
            .server_addr()
            .to_ip()
            .context("server has no IP address")?;
        let http = Arc::new(http);
        let stopping = Arc::new(AtomicBool::new(false));
        let state = Arc::new(State {
            storage,
            token: opts.token,
            remaining_failures: AtomicUsize::new(opts.faults.fail_first),
            faults: opts.faults,
            stopping: stopping.clone(),
            log: opts.log,
        });
        let workers = (0..opts.threads.max(1))
            .map(|_| {
                let http = http.clone();
                let state = state.clone();
                std::thread::spawn(move || {
                    while let Ok(req) = http.recv() {
                        handle(&state, req);
                    }
                })
            })
            .collect();
        Ok(Server {
            http,
            addr,
            stopping,
            workers,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn shutdown(self) {}
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.http.unblock();
        // A worker reading the body of a client that vanished mid-upload can be
        // parked in the kernel with no way to interrupt it. Shutdown waits for
        // the orderly case and then stops waiting: a stuck read holds nothing
        // but its own thread, since every commit is a rename.
        let workers = std::mem::take(&mut self.workers);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for w in workers {
                let _ = w.join();
            }
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
    }
}

fn handle(state: &State, req: Request) {
    // Accept and never answer, but stay interruptible so shutdown is not held
    // hostage by an injected fault.
    if state.faults.hang {
        while !state.stopping.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        return;
    }
    if state.faults.delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(state.faults.delay_ms));
    }
    if state.remaining_failures.load(Ordering::Relaxed) > 0
        && state
            .remaining_failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
    {
        return fail(req, 503, "temporarily unavailable");
    }
    let method = req.method().clone();
    let url = req.url().to_string();
    if state.log {
        // Never the Authorization header, and never a token.
        eprintln!("{method} {url}");
    }
    match route(state, &method, &url, req) {
        Ok(()) => {}
        Err(e) => eprintln!("arc-cache: {e}"),
    }
}

fn route(state: &State, method: &Method, url: &str, mut req: Request) -> Result<()> {
    let Some(path) = url.strip_prefix(protocol::PROTOCOL_PREFIX) else {
        return fail_ok(req, 404, "unknown protocol version");
    };
    let path = path.trim_start_matches('/');
    let segs: Vec<&str> = path.split('/').collect();

    if segs.as_slice() == ["info"] {
        return json(
            req,
            200,
            &Info {
                protocol: protocol::PROTOCOL_VERSION,
                server: "arc-cache".into(),
                version: arc_core::VERSION.into(),
                encodings: vec![protocol::ENCODING_DEFLATE.into()],
            },
        );
    }

    if !authorised(state, &req) {
        return fail_ok(req, 401, "missing or invalid credentials");
    }

    let (ns, rest) = match segs.split_first() {
        Some((ns, rest)) if protocol::valid_namespace(ns) && !rest.is_empty() => (*ns, rest),
        _ => return fail_ok(req, 404, "unknown resource"),
    };

    match (method, rest) {
        (Method::Post, ["objects", "missing"]) => {
            let body: MissingRequest = match read_json(&mut req, protocol::MAX_METADATA_BYTES) {
                Ok(b) => b,
                Err(e) => return fail_ok(req, 400, &e.to_string()),
            };
            if body.digests.len() > protocol::MAX_BATCH_DIGESTS {
                return fail_ok(req, 413, "too many digests in one batch");
            }
            let missing = body
                .digests
                .into_iter()
                .filter(|d| protocol::valid_digest(d) && !state.storage.has_object(d))
                .collect();
            json(req, 200, &MissingResponse { missing })
        }
        (Method::Head, ["objects", d]) | (Method::Get, ["objects", d]) => {
            if !protocol::valid_digest(d) {
                return fail_ok(req, 400, "malformed digest");
            }
            let Some(size) = state.storage.object_size(d) else {
                return fail_ok(req, 404, "no such object");
            };
            if method == &Method::Head {
                return Ok(req.respond(Response::empty(200).with_header(len_header(size)))?);
            }
            serve_object(state, req, d, size)
        }
        (Method::Put, ["objects", d]) => {
            if !protocol::valid_digest(d) {
                return fail_ok(req, 400, "malformed digest");
            }
            let compressed = header(&req, protocol::HEADER_ENCODING)
                .map(|v| v == protocol::ENCODING_DEFLATE)
                .unwrap_or(false);
            let digest = d.to_string();
            let mut body = req.as_reader();
            let result = if compressed {
                let mut dec = flate2::read::DeflateDecoder::new(&mut body);
                state.storage.put_object(&digest, &mut dec)
            } else {
                state.storage.put_object(&digest, &mut body)
            };
            match result {
                Ok(_) => Ok(req.respond(Response::empty(201))?),
                Err(e) => fail_ok(req, 400, &e.to_string()),
            }
        }
        (Method::Get, ["executions", key]) => {
            if !protocol::valid_digest(key) {
                return fail_ok(req, 400, "malformed execution key");
            }
            match state.storage.get_record(ns, key) {
                Some(bytes) => Ok(req.respond(
                    Response::from_data(bytes)
                        .with_header(content_type("application/json"))
                        .with_header(protocol_header()),
                )?),
                None => fail_ok(req, 404, "no such execution"),
            }
        }
        (Method::Put, ["executions", key]) => {
            if !protocol::valid_digest(key) {
                return fail_ok(req, 400, "malformed execution key");
            }
            let rec: RemoteExecution = match read_json(&mut req, protocol::MAX_METADATA_BYTES) {
                Ok(r) => r,
                Err(e) => return fail_ok(req, 400, &e.to_string()),
            };
            if let Err(e) = rec.validate(Some(key)) {
                return fail_ok(req, 400, &e);
            }
            // A record may only be published once every object it names is
            // present, so no client can ever fetch a record it cannot replay.
            if let Some(d) = rec.digests().iter().find(|d| !state.storage.has_object(d)) {
                return fail_ok(
                    req,
                    409,
                    &format!("object {} has not been uploaded", &d[..12]),
                );
            }
            match state.storage.put_record(ns, key, &rec)? {
                Publish::Created => Ok(req.respond(Response::empty(201))?),
                Publish::Identical => Ok(req.respond(Response::empty(200))?),
                Publish::Conflict => fail_ok(req, 409, "a different result is already published"),
            }
        }
        _ => fail_ok(req, 404, "unknown resource"),
    }
}

fn serve_object(state: &State, req: Request, digest: &str, size: u64) -> Result<()> {
    let wants_deflate = header(&req, protocol::HEADER_ACCEPT_ENCODING)
        .map(|v| v.contains(protocol::ENCODING_DEFLATE))
        .unwrap_or(false);
    let mut file = state.storage.open_object(digest)?;

    if state.faults.corrupt_objects || state.faults.truncate_objects {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if state.faults.truncate_objects {
            bytes.truncate(bytes.len() / 2);
        } else if let Some(b) = bytes.first_mut() {
            *b ^= 0xff;
        } else {
            bytes.push(0);
        }
        return Ok(req.respond(Response::from_data(bytes))?);
    }

    // Compression buffers the object, so it is applied only where the saving is
    // worth the memory. Large objects stream from disk untouched.
    if wants_deflate && (protocol::COMPRESS_MIN_BYTES..=64 << 20).contains(&size) {
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::copy(&mut file, &mut enc)?;
        let body = enc.finish()?;
        if (body.len() as u64) < size {
            return Ok(req.respond(Response::from_data(body).with_header(encoding_header()))?);
        }
        file = state.storage.open_object(digest)?;
    }
    Ok(req.respond(Response::from_file(file))?)
}

fn authorised(state: &State, req: &Request) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    header(req, "authorization")
        .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
        .map(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
}

/// Comparison whose duration does not depend on how many leading bytes match.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn header(req: &Request, name: &'static str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

fn read_json<T: serde::de::DeserializeOwned>(req: &mut Request, limit: usize) -> Result<T> {
    let mut buf = Vec::new();
    req.as_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut buf)?;
    anyhow::ensure!(buf.len() <= limit, "request body is too large");
    serde_json::from_slice(&buf).context("malformed request body")
}

fn json<T: serde::Serialize>(req: Request, code: u16, body: &T) -> Result<()> {
    let bytes = serde_json::to_vec(body)?;
    Ok(req.respond(
        Response::from_data(bytes)
            .with_status_code(StatusCode(code))
            .with_header(content_type("application/json"))
            .with_header(protocol_header()),
    )?)
}

fn fail_ok(req: Request, code: u16, message: &str) -> Result<()> {
    fail(req, code, message);
    Ok(())
}

fn fail(req: Request, code: u16, message: &str) {
    let body = serde_json::to_vec(&ErrorBody {
        error: message.to_string(),
    })
    .unwrap_or_default();
    let _ = req.respond(
        Response::from_data(body)
            .with_status_code(StatusCode(code))
            .with_header(content_type("application/json")),
    );
}

fn content_type(v: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], v.as_bytes()).unwrap()
}

fn protocol_header() -> Header {
    Header::from_bytes(
        protocol::HEADER_PROTOCOL.as_bytes(),
        protocol::PROTOCOL_VERSION.to_string().as_bytes(),
    )
    .unwrap()
}

fn encoding_header() -> Header {
    Header::from_bytes(
        protocol::HEADER_ENCODING.as_bytes(),
        protocol::ENCODING_DEFLATE.as_bytes(),
    )
    .unwrap()
}

fn len_header(size: u64) -> Header {
    Header::from_bytes(&b"Arc-Object-Length"[..], size.to_string().as_bytes()).unwrap()
}
