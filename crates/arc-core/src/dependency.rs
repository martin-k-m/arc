//! What Arc has learned an execution family depends on.
//!
//! A dependency set is never a bare `Vec<PathBuf>`. Knowing *what* Arc observed
//! is useless without knowing *how much* it could observe, so every set carries
//! a [`Completeness`] and, when that is anything less than complete, the
//! [`Downgrade`] reasons why.
//!
//! The rule that keeps this safe is one-directional:
//!
//! * knowledge may **add** to a cache key (more executables, more files) under
//!   any completeness, because a key covering more can only cause misses;
//! * knowledge may **narrow** the input set only through [`can_narrow`], which
//!   is the single gate in Arc for that decision, because dropping an input
//!   that mattered is exactly a false hit.
//!
//! Three things a complete set must contain, and why each one alone would make
//! narrowing unsound:
//!
//! * **negative dependencies** — a program that does `if config.local.toml
//!   exists` depended on its absence. Narrowing to the files it read would
//!   replay across that file appearing.
//! * **directory dependencies** — a program that enumerates `plugins/` depends
//!   on the entry set. Narrowing to the plugins that existed would replay
//!   across a new one being added.
//! * **temporal order** — a file this execution created and then read back is
//!   not an input. Recording it as one demands, on the next run, a file that
//!   only exists because of the previous run.

use crate::hash::{Digest, Hasher};
use crate::paths::{display_form, Scope};
use crate::scan::FingerprintMap;
use crate::trace::model::{Downgrade, TRACE_SEMANTICS_VERSION};
use crate::trace::{Capabilities, FileOp, Observations, TRACE_SCHEMA_VERSION};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Bumped when the meaning of a stored dependency set changes. A set recorded
/// under a different version is discarded, never reinterpreted.
///
/// v2 added directory and negative dependencies, external file dependencies,
/// structured downgrade reasons, and temporal classification.
pub const DEPENDENCY_SCHEMA_VERSION: u32 = 2;

/// Ceilings on one learned set. A pathological execution — a package manager
/// probing tens of thousands of candidate paths — must not turn every later run
/// into a filesystem sweep. Past the limit the set stops growing and stops
/// claiming to be complete.
const MAX_INPUTS: usize = 100_000;
const MAX_ABSENT: usize = 20_000;
const MAX_EXTERNAL: usize = 20_000;

/// How much of the execution the backend could actually see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Completeness {
    /// Every read, directory enumeration, existence check and descendant
    /// process was observed, and nothing downgraded the run.
    Complete,
    /// Real observations, but not all of them. Usable to strengthen a key,
    /// never to weaken one.
    Partial,
    /// No backend was available, or tracing was switched off.
    Unsupported,
    /// Stored data did not survive validation. Treated as no knowledge at all.
    Invalid,
}

impl Completeness {
    pub fn label(&self) -> &'static str {
        match self {
            Completeness::Complete => "complete",
            Completeness::Partial => "partial",
            Completeness::Unsupported => "unsupported",
            Completeness::Invalid => "invalid",
        }
    }

    /// Derived from what the backend claims, whether this run lost events, and
    /// whether anything downgraded it. Note that a backend claiming every
    /// capability still only reaches `Complete` with an empty downgrade list.
    pub fn of(caps: &Capabilities, obs: &Observations) -> Completeness {
        if obs.lossy || !obs.downgrades.is_empty() || !caps.observes_everything() {
            Completeness::Partial
        } else {
            Completeness::Complete
        }
    }
}

