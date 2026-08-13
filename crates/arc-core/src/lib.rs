//! Arc: an execution cache. Never repeat work Arc can prove is already done.
//!
//! Correctness rule for every decision in this crate: when uncertain, execute.
//! A false miss costs time; a false hit costs trust.

pub mod affected;
pub mod db;
pub mod dependency;
pub mod engine;
pub mod exec;
pub mod family;
pub mod git;
pub mod graph;
pub mod hash;
pub mod key;
pub mod maintenance;
pub mod outputs;
pub mod paths;
pub mod project;
pub mod record;
pub mod scan;
pub mod store;
pub mod trace;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Bumped whenever anything that feeds an execution key changes meaning. Old
/// entries then simply stop matching instead of being misinterpreted.
///
/// v2 added family identity and learned-dependency digests to the key, so v1
/// entries must not be reused: they were computed under different semantics.
pub const SCHEMA_VERSION: u32 = 2;

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `$ARC_HOME`, else `~/.arc`.
pub fn arc_home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("ARC_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("neither ARC_HOME nor HOME/USERPROFILE is set; cannot locate the Arc cache")?;
    Ok(PathBuf::from(home).join(".arc"))
}
