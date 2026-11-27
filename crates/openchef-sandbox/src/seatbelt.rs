//! macOS: a Seatbelt profile per session, applied with `sandbox-exec` (S1, D-032).
//!
//! The profile allows by default (agent CLIs touch many mach services, sysctls and
//! devices) and confines the file system: no writes except the policy's writable paths
//! and a few devices, no reads under the home directory except what the policy allows,
//! and nothing at all of the private paths. Later rules win in Seatbelt, so each deny
//! comes before the allows that carve it open, and the private deny comes last.
//!
//! Paths are passed as `-D NAME=value` parameters: the profile text contains only
//! fixed parameter names, so a path with quotes or parentheses cannot change it.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use openchef_procs::Spec;

use crate::Policy;
#[cfg(target_os = "macos")]
use crate::{Error, Support};

/// The tool that applies a profile (deprecated by Apple but working; S1).
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Device and Claude Code paths every agent writes (S1): `/dev/ptmx` and ttys for
/// agy's PTY, Claude's Bash tool tracking its directory in `/private/tmp`
/// (`claude-<hex>-cwd` since Claude Code 2.1; S1 saw decimal pids).
const WRITABLE_FIXED: &str = r#"  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/ptmx")
  (regex #"^/dev/tty")
  (regex #"^/dev/fd/")
  (regex #"^/private/tmp/claude-[0-9a-f]+-cwd$")
"#;

/// The profile for a policy with `w` writable, `r` readable and `x` private paths.
pub(crate) fn profile(w: usize, r: usize, x: usize) -> String {
    let rules = |prefix: &str, n: usize| -> String {
        (0..n).fold(String::new(), |mut s, i| {
            // Writing to a String cannot fail.
            let _written = writeln!(s, "  (subpath (param \"{prefix}{i}\"))");
            s
        })
    };
    let mut p = String::from(
        ";; OpenChef sandbox profile (architecture §21.3).\n(version 1)\n(allow default)\n",
    );
    p.push_str("(deny file-write*)\n(allow file-write*\n");
    p.push_str(&rules("W", w));
    p.push_str(WRITABLE_FIXED);
    // Metadata (stat, realpath) stays readable everywhere: git and toolchains resolve
    // the directories above an allowed path. Contents and listings stay denied; Landlock
    // does not restrict metadata either, so both platforms agree.
    p.push_str(")\n(deny file-read* (subpath (param \"HOME\")))\n(allow file-read-metadata)\n(allow file-read*\n  (literal (param \"HOME\"))\n");
    p.push_str(&rules("W", w));
    p.push_str(&rules("R", r));
    p.push_str(")\n");
    if x > 0 {
        p.push_str("(deny file-read* file-write*\n");
        p.push_str(&rules("X", x));
        p.push_str(")\n");
    }
    p
}

fn define(name: &str, path: &Path) -> [OsString; 2] {
    let mut v = OsString::from(format!("{name}="));
    v.push(path.as_os_str());
    ["-D".into(), v]
}

/// `spec` started by `sandbox-exec` under the policy's profile.
pub(crate) fn wrap(policy: &Policy, spec: Spec) -> Spec {
    let mut args: Vec<OsString> = vec![
        "-p".into(),
        profile(
            policy.writable.len(),
            policy.readable.len(),
            policy.private.len(),
        )
        .into(),
    ];
    args.extend(define("HOME", &policy.home));
    for (prefix, paths) in [
        ("W", &policy.writable),
        ("R", &policy.readable),
        ("X", &policy.private),
    ] {
        for (i, p) in paths.iter().enumerate() {
            args.extend(define(&format!("{prefix}{i}"), p));
        }
    }
    args.push(spec.program.into_os_string());
    args.extend(spec.args);
    Spec {
        program: PathBuf::from(SANDBOX_EXEC),
        args,
        ..spec
    }
}

/// Applies a trivial profile to `/usr/bin/true`: `sandbox-exec` exists and works.
#[cfg(target_os = "macos")]
pub(crate) fn check() -> Result<Support, Error> {
    let out = std::process::Command::new(SANDBOX_EXEC)
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| Error::Unavailable(format!("{SANDBOX_EXEC}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Unavailable(format!(
            "{SANDBOX_EXEC} cannot apply a profile ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(Support {
        mechanism: "seatbelt",
        detail: "sandbox-exec applies profiles".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use openchef_procs::{Spec, Stderr};

    use super::{profile, wrap};
    use crate::Policy;

    #[test]
    fn profile_names_parameters_only() {
        let p = profile(2, 1, 1);
        for want in [
            "(allow default)",
            "(deny file-write*)",
            "(subpath (param \"W1\"))",
            "(subpath (param \"R0\"))",
            "(deny file-read* (subpath (param \"HOME\")))\n(allow file-read-metadata)",
            "(deny file-read* file-write*\n  (subpath (param \"X0\"))",
        ] {
            assert!(p.contains(want), "profile lacks {want:?}:\n{p}");
        }
        // The private deny is the last rule, so nothing re-allows it.
        assert!(p.rfind("X0") > p.rfind("R0"));
        assert!(!profile(1, 0, 0).contains("X0"));
    }

    #[test]
    fn wrap_passes_paths_as_parameters() {
        let policy = Policy {
            home: "/Users/u".into(),
            writable: vec![PathBuf::from("/w/with \"quote\")")],
            readable: vec!["/Users/u/.cargo".into()],
            private: Vec::new(),
        };
        let spec = Spec {
            program: "/bin/agent".into(),
            args: vec!["--x".into()],
            dir: Some("/w".into()),
            env: vec![("A".into(), "1".into())],
            stderr: Stderr::Discard,
        };
        let w = wrap(&policy, spec);
        assert_eq!(w.program, PathBuf::from("/usr/bin/sandbox-exec"));
        let args: Vec<String> = w
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-p");
        assert!(!args[1].contains("quote"), "paths never enter the profile");
        assert!(args.contains(&"W0=/w/with \"quote\")".to_owned()));
        assert!(args.contains(&"HOME=/Users/u".to_owned()));
        assert_eq!(&args[args.len() - 2..], ["/bin/agent", "--x"]);
        assert_eq!(w.dir, Some(PathBuf::from("/w")));
        assert_eq!(w.env, vec![(OsString::from("A"), OsString::from("1"))]);
    }
}