/// A path outside the project, with the hash of its contents when it was
/// observed. Covers toolchains, shared libraries, interpreters and system
/// configuration — everything a build reads that a project-relative walk would
/// never find.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PathDep {
    pub path: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencySet {
    pub schema: u32,
    pub trace_schema: u32,
    /// Which version of the backend's *rules* produced this set. A set learned
    /// when `/proc` counted as an ordinary file must not be reused once it
    /// counts as volatile.
    #[serde(default)]
    pub trace_semantics: u32,
    pub family_key: String,
    pub backend: String,
    pub capabilities: Capabilities,
    pub completeness: Completeness,
    /// Everything standing between this set and `Complete`.
    #[serde(default)]
    pub downgrades: Vec<Downgrade>,
    /// Project-relative files the execution could read.
    pub inputs: Vec<String>,
    /// Project-relative directories whose entry set the execution enumerated.
    pub directories: Vec<String>,
    /// Absolute paths whose *presence or absence* the execution depended on,
    /// without depending on their contents: a config file that was looked for
    /// and not found, a directory whose existence was checked but whose entries
    /// were never listed. The dependency is the boolean, and it is a dependency
    /// in both directions — a path that was absent appearing, and a path that
    /// was there disappearing, both change the answer.
    pub existence: Vec<String>,
    /// Files outside the project the execution read.
    #[serde(default)]
    pub external: Vec<PathDep>,
    /// Directories outside the project the execution enumerated.
    #[serde(default)]
    pub external_directories: Vec<String>,
    /// Project-relative files the execution created, modified or deleted.
    pub outputs: Vec<String>,
    /// Inputs this execution also wrote to. Their state at the end of the run is
    /// not the state it consumed, which is why a result must never be filed
    /// under a key computed after the fact.
    #[serde(default)]
    pub self_modified: Vec<String>,
    /// Input globs the user declared for this family via `[inputs]` or a
    /// matching `[[command]]` block.
    pub declared_inputs: Vec<String>,
    pub executables: Vec<PathDep>,
    /// Environment variable names the execution was observed to read. Empty:
    /// environment access is memory-based and invisible to a syscall tracer, so
    /// Arc keeps its conservative configured set instead.
    pub env: Vec<String>,
    pub created_at: i64,
    pub last_validated_at: i64,
    pub arc_version: String,
    /// Executions folded into this set.
    pub observations: u64,
}

impl DependencySet {
    pub fn empty(family_key: &str, now: i64) -> DependencySet {
        DependencySet {
            schema: DEPENDENCY_SCHEMA_VERSION,
            trace_schema: TRACE_SCHEMA_VERSION,
            trace_semantics: TRACE_SEMANTICS_VERSION,
            family_key: family_key.to_string(),
            backend: "none".into(),
            capabilities: Capabilities::NONE,
            completeness: Completeness::Unsupported,
            downgrades: Vec::new(),
            inputs: Vec::new(),
            directories: Vec::new(),
            existence: Vec::new(),
            external: Vec::new(),
            external_directories: Vec::new(),
            outputs: Vec::new(),
            self_modified: Vec::new(),
            declared_inputs: Vec::new(),
            executables: Vec::new(),
            env: Vec::new(),
            created_at: now,
            last_validated_at: now,
            arc_version: crate::VERSION.to_string(),
            observations: 0,
        }
    }

    /// Build a set from one traced execution, applying temporal classification.
    pub fn from_observations(
        family_key: &str,
        backend: &str,
        caps: Capabilities,
        obs: &Observations,
        root: &Path,
        classifier: &crate::paths::Classifier,
        now: i64,
    ) -> DependencySet {
        let mut set = DependencySet::empty(family_key, now);
        set.backend = backend.to_string();
        set.capabilities = caps;
        set.downgrades = obs.downgrades.clone();
        set.completeness = Completeness::of(&caps, obs);
        set.observations = 1;

        let mut tracks: BTreeMap<&str, Track> = BTreeMap::new();
        let mut order: Vec<&str> = Vec::new();
        for f in &obs.files {
            let t = tracks.entry(f.path.as_str()).or_insert_with(|| {
                order.push(f.path.as_str());
                Track::new(f.rel.clone(), f.scope)
            });
            t.apply(f.op);
        }

        let mut overflow = false;
        for path in order {
            let t = &tracks[path];
            match t.classify() {
                Role::Output => {
                    if let Some(rel) = &t.rel {
                        push_capped(&mut set.outputs, rel.clone(), MAX_INPUTS, &mut overflow);
                    }
                }
                // A directory that was opened or stat'd but never enumerated is
                // a dependency on its *existence*, not on its contents. Treating
                // it as a file would hash nothing useful; treating it as an
                // enumeration would invalidate on every unrelated file added
                // next to it.
                Role::Input if Path::new(path).is_dir() => push_capped(
                    &mut set.existence,
                    path.to_string(),
                    MAX_ABSENT,
                    &mut overflow,
                ),
                Role::Input if t.produced => {
                    // Read first, then rewritten. It is an input, and it is also
                    // the reason this run's result cannot be attributed to the
                    // file's state afterwards.
                    if let Some(rel) = &t.rel {
                        push_capped(&mut set.inputs, rel.clone(), MAX_INPUTS, &mut overflow);
                        set.self_modified.push(rel.clone());
                    }
                }
                Role::Input => match (&t.rel, t.scope) {
                    (Some(rel), _) => {
                        push_capped(&mut set.inputs, rel.clone(), MAX_INPUTS, &mut overflow)
                    }
                    (None, Scope::External | Scope::System) => push_capped(
                        &mut set.external,
                        PathDep {
                            digest: digest_of(Path::new(path)),
                            path: path.to_string(),
                        },
                        MAX_EXTERNAL,
                        &mut overflow,
                    ),
                    (None, _) => {}
                },
                Role::Directory => match &t.rel {
                    Some(rel) => {
                        push_capped(&mut set.directories, rel.clone(), MAX_INPUTS, &mut overflow)
                    }
                    None => push_capped(
                        &mut set.external_directories,
                        path.to_string(),
                        MAX_EXTERNAL,
                        &mut overflow,
                    ),
                },
                Role::Existence => push_capped(
                    &mut set.existence,
                    path.to_string(),
                    MAX_ABSENT,
                    &mut overflow,
                ),
                Role::Nothing => {}
            }
        }

        for p in &obs.processes {
            let Some(image) = &p.image else { continue };
            // An executable Arc itself materialised is Arc's own state, not the
            // machine's. Its identity is already covered — precisely, and
            // portably — by the environment id in the execution key, whereas its
            // *path* is a directory name that changes with every capture. Left
            // in, it would bind the key to this machine, which is the opposite
            // of what an environment is for.
            if classifier.classify(Path::new(image)) == crate::paths::Scope::ArcInternal {
                continue;
            }
            if set.executables.len() >= MAX_EXTERNAL {
                overflow = true;
                break;
            }
            set.executables.push(PathDep {
                digest: digest_of(Path::new(image)),
                path: image.clone(),
            });
        }

        set.expand_symlinks(root);
        set.tidy();
        if overflow {
            set.downgrades.push(Downgrade::EventOverflow);
            set.completeness = Completeness::Partial;
        }
        set
    }

