//! Cache accounting, garbage collection, pruning and verification.

use crate::db::Db;
use crate::hash::Digest;
use crate::store::Store;
use anyhow::Result;
use std::collections::HashSet;

/// History rows older than this are dropped by `prune`, releasing their blobs.
pub const HISTORY_RETENTION: usize = 2_000;

#[derive(Debug, Default)]
pub struct Stats {
    pub cache_entries: usize,
    pub executions: usize,
    pub blobs: usize,
    pub stored_bytes: u64,
    pub logical_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub ms_saved: u64,
}

impl Stats {
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        (total > 0).then(|| self.hits as f64 * 100.0 / total as f64)
    }
    pub fn deduplicated_bytes(&self) -> u64 {
        self.logical_bytes.saturating_sub(self.stored_bytes)
    }
}

pub fn stats(store: &Store, db: &Db) -> Result<Stats> {
    let blobs = store.iter_blobs()?;
    let executions = db.all_executions()?;
    let (hits, misses, ms_saved) = db.counters()?;
    let logical = executions
        .iter()
        .map(|e| {
            e.outputs.iter().map(|o| o.size).sum::<u64>()
                + e.stdout.iter().map(|b| b.size).sum::<u64>()
                + e.stderr.iter().map(|b| b.size).sum::<u64>()
        })
        .sum();
    Ok(Stats {
        cache_entries: db.cache_entries()?.len(),
        executions: executions.len(),
        blobs: blobs.len(),
        stored_bytes: blobs.iter().map(|(_, s)| s).sum(),
        logical_bytes: logical,
        hits,
        misses,
        ms_saved,
    })
}

/// Delete blobs no stored execution refers to. Reachability is computed from
/// the full execution set, so a blob is only removed once nothing can name it.
pub fn gc(store: &Store, db: &Db) -> Result<(usize, u64)> {
    let mut reachable: HashSet<String> = HashSet::new();
    for e in db.all_executions()? {
        reachable.extend(e.blob_digests());
    }
    let mut removed = 0;
    let mut freed = 0;
    for (d, size) in store.iter_blobs()? {
        if !reachable.contains(&d.hex()) {
            store.remove(&d)?;
            removed += 1;
            freed += size;
        }
    }
    Ok((removed, freed))
}

pub struct PruneReport {
    pub entries_removed: usize,
    pub executions_removed: usize,
    pub blobs_removed: usize,
    pub bytes_freed: u64,
    pub final_bytes: u64,
}

/// Bring the store under `max_bytes` by evicting least-recently-used entries.
pub fn prune(store: &Store, db: &Db, max_bytes: u64) -> Result<PruneReport> {
    let target = max_bytes * 4 / 5;
    let mut report = PruneReport {
        entries_removed: 0,
        executions_removed: 0,
        blobs_removed: 0,
        bytes_freed: 0,
        final_bytes: 0,
    };

    // Drop history beyond retention first: those records pin blobs but can no
    // longer be replayed from.
    let mut executions = db.all_executions()?;
    executions.sort_by_key(|e| std::cmp::Reverse(e.started_at));
    let live: HashSet<String> = db
        .cache_entries()?
        .into_iter()
        .map(|(_, e)| e.execution_id)
        .collect();
    let stale: Vec<String> = executions
        .iter()
        .skip(HISTORY_RETENTION)
        .filter(|e| !live.contains(&e.id))
        .map(|e| e.id.clone())
        .collect();
    if !stale.is_empty() {
        report.executions_removed += stale.len();
        db.delete_executions(&stale)?;
    }

    let before = store.total_size()?;
    let (n, freed) = gc(store, db)?;
    report.blobs_removed += n;
    report.bytes_freed += freed;

    let mut size = before - freed;
    if size > max_bytes {
        let mut entries = db.cache_entries()?;
        entries.sort_by_key(|(_, e)| e.last_accessed);
        for chunk in entries.chunks(8) {
            for (key, entry) in chunk {
                db.drop_entry(key)?;
                db.delete_executions(std::slice::from_ref(&entry.execution_id))?;
                report.entries_removed += 1;
                report.executions_removed += 1;
            }
            let (n, freed) = gc(store, db)?;
            report.blobs_removed += n;
            report.bytes_freed += freed;
            size = size.saturating_sub(freed);
            if size <= target {
                break;
            }
        }
    }
    report.final_bytes = store.total_size()?;
    Ok(report)
}

pub struct Corruption {
    pub expected: Digest,
    pub actual: Digest,
    pub quarantined: String,
}

/// Re-hash every stored blob. Anything that fails is quarantined and the cache
/// entries depending on it are dropped, so a corrupt object can never be served.
pub fn verify(store: &Store, db: &Db) -> Result<(usize, Vec<Corruption>)> {
    let blobs = store.iter_blobs()?;
    let mut bad = Vec::new();
    for (d, _) in &blobs {
        if let Some(actual) = store.verify(d)? {
            let path = store.quarantine(d)?;
            bad.push(Corruption {
                expected: *d,
                actual,
                quarantined: path.to_string_lossy().to_string(),
            });
        }
    }
    if !bad.is_empty() {
        let corrupt: HashSet<String> = bad.iter().map(|c| c.expected.hex()).collect();
        let executions = db.all_executions()?;
        for (key, entry) in db.cache_entries()? {
            if let Some(e) = executions.iter().find(|e| e.id == entry.execution_id) {
                if e.blob_digests().iter().any(|d| corrupt.contains(d)) {
                    db.drop_entry(&key)?;
                }
            }
        }
    }
    Ok((blobs.len(), bad))
}
