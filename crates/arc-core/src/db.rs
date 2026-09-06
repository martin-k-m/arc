//! Metadata store (redb).
//!
//! The database is opened for the duration of a transaction and closed again,
//! never held across a child process execution. redb takes an exclusive file
//! lock, so short critical sections plus bounded retry is what makes two
//! concurrent `arc run` invocations safe rather than corrupt.

use crate::dependency::DependencySet;
use crate::family::ExecutionFamily;
use crate::graph::TaskNode;
use crate::hash::Digest;
use crate::record::{CacheEntry, ExecutionRecord};
use crate::scan::FingerprintMap;
use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const EXECUTIONS: TableDefinition<&str, &str> = TableDefinition::new("executions");
/// `{started_at:013}-{id}` -> execution id, giving cheap reverse-chronological history.
const HISTORY: TableDefinition<&str, &str> = TableDefinition::new("history");
const CACHE: TableDefinition<&str, &str> = TableDefinition::new("cache_entries");
const FINGERPRINTS: TableDefinition<&str, &[u8]> = TableDefinition::new("fingerprints");
const COUNTERS: TableDefinition<&str, u64> = TableDefinition::new("counters");
/// family key -> `ExecutionFamily`.
const FAMILIES: TableDefinition<&str, &str> = TableDefinition::new("execution_families");
/// family key -> `DependencySet`.
const DEPENDENCIES: TableDefinition<&str, &str> = TableDefinition::new("dependency_sets");
/// `{project_id}\0{family key}` -> `TaskNode`.
///
/// Everything the task graph needs, compactly, so building it is one contiguous
/// range scan per project. Edges are derived from these rows in memory and never
/// stored, which makes a stale edge structurally impossible.
const GRAPH: TableDefinition<&str, &str> = TableDefinition::new("task_graph");
/// family key -> `TaskTiming`. How long this kind of work actually takes, so a
/// CI summary can say what was avoided instead of guessing.
const TIMINGS: TableDefinition<&str, &str> = TableDefinition::new("task_timings");
/// Metadata schema marker. A database written by an incompatible version is
/// rebuilt rather than reinterpreted.
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");

/// Bumped when table layouts change incompatibly.
pub const DB_SCHEMA_VERSION: &str = "5";

/// Recent execution durations for one family, bounded so history cannot grow
/// without limit. The median of the retained window is robust against the one
/// cold run that took thirty times as long as the rest.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TaskTiming {
    pub runs: u64,
    pub last_ms: u64,
    #[serde(default)]
    pub recent: Vec<u64>,
}

/// How many durations are kept per family.
pub const TIMING_WINDOW: usize = 16;

impl TaskTiming {
    pub fn median_ms(&self) -> Option<u64> {
        if self.recent.is_empty() {
            return None;
        }
        let mut v = self.recent.clone();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }

    fn record(&mut self, ms: u64) {
        self.runs = self.runs.saturating_add(1);
        self.last_ms = ms;
        self.recent.push(ms);
        let overflow = self.recent.len().saturating_sub(TIMING_WINDOW);
        self.recent.drain(..overflow);
    }
}

pub const COUNTER_HITS: &str = "hits";
pub const COUNTER_MISSES: &str = "misses";
pub const COUNTER_MS_SAVED: &str = "ms_saved";

const LOCK_TIMEOUT: Duration = Duration::from_secs(20);

/// Append one measurement to the file named by `ARC_DB_TRACE`, if it is set.
///
/// This exists for `docs/BUGS.md` #14, which is open because the hold times
/// were never measured: the entry offers two candidate mechanisms, a poll that
/// keeps six contenders in lockstep or a phase that keeps the handle far longer
/// than intended, and says distinguishing them needs exactly this log.
///
/// A line is `<pid> <event> <ms>`, appended, one write per event, so several
/// processes can share one file without coordinating. Failures are ignored on
/// purpose: a diagnostic that can fail a user's command is worse than no
/// diagnostic, and this one runs on the path that opens the database.
fn trace_db(event: &str, ms: u128) {
    let Ok(path) = std::env::var("ARC_DB_TRACE") else {
        return;
    };
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{} {} {}", std::process::id(), event, ms);
    }
}

