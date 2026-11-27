//! Desktop notifications for requests that wait while nobody watches their task
//! (§9.2), ported from Go `internal/app/notify.go`. Title and body go to the helper as
//! separate arguments, never through a shell or a script string, so request text
//! cannot inject commands.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openchef_engine::service::Notify;

/// How long a helper may take before it is killed.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a running helper is checked.
const POLL: Duration = Duration::from_millis(50);
/// The longest body shown, in characters.
const MAX_BODY: usize = 300;

/// The platform's notification helper and how to call it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Helper {
    /// macOS: `osascript` with an `on run argv` script reading its arguments.
    Osascript,
    /// Linux and the BSDs: `notify-send`.
    NotifySend,
}

impl Helper {
    const fn for_platform() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Osascript)
        } else if cfg!(any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd"
        )) {
            Some(Self::NotifySend)
        } else {
            // Windows: no dependency-free notifier; `openchef approvals` lists requests.
            None
        }
    }

    const fn program(self) -> &'static str {
        match self {
            Self::Osascript => "osascript",
            Self::NotifySend => "notify-send",
        }
    }

    fn args(self, title: &str, body: &str) -> Vec<String> {
        let body = first_chars(body, MAX_BODY);
        match self {
            Self::Osascript => [
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                title,
                &body,
            ]
            .map(str::to_owned)
            .to_vec(),
            Self::NotifySend => ["--app-name=OpenChef", "--", title, &body]
                .map(str::to_owned)
                .to_vec(),
        }
    }
}

/// The desktop notifier, or `None` where the platform has no helper installed.
pub(crate) fn desktop_notifier() -> Option<Notify> {
    let helper = Helper::for_platform()?;
    let Some(path) = find_program(helper.program(), std::env::var_os("PATH").as_deref()) else {
        log(&format!(
            "desktop notifications unavailable: {} not found",
            helper.program()
        ));
        return None;
    };
    Some(Arc::new(move |title: &str, body: &str| {
        let (path, args) = (path.clone(), helper.args(title, body));
        // Best effort and never blocking the caller: the helper runs on its own thread.
        let spawned = std::thread::Builder::new()
            .name("openchef-notify".into())
            .spawn(move || {
                if let Err(e) = run_helper(&path, &args) {
                    log(&format!("desktop notification failed: {e}"));
                }
            });
        if let Err(e) = spawned {
            log(&format!("desktop notification failed: {e}"));
        }
    }))
}

/// Runs the helper, killing it after [`HELPER_TIMEOUT`].
fn run_helper(path: &Path, args: &[String]) -> Result<(), String> {
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start {}: {e}", path.display()))?;
    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{} exited with {status}", path.display())),
            Ok(None) if Instant::now() >= deadline => {
                // Reaping after the kill keeps no zombie; a failed kill means it exited.
                let _killed = child.kill();
                let _reaped = child.wait();
                return Err(format!("{} timed out", path.display()));
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => return Err(format!("wait for {}: {e}", path.display())),
        }
    }
}

/// The first executable file named `name` on `path_var`.
fn find_program(name: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// `s`, cut to `n` characters with an ellipsis.
fn first_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn log(message: &str) {
    use std::io::Write as _;
    // The daemon log is best effort.
    let _logged = writeln!(std::io::stderr(), "{message}");
}

#[cfg(test)]
mod tests {
    use super::{Helper, MAX_BODY, find_program, first_chars};

    #[test]
    fn helper_arguments() {
        let evil = "\"; do shell script \"rm -rf ~\" --";
        let args = Helper::Osascript.args("OpenChef needs you", evil);
        assert_eq!(
            args.last().map(String::as_str),
            Some(evil),
            "the body stays one argument"
        );
        assert_eq!(args[args.len() - 2], "OpenChef needs you");
        assert!(
            args.iter().take(6).all(|a| !a.contains("rm -rf")),
            "request text never reaches the script"
        );
        let args = Helper::NotifySend.args("t", "-x");
        assert_eq!(args, ["--app-name=OpenChef", "--", "t", "-x"]);
    }

    #[test]
    fn body_is_cut() {
        assert_eq!(first_chars("abc", 3), "abc");
        assert_eq!(first_chars("abcd", 3), "ab…");
        let long = "ğ".repeat(MAX_BODY + 10);
        let shown = Helper::NotifySend.args("t", &long);
        assert_eq!(shown[3].chars().count(), MAX_BODY);
    }

    #[cfg(unix)]
    #[test]
    fn finds_executables_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "").unwrap_or_else(|e| panic!("{e}"));
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "").unwrap_or_else(|e| panic!("{e}"));
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("{e}"));
        let path = std::env::join_paths(["relative", dir.path().to_str().unwrap_or("")])
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(find_program("tool", Some(&path)), Some(tool));
        assert_eq!(find_program("plain", Some(&path)), None);
        assert_eq!(find_program("tool", None), None);
    }

    #[cfg(windows)]
    #[test]
    fn no_helper_on_windows() {
        assert_eq!(Helper::for_platform(), None);
    }
}
