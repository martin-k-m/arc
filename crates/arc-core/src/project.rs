//! Project discovery and configuration (`arc.toml`).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_NAME: &str = "arc.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub cache: CacheConfig,
    pub inputs: InputsConfig,
    pub outputs: OutputsConfig,
    pub env: EnvConfig,
    pub trace: TraceConfig,
    pub remote: crate::remote::RemoteConfig,
    pub ci: CiConfig,
    /// Named execution environments, written as `[environment.<alias>]`. The
    /// alias is configuration; the identity is the captured content.
    #[serde(default)]
    pub environment: std::collections::BTreeMap<String, EnvironmentConfig>,
    /// Per-command scoping, written as repeated `[[command]]` tables.
    #[serde(rename = "command")]
    pub commands: Vec<CommandConfig>,
    /// The environment alias in force for one command line, filled in by
    /// [`Config::resolve`]. Not configuration in its own right.
    #[serde(skip)]
    pub selected_environment: Option<String>,
}

/// What `arc env capture <alias>` will take from this machine.
///
/// Deliberately not a package manager: every entry names something that already
/// exists here. Arc captures bytes, it does not fetch them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EnvironmentConfig {
    /// Programs resolved on this machine's PATH and captured into `bin/`.
    pub tools: Vec<String>,
    /// Directories taken wholesale, at a destination the project chooses.
    #[serde(rename = "tree")]
    pub trees: Vec<TreeConfig>,
    /// Variables every command in this environment gets. Secret-shaped names
    /// are refused: a manifest is published to other machines.
    pub env: std::collections::BTreeMap<String, String>,
    /// Extra `PATH` entries inside the environment root.
    pub path: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TreeConfig {
    /// Source on the capturing machine. A leading `~` is this machine's home.
    /// Never part of the environment's identity.
    pub from: String,
    /// Destination inside the environment root, which *is* part of identity.
    pub to: String,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TraceConfig {
    /// Observe executions to learn their dependencies. Costs one metadata walk
    /// of the project per run; turn it off for very large trees where the walk
    /// outweighs the command.
    pub enabled: bool,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Narrows what a *particular* command depends on, when you know something Arc
/// cannot observe.
///
/// Declared inputs are additive with `[inputs] include`, and are always
/// fingerprinted even if an exclude pattern would have dropped them: an
/// explicit include is a statement of fact, an exclude is only a hint.
/// Whether a command may be sent to a worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemotePolicy {
    /// Eligible, if Arc can build a complete enough environment for it. There
    /// is deliberately no `always`: eligibility is not negotiable.
    #[default]
    Auto,
    /// Never. For commands whose effects Arc cannot see — a deploy, a publish,
    /// anything that touches the world outside the project.
    Never,
}

impl RemotePolicy {
    pub fn label(&self) -> &'static str {
        match self {
            RemotePolicy::Auto => "auto",
            RemotePolicy::Never => "never",
        }
    }
}

/// Which CI events may publish to the remote cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteWrite {
    /// Publish only from events Arc can positively identify as trusted. A fork
    /// pull request is not one of them.
    #[default]
    Trusted,
    Always,
    Never,
}

impl RemoteWrite {
    pub fn label(&self) -> &'static str {
        match self {
            RemoteWrite::Trusted => "trusted",
            RemoteWrite::Always => "always",
            RemoteWrite::Never => "never",
        }
    }
}