pub struct Db {
    path: PathBuf,
    /// When the currently open handle was acquired, for `ARC_DB_TRACE`.
    ///
    /// `docs/BUGS.md` #14 is open because nobody has measured how long a holder
    /// keeps this database. The entry names two candidates and says
    /// distinguishing them "means logging how long each holder keeps the
    /// database, which has not been done". This is that log, and it is inert
    /// unless the variable is set.
    held_since: std::cell::RefCell<Option<Instant>>,
    /// The open database, reused across transactions within one phase of a run.
    ///
    /// Opening redb takes an exclusive file lock, so this handle must never be
    /// held across a child execution or concurrent Arc processes would serialise
    /// on the slowest command. [`Db::release`] drops it at exactly that point;
    /// the next call transparently reopens. Reusing it elsewhere turns seven
    /// file opens per run into two.
    open: std::cell::RefCell<Option<Database>>,
}

impl Db {
    pub fn open(arc_home: &Path) -> Result<Db> {
        std::fs::create_dir_all(arc_home)?;
        let db = Db {
            path: arc_home.join("arc.redb"),
            open: std::cell::RefCell::new(None),
            held_since: std::cell::RefCell::new(None),
        };
        // Metadata from an incompatible layout — or a file too damaged to read
        // at all — is discarded, not migrated. Cache contents are disposable;
        // misreading them is not, and failing the user's command over a broken
        // cache would be worse than either.
        if db.path.exists() && db.schema_version() != Some(DB_SCHEMA_VERSION.to_string()) {
            // Windows refuses to unlink a file that is still open.
            db.release();
            std::fs::remove_file(&db.path)
                .with_context(|| format!("replacing unusable {}", db.path.display()))?;
        }
        db.write(|txn| {
            txn.open_table(EXECUTIONS)?;
            txn.open_table(HISTORY)?;
            txn.open_table(CACHE)?;
            txn.open_table(FINGERPRINTS)?;
            txn.open_table(COUNTERS)?;
            txn.open_table(FAMILIES)?;
            txn.open_table(DEPENDENCIES)?;
            txn.open_table(GRAPH)?;
            txn.open_table(TIMINGS)?;
            txn.open_table(META)?.insert("schema", DB_SCHEMA_VERSION)?;
            Ok(())
        })?;
        Ok(db)
    }