    /// Add the link *and* its target for anything reached through a symlink.
    ///
    /// A trace sees the path the program asked for. If `current.conf` points at
    /// `configs/a.conf`, fingerprinting only `current.conf` catches the link
    /// being retargeted but not the target's contents changing, and
    /// fingerprinting only the target catches the reverse. Both are dependencies
    /// and both are recorded.
    fn expand_symlinks(&mut self, root: &Path) {
        let inside = display_form(root);
        let mut outside: Vec<PathDep> = Vec::new();
        let mut project: Vec<String> = Vec::new();
        let candidates: Vec<String> = self
            .inputs
            .iter()
            .map(|rel| display_form(&root.join(rel)))
            .chain(self.external.iter().map(|e| e.path.clone()))
            .chain(self.executables.iter().map(|e| e.path.clone()))
            .collect();
        for path in candidates {
            let Ok(real) = Path::new(&path).canonicalize() else {
                continue;
            };
            let real = display_form(&real);
            if real == path {
                continue;
            }
            // A resolved target inside the project stays project-relative, so it
            // is fingerprinted and reported the same way as any other source
            // file rather than as a mysterious absolute path.
            match real.strip_prefix(&inside).and_then(|r| r.strip_prefix('/')) {
                Some(rel) => project.push(rel.to_string()),
                None => outside.push(PathDep {
                    digest: digest_of(Path::new(&real)),
                    path: real,
                }),
            }
        }
        self.inputs.extend(project);
        self.external.extend(outside);
    }

    /// Sort, deduplicate, and resolve the contradictions a union can produce.
    fn tidy(&mut self) {
        dedupe(&mut self.inputs);
        dedupe(&mut self.directories);
        dedupe(&mut self.outputs);
        dedupe(&mut self.self_modified);
        dedupe(&mut self.existence);
        dedupe(&mut self.external_directories);
        dedupe(&mut self.env);
        dedupe_paths(&mut self.external);
        dedupe_paths(&mut self.executables);

        // A file that is both read and written stays an input: demoting it to an
        // output would drop it from the key, which is the unsafe direction.
        let inputs: BTreeSet<&String> = self.inputs.iter().collect();
        self.outputs.retain(|o| !inputs.contains(o));

        // An existence claim about a path that is also tracked by content is
        // redundant: fingerprinting already distinguishes present from missing.
        // Keeping both would only add churn.
        let tracked: BTreeSet<String> = self
            .external
            .iter()
            .map(|e| e.path.clone())
            .chain(self.executables.iter().map(|e| e.path.clone()))
            .collect();
        self.existence.retain(|a| !tracked.contains(a));
    }