/// What `arc ci` runs, and what it is allowed to publish.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CiConfig {
    /// Names of `[[command]]` blocks to consider. Empty means every runnable
    /// named command.
    pub tasks: Vec<String>,
    pub remote_write: RemoteWrite,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CommandConfig {
    /// Glob matched against the full command line, e.g. `"cargo test*"`.
    /// Derived from `command`/`args` when left empty.
    #[serde(rename = "match")]
    pub match_: String,
    /// Human-readable task name, used by `arc graph`, `arc affected` and by
    /// `after`. Never part of execution identity.
    pub name: Option<String>,
    /// Program to execute, making this block a task `arc ci` can run rather
    /// than only a scoping rule.
    pub command: Option<String>,
    pub args: Vec<String>,
    /// Free-form labels, for selecting subsets in CI.
    pub tags: Vec<String>,
    /// Whether this command may run on a remote worker: `auto` or `never`.
    ///
    /// Deliberately absent from the family key: where a command runs must not
    /// change its identity, or a result produced locally could never be reused
    /// remotely and vice versa.
    pub remote: Option<RemotePolicy>,
    /// The `[environment.<alias>]` this command runs inside. Absent means the
    /// host's own toolchain, exactly as before v0.8.
    pub environment: Option<String>,
    /// Task names this command must follow, for dependencies no filesystem
    /// observation can reveal.
    pub after: Vec<String>,
    pub inputs: Vec<String>,
    pub exclude: Vec<String>,
    pub outputs: Vec<String>,
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    pub enabled: bool,
    /// Cap on stored blob bytes; `arc cache prune` enforces it.
    pub max_size: String,
    /// Cache executions that exited non-zero. Off by default: failures are
    /// far more often environment- or flake-dependent than successes.
    pub cache_failures: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: "20GB".into(),
            cache_failures: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct InputsConfig {
    /// When non-empty, only these globs are fingerprinted. Otherwise Arc walks
    /// the project honouring .gitignore plus `DEFAULT_EXCLUDES`.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OutputsConfig {
    /// Files matching these globs are captured after a miss and restored on a
    /// hit. Empty means Arc caches only stdout/stderr/exit status.
    pub include: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EnvConfig {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// Directories that are derived output or VCS state. Never inputs unless the
/// user says so via `[inputs] include`.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".git/**",
    ".arc/**",
    "**/node_modules/**",
    "**/target/**",
    "**/.venv/**",
    "**/venv/**",
    "**/__pycache__/**",
    "**/.mypy_cache/**",
    "**/.pytest_cache/**",
    "**/dist/**",
    "**/build/**",
    "**/.next/**",
    "**/.gradle/**",
];

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub git: bool,
}

impl Project {
    /// Nearest ancestor holding `arc.toml`, else nearest holding `.git`, else cwd.
    pub fn discover(cwd: &Path) -> Result<Project> {
        let cwd = dunce_canonicalize(cwd)?;
        let mut config_root = None;
        let mut git_root = None;
        for dir in cwd.ancestors() {
            if config_root.is_none() && dir.join(CONFIG_NAME).is_file() {
                config_root = Some(dir.to_path_buf());
            }
            if git_root.is_none() && dir.join(".git").exists() {
                git_root = Some(dir.to_path_buf());
            }
        }
        let root = config_root
            .clone()
            .or_else(|| git_root.clone())
            .unwrap_or(cwd);
        let config_path = config_root.map(|r| r.join(CONFIG_NAME));
        let config = match &config_path {
            Some(p) => load_config(p)?,
            None => Config::default(),
        };
        Ok(Project {
            git: root.join(".git").exists(),
            root,
            config,
            config_path,
        })
    }

    pub fn max_size_bytes(&self) -> Result<u64> {
        parse_size(&self.config.cache.max_size)
    }

    /// The configuration in force for one command line: the project defaults
    /// plus every matching `[[command]]` block folded in.
    ///
    /// Folding is a union, never a replacement, so two overlapping blocks
    /// cannot silently cancel each other's declarations.
    pub fn config_for(&self, command_line: &str) -> Result<Config> {
        self.config.resolve(command_line)
    }
}

impl CommandConfig {
    /// The command line this block runs, when it declares one.
    pub fn command_line(&self) -> Option<String> {
        let program = self.command.as_deref()?;
        Some(crate::record::format_command(program, &self.args))
    }

    /// Whether this block applies to a command line: by glob, or by being an
    /// exact declaration of it.
    pub fn matches(&self, command_line: &str) -> Result<bool> {
        if self.command_line().as_deref() == Some(command_line) {
            return Ok(true);
        }
        command_matches(&self.match_, command_line)
    }
}

impl Config {
    fn resolve(&self, command_line: &str) -> Result<Config> {
        let mut cfg = self.clone();
        for c in &self.commands {
            if !c.matches(command_line)? {
                continue;
            }
            cfg.inputs.include.extend(c.inputs.iter().cloned());
            cfg.inputs.exclude.extend(c.exclude.iter().cloned());
            cfg.outputs.include.extend(c.outputs.iter().cloned());
            cfg.env.include.extend(c.env.iter().cloned());
            // The most restrictive matching block wins: one block saying a
            // command must not leave this machine is not overridden by another
            // that is merely silent on the question.
            if c.remote == Some(RemotePolicy::Never) {
                cfg.remote.execution.enabled = false;
            }
            // Two blocks naming different environments is a contradiction, not
            // a merge: one command runs in one environment.
            if let Some(alias) = &c.environment {
                match &cfg.selected_environment {
                    Some(existing) if existing != alias => bail!(
                        "`{command_line}` matches [[command]] blocks naming both environment `{existing}` and `{alias}`"
                    ),
                    _ => cfg.selected_environment = Some(alias.clone()),
                }
            }
        }
        if let Some(alias) = &cfg.selected_environment {
            anyhow::ensure!(
                self.environment.contains_key(alias),
                "`{command_line}` names environment `{alias}`, which no [environment.{alias}] block defines"
            );
        }
        Ok(cfg)
    }

    /// Every `[[command]]` block whose glob matches, in declaration order.
    pub fn commands_matching(&self, command_line: &str) -> Vec<&CommandConfig> {
        self.commands
            .iter()
            .filter(|c| c.matches(command_line).unwrap_or(false))
            .collect()
    }
}

/// A `[[command]] match` glob is matched against the whole command line.
/// Separators are not special here — a command line is not a path.
pub fn command_matches(pattern: &str, command_line: &str) -> Result<bool> {
    if pattern.is_empty() {
        return Ok(false);
    }
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .with_context(|| format!("invalid [[command]] match pattern: {pattern}"))?;
    Ok(glob.compile_matcher().is_match(command_line))
}

fn load_config(path: &Path) -> Result<Config> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "{} is not valid Arc configuration.\n\n{}",
            path.display(),
            e
        )
    })
}