    /// `None` covers every reason the marker could not be read: the table does
    /// not exist (an older Arc), or the file is not a database at all. Both mean
    /// the same thing to the caller — do not trust what is there.
    fn schema_version(&self) -> Option<String> {
        self.read(|txn| match txn.open_table(META) {
            Ok(t) => Ok(t.get("schema")?.map(|v| v.value().to_string())),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        })
        .unwrap_or(None)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Close the database if it is open. Call this before running a child
    /// process so other Arc processes are not locked out for its duration.
    pub fn release(&self) {
        self.release_at("release");
    }

    /// `release`, with a label naming the call site, for `ARC_DB_TRACE`.
    ///
    /// The label is what turns "somebody held the database for five seconds"
    /// into "this phase did". Without it the measurement says a hold is long
    /// and cannot say which of the four release points ended it.
    pub fn release_at(&self, label: &str) {
        let was_open = self.open.borrow().is_some();
        *self.open.borrow_mut() = None;
        if was_open {
            if let Some(since) = self.held_since.borrow_mut().take() {
                trace_db(label, since.elapsed().as_millis());
            }
        }
    }

    fn with_db<T>(&self, f: impl FnOnce(&Database) -> Result<T>) -> Result<T> {
        let mut slot = self.open.borrow_mut();
        if slot.is_none() {
            *slot = Some(self.database()?);
            *self.held_since.borrow_mut() = Some(Instant::now());
        }
        f(slot.as_ref().expect("just opened"))
    }

    fn database(&self) -> Result<Database> {
        let start = Instant::now();
        loop {
            match Database::create(&self.path) {
                Ok(db) => {
                    trace_db("open", start.elapsed().as_millis());
                    return Ok(db);
                }
                Err(redb::DatabaseError::DatabaseAlreadyOpen)
                    if start.elapsed() < LOCK_TIMEOUT =>
                {
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Arc could not open its metadata database.\n\n  {}\n\n{}\n\nIf the database is damaged, `arc clean --all` removes the cache and starts over.",
                        self.path.display(),
                        e
                    ))
                }
            }
        }
    }

    fn write<T>(&self, f: impl FnOnce(&redb::WriteTransaction) -> Result<T>) -> Result<T> {
        self.with_db(|db| {
            let txn = db.begin_write().context("beginning write transaction")?;
            let out = f(&txn)?;
            txn.commit().context("committing transaction")?;
            Ok(out)
        })
    }

    fn read<T>(&self, f: impl FnOnce(&redb::ReadTransaction) -> Result<T>) -> Result<T> {
        self.with_db(|db| f(&db.begin_read().context("beginning read transaction")?))
    }

    pub fn put_execution(&self, rec: &ExecutionRecord, entry: Option<&CacheEntry>) -> Result<()> {
        let json = serde_json::to_string(rec)?;
        let hist_key = format!("{:013}-{}", rec.started_at, rec.id);
        let entry_json = entry.map(serde_json::to_string).transpose()?;
        self.write(|txn| {
            txn.open_table(EXECUTIONS)?
                .insert(rec.id.as_str(), json.as_str())?;
            txn.open_table(HISTORY)?
                .insert(hist_key.as_str(), rec.id.as_str())?;
            if let Some(e) = &entry_json {
                txn.open_table(CACHE)?
                    .insert(rec.key.as_str(), e.as_str())?;
            }
            let mut c = txn.open_table(COUNTERS)?;
            let name = match rec.cache_status {
                crate::record::CacheStatus::Hit => COUNTER_HITS,
                crate::record::CacheStatus::Miss => COUNTER_MISSES,
                crate::record::CacheStatus::Bypass => return Ok(()),
            };
            let prev = c.get(name)?.map(|v| v.value()).unwrap_or(0);
            c.insert(name, prev + 1)?;
            Ok(())
        })
    }

    pub fn add_saved_ms(&self, ms: u64) -> Result<()> {
        self.write(|txn| {
            let mut c = txn.open_table(COUNTERS)?;
            let prev = c.get(COUNTER_MS_SAVED)?.map(|v| v.value()).unwrap_or(0);
            c.insert(COUNTER_MS_SAVED, prev + ms)?;
            Ok(())
        })
    }

    pub fn counters(&self) -> Result<(u64, u64, u64)> {
        self.read(|txn| {
            let c = txn.open_table(COUNTERS)?;
            let get = |k: &str| -> Result<u64> { Ok(c.get(k)?.map(|v| v.value()).unwrap_or(0)) };
            Ok((
                get(COUNTER_HITS)?,
                get(COUNTER_MISSES)?,
                get(COUNTER_MS_SAVED)?,
            ))
        })
    }

    /// Look up a cache entry and the execution it points at, in one transaction.
    pub fn lookup(&self, key: &str) -> Result<Option<(CacheEntry, ExecutionRecord)>> {
        self.read(|txn| {
            let cache = txn.open_table(CACHE)?;
            let Some(raw) = cache.get(key)? else {
                return Ok(None);
            };
            let entry: CacheEntry = serde_json::from_str(raw.value())?;
            let execs = txn.open_table(EXECUTIONS)?;
            let Some(rec) = execs.get(entry.execution_id.as_str())? else {
                return Ok(None);
            };
            Ok(Some((entry, serde_json::from_str(rec.value())?)))
        })
    }

    pub fn touch_entry(&self, key: &str, now: i64) -> Result<()> {
        self.write(|txn| {
            let mut cache = txn.open_table(CACHE)?;
            let Some(raw) = cache.get(key)?.map(|v| v.value().to_string()) else {
                return Ok(());
            };
            let mut entry: CacheEntry = serde_json::from_str(&raw)?;
            entry.hits += 1;
            entry.last_accessed = now;
            cache.insert(key, serde_json::to_string(&entry)?.as_str())?;
            Ok(())
        })
    }

    pub fn drop_entry(&self, key: &str) -> Result<()> {
        self.write(|txn| {
            txn.open_table(CACHE)?.remove(key)?;
            Ok(())
        })
    }

    pub fn cache_entries(&self) -> Result<Vec<(String, CacheEntry)>> {
        self.read(|txn| {
            let cache = txn.open_table(CACHE)?;
            let mut out = Vec::new();
            for row in cache.iter()? {
                let (k, v) = row?;
                out.push((k.value().to_string(), serde_json::from_str(v.value())?));
            }
            Ok(out)
        })
    }

    pub fn history(&self, limit: usize) -> Result<Vec<ExecutionRecord>> {
        self.read(|txn| {
            let hist = txn.open_table(HISTORY)?;
            let execs = txn.open_table(EXECUTIONS)?;
            let mut out = Vec::new();
            for row in hist.iter()?.rev() {
                let (_, id) = row?;
                if let Some(rec) = execs.get(id.value())? {
                    out.push(serde_json::from_str(rec.value())?);
                }
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
    }

    pub fn all_executions(&self) -> Result<Vec<ExecutionRecord>> {
        self.read(|txn| {
            let execs = txn.open_table(EXECUTIONS)?;
            let mut out = Vec::new();
            for row in execs.iter()? {
                out.push(serde_json::from_str(row?.1.value())?);
            }
            Ok(out)
        })
    }

    /// Resolve a full or abbreviated execution id.
    pub fn find_execution(&self, prefix: &str) -> Result<Option<ExecutionRecord>> {
        self.read(|txn| {
            let execs = txn.open_table(EXECUTIONS)?;
            if let Some(rec) = execs.get(prefix)? {
                return Ok(Some(serde_json::from_str(rec.value())?));
            }
            for row in execs.iter()? {
                let (k, v) = row?;
                if k.value().starts_with(prefix) {
                    return Ok(Some(serde_json::from_str(v.value())?));
                }
            }
            Ok(None)
        })
    }

    pub fn delete_executions(&self, ids: &[String]) -> Result<()> {
        self.write(|txn| {
            let mut execs = txn.open_table(EXECUTIONS)?;
            let mut hist = txn.open_table(HISTORY)?;
            let stale: Vec<String> = hist
                .iter()?
                .filter_map(|r| r.ok())
                .filter(|(_, v)| ids.iter().any(|i| i == v.value()))
                .map(|(k, _)| k.value().to_string())
                .collect();
            for id in ids {
                execs.remove(id.as_str())?;
            }
            for k in stale {
                hist.remove(k.as_str())?;
            }
            Ok(())
        })
    }

    pub fn load_fingerprints(&self, project: &str) -> Result<FingerprintMap> {
        self.read(|txn| {
            let t = txn.open_table(FINGERPRINTS)?;
            Ok(t.get(project)?
                .map(|v| decode_fingerprints(v.value()))
                .unwrap_or_default())
        })
    }

    pub fn save_fingerprints(&self, project: &str, map: &FingerprintMap) -> Result<()> {
        let bytes = encode_fingerprints(map);
        self.write(|txn| {
            txn.open_table(FINGERPRINTS)?
                .insert(project, bytes.as_slice())?;
            Ok(())
        })
    }

    /// Record that a family was seen, creating it on first sight.
    pub fn touch_family(&self, family: &ExecutionFamily) -> Result<()> {
        let fresh = serde_json::to_string(family)?;
        self.write(|txn| {
            let mut t = txn.open_table(FAMILIES)?;
            let merged = match t.get(family.key.as_str())? {
                Some(raw) => match serde_json::from_str::<ExecutionFamily>(raw.value()) {
                    Ok(prev) => serde_json::to_string(&ExecutionFamily {
                        first_seen: prev.first_seen,
                        runs: prev.runs + 1,
                        ..family.clone()
                    })?,
                    // A record that will not parse is replaced, not trusted.
                    Err(_) => fresh.clone(),
                },
                None => fresh.clone(),
            };
            t.insert(family.key.as_str(), merged.as_str())?;
            Ok(())
        })
    }

    pub fn families(&self) -> Result<Vec<ExecutionFamily>> {
        self.read(|txn| {
            let t = txn.open_table(FAMILIES)?;
            let mut out = Vec::new();
            for row in t.iter()? {
                if let Ok(f) = serde_json::from_str(row?.1.value()) {
                    out.push(f);
                }
            }
            Ok(out)
        })
    }

    /// A malformed dependency record reads as absent, so Arc retraces instead
    /// of trusting data it cannot parse.
    pub fn dependency_set(&self, family_key: &str) -> Result<Option<DependencySet>> {
        self.read(|txn| {
            let t = txn.open_table(DEPENDENCIES)?;
            Ok(t.get(family_key)?
                .and_then(|v| serde_json::from_str(v.value()).ok()))
        })
    }

    /// Store a dependency set together with the task-graph row derived from it,
    /// in one transaction, so the graph can never describe a family whose
    /// dependency set was not written.
    pub fn put_dependency_set(
        &self,
        project_id: &str,
        set: &DependencySet,
        node: &TaskNode,
    ) -> Result<()> {
        let json = serde_json::to_string(set)?;
        let node_json = serde_json::to_string(node)?;
        let family = set.family_key.clone();
        let graph_key = format!("{project_id}\0{family}");
        self.write(|txn| {
            txn.open_table(DEPENDENCIES)?
                .insert(family.as_str(), json.as_str())?;
            txn.open_table(GRAPH)?
                .insert(graph_key.as_str(), node_json.as_str())?;
            Ok(())
        })
    }

    /// Every task-graph row for one project, in key order.
    pub fn task_nodes(&self, project_id: &str) -> Result<Vec<TaskNode>> {
        let (lo, hi) = project_range(project_id);
        self.read(|txn| {
            let t = txn.open_table(GRAPH)?;
            let mut out = Vec::new();
            for row in t.range(lo.as_str()..hi.as_str())? {
                if let Ok(n) = serde_json::from_str(row?.1.value()) {
                    out.push(n);
                }
            }
            Ok(out)
        })
    }

    pub fn task_node(&self, project_id: &str, family_key: &str) -> Result<Option<TaskNode>> {
        let key = format!("{project_id}\0{family_key}");
        self.read(|txn| {
            let t = txn.open_table(GRAPH)?;
            Ok(t.get(key.as_str())?
                .and_then(|v| serde_json::from_str(v.value()).ok()))
        })
    }

    pub fn drop_task_node(&self, project_id: &str, family_key: &str) -> Result<()> {
        let key = format!("{project_id}\0{family_key}");
        self.write(|txn| {
            txn.open_table(GRAPH)?.remove(key.as_str())?;
            Ok(())
        })
    }

    /// Fold one real execution's duration into a family's rolling window.
    /// Replays are not recorded: a hit measures restoration, not work.
    pub fn record_duration(&self, family_key: &str, ms: u64) -> Result<()> {
        if family_key.is_empty() {
            return Ok(());
        }
        self.write(|txn| {
            let mut t = txn.open_table(TIMINGS)?;
            let mut timing: TaskTiming = t
                .get(family_key)?
                .and_then(|v| serde_json::from_str(v.value()).ok())
                .unwrap_or_default();
            timing.record(ms);
            t.insert(family_key, serde_json::to_string(&timing)?.as_str())?;
            Ok(())
        })
    }

    pub fn timings(&self) -> Result<std::collections::HashMap<String, TaskTiming>> {
        self.read(|txn| {
            let t = txn.open_table(TIMINGS)?;
            let mut out = std::collections::HashMap::new();
            for row in t.iter()? {
                let (k, v) = row?;
                if let Ok(timing) = serde_json::from_str(v.value()) {
                    out.insert(k.value().to_string(), timing);
                }
            }
            Ok(out)
        })
    }

    pub fn clear_all(&self) -> Result<()> {
        self.release();
        std::fs::remove_file(&self.path).ok();
        Ok(())
    }
}

// Fingerprints are stored as one packed blob per project: a few hundred
// kilobytes rewritten per run beats a row lookup per file.
fn encode_fingerprints(map: &FingerprintMap) -> Vec<u8> {
    let mut out = Vec::with_capacity(map.len() * 64);
    for (path, (size, mtime, digest)) in map {
        let p = path.as_bytes();
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&mtime.to_le_bytes());
        out.extend_from_slice(digest.bytes());
    }
    out
}

fn decode_fingerprints(mut b: &[u8]) -> FingerprintMap {
    let mut map = FingerprintMap::new();
    while b.len() >= 4 {
        let len = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
        if b.len() < 4 + len + 48 {
            break;
        }
        let path = String::from_utf8_lossy(&b[4..4 + len]).to_string();
        let rest = &b[4 + len..];
        let size = u64::from_le_bytes(rest[..8].try_into().unwrap());
        let mtime = i64::from_le_bytes(rest[8..16].try_into().unwrap());
        let digest = Digest::parse(&hex(&rest[16..48]));
        if let Ok(d) = digest {
            map.insert(path, (size, mtime, d));
        }
        b = &rest[48..];
    }
    map
}

/// Bounds of one project's slice of a `{project_id}\0…` keyspace. `\u{1}` is the
/// next code point after the NUL separator, so the range covers every key for
/// this project and none for any other.
fn project_range(project_id: &str) -> (String, String) {
    (format!("{project_id}\0"), format!("{project_id}\u{1}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;

    #[test]
    fn fingerprint_roundtrip() {
        let mut m = FingerprintMap::new();
        m.insert("src/a.rs".into(), (12, 34, hash_bytes(b"a")));
        m.insert("b.rs".into(), (0, -1, hash_bytes(b"b")));
        assert_eq!(decode_fingerprints(&encode_fingerprints(&m)), m);
    }

    #[test]
    fn timing_history_is_bounded_and_robust_to_one_slow_run() {
        let mut t = TaskTiming::default();
        assert_eq!(t.median_ms(), None, "no history is not zero");
        for ms in [100, 110, 90, 100_000] {
            t.record(ms);
        }
        assert_eq!(
            t.median_ms(),
            Some(110),
            "one cold run does not set the estimate"
        );
        for i in 0..1000 {
            t.record(i);
        }
        assert_eq!(t.recent.len(), TIMING_WINDOW);
        assert_eq!(t.runs, 1004);
        assert_eq!(t.last_ms, 999);
    }

    #[test]
    fn a_timing_window_of_extreme_values_neither_overflows_nor_goes_negative() {
        let mut t = TaskTiming::default();
        for _ in 0..100 {
            t.record(u64::MAX);
        }
        assert_eq!(t.median_ms(), Some(u64::MAX));
        assert_eq!(t.runs, 100);
    }

    #[test]
    fn truncated_fingerprint_blob_does_not_panic() {
        let mut m = FingerprintMap::new();
        m.insert("src/a.rs".into(), (12, 34, hash_bytes(b"a")));
        let enc = encode_fingerprints(&m);
        for cut in 0..enc.len() {
            decode_fingerprints(&enc[..cut]);
        }
    }
}
