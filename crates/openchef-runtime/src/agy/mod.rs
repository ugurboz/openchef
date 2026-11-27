//! Drives the Antigravity CLI over the Agent Client Protocol (`agy acp`), per
//! `docs/architecture.md` §6.4 and D-015…D-017.

mod protocol;
#[cfg(test)]
mod replay_tests;
mod session;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

pub use session::AgySession;

use crate::{Error, Runtime, Session, SessionRef, SessionSpec, TurnEvents, TurnInput};

/// The runtime identifier.
pub const NAME: &str = "agy";

/// How to run agy.
#[derive(Debug, Clone)]
pub struct AgyOptions {
    /// The executable (default `agy`, resolved through `PATH`).
    pub binary: PathBuf,
    /// Arguments that start the ACP server, before OpenChef's flags (default `["acp"]`).
    pub args: Vec<String>,
    /// agy's `--cli_dir_name` (default `openchef`): OpenChef's own settings and logs,
    /// apart from the user's agy configuration.
    pub cli_dir_name: String,
    /// Where `settings.json` is written (default
    /// `<HOME>/.gemini/antigravity-cli/<cli_dir_name>`, `HOME` from the spec's env).
    pub settings_dir: Option<PathBuf>,
    /// Permits workspaces under the temp tree, where agy's sandbox does not contain
    /// writes (D-015). Only tests set it.
    pub allow_temp_dir: bool,
    /// Reported to agy in `initialize`.
    pub client_version: String,
    /// D-016: how long an interrupt waits for `cancelled`.
    pub interrupt_wait: Duration,
    /// How long close waits for an in-flight turn (as for Claude, D-010).
    pub close_wait: Duration,
}

impl Default for AgyOptions {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("agy"),
            args: vec!["acp".to_owned()],
            cli_dir_name: "openchef".to_owned(),
            settings_dir: None,
            allow_temp_dir: false,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            interrupt_wait: Duration::from_secs(10),
            close_wait: Duration::from_secs(3),
        }
    }
}

/// agy as a [`Runtime`].
#[derive(Debug, Clone, Default)]
pub struct AgyRuntime {
    options: Arc<AgyOptions>,
}

impl AgyRuntime {
    /// A runtime with these options.
    #[must_use]
    pub fn new(options: AgyOptions) -> Self {
        Self {
            options: Arc::new(options),
        }
    }

    async fn begin(&self, spec: SessionSpec, id: Option<String>) -> Result<AgySession, Error> {
        if spec.dir.as_os_str().is_empty() {
            return Err(start_error("the session needs a working directory"));
        }
        if !self.options.allow_temp_dir {
            refuse_temp_dir(&spec.dir)?;
        }
        write_settings(&settings_dir(&self.options, &spec.env)?)?;
        AgySession::begin(Arc::clone(&self.options), spec, id).await
    }
}

fn start_error(message: impl Into<String>) -> Error {
    Error::Start {
        runtime: NAME,
        message: message.into(),
    }
}

impl Runtime for AgyRuntime {
    type Session = AgySession;

    fn name(&self) -> &'static str {
        NAME
    }

    /// Starts a new ACP session.
    async fn open(&self, spec: SessionSpec) -> Result<AgySession, Error> {
        self.begin(spec, None).await
    }

    /// Loads a session in a new process (`session/load`).
    async fn resume(&self, session: SessionRef, spec: SessionSpec) -> Result<AgySession, Error> {
        if session.runtime != NAME || session.id.is_empty() {
            return Err(start_error(format!("cannot resume {session:?}")));
        }
        self.begin(spec, Some(session.id)).await
    }
}

impl Session for AgySession {
    fn session_ref(&self) -> &SessionRef {
        AgySession::session_ref(self)
    }

    async fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> Result<TurnEvents, Error> {
        AgySession::turn(self, input, cancel).await
    }

    async fn interrupt(&mut self) -> Result<(), Error> {
        AgySession::interrupt(self).await
    }

    async fn close(self) -> Result<(), Error> {
        AgySession::close(self).await
    }
}

fn settings_dir(options: &AgyOptions, env: &[(String, String)]) -> Result<PathBuf, Error> {
    if let Some(dir) = &options.settings_dir {
        return Ok(dir.clone());
    }
    let home = env
        .iter()
        .rev()
        .find(|(k, _)| k == "HOME")
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
        })
        .ok_or_else(|| start_error("cannot find the home directory"))?;
    Ok(home
        .join(".gemini")
        .join("antigravity-cli")
        .join(&options.cli_dir_name))
}