    /// Fold a new observation into stored knowledge.
    ///
    /// Everything is unioned: an execution may take a different branch next
    /// time, and forgetting a dependency observed once is the unsafe direction.
    /// Executables and external files are unioned by path with the newest digest
    /// winning, so a toolchain upgrade is reflected rather than pinned.
    /// Completeness is the weakest of everything folded in, never the best run's.
    pub fn merge(&mut self, new: &DependencySet, now: i64) {
        union(&mut self.inputs, &new.inputs);
        union(&mut self.directories, &new.directories);
        union(&mut self.existence, &new.existence);
        union(&mut self.outputs, &new.outputs);
        union(&mut self.self_modified, &new.self_modified);
        union(&mut self.external_directories, &new.external_directories);
        union(&mut self.env, &new.env);
        union_paths(&mut self.external, &new.external);
        union_paths(&mut self.executables, &new.executables);

        // Declared scope is replaced, not unioned: it mirrors the current
        // configuration, and a glob the user deleted must stop applying.
        self.declared_inputs = new.declared_inputs.clone();

        self.backend = new.backend.clone();
        self.capabilities = new.capabilities;
        self.trace_semantics = new.trace_semantics;
        self.completeness = match (self.completeness, new.completeness) {
            (Completeness::Complete, Completeness::Complete) => Completeness::Complete,
            (Completeness::Unsupported, c) | (c, Completeness::Unsupported) => c,
            _ => Completeness::Partial,
        };
        let mut downgrades: Vec<Downgrade> = std::mem::take(&mut self.downgrades);
        for d in &new.downgrades {
            if !downgrades.contains(d) {
                downgrades.push(d.clone());
            }
        }
        downgrades.truncate(32);
        self.downgrades = downgrades;

        self.last_validated_at = now;
        self.arc_version = crate::VERSION.to_string();
        self.observations += new.observations;
        self.tidy();
    }

    /// Reject stored knowledge that may no longer describe reality.
    ///
    /// Anything that changes how Arc traces, what it can observe, or what counts
    /// as an input invalidates the set. Retracing costs one execution; trusting
    /// a stale set costs correctness.
    pub fn validate(&self, family_key: &str, caps: &Capabilities) -> Completeness {
        if self.schema != DEPENDENCY_SCHEMA_VERSION
            || self.trace_schema != TRACE_SCHEMA_VERSION
            || self.trace_semantics != TRACE_SEMANTICS_VERSION
            || self.family_key != family_key
            || self.arc_version != crate::VERSION
            || &self.capabilities != caps
        {
            return Completeness::Invalid;
        }
        self.completeness
    }

    /// The part of a dependency set that participates in the execution key when
    /// inputs are *not* narrowed.
    ///
    /// Only executables contribute, and their digests come from the *stored*
    /// set, so the key is decided before the command runs rather than drifting
    /// between observations. This is the strictly additive path: it can only
    /// cause misses.
    pub fn key_digest(&self) -> Digest {
        let mut h = Hasher::new();
        h.field(DEPENDENCY_SCHEMA_VERSION.to_le_bytes());
        h.field((self.executables.len() as u64).to_le_bytes());
        for e in &self.executables {
            h.field(&e.path);
            h.field(&e.digest);
        }
        h.finish()
    }

    /// Whether Arc knows enough to say a change is *irrelevant* to this family.
    ///
    /// Without this, "not in the dependency list" means "not observed yet",
    /// which is not the same as "does not matter" — and reporting the second
    /// when only the first is true is how a dependency tool starts lying.
    pub fn inputs_are_narrowed(&self) -> bool {
        !self.declared_inputs.is_empty() || self.completeness == Completeness::Complete
    }

    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
            && self.outputs.is_empty()
            && self.executables.is_empty()
            && self.directories.is_empty()
            && self.external.is_empty()
    }

    /// Everything the narrowed fingerprint will look at, for reporting.
    pub fn tracked_count(&self) -> usize {
        self.inputs.len()
            + self.directories.len()
            + self.existence.len()
            + self.external.len()
            + self.external_directories.len()
            + self.executables.len()
    }
}

/// The one place Arc decides whether learned knowledge may replace the
/// conservative project scan.
///
/// Every condition is a reason the stored set might not describe what the next
/// execution will do. They are checked here and nowhere else, so a new caller
/// cannot accidentally implement a weaker version of this test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Narrow {
    Yes,
    No(&'static str),
}