/// Strip Windows `\\?\` verbatim prefixes so paths stay printable and comparable.
pub fn dunce_canonicalize(p: &Path) -> Result<PathBuf> {
    let c = p
        .canonicalize()
        .with_context(|| format!("resolving {}", p.display()))?;
    let s = c.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.len() > 2 && rest.as_bytes()[1] == b':' => PathBuf::from(rest),
        _ => c,
    })
}

pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim().to_ascii_uppercase();
    let (num, mult) = if let Some(n) = t.strip_suffix("TB") {
        (n, 1u64 << 40)
    } else if let Some(n) = t.strip_suffix("GB") {
        (n, 1 << 30)
    } else if let Some(n) = t.strip_suffix("MB") {
        (n, 1 << 20)
    } else if let Some(n) = t.strip_suffix("KB") {
        (n, 1 << 10)
    } else if let Some(n) = t.strip_suffix('B') {
        (n, 1)
    } else {
        (t.as_str(), 1)
    };
    let v: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid size: {s}"))?;
    anyhow::ensure!(v >= 0.0, "invalid size: {s}");
    Ok((v * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("20GB").unwrap(), 20 << 30);
        assert_eq!(parse_size("1.5MB").unwrap(), 1_572_864);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert!(parse_size("big").is_err());
    }

    #[test]
    fn per_command_scoping_folds_into_the_effective_config() {
        let cfg: Config = toml::from_str(
            "[[command]]\nmatch = \"cargo test*\"\ninputs = [\"src/**\"]\n\n[[command]]\nmatch = \"npm*\"\ninputs = [\"app/**\"]\n",
        )
        .unwrap();
        let p = Project {
            root: PathBuf::from("."),
            config: cfg,
            config_path: None,
            git: false,
        };
        assert_eq!(
            p.config_for("cargo test --all").unwrap().inputs.include,
            vec!["src/**"]
        );
        assert!(p
            .config_for("cargo build")
            .unwrap()
            .inputs
            .include
            .is_empty());
    }

    #[test]
    fn a_declared_command_scopes_its_own_command_line_without_a_glob() {
        let cfg: Config = toml::from_str(
            "[[command]]\nname = \"test\"\ncommand = \"cargo\"\nargs = [\"test\", \"-p\", \"arc-core\"]\ninputs = [\"crates/**\"]\n",
        )
        .unwrap();
        let p = Project {
            root: PathBuf::from("."),
            config: cfg,
            config_path: None,
            git: false,
        };
        assert_eq!(
            p.config_for("cargo test -p arc-core")
                .unwrap()
                .inputs
                .include,
            vec!["crates/**"]
        );
        assert!(p
            .config_for("cargo test")
            .unwrap()
            .inputs
            .include
            .is_empty());
    }

    fn project(toml: &str) -> Project {
        Project {
            root: PathBuf::from("."),
            config: toml::from_str(toml).unwrap(),
            config_path: None,
            git: false,
        }
    }

    #[test]
    fn a_command_selects_the_environment_its_block_names() {
        let p = project(
            "[environment.rust]\ntools = [\"cargo\"]\n\n[[command]]\nmatch = \"cargo*\"\nenvironment = \"rust\"\n",
        );
        assert_eq!(
            p.config_for("cargo test").unwrap().selected_environment,
            Some("rust".into())
        );
        // A command no block matches keeps the pre-v0.8 behaviour.
        assert_eq!(p.config_for("npm test").unwrap().selected_environment, None);
    }

    #[test]
    fn two_blocks_naming_different_environments_is_a_contradiction() {
        let p = project(
            "[environment.a]\ntools = [\"cargo\"]\n\n[environment.b]\ntools = [\"cargo\"]\n\n\
             [[command]]\nmatch = \"cargo*\"\nenvironment = \"a\"\n\n\
             [[command]]\nmatch = \"*test*\"\nenvironment = \"b\"\n",
        );
        let e = p.config_for("cargo test").unwrap_err().to_string();
        assert!(e.contains('a') && e.contains('b'), "{e}");
        // Blocks that agree are not a contradiction.
        assert!(p.config_for("cargo build").is_ok());
    }

    #[test]
    fn naming_an_environment_no_block_defines_is_an_error() {
        let p = project("[[command]]\nmatch = \"cargo*\"\nenvironment = \"ghost\"\n");
        let e = p.config_for("cargo test").unwrap_err().to_string();
        assert!(e.contains("ghost"), "{e}");
    }

    #[test]
    fn an_environment_block_parses_tools_trees_and_env() {
        let cfg: Config = toml::from_str(
            "[environment.rust]\ntools = [\"cargo\", \"rustc\"]\npath = [\"rust/libexec\"]\n\
             env = { RUST_BACKTRACE = \"1\" }\n\n\
             [[environment.rust.tree]]\nfrom = \"~/.rustup\"\nto = \"rust\"\nexclude = [\"**/doc/**\"]\n",
        )
        .unwrap();
        let e = &cfg.environment["rust"];
        assert_eq!(e.tools, vec!["cargo", "rustc"]);
        assert_eq!(e.trees.len(), 1);
        assert_eq!(e.trees[0].to, "rust");
        assert_eq!(e.env["RUST_BACKTRACE"], "1");
        assert_eq!(e.path, vec!["rust/libexec"]);
    }

    #[test]
    fn unknown_config_key_is_an_error() {
        let e = toml::from_str::<Config>("[cache]\nenabld = true\n").unwrap_err();
        assert!(e.to_string().contains("enabld"));
    }
}
