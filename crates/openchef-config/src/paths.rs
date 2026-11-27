//! Every location OpenChef reads or writes (§13).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::Error;

/// Overrides the OpenChef home directory (default `~/.openchef` on every OS).
pub const HOME_ENV: &str = "OPENCHEF_HOME";

/// OpenChef's file locations. All paths are absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The OpenChef home.
    pub home: PathBuf,
    /// `config.toml`.
    pub config: PathBuf,
    /// `settings.toml` (permissions, §9).
    pub settings: PathBuf,
    /// `state.db`.
    pub state: PathBuf,
    /// Daemon logs.
    pub logs: PathBuf,
    /// Per-session event logs.
    pub sessions: PathBuf,
    /// Task worktrees.
    pub worktrees: PathBuf,
    /// The daemon socket.
    pub socket: PathBuf,
    /// The daemon's single-instance lock.
    pub lock: PathBuf,
}

impl Paths {
    /// The paths for the current user, honouring [`HOME_ENV`].
    pub fn resolve() -> Result<Self, Error> {
        if let Some(home) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
            return Self::in_home(Path::new(&home));
        }
        let user_home = std::env::home_dir().ok_or(Error::NoHome)?;
        Self::in_home(&user_home.join(".openchef"))
    }

    /// The paths rooted at `home`.
    pub fn in_home(home: &Path) -> Result<Self, Error> {
        if home.as_os_str().is_empty() {
            return Err(Error::NoHome);
        }
        let home = std::path::absolute(home).map_err(|source| Error::Read {
            path: home.to_path_buf(),
            source,
        })?;
        Ok(Self {
            config: home.join("config.toml"),
            settings: home.join("settings.toml"),
            state: home.join("state.db"),
            logs: home.join("logs"),
            sessions: home.join("sessions"),
            worktrees: home.join("worktrees"),
            socket: home.join("daemon.sock"),
            lock: home.join("daemon.lock"),
            home,
        })
    }

    /// Creates the home and its directories, accessible only by the user on Unix.
    pub fn ensure_dirs(&self) -> Result<(), Error> {
        for dir in [&self.home, &self.logs, &self.sessions, &self.worktrees] {
            create_private_dir(dir)?;
        }
        Ok(())
    }
}

fn create_private_dir(dir: &Path) -> Result<(), Error> {
    let write_err = |source| Error::Write {
        path: dir.to_path_buf(),
        source,
    };
    let mut builder = fs::DirBuilder::new();
    let _ = builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let _ = builder.mode(0o700);
    }
    builder.create(dir).map_err(write_err)
}

/// Writes `data` to `path` through a temporary file in the same directory and a rename,
/// so readers never see a half-written file. The file is readable only by the user on
/// Unix.
pub(crate) fn write_private_atomic(path: &Path, data: &[u8]) -> Result<(), Error> {
    let write_err = |source| Error::Write {
        path: path.to_path_buf(),
        source,
    };
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map_or_default(|n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut f| f.write_all(data).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        // The temporary file is ours; if removing it fails too, the original error is
        // what matters to the caller.
        drop(fs::remove_file(&tmp));
    }
    result.map_err(write_err)
}