impl Narrow {
    pub fn allowed(&self) -> bool {
        *self == Narrow::Yes
    }
    pub fn reason(&self) -> &'static str {
        match self {
            Narrow::Yes => "learned dependencies are complete",
            Narrow::No(r) => r,
        }
    }
}

pub fn can_narrow(set: &DependencySet, caps: &Capabilities, family_key: &str) -> Narrow {
    if set.schema != DEPENDENCY_SCHEMA_VERSION {
        return Narrow::No("dependency schema changed");
    }
    if set.trace_schema != TRACE_SCHEMA_VERSION {
        return Narrow::No("trace schema changed");
    }
    if set.trace_semantics != TRACE_SEMANTICS_VERSION {
        return Narrow::No("tracing semantics changed");
    }
    if set.arc_version != crate::VERSION {
        return Narrow::No("learned by a different Arc version");
    }
    if set.family_key != family_key {
        return Narrow::No("learned for a different execution");
    }
    if set.capabilities != *caps {
        return Narrow::No("tracing capabilities changed");
    }
    if !caps.observes_everything() {
        return Narrow::No("this platform cannot observe every dependency");
    }
    if set.completeness != Completeness::Complete {
        return Narrow::No("the dependency model is not complete");
    }
    if !set.downgrades.is_empty() {
        return Narrow::No("the last trace was downgraded");
    }
    if set.observations == 0 {
        return Narrow::No("nothing has been observed yet");
    }
    // A set that names no input at all would narrow to nothing and hit on every
    // change. Real executions read their own binary at the very least, so an
    // empty set means something went wrong rather than that nothing matters.
    if set.inputs.is_empty() && set.external.is_empty() && set.executables.is_empty() {
        return Narrow::No("no inputs were observed");
    }
    Narrow::Yes
}

/// The live state of everything a complete dependency set names.
#[derive(Debug, Default)]
pub struct Fingerprint {
    pub digest: Digest,
    pub bytes_hashed: u64,
    pub reused: usize,
    pub tracked: usize,
    /// `(path, digest)` for every tracked file, so `--explain` can name what
    /// changed without re-walking the project.
    pub files: Vec<(String, String)>,
}

/// Hash the current state of a learned dependency set.
///
/// This replaces the project-wide scan when [`can_narrow`] allows it, so it must
/// cover exactly the same ground: file contents, directory entry sets, the
/// continued absence of paths that were absent, and the binaries involved.
/// Anything it fails to read becomes a distinct marker rather than being
/// skipped — a dependency Arc cannot fingerprint must change the key, not
/// vanish from it.
pub fn fingerprint(
    set: &DependencySet,
    root: &Path,
    fps: &mut FingerprintMap,
) -> Result<Fingerprint> {
    let mut out = Fingerprint {
        tracked: set.tracked_count(),
        ..Default::default()
    };
    let mut h = Hasher::new();
    h.field(crate::SCHEMA_VERSION.to_le_bytes());
    h.field(DEPENDENCY_SCHEMA_VERSION.to_le_bytes());
    h.field(TRACE_SEMANTICS_VERSION.to_le_bytes());

    h.field(b"files");
    for rel in &set.inputs {
        let d = file_digest(&root.join(rel), rel, fps, &mut out);
        h.field(rel);
        h.field(&d);
        out.files.push((rel.clone(), d));
    }

    h.field(b"external");
    for e in &set.external {
        let d = file_digest(Path::new(&e.path), &e.path, fps, &mut out);
        h.field(&e.path);
        h.field(&d);
        out.files.push((e.path.clone(), d));
    }

    h.field(b"executables");
    for e in &set.executables {
        let d = file_digest(Path::new(&e.path), &e.path, fps, &mut out);
        h.field(&e.path);
        h.field(&d);
    }

    // A directory dependency is the *entry set*, not the files in it. Hashing
    // only the files that existed when the trace ran would replay across a new
    // entry appearing, which is the case this exists to catch.
    h.field(b"directories");
    for rel in &set.directories {
        h.field(rel);
        h.field(dir_digest(&root.join(rel)).bytes());
    }
    for abs in &set.external_directories {
        h.field(abs);
        h.field(dir_digest(Path::new(abs)).bytes());
    }

    // Presence is the whole dependency here: there is nothing to hash, but a
    // path appearing where one was absent must still change the key.
    h.field(b"existence");
    for path in &set.existence {
        h.field(path);
        h.field([Path::new(path).symlink_metadata().is_ok() as u8]);
    }

    out.digest = h.finish();
    Ok(out)
}