/// OpenChef's own agy settings (D-015): commands run unasked only inside the read-only,
/// offline sandbox; every escape reaches OpenChef. Written atomically, and only when
/// the content differs, so concurrent sessions never see a partial file.
fn write_settings(dir: &Path) -> Result<(), Error> {
    let want = serde_json::to_vec_pretty(
        &json!({ "enableTerminalSandbox": true, "toolPermission": "proceed-in-sandbox" }),
    )
    .map_err(|e| start_error(format!("encode settings: {e}")))?;
    let path = dir.join("settings.json");
    if std::fs::read(&path).is_ok_and(|have| have == want) {
        return Ok(());
    }
    let fail = |e: std::io::Error| start_error(format!("write settings {}: {e}", path.display()));
    std::fs::create_dir_all(dir).map_err(fail)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".settings-")
        .suffix(".json")
        .tempfile_in(dir)
        .map_err(fail)?;
    std::io::Write::write_all(&mut tmp, &want).map_err(fail)?;
    tmp.persist(&path).map_err(|e| fail(e.error))?;
    Ok(())
}

/// Refuses workspaces inside the temp tree: agy's sandbox allows writes there without
/// asking (m1-findings §11), so containment would not hold.
fn refuse_temp_dir(dir: &Path) -> Result<(), Error> {
    let abs = std::path::absolute(dir).map_err(|e| start_error(format!("workspace: {e}")))?;
    let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
    let mut roots = vec![std::env::temp_dir()];
    if cfg!(unix) {
        roots.extend(["/tmp", "/var/tmp", "/var/folders"].map(PathBuf::from));
    }
    for root in roots {
        let resolved = std::fs::canonicalize(&root).ok();
        for candidate in std::iter::once(root).chain(resolved) {
            if abs.starts_with(&candidate) {
                return Err(start_error(format!(
                    "workspace {} is inside the temp tree ({}), where agy's sandbox does not contain writes; use a worktree elsewhere",
                    dir.display(),
                    candidate.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{AgyOptions, AgyRuntime, NAME, refuse_temp_dir, settings_dir, write_settings};
    use crate::Runtime as _;

    #[test]
    fn settings_file() {
        let home = tempfile::tempdir().unwrap();
        let env = vec![
            (
                "HOME".to_owned(),
                home.path().to_string_lossy().into_owned(),
            ),
            ("PATH".to_owned(), "/bin".to_owned()),
        ];
        let dir = settings_dir(&AgyOptions::default(), &env).unwrap();
        assert_eq!(dir, home.path().join(".gemini/antigravity-cli/openchef"));
        write_settings(&dir).unwrap();
        let path = dir.join("settings.json");
        let got: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            got,
            serde_json::json!({"toolPermission": "proceed-in-sandbox", "enableTerminalSandbox": true})
        );
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        write_settings(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified,
            "identical settings were rewritten"
        );
        std::fs::write(&path, r#"{"toolPermission":"always-proceed"}"#).unwrap();
        write_settings(&dir).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("proceed-in-sandbox"),
            "tampered settings not restored"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "temp files left behind"
        );
        let custom = home.path().join("custom");
        let options = AgyOptions {
            settings_dir: Some(custom.clone()),
            ..AgyOptions::default()
        };
        assert_eq!(settings_dir(&options, &[]).unwrap(), custom);
    }

    #[test]
    fn refuses_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            refuse_temp_dir(tmp.path()).is_err(),
            "a workspace in the temp tree was accepted"
        );
        assert!(
            refuse_temp_dir(&std::env::temp_dir()).is_err(),
            "the temp root itself was accepted"
        );
        assert!(
            refuse_temp_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).is_ok(),
            "the repository was refused"
        );
        if cfg!(unix) {
            assert!(
                refuse_temp_dir(Path::new("/tmpfoo/x")).is_ok(),
                "a sibling of /tmp was refused"
            );
        }
    }

    #[test]
    fn defaults() {
        let o = AgyOptions::default();
        assert_eq!(
            (
                o.binary.to_str(),
                o.cli_dir_name.as_str(),
                o.args.as_slice()
            ),
            (Some("agy"), "openchef", &["acp".to_owned()][..])
        );
        assert_eq!(AgyRuntime::default().name(), NAME);
    }
}
