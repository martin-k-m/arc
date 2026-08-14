//! The part of an execution environment that cannot be put in a CAS.
//!
//! Arc packages userspace, not kernels and not the C library. What is left over
//! is stated here rather than assumed, so "this worker can run this
//! environment" is a checkable claim instead of a hope.

use super::manifest::HostRequirements;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapability {
    pub os: String,
    pub arch: String,
    pub libc: String,
    /// What the sandbox on this host actually does. Named honestly: an empty
    /// list is not a claim of isolation.
    pub sandbox: Vec<String>,
}

pub fn host_capability() -> HostCapability {
    HostCapability {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        libc: libc_flavour().into(),
        sandbox: sandbox_features(),
    }
}

/// Directories a Linux dynamic loader searches by default. A library found here
/// belongs to the host distribution; capturing it would package glibc, which
/// Arc deliberately does not do.
pub fn system_library_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["/lib64", "/usr/lib64", "/lib", "/usr/lib", "/usr/local/lib"]
        .iter()
        .map(PathBuf::from)
        .collect();
    // Multiarch layouts, e.g. /usr/lib/x86_64-linux-gnu.
    for triple in [
        "x86_64-linux-gnu",
        "aarch64-linux-gnu",
        "arm-linux-gnueabihf",
    ] {
        dirs.push(PathBuf::from("/lib").join(triple));
        dirs.push(PathBuf::from("/usr/lib").join(triple));
    }
    dirs
}

pub fn is_system_library(path: &Path) -> bool {
    system_library_dirs().iter().any(|d| path.starts_with(d))
}

/// Find a soname the way the loader would, minus the environment overrides
/// capture must not depend on.
pub fn find_system_library(soname: &str) -> Option<PathBuf> {
    if soname.contains('/') || soname.contains('\0') {
        return None;
    }
    system_library_dirs()
        .into_iter()
        .map(|d| d.join(soname))
        .find(|p| p.exists())
}

/// Whether this machine can run an environment. Every failure names one
/// concrete reason, because the caller's response is to say why it fell back.
pub fn supports(host: &HostCapability, req: &HostRequirements) -> Result<(), String> {
    if req.os != host.os || req.arch != host.arch {
        return Err(format!(
            "environment targets {}/{}, this host is {}/{}",
            req.os, req.arch, host.os, host.arch
        ));
    }
    if req.libc != host.libc {
        return Err(format!(
            "environment was captured against {} userspace, this host is {}",
            req.libc, host.libc
        ));
    }
    for interp in &req.interpreters {
        if !Path::new(interp).exists() {
            return Err(format!("this host has no dynamic loader at {interp}"));
        }
    }
    for lib in &req.libraries {
        if find_system_library(lib).is_none() {
            return Err(format!("this host does not provide {lib}"));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn libc_flavour() -> &'static str {
    if std::path::Path::new("/lib/ld-musl-x86_64.so.1").exists()
        || std::path::Path::new("/lib/ld-musl-aarch64.so.1").exists()
    {
        "musl"
    } else {
        "gnu"
    }
}

#[cfg(not(target_os = "linux"))]
pub fn libc_flavour() -> &'static str {
    "n/a"
}

/// What the reference sandbox provides on this host. Deliberately short: Arc
/// advertises isolation it implements, not isolation it would like to have.
fn sandbox_features() -> Vec<String> {
    #[allow(unused_mut)]
    let mut f = vec!["workspace".to_string(), "home".into(), "tmp".into()];
    #[cfg(unix)]
    f.push("process-group".into());
    #[cfg(target_os = "linux")]
    f.push("readonly-environment".into());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> HostRequirements {
        HostRequirements {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            libc: libc_flavour().into(),
            interpreters: Vec::new(),
            libraries: Vec::new(),
        }
    }

    #[test]
    fn a_matching_host_supports_an_environment_with_no_extra_requirements() {
        assert!(supports(&host_capability(), &req()).is_ok());
    }

    #[test]
    fn platform_mismatches_are_named() {
        let host = host_capability();
        let mut r = req();
        r.os = "plan9".into();
        assert!(supports(&host, &r).unwrap_err().contains("plan9"));

        let mut r = req();
        r.arch = "sparc64".into();
        assert!(supports(&host, &r).unwrap_err().contains("sparc64"));

        let mut r = req();
        r.libc = "nonesuch".into();
        assert!(supports(&host, &r).unwrap_err().contains("nonesuch"));
    }

    #[test]
    fn a_missing_loader_or_library_is_a_refusal_not_a_warning() {
        let host = host_capability();
        let mut r = req();
        r.interpreters = vec!["/nonexistent/ld.so".into()];
        assert!(supports(&host, &r).is_err());

        let mut r = req();
        r.libraries = vec!["libdefinitelynotinstalled.so.99".into()];
        assert!(supports(&host, &r).is_err());
    }

    #[test]
    fn a_soname_cannot_be_a_path() {
        assert!(find_system_library("../../etc/passwd").is_none());
        assert!(find_system_library("/etc/passwd").is_none());
    }
}
