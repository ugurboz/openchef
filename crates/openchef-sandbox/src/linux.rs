//! Linux: the helper confines itself with Landlock (files) and a seccomp deny list,
//! then execs the command (S2, D-032). No user namespaces: Ubuntu 24.04 blocks
//! unprivileged ones, and Landlock and seccomp need no privileges.
//!
//! Landlock is an allowlist: everything not granted is denied, which covers the home
//! directory. It cannot carve a denied path out of a granted one, so a policy that
//! grants a parent of a private path is refused rather than weakened.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, path_beneath_rules,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};

use crate::{Error, Policy, Support};

/// System locations every process reads (S2), plus what `/etc/resolv.conf` points to.
const SYSTEM_READ: [&str; 12] = [
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt", "/proc",
    "/sys", "/nix",
];

/// Devices: `/dev/null`, ttys and PTYs for agents that run commands on one.
const SYSTEM_WRITE: [&str; 1] = ["/dev"];

/// Syscalls an agent never needs and that could weaken the sandbox (S2).
const DENIED_SYSCALLS: [i64; 17] = [
    libc::SYS_ptrace,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_bpf,
    libc::SYS_kexec_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_perf_event_open,
    libc::SYS_swapon,
    libc::SYS_reboot,
];

/// The Landlock ABI version the kernel supports, `None` without Landlock.
fn kernel_abi() -> Option<i64> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: with a null attribute and size 0, the VERSION flag only queries the ABI;
    // the kernel reads no memory.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0_usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (rc > 0).then_some(rc)
}

pub(crate) fn check() -> Result<Support, Error> {
    match kernel_abi() {
        Some(abi) => Ok(Support {
            mechanism: "landlock",
            detail: format!("Landlock ABI {abi}"),
        }),
        None => Err(Error::Unavailable(
            "this kernel has no Landlock (Linux 5.13+ with Landlock enabled is required)".into(),
        )),
    }
}

/// The paths granted, split into read-only and read-write, existing ones only (Landlock
/// opens each path). Refuses a grant that contains a private path.
fn grants(policy: &Policy) -> Result<(Vec<PathBuf>, Vec<PathBuf>), Error> {
    let mut ro: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
    // DNS: the resolver file is usually a symlink into /run (S2 round 2).
    if let Ok(target) = std::fs::canonicalize("/etc/resolv.conf") {
        ro.push(target);
    }
    ro.extend(policy.readable.iter().cloned());
    let mut rw: Vec<PathBuf> = SYSTEM_WRITE.iter().map(PathBuf::from).collect();
    rw.extend(policy.writable.iter().cloned());
    for granted in ro.iter().chain(&rw) {
        if let Some(p) = policy.private.iter().find(|p| p.starts_with(granted)) {
            return Err(Error::Policy(format!(
                "{} would expose the private path {}",
                granted.display(),
                p.display()
            )));
        }
    }
    let exists = |p: &PathBuf| p.exists();
    Ok((
        ro.into_iter().filter(exists).collect(),
        rw.into_iter().filter(exists).collect(),
    ))
}

fn landlock(policy: &Policy) -> Result<RulesetStatus, Error> {
    let (ro, rw) = grants(policy)?;
    let fail = |e: &dyn std::fmt::Display| Error::Unavailable(format!("landlock: {e}"));
    // Ask for the newest ABI this crate knows; best effort downgrades on older kernels
    // and the status reports what was really enforced.
    let abi = ABI::V5;
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| fail(&e))?
        .create()
        .map_err(|e| fail(&e))?
        .add_rules(path_beneath_rules(&ro, AccessFs::from_read(abi)))
        .map_err(|e| fail(&e))?
        .add_rules(path_beneath_rules(&rw, AccessFs::from_all(abi)))
        .map_err(|e| fail(&e))?
        .restrict_self()
        .map_err(|e| fail(&e))?;
    Ok(status.ruleset)
}

fn seccomp() -> Result<(), Error> {
    let fail = |e: &dyn std::fmt::Display| Error::Unavailable(format!("seccomp: {e}"));
    let rules: BTreeMap<i64, Vec<SeccompRule>> = DENIED_SYSCALLS
        .into_iter()
        .map(|s| (s, Vec::new()))
        .collect();
    let arch = if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else {
        TargetArch::x86_64
    };
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(u32::try_from(libc::EPERM).unwrap_or(1)),
        arch,
    )
    .map_err(|e| fail(&e))?;
    let program: BpfProgram = filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| fail(&e))?;
    seccompiler::apply_filter(&program).map_err(|e| fail(&e))
}

/// Parses `--policy <json> -- CMD…`.
fn parse(args: &[OsString]) -> Result<(Policy, &[OsString]), Error> {
    let usage = || Error::Helper("usage: __sandbox --policy <json> -- CMD [ARGS]...".into());
    let [flag, json, dashes, cmd @ ..] = args else {
        return Err(usage());
    };
    if flag != "--policy" || dashes != "--" || cmd.is_empty() {
        return Err(usage());
    }
    let json = json.to_str().ok_or_else(usage)?;
    let policy: Policy =
        serde_json::from_str(json).map_err(|e| Error::Helper(format!("policy: {e}")))?;
    Ok((policy.canonical()?, cmd))
}

pub(crate) fn run_helper(args: &[OsString]) -> Result<std::convert::Infallible, Error> {
    let (policy, cmd) = parse(args)?;
    match landlock(&policy)? {
        RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => {}
        // Fail closed: never run a command the kernel does not confine.
        RulesetStatus::NotEnforced => {
            return Err(Error::Unavailable(
                "landlock: the kernel enforced nothing".into(),
            ));
        }
    }
    seccomp()?;
    let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
    Err(Error::Helper(format!(
        "exec {}: {err}",
        Path::new(&cmd[0]).display()
    )))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{grants, parse};
    use crate::Policy;

    #[test]
    fn private_paths_are_never_granted() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let home = dir.path().to_path_buf();
        let p = Policy {
            home: home.clone(),
            writable: vec![home.clone()],
            readable: Vec::new(),
            private: vec![home.join(".openchef/state.db")],
        };
        assert!(
            grants(&p).is_err(),
            "granting the home would expose the state"
        );
        let ok = Policy {
            writable: vec![home.join("ws")],
            ..p
        };
        std::fs::create_dir_all(home.join("ws")).unwrap_or_else(|e| panic!("{e}"));
        let (_, rw) = grants(&ok).unwrap_or_else(|e| panic!("{e}"));
        assert!(rw.contains(&home.join("ws")));
    }

    #[test]
    fn helper_arguments() {
        let json = serde_json::to_string(&Policy {
            home: "/h".into(),
            ..Policy::default()
        })
        .unwrap_or_else(|e| panic!("{e}"));
        let args: Vec<OsString> = ["--policy", &json, "--", "/bin/true", "x"]
            .iter()
            .map(OsString::from)
            .collect();
        let (policy, cmd) = parse(&args).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(policy.home, PathBuf::from("/h"));
        assert_eq!(cmd, [OsString::from("/bin/true"), OsString::from("x")]);
        assert!(parse(&args[..3]).is_err(), "no command");
        assert!(parse(&[OsString::from("--x")]).is_err());
    }
}
