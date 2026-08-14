//! Whether Arc may execute a command somewhere else.
//!
//! One gate, one place. The question is not "would remote execution be faster"
//! but "can Arc construct an execution environment complete and compatible
//! enough that the result means the same thing". When the answer is anything
//! other than a clear yes, the command runs locally.

use super::execution::{Capabilities, ToolRequirement};
use crate::dependency::{Completeness, DependencySet, Narrow};
use crate::environment::EnvironmentManifest;
use crate::key::{looks_secret, EnvFingerprint};
use serde::{Deserialize, Serialize};

/// Which input set a remote execution would materialise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Materialisation {
    /// Exactly the dependencies a complete trace observed. Only ever chosen
    /// when Arc could also have narrowed the cache key to them.
    Narrowed,
    /// Every project file Arc fingerprinted. The safe answer when dependency
    /// knowledge is absent or incomplete: it can only send too much.
    Project,
}

impl Materialisation {
    pub fn label(&self) -> &'static str {
        match self {
            Materialisation::Narrowed => "learned dependencies",
            Materialisation::Project => "project files",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Eligibility {
    Yes(Materialisation),
    No(String),
}

impl Eligibility {
    pub fn allowed(&self) -> bool {
        matches!(self, Eligibility::Yes(_))
    }

    pub fn reason(&self) -> String {
        match self {
            Eligibility::Yes(m) => format!("eligible, sending {}", m.label()),
            Eligibility::No(r) => r.clone(),
        }
    }

    pub fn materialisation(&self) -> Option<Materialisation> {
        match self {
            Eligibility::Yes(m) => Some(*m),
            Eligibility::No(_) => None,
        }
    }
}

/// Everything the decision looks at. Grouped so the gate is a pure function of
/// stated facts rather than of ambient state.
pub struct Candidate<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub rel_cwd: &'a str,
    pub deps: &'a DependencySet,
    pub narrow: &'a Narrow,
    pub dep_state: Completeness,
    pub env: &'a EnvFingerprint,
    /// Variables the project explicitly allows sending to a worker.
    pub allow_env: &'a [String],
    /// The Arc environment this command runs inside, when one is configured.
    ///
    /// With one, worker eligibility stops being "does this machine already have
    /// the same compiler" and becomes "can this machine run this environment" —
    /// which is the entire point of v0.8.
    pub environment: Option<&'a EnvironmentManifest>,
}

pub fn can_remote_execute(c: &Candidate<'_>, caps: &Capabilities) -> Eligibility {
    if caps.protocol != super::execution::EXEC_PROTOCOL_VERSION {
        return no(format!(
            "worker speaks execution protocol v{}, this Arc speaks v{}",
            caps.protocol,
            super::execution::EXEC_PROTOCOL_VERSION
        ));
    }
    if caps.key_semantics != crate::SCHEMA_VERSION {
        return no(format!(
            "worker uses cache semantics v{} (this Arc uses v{})",
            caps.key_semantics,
            crate::SCHEMA_VERSION
        ));
    }
    if caps.os != std::env::consts::OS || caps.arch != std::env::consts::ARCH {
        return no(format!(
            "worker is {}/{}, this machine is {}/{}",
            caps.os,
            caps.arch,
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    }
    if let Some(m) = c.environment {
        if !caps.has(super::execution::FEATURE_ENVIRONMENT) {
            return no("this worker cannot materialise Arc environments".into());
        }
        // What the client can check here is the coarse claim. Whether the
        // worker's host really has the loader and system libraries the
        // environment needs is rechecked there, where the answer is knowable.
        if let Some(host) = &caps.host {
            if let Err(e) = crate::environment::host::supports(host, &m.host) {
                return no(e);
            }
        }
        if m.completeness != crate::environment::Completeness::Complete {
            return no(format!(
                "environment is {} ({})",
                m.completeness.label(),
                m.gaps
                    .first()
                    .map(String::as_str)
                    .unwrap_or("unspecified gap")
            ));
        }
    }

    // An argument naming a path on *this* machine cannot mean the same thing on
    // another one. Arc does not guess which arguments are paths; it refuses the
    // ones that unambiguously are.
    if let Some(a) = c.args.iter().find(|a| host_bound(a)) {
        return no(format!(
            "argument `{}` names a path on this machine",
            trim(a)
        ));
    }
    if host_bound(c.program) && !c.program.starts_with("./") {
        return no(format!(
            "`{}` is an absolute path on this machine",
            trim(c.program)
        ));
    }

    // Sending a secret to another machine is a decision a project makes
    // explicitly or not at all. Arc hashes secret-shaped variables for cache
    // identity; that is not consent to transmit them.
    if let Some(v) = c
        .env
        .vars
        .iter()
        .find(|v| v.present && looks_secret(&v.name) && !allowed(&v.name, c.allow_env))
    {
        return no(format!(
            "`{}` looks like a secret and would have to be sent",
            v.name
        ));
    }

    if c.narrow.allowed() {
        // A complete trace names everything the command read. Anything it read
        // outside the project cannot be materialised into a sandbox, so the
        // command is not portable.
        if let Some(e) = c.deps.external.first() {
            return no(format!(
                "reads `{}`, which is outside the project",
                trim(&e.path)
            ));
        }
        if let Some(d) = c.deps.external_directories.first() {
            return no(format!(
                "enumerates `{}`, which is outside the project",
                trim(d)
            ));
        }
        return Eligibility::Yes(Materialisation::Narrowed);
    }

    // Without complete knowledge Arc cannot name the dependencies, so it sends
    // every project file it fingerprinted. That can only be an over-approximation
    // of what the command reads inside the project — but it says nothing about
    // what the command reads outside it, which is what the worker environment
    // has to supply.
    if c.dep_state == Completeness::Invalid {
        return no("learned dependencies could not be validated".into());
    }
    Eligibility::Yes(Materialisation::Project)
}

/// The executables a worker must be able to produce byte-for-byte before it may
/// run this command.
///
/// Always includes the program itself. When the trace was complete it includes
/// every other executable the command was seen running, because a build that
/// shells out to a different linker is a different build.
pub fn tool_requirements(
    program: &str,
    program_digest: &str,
    deps: &DependencySet,
    complete: bool,
    environment: bool,
) -> Vec<ToolRequirement> {
    // An environment *is* the tool requirement. Asking a worker to also hold
    // matching host binaries would defeat the point: the whole reason it can
    // run this command is that it does not need them.
    if environment {
        return Vec::new();
    }
    let mut out = vec![ToolRequirement {
        program: program.to_string(),
        digest: program_digest.to_string(),
    }];
    if complete {
        for e in &deps.executables {
            if out.iter().any(|t| t.digest == e.digest) {
                continue;
            }
            out.push(ToolRequirement {
                program: e.path.clone(),
                digest: e.digest.clone(),
            });
        }
    }
    out.truncate(super::execution::MAX_TOOLS);
    out
}

/// Variables to send with the request: the ones the execution key depends on
/// and that are present, minus anything secret-shaped the project has not
/// explicitly allowed.
pub fn transmittable_env(env: &EnvFingerprint, allow: &[String]) -> Vec<String> {
    env.vars
        .iter()
        .filter(|v| v.present)
        .filter(|v| !looks_secret(&v.name) || allowed(&v.name, allow))
        .map(|v| v.name.clone())
        .collect()
}

fn allowed(name: &str, allow: &[String]) -> bool {
    allow.iter().any(|a| a.eq_ignore_ascii_case(name))
}

/// Whether a string is unambiguously a path rooted on this host.
fn host_bound(s: &str) -> bool {
    if s.starts_with('/') || s.starts_with("\\\\") {
        return true;
    }
    let b = s.as_bytes();
    // `C:\...` and `C:/...`, but not a bare `C:` or an `option:value`.
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn no(reason: String) -> Eligibility {
    Eligibility::No(reason)
}

fn trim(s: &str) -> String {
    if s.chars().count() > 60 {
        s.chars().take(57).collect::<String>() + "..."
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency::PathDep;
    use crate::key::EnvVar;
    use crate::remote::execution::{environment_id, NetworkPolicy};

    fn caps() -> Capabilities {
        Capabilities {
            protocol: super::super::execution::EXEC_PROTOCOL_VERSION,
            key_semantics: crate::SCHEMA_VERSION,
            worker: "arc-worker".into(),
            version: crate::VERSION.into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            environment_id: environment_id(),
            max_jobs: 4,
            queue_limit: 16,
            active: 0,
            queued: 0,
            network: NetworkPolicy::Unrestricted,
            cache_endpoint: None,
            features: vec![super::super::execution::FEATURE_ENVIRONMENT.into()],
            host: Some(crate::environment::host_capability()),
        }
    }

    fn env(vars: &[(&str, bool)]) -> EnvFingerprint {
        EnvFingerprint {
            digest: "e".into(),
            vars: vars
                .iter()
                .map(|(n, present)| EnvVar {
                    name: (*n).into(),
                    value_digest: "d".into(),
                    present: *present,
                    redacted: looks_secret(n),
                })
                .collect(),
        }
    }

    struct Fixture {
        deps: DependencySet,
        narrow: Narrow,
        env: EnvFingerprint,
        args: Vec<String>,
        allow: Vec<String>,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                deps: DependencySet::empty("family", 0),
                narrow: Narrow::No("nothing observed yet"),
                env: env(&[("PATH", true)]),
                args: vec!["-c".into(), "true".into()],
                allow: Vec::new(),
            }
        }

        fn decide(&self) -> Eligibility {
            can_remote_execute(
                &Candidate {
                    program: "sh",
                    args: &self.args,
                    rel_cwd: "",
                    deps: &self.deps,
                    narrow: &self.narrow,
                    dep_state: Completeness::Unsupported,
                    env: &self.env,
                    allow_env: &self.allow,
                    environment: None,
                },
                &caps(),
            )
        }
    }

    #[test]
    fn without_dependency_knowledge_arc_sends_the_whole_project() {
        assert_eq!(
            Fixture::new().decide(),
            Eligibility::Yes(Materialisation::Project)
        );
    }

    #[test]
    fn a_complete_trace_sends_only_what_it_observed() {
        let mut f = Fixture::new();
        f.narrow = Narrow::Yes;
        assert_eq!(f.decide(), Eligibility::Yes(Materialisation::Narrowed));
    }

    #[test]
    fn a_command_reading_outside_the_project_is_not_portable() {
        let mut f = Fixture::new();
        f.narrow = Narrow::Yes;
        f.deps.external = vec![PathDep {
            path: "/home/dev/.cargo/registry/x".into(),
            digest: "a".repeat(64),
        }];
        assert!(!f.decide().allowed());
        assert!(f.decide().reason().contains("outside the project"));

        // The same command without complete knowledge is still eligible: Arc
        // has not observed the external read, and sends the whole project.
        f.narrow = Narrow::No("no trace");
        assert!(f.decide().allowed());
    }

    #[test]
    fn an_argument_naming_a_host_path_is_refused() {
        for arg in [
            "/etc/hosts",
            "C:\\Users\\dev\\x",
            "C:/Users/dev/x",
            "\\\\srv\\s",
        ] {
            let mut f = Fixture::new();
            f.args = vec![arg.into()];
            assert!(!f.decide().allowed(), "{arg} should be refused");
        }
        // Not paths, and not refused.
        for arg in ["--jobs=4", "test:unit", "C:", "./local", "src/main.rs"] {
            let mut f = Fixture::new();
            f.args = vec![arg.into()];
            assert!(f.decide().allowed(), "{arg} should be allowed");
        }
    }

    #[test]
    fn a_present_secret_makes_the_command_local_unless_explicitly_allowed() {
        let mut f = Fixture::new();
        f.env = env(&[("PATH", true), ("AWS_SECRET_ACCESS_KEY", true)]);
        assert!(!f.decide().allowed());
        assert!(f.decide().reason().contains("AWS_SECRET_ACCESS_KEY"));

        // Absent secrets are not a reason to refuse: nothing would be sent.
        f.env = env(&[("PATH", true), ("AWS_SECRET_ACCESS_KEY", false)]);
        assert!(f.decide().allowed());

        // And a project may say a variable is safe.
        f.env = env(&[("PATH", true), ("BUILD_TOKEN", true)]);
        assert!(!f.decide().allowed());
        f.allow = vec!["BUILD_TOKEN".into()];
        assert!(f.decide().allowed());
    }

    #[test]
    fn secrets_are_never_in_the_transmittable_set_by_default() {
        let e = env(&[("PATH", true), ("GH_TOKEN", true), ("ABSENT", false)]);
        assert_eq!(transmittable_env(&e, &[]), vec!["PATH".to_string()]);
        assert_eq!(
            transmittable_env(&e, &["GH_TOKEN".into()]),
            vec!["PATH".to_string(), "GH_TOKEN".to_string()]
        );
    }

    #[test]
    fn an_incompatible_worker_is_refused_for_a_stated_reason() {
        let f = Fixture::new();
        let decide = |mutate: fn(&mut Capabilities)| {
            let mut c = caps();
            mutate(&mut c);
            can_remote_execute(
                &Candidate {
                    program: "sh",
                    args: &f.args,
                    rel_cwd: "",
                    deps: &f.deps,
                    narrow: &f.narrow,
                    dep_state: Completeness::Unsupported,
                    env: &f.env,
                    allow_env: &[],
                    environment: None,
                },
                &c,
            )
        };
        assert!(!decide(|c| c.os = "plan9".into()).allowed());
        assert!(!decide(|c| c.arch = "sparc64".into()).allowed());
        assert!(!decide(|c| c.key_semantics += 1).allowed());
        assert!(!decide(|c| c.protocol += 1).allowed());
    }

    #[test]
    fn a_complete_trace_requires_every_observed_executable() {
        let mut deps = DependencySet::empty("family", 0);
        deps.executables = vec![
            PathDep {
                path: "/usr/bin/cc".into(),
                digest: "a".repeat(64),
            },
            PathDep {
                path: "/usr/bin/ld".into(),
                digest: "b".repeat(64),
            },
        ];
        let tools = tool_requirements("sh", &"c".repeat(64), &deps, true, false);
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].program, "sh");

        // Without a complete trace Arc only knows about the program itself.
        let tools = tool_requirements("sh", &"c".repeat(64), &deps, false, false);
        assert_eq!(tools.len(), 1);

        // Inside an environment there are no host tools to match at all.
        assert!(tool_requirements("sh", &"c".repeat(64), &deps, true, true).is_empty());
    }
}
