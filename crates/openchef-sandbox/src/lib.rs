//! OpenChef's built-in OS sandbox (`docs/architecture.md` §21.3, D-028, D-032, D-036):
//! one policy intent, enforced by the operating system's own primitives.
//!
//! - **macOS:** a Seatbelt profile generated per session and applied with
//!   `sandbox-exec`. Paths reach the profile as `-D` parameters, never inside the
//!   profile text, so no path can change the profile's meaning.
//! - **Linux:** the hidden helper `openchef __sandbox` confines itself with Landlock and
//!   a seccomp deny list, then `exec`s the command ([`run_helper`]).
//! - **Windows:** not yet (R4.7); [`wrap`] fails closed.
//!
//! The sandbox wraps a process's [`Spec`] before it starts; `openchef-procs` still owns
//! the process tree. It confines files; the network stays open in v1.
//!
//! This crate talks to the operating system directly, so `unsafe` is allowed here
//! (D-030). Every `unsafe` block must carry a `// SAFETY:` comment.
#![allow(unsafe_code)]

mod env;
#[cfg(target_os = "linux")]
mod linux;
mod policy;
#[cfg(any(target_os = "macos", test))]
mod seatbelt;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub use env::{CACHE_VARS, sandbox_env};
use openchef_procs::Spec;
pub use policy::{AgentCli, Policy, agent_state};

/// The hidden subcommand of the `openchef` binary that confines itself and execs
/// (Linux).
pub const HELPER_COMMAND: &str = "__sandbox";

/// Why a process cannot be confined.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// This platform has no sandbox yet.
    #[error("sandbox: not available on this platform yet")]
    Unsupported,
    /// The platform's sandbox is missing or does not apply.
    #[error("sandbox: {0}")]
    Unavailable(String),
    /// The policy is unusable (a relative or unresolvable path).
    #[error("sandbox: policy: {0}")]
    Policy(String),
    /// The helper was called wrongly.
    #[error("sandbox: helper: {0}")]
    Helper(String),
}

/// Rewrites `spec` so that the process starts confined by `policy`. `helper` is the
/// `openchef` executable (Linux runs `helper __sandbox …`).
///
/// # Errors
///
/// [`Error::Unsupported`] on platforms without a sandbox; [`Error::Policy`] for a bad
/// policy. It never returns an unconfined spec.
pub fn wrap(policy: &Policy, spec: Spec, helper: &Path) -> Result<Spec, Error> {
    platform_wrap(&policy.canonical()?, spec, helper)
}

/// macOS needs no helper: `sandbox-exec` applies the profile.
#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)] // the same signature on every platform
fn platform_wrap(policy: &Policy, spec: Spec, _helper: &Path) -> Result<Spec, Error> {
    Ok(seatbelt::wrap(policy, spec))
}

#[cfg(target_os = "linux")]
fn platform_wrap(policy: &Policy, spec: Spec, helper: &Path) -> Result<Spec, Error> {
    let json = serde_json::to_string(policy).map_err(|e| Error::Policy(e.to_string()))?;
    let mut args: Vec<OsString> = vec![
        HELPER_COMMAND.into(),
        "--policy".into(),
        json.into(),
        "--".into(),
        spec.program.into_os_string(),
    ];
    args.extend(spec.args);
    Ok(Spec {
        program: helper.to_path_buf(),
        args,
        ..spec
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_wrap(_policy: &Policy, _spec: Spec, _helper: &Path) -> Result<Spec, Error> {
    Err(Error::Unsupported)
}

/// What the platform's sandbox can enforce, for `openchef doctor` and fail-closed
/// checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Support {
    /// The mechanism (`seatbelt`, `landlock`).
    pub mechanism: &'static str,
    /// Details for people (`Landlock ABI 4, partially enforced`).
    pub detail: String,
}

/// Checks that the sandbox applies here by confining a trivial command.
///
/// # Errors
///
/// [`Error::Unsupported`] or [`Error::Unavailable`] with the reason.
pub fn check() -> Result<Support, Error> {
    #[cfg(target_os = "macos")]
    {
        seatbelt::check()
    }
    #[cfg(target_os = "linux")]
    {
        linux::check()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(Error::Unsupported)
    }
}

/// The helper's whole job (`openchef __sandbox --policy <json> -- CMD…`): confine this
/// process, then exec the command. Returns only on failure.
///
/// # Errors
///
/// [`Error::Helper`] for bad arguments, [`Error::Unavailable`] when confinement fails,
/// in which case nothing runs.
#[cfg(target_os = "linux")]
pub fn run_helper(args: &[OsString]) -> Result<std::convert::Infallible, Error> {
    linux::run_helper(args)
}

/// The helper exists only on Linux.
///
/// # Errors
///
/// Always [`Error::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn run_helper(_args: &[OsString]) -> Result<std::convert::Infallible, Error> {
    Err(Error::Unsupported)
}

/// The per-session temp dir and its parent, created owner-only.
///
/// # Errors
///
/// [`Error::Policy`] when the directory cannot be created.
pub fn private_dir(path: &Path) -> Result<PathBuf, Error> {
    std::fs::create_dir_all(path)
        .map_err(|e| Error::Policy(format!("create {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::Policy(format!("restrict {}: {e}", path.display())))?;
    }
    Ok(path.to_path_buf())
}