/// A file's digest, reusing the size/mtime cache the project scan already
/// maintains. `MISSING` is a value, not an absence: a dependency that has
/// disappeared must change the key.
fn file_digest(path: &Path, key: &str, fps: &mut FingerprintMap, out: &mut Fingerprint) -> String {
    const MISSING: &str = "<missing>";
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return MISSING.to_string();
    };
    let size = md.len();
    let mtime = crate::scan::mtime_millis(&md);
    let fresh = crate::scan::now_millis() - mtime < crate::scan::MTIME_TRUST_LAG_MS;
    if !md.file_type().is_symlink() && !fresh {
        if let Some((s, m, d)) = fps.get(key) {
            if *s == size && *m == mtime {
                out.reused += 1;
                return d.hex();
            }
        }
    }
    // A symlink is fingerprinted by its target path, not its target's contents.
    // The target is a dependency in its own right, added when the set was
    // learned, so retargeting and rewriting are both caught.
    let digest = if md.file_type().is_symlink() {
        std::fs::read_link(path)
            .map(|t| crate::hash::hash_bytes(display_form(&t).as_bytes()))
            .ok()
    } else {
        crate::hash::hash_file(path).ok()
    };
    match digest {
        Some(d) => {
            out.bytes_hashed += size;
            fps.insert(key.to_string(), (size, mtime, d));
            d.hex()
        }
        None => MISSING.to_string(),
    }
}

/// The entry set of one directory: names and whether each is itself a
/// directory. Deliberately not recursive — the dependency is what this
/// enumeration returned, and a file's contents are covered by having been read.
fn dir_digest(path: &Path) -> Digest {
    let Ok(rd) = std::fs::read_dir(path) else {
        return crate::hash::hash_bytes(b"<unreadable>");
    };
    let mut entries: Vec<(String, bool)> = rd
        .filter_map(|e| e.ok())
        .map(|e| {
            (
                display_form(Path::new(&e.file_name())),
                e.file_type().map(|t| t.is_dir()).unwrap_or(false),
            )
        })
        .collect();
    entries.sort();
    let mut h = Hasher::new();
    for (name, is_dir) in entries {
        h.field(&name);
        h.field([is_dir as u8]);
    }
    h.finish()
}

fn digest_of(path: &Path) -> String {
    crate::hash::hash_file(path)
        .map(|d| d.hex())
        .unwrap_or_default()
}

/// Per-path accumulator for temporal classification.
struct Track {
    rel: Option<String>,
    scope: Scope,
    /// The very first thing the execution did to this path produced it rather
    /// than consumed it, which is what makes it an intermediate.
    first_produced: Option<bool>,
    consumed: bool,
    produced: bool,
    listed: bool,
    absent: bool,
}

enum Role {
    Input,
    Output,
    Directory,
    Existence,
    Nothing,
}

impl Track {
    fn new(rel: Option<String>, scope: Scope) -> Track {
        Track {
            rel,
            scope,
            first_produced: None,
            consumed: false,
            produced: false,
            listed: false,
            absent: false,
        }
    }

    fn apply(&mut self, op: FileOp) {
        if self.first_produced.is_none() && op != FileOp::Absent {
            self.first_produced = Some(op.is_producing());
        }
        match op {
            FileOp::ListDir => self.listed = true,
            FileOp::Absent => self.absent = true,
            _ => {
                self.consumed |= op.is_input();
                self.produced |= op.is_producing();
            }
        }
    }

    fn classify(&self) -> Role {
        // A directory that was enumerated is a directory dependency, never a
        // file input, even though opening it also looks like a read.
        if self.listed && !self.produced {
            return Role::Directory;
        }
        // Created first, used afterwards: an intermediate this execution made.
        // Demanding it as a pre-existing input would make the next run require
        // a file that only the previous run produced.
        if self.first_produced == Some(true) {
            return Role::Output;
        }
        if self.consumed {
            return Role::Input;
        }
        if self.produced {
            return Role::Output;
        }
        // Absence only counts when the execution did not then create the path;
        // if it did, the lookup was about its own intermediate.
        if self.absent {
            return Role::Existence;
        }
        Role::Nothing
    }
}

fn push_capped<T>(dst: &mut Vec<T>, item: T, cap: usize, overflow: &mut bool) {
    if dst.len() >= cap {
        *overflow = true;
        return;
    }
    dst.push(item);
}

fn dedupe(v: &mut Vec<String>) {
    let set: BTreeSet<String> = std::mem::take(v).into_iter().collect();
    *v = set.into_iter().collect();
}

