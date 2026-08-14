//! The Arc remote cache wire format.
//!
//! Every type here crosses a trust boundary in both directions, so each one
//! carries its own validation. A client validates what a server returns; the
//! reference server validates what a client sends. Neither side may assume the
//! other is a well-behaved Arc.

use serde::{Deserialize, Serialize};

/// Bumped when the meaning of any endpoint or payload changes incompatibly.
/// Also present in the URL prefix, so a mismatch is detectable before a body is
/// ever parsed.
pub const PROTOCOL_VERSION: u32 = 1;
pub const PROTOCOL_PREFIX: &str = "/v1";

pub const DIGEST_HEX_LEN: usize = 64;
pub const MAX_METADATA_BYTES: usize = 16 << 20;
pub const MAX_OUTPUT_ENTRIES: usize = 250_000;
pub const MAX_BATCH_DIGESTS: usize = 4096;
pub const MAX_OBJECT_BYTES: u64 = 16 << 30;
pub const MAX_NAMESPACE_LEN: usize = 128;

/// Object-level compression. Named explicitly rather than reusing HTTP's
/// `gzip`, so no intermediary can compress or decompress on Arc's behalf and
/// leave the digest describing the wrong bytes.
pub const ENCODING_DEFLATE: &str = "arc-deflate";
pub const HEADER_ENCODING: &str = "Arc-Object-Encoding";
pub const HEADER_ACCEPT_ENCODING: &str = "Arc-Accept-Object-Encoding";
pub const HEADER_PROTOCOL: &str = "Arc-Protocol";

/// Below this, deflate costs more than it saves on the wire.
pub const COMPRESS_MIN_BYTES: u64 = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub protocol: u32,
    pub server: String,
    pub version: String,
    #[serde(default)]
    pub encodings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathEncoding {
    Utf8,
    /// Standard base64 of the raw bytes, for platforms where a path is not
    /// required to be valid UTF-8.
    B64,
}

/// A project-relative output path. Identity stays bytes; `Utf8` is a
/// readability convenience, not a promise that all paths are text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePath {
    pub enc: PathEncoding,
    pub v: String,
}

impl WirePath {
    pub fn from_rel(rel: &str) -> WirePath {
        WirePath {
            enc: PathEncoding::Utf8,
            v: rel.to_string(),
        }
    }

    pub fn decode(&self) -> Result<String, String> {
        let rel = match self.enc {
            PathEncoding::Utf8 => self.v.clone(),
            PathEncoding::B64 => {
                let bytes = base64_decode(&self.v).ok_or("malformed base64 path")?;
                String::from_utf8(bytes).map_err(|_| "non-UTF-8 path on a UTF-8 platform")?
            }
        };
        validate_rel_path(&rel)?;
        Ok(rel)
    }
}

/// Reject anything that is not unambiguously a relative path inside a project.
/// This runs before Arc's own restore validation, not instead of it.
pub fn validate_rel_path(rel: &str) -> Result<(), String> {
    if rel.is_empty() {
        return Err("empty output path".into());
    }
    if rel.len() > 4096 {
        return Err("output path is too long".into());
    }
    if rel.contains('\0') || rel.contains('\\') {
        return Err(format!("illegal character in output path `{rel}`"));
    }
    if rel.starts_with('/') {
        return Err(format!("absolute output path `{rel}`"));
    }
    // `C:foo` is relative on Windows but relative to a *drive*, not to us.
    let b = rel.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return Err(format!("drive-qualified output path `{rel}`"));
    }
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!("unsafe output path `{rel}`"));
        }
    }
    Ok(())
}

