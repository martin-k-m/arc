//! Project discovery and configuration (`arc.toml`).

use anyhow::{Context, Result};
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
    fn unknown_config_key_is_an_error() {
        let e = toml::from_str::<Config>("[cache]\nenabld = true\n").unwrap_err();
        assert!(e.to_string().contains("enabld"));
    }
}