fn dedupe_paths(v: &mut Vec<PathDep>) {
    let map: BTreeMap<String, String> = std::mem::take(v)
        .into_iter()
        .map(|e| (e.path, e.digest))
        .collect();
    *v = map
        .into_iter()
        .map(|(path, digest)| PathDep { path, digest })
        .collect();
}

fn union(dst: &mut Vec<String>, src: &[String]) {
    dst.extend(src.iter().cloned());
    dedupe(dst);
}

fn union_paths(dst: &mut Vec<PathDep>, src: &[PathDep]) {
    // `src` last so a newer digest replaces the stored one: a toolchain upgrade
    // should be reflected rather than pinned to whatever ran first.
    let mut merged = std::mem::take(dst);
    merged.extend(src.iter().cloned());
    *dst = merged;
    dedupe_paths(dst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::model::{FileObservation, ProcessObservation};

    fn caps(full: bool) -> Capabilities {
        Capabilities {
            file_reads: full,
            file_writes: true,
            directory_reads: full,
            existence_checks: full,
            process_tree: true,
            executables: true,
            outside_project: full,
            network_detection: full,
        }
    }

    fn file(rel: &str, op: FileOp) -> FileObservation {
        FileObservation {
            path: format!("/repo/{rel}"),
            rel: Some(rel.to_string()),
            op,
            scope: Scope::Project,
            existed_before: None,
        }
    }

    fn obs(files: Vec<FileObservation>) -> Observations {
        Observations {
            files,
            ..Default::default()
        }
    }

    fn learn(o: &Observations) -> DependencySet {
        DependencySet::from_observations(
            "f",
            "test",
            caps(true),
            o,
            Path::new("/repo"),
            &crate::paths::Classifier::new(Path::new("/repo"), Path::new("/arc-home")),
            0,
        )
    }

    #[test]
    fn a_generated_intermediate_is_not_an_input() {
        // create → write → read back, all within one execution.
        let set = learn(&obs(vec![
            file("tmp.o", FileOp::Create),
            file("tmp.o", FileOp::Write),
            file("tmp.o", FileOp::Read),
        ]));
        assert!(set.inputs.is_empty(), "inputs: {:?}", set.inputs);
        assert_eq!(set.outputs, vec!["tmp.o"]);
    }

    #[test]
    fn a_file_read_before_being_overwritten_is_an_input() {
        let set = learn(&obs(vec![
            file("state.txt", FileOp::Read),
            file("state.txt", FileOp::Write),
        ]));
        assert_eq!(set.inputs, vec!["state.txt"]);
        assert!(set.outputs.is_empty());
    }

    #[test]
    fn an_enumerated_directory_is_a_directory_dependency_not_a_file() {
        let set = learn(&obs(vec![
            file("plugins", FileOp::Read),
            file("plugins", FileOp::ListDir),
        ]));
        assert_eq!(set.directories, vec!["plugins"]);
        assert!(set.inputs.is_empty());
    }

    #[test]
    fn a_missing_path_is_a_negative_dependency_unless_we_created_it() {
        let looked = learn(&obs(vec![file("optional.cfg", FileOp::Absent)]));
        assert_eq!(looked.existence, vec!["/repo/optional.cfg"]);

        let made = learn(&obs(vec![
            file("out.tmp", FileOp::Absent),
            file("out.tmp", FileOp::Create),
        ]));
        assert!(made.existence.is_empty(), "existence: {:?}", made.existence);
        assert_eq!(made.outputs, vec!["out.tmp"]);
    }

    #[test]
    fn narrowing_needs_a_complete_backend_a_clean_run_and_matching_versions() {
        let set = learn(&obs(vec![file("a.rs", FileOp::Read)]));
        assert_eq!(set.completeness, Completeness::Complete);
        assert_eq!(can_narrow(&set, &caps(true), "f"), Narrow::Yes);

        // A backend that cannot see reads can never narrow, whatever it stored.
        assert!(!can_narrow(&set, &caps(false), "f").allowed());
        // Nor may knowledge learned for one execution be applied to another.
        assert!(!can_narrow(&set, &caps(true), "other").allowed());
    }

    #[test]
    fn any_downgrade_revokes_the_completeness_claim() {
        for reason in [
            Downgrade::NetworkAccess,
            Downgrade::UnsupportedSyscall(451),
            Downgrade::VolatileRead("/proc/cpuinfo".into()),
            Downgrade::EventOverflow,
        ] {
            let o = Observations {
                files: vec![file("a.rs", FileOp::Read)],
                downgrades: vec![reason.clone()],
                ..Default::default()
            };
            let set = learn(&o);
            assert_eq!(set.completeness, Completeness::Partial, "{reason:?}");
            assert!(!can_narrow(&set, &caps(true), "f").allowed(), "{reason:?}");
        }
    }

    #[test]
    fn an_empty_observation_never_narrows() {
        let set = learn(&obs(vec![]));
        assert!(!can_narrow(&set, &caps(true), "f").allowed());
    }

    #[test]
    fn merging_unions_knowledge_and_keeps_the_weakest_confidence() {
        let a = learn(&obs(vec![file("a.rs", FileOp::Read)]));
        let b = DependencySet::from_observations(
            "f",
            "test",
            caps(true),
            &Observations {
                files: vec![file("b.rs", FileOp::Read)],
                downgrades: vec![Downgrade::NetworkAccess],
                ..Default::default()
            },
            Path::new("/repo"),
            &crate::paths::Classifier::new(Path::new("/repo"), Path::new("/arc-home")),
            0,
        );
        let mut merged = a.clone();
        merged.merge(&b, 1);
        assert_eq!(merged.inputs, vec!["a.rs", "b.rs"]);
        assert_eq!(merged.completeness, Completeness::Partial);
        assert_eq!(merged.observations, 2);
        assert!(!can_narrow(&merged, &caps(true), "f").allowed());
    }

    #[test]
    fn stale_or_foreign_sets_are_invalid() {
        let set = learn(&obs(vec![file("a.rs", FileOp::Read)]));
        assert_eq!(set.validate("f", &caps(true)), Completeness::Complete);
        assert_eq!(set.validate("other", &caps(true)), Completeness::Invalid);
        // A backend upgrade that changes what can be seen invalidates knowledge
        // gathered under the old capabilities.
        assert_eq!(set.validate("f", &caps(false)), Completeness::Invalid);
    }

    #[test]
    fn executables_contribute_to_the_key_and_an_upgrade_changes_it() {
        let mk = |digest: &str| DependencySet {
            executables: vec![PathDep {
                path: "/bin/cargo".into(),
                digest: digest.into(),
            }],
            ..DependencySet::empty("f", 0)
        };
        assert_eq!(mk("aa").key_digest(), mk("aa").key_digest());
        assert_ne!(mk("aa").key_digest(), mk("bb").key_digest());
    }

    #[test]
    fn observed_processes_become_executable_dependencies() {
        let o = Observations {
            processes: vec![ProcessObservation {
                pid: 4,
                image: Some("/bin/rustc".into()),
                scope: Some(Scope::External),
                descendant: true,
            }],
            ..Default::default()
        };
        let set = learn(&o);
        assert_eq!(set.executables.len(), 1);
        assert_eq!(set.executables[0].path, "/bin/rustc");
    }

    #[test]
    fn the_fingerprint_covers_contents_directories_and_absences() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("plugins")).unwrap();
        std::fs::write(root.join("a.txt"), "one").unwrap();

        let mut set = DependencySet::empty("f", 0);
        set.inputs = vec!["a.txt".into()];
        set.directories = vec!["plugins".into()];
        set.existence = vec![display_form(&root.join("maybe.cfg"))];

        let mut fps = FingerprintMap::new();
        let base = fingerprint(&set, root, &mut fps).unwrap().digest;

        // Contents.
        std::fs::write(root.join("a.txt"), "two").unwrap();
        let changed = fingerprint(&set, root, &mut fps).unwrap().digest;
        assert_ne!(base, changed);
        std::fs::write(root.join("a.txt"), "one").unwrap();
        assert_eq!(fingerprint(&set, root, &mut fps).unwrap().digest, base);

        // A new directory entry, which no per-file dependency would notice.
        std::fs::write(root.join("plugins/new.so"), "x").unwrap();
        assert_ne!(fingerprint(&set, root, &mut fps).unwrap().digest, base);
        std::fs::remove_file(root.join("plugins/new.so")).unwrap();

        // A path appearing where absence was the dependency.
        std::fs::write(root.join("maybe.cfg"), "").unwrap();
        assert_ne!(fingerprint(&set, root, &mut fps).unwrap().digest, base);
        std::fs::remove_file(root.join("maybe.cfg")).unwrap();

        // A tracked input disappearing.
        std::fs::remove_file(root.join("a.txt")).unwrap();
        assert_ne!(fingerprint(&set, root, &mut fps).unwrap().digest, base);
    }
}