pub fn valid_digest(hex: &str) -> bool {
    hex.len() == DIGEST_HEX_LEN
        && hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn valid_namespace(ns: &str) -> bool {
    !ns.is_empty()
        && ns.len() <= MAX_NAMESPACE_LEN
        && ns
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && ns != "."
        && ns != ".."
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireBlob {
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireOutput {
    pub path: WirePath,
    pub digest: String,
    pub size: u64,
    pub exec: bool,
}

/// What one machine needs in order to replay an execution another machine
/// performed. Deliberately not the internal record: local ids, absolute paths,
/// environment fingerprints and lock state have no business on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteExecution {
    pub protocol: u32,
    /// Arc's cache-semantics version. An older client's notion of what an
    /// execution key covers must never authorise a newer one's replay.
    pub key_semantics: u32,
    pub execution_key: String,
    pub os: String,
    pub arch: String,
    pub program: String,
    pub args: Vec<String>,
    pub rel_cwd: String,
    pub family_key: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub outputs: Vec<WireOutput>,
    pub stdout: Option<WireBlob>,
    pub stderr: Option<WireBlob>,
    pub arc_version: String,
}

impl RemoteExecution {
    /// Structural validation, independent of who is asking. `expect_key` is
    /// supplied by a client that already computed the key it wants, and by a
    /// server checking the record against its own URL.
    pub fn validate(&self, expect_key: Option<&str>) -> Result<(), String> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(format!("unsupported record protocol {}", self.protocol));
        }
        if !valid_digest(&self.execution_key) {
            return Err("malformed execution key".into());
        }
        if let Some(k) = expect_key {
            if k != self.execution_key {
                return Err("record does not describe the requested execution key".into());
            }
        }
        if self.outputs.len() > MAX_OUTPUT_ENTRIES {
            return Err(format!("{} output entries is too many", self.outputs.len()));
        }
        let mut seen = std::collections::HashSet::with_capacity(self.outputs.len());
        for o in &self.outputs {
            let rel = o.path.decode()?;
            if !seen.insert(rel.clone()) {
                return Err(format!("duplicate output entry `{rel}`"));
            }
            if !valid_digest(&o.digest) {
                return Err(format!("malformed digest for `{rel}`"));
            }
            if o.size > MAX_OBJECT_BYTES {
                return Err(format!("output `{rel}` exceeds the object size limit"));
            }
        }
        for b in [&self.stdout, &self.stderr].into_iter().flatten() {
            if !valid_digest(&b.digest) {
                return Err("malformed stream digest".into());
            }
        }
        Ok(())
    }

    /// Whether this record was produced by a machine whose results this one may
    /// replay at all. The execution key already covers OS and architecture;
    /// checking them explicitly turns a silent non-match into a stated reason.
    pub fn compatible_with_host(&self) -> Result<(), String> {
        if self.key_semantics != crate::SCHEMA_VERSION {
            return Err(format!(
                "record uses cache semantics v{} (this Arc uses v{})",
                self.key_semantics,
                crate::SCHEMA_VERSION
            ));
        }
        if self.os != std::env::consts::OS || self.arch != std::env::consts::ARCH {
            return Err(format!("record was produced on {}/{}", self.os, self.arch));
        }
        Ok(())
    }

    pub fn digests(&self) -> Vec<String> {
        let mut v: Vec<String> = self.outputs.iter().map(|o| o.digest.clone()).collect();
        v.extend(self.stdout.iter().map(|b| b.digest.clone()));
        v.extend(self.stderr.iter().map(|b| b.digest.clone()));
        v.sort();
        v.dedup();
        v
    }

    /// Byte form used both on the wire and for the server's identical-record
    /// comparison. Field order is the declaration order above, so two clients
    /// publishing the same result produce identical bytes.
    pub fn canonical(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// The part of a record two publishers must agree on. How long a command
    /// took is a property of the machine that ran it, not of the result, so
    /// two machines producing the same outputs are not in conflict merely
    /// because one of them was slower.
    pub fn identity(&self) -> Vec<u8> {
        let mut copy = self.clone();
        copy.duration_ms = 0;
        copy.arc_version.clear();
        copy.canonical()
    }
}

pub const MAX_TASK_PATHS: usize = 100_000;
pub const MAX_BATCH_TASKS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireConsumes {
    File,
    Directory,
    Existence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireConsumed {
    pub path: WirePath,
    pub kind: WireConsumes,
}

/// What one machine knows about a task's *dependencies*, published so another
/// machine need not rediscover it by running everything once.
///
/// This is optimisation data and nothing more. It carries `program` and `args`
/// only so a recipient can confirm the record describes the task it already
/// intends to run; a recipient never learns a command from here. See
/// [`RemoteTask::matches_local`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTask {
    pub protocol: u32,
    /// Meaning of the task row itself.
    pub graph_semantics: u32,
    /// Meaning of the dependency set it was derived from.
    pub dependency_semantics: u32,
    /// Meaning of the trace observations behind that dependency set.
    pub trace_semantics: u32,
    pub os: String,
    pub arch: String,
    pub family_key: String,
    pub program: String,
    pub args: Vec<String>,
    pub rel_cwd: String,
    pub completeness: String,
    /// Whether the publisher's knowledge was complete enough to rule changes
    /// out. False means the recipient must treat the task as unknown.
    pub inputs_narrowed: bool,
    pub produces: Vec<WirePath>,
    pub consumes: Vec<WireConsumed>,
    pub declared_inputs: Vec<String>,
    pub observations: u64,
    pub arc_version: String,
}

impl RemoteTask {
    pub fn validate(&self, expect_family: Option<&str>) -> Result<(), String> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(format!("unsupported task protocol {}", self.protocol));
        }
        if !valid_digest(&self.family_key) {
            return Err("malformed family key".into());
        }
        if let Some(f) = expect_family {
            if f != self.family_key {
                return Err("task record does not describe the requested family".into());
            }
        }
        if self.produces.len() + self.consumes.len() > MAX_TASK_PATHS {
            return Err("task record names too many paths".into());
        }
        for p in &self.produces {
            p.decode()?;
        }
        for c in &self.consumes {
            c.path.decode()?;
        }
        for g in &self.declared_inputs {
            if g.len() > 4096 || g.contains('\0') {
                return Err("malformed declared input pattern".into());
            }
        }
        Ok(())
    }

    /// Whether this Arc may read the record at all. Dependency knowledge derived
    /// under different semantics describes a different question, and a
    /// dependency set observed on another platform says nothing about this one.
    pub fn compatible_with_host(&self) -> Result<(), String> {
        if self.graph_semantics != crate::graph::GRAPH_SCHEMA_VERSION
            || self.dependency_semantics != crate::dependency::DEPENDENCY_SCHEMA_VERSION
            || self.trace_semantics != crate::trace::TRACE_SEMANTICS_VERSION
        {
            return Err("task record uses different dependency semantics".into());
        }
        if self.os != std::env::consts::OS || self.arch != std::env::consts::ARCH {
            return Err(format!(
                "task record was observed on {}/{}",
                self.os, self.arch
            ));
        }
        Ok(())
    }

    /// The identity check that keeps a cache server from ever influencing *what*
    /// CI executes: the record must describe the command the checked-out
    /// repository already defines, or it is discarded.
    pub fn matches_local(&self, program: &str, args: &[String], rel_cwd: &str) -> bool {
        self.program == program && self.args == args && self.rel_cwd == rel_cwd
    }

    pub fn canonical(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLookupRequest {
    pub families: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLookupResponse {
    pub tasks: Vec<RemoteTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingRequest {
    pub digests: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingResponse {
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for ch in s.bytes() {
        let v = B64.iter().position(|&b| b == ch)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_paths_are_refused() {
        for bad in [
            "../evil",
            "/etc/passwd",
            "a/../../b",
            "C:/windows",
            "a\\b",
            "",
            "a//b",
            "./a",
        ] {
            assert!(validate_rel_path(bad).is_err(), "{bad} should be refused");
        }
        assert!(validate_rel_path("target/debug/app").is_ok());
    }

    #[test]
    fn digests_and_namespaces_are_syntax_checked() {
        assert!(valid_digest(&"a".repeat(64)));
        assert!(!valid_digest(&"A".repeat(64)));
        assert!(!valid_digest("abc"));
        assert!(!valid_digest(&"g".repeat(64)));
        assert!(valid_namespace("my-project.1"));
        assert!(!valid_namespace("../etc"));
        assert!(!valid_namespace(""));
        assert!(!valid_namespace(".."));
    }

    #[test]
    fn a_byte_encoded_path_decodes_and_is_still_validated() {
        let ok = WirePath {
            enc: PathEncoding::B64,
            v: base64_encode(b"target/debug/app"),
        };
        assert_eq!(ok.decode().unwrap(), "target/debug/app");
        let bad = WirePath {
            enc: PathEncoding::B64,
            v: base64_encode(b"../escape"),
        };
        assert!(bad.decode().is_err());
    }

    #[test]
    fn base64_roundtrips() {
        for case in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[0u8, 255, 128][..]] {
            assert_eq!(base64_decode(&base64_encode(case)).unwrap(), case);
        }
        assert!(base64_decode("!!!!").is_none());
    }

    #[test]
    fn a_record_claiming_a_different_key_is_rejected() {
        let rec = RemoteExecution {
            protocol: PROTOCOL_VERSION,
            key_semantics: crate::SCHEMA_VERSION,
            execution_key: "a".repeat(64),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            program: "echo".into(),
            args: vec![],
            rel_cwd: String::new(),
            family_key: "f".into(),
            exit_code: 0,
            duration_ms: 1,
            outputs: vec![],
            stdout: None,
            stderr: None,
            arc_version: "0.5.0".into(),
        };
        assert!(rec.validate(Some(&"a".repeat(64))).is_ok());
        assert!(rec.validate(Some(&"b".repeat(64))).is_err());
        assert!(rec.compatible_with_host().is_ok());
    }

    #[test]
    fn a_traversing_output_entry_invalidates_the_record() {
        let mut rec = RemoteExecution {
            protocol: PROTOCOL_VERSION,
            key_semantics: crate::SCHEMA_VERSION,
            execution_key: "a".repeat(64),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            program: "echo".into(),
            args: vec![],
            rel_cwd: String::new(),
            family_key: "f".into(),
            exit_code: 0,
            duration_ms: 1,
            outputs: vec![WireOutput {
                path: WirePath::from_rel("../../evil"),
                digest: "b".repeat(64),
                size: 1,
                exec: false,
            }],
            stdout: None,
            stderr: None,
            arc_version: "0.5.0".into(),
        };
        assert!(rec.validate(None).is_err());
        rec.outputs[0].path = WirePath::from_rel("ok.txt");
        assert!(rec.validate(None).is_ok());
    }
}
