//! Layered settings files (§9.1), ported from Go `internal/permissions/settings.go`:
//! the user's `~/.openchef/settings.toml`, then the repository's
//! `.openchef/settings.toml` and `.openchef/settings.local.toml`, with the keys of
//! Claude Code's JSON settings.

use std::path::{Path, PathBuf};

use openchef_core::PermissionMode;
use serde::Deserialize;

use crate::{Error, Rule};

/// The project settings file inside a repository.
pub const PROJECT_SETTINGS: &str = ".openchef/settings.toml";
/// The personal, uncommitted settings file inside a repository.
pub const LOCAL_SETTINGS: &str = ".openchef/settings.local.toml";

/// A merged permission configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// The permission mode.
    pub mode: PermissionMode,
    /// Rules that allow.
    pub allow: Vec<Rule>,
    /// Rules that ask.
    pub ask: Vec<Rule>,
    /// Rules that deny; they win over everything.
    pub deny: Vec<Rule>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Manual,
            allow: Vec::new(),
            ask: Vec::new(),
            deny: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSettings {
    #[serde(default)]
    permissions: Permissions,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Permissions {
    #[serde(default)]
    default_mode: Option<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

/// Merges the user settings file and the repository's project and local files, in
/// that order: rules accumulate and the most specific file's `defaultMode` wins.
/// Missing files are skipped; with none the mode is manual.
///
/// # Errors
///
/// [`Error::Settings`] for an unreadable file, unknown keys, a bad mode or any invalid
/// rule: a typo in a deny rule must never silently allow.
pub fn load(user_file: Option<&Path>, repo_root: Option<&Path>) -> Result<Policy, Error> {
    let mut files: Vec<PathBuf> = user_file.map(Path::to_path_buf).into_iter().collect();
    if let Some(root) = repo_root {
        files.push(root.join(PROJECT_SETTINGS));
        files.push(root.join(LOCAL_SETTINGS));
    }
    let mut policy = Policy::default();
    for file in files {
        if let Some(settings) = read(&file)? {
            merge(&mut policy, &file, settings)?;
        }
    }
    Ok(policy)
}

fn settings_error(path: &Path, problems: Vec<String>) -> Error {
    Error::Settings {
        path: path.to_path_buf(),
        problems,
    }
}

fn read(path: &Path) -> Result<Option<FileSettings>, Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(settings_error(path, vec![format!("read: {e}")])),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| settings_error(path, vec![e.to_string()]))
}

fn merge(policy: &mut Policy, path: &Path, s: FileSettings) -> Result<(), Error> {
    let mut problems = Vec::new();
    if let Some(m) = s.permissions.default_mode.filter(|m| !m.is_empty()) {
        match openchef_runtime::parse_mode(&m) {
            Ok(mode) => policy.mode = mode,
            Err(e) => problems.push(format!("defaultMode: {e}")),
        }
    }
    for (kind, list, dst) in [
        ("allow", s.permissions.allow, &mut policy.allow),
        ("ask", s.permissions.ask, &mut policy.ask),
        ("deny", s.permissions.deny, &mut policy.deny),
    ] {
        for raw in list {
            match Rule::parse(&raw) {
                Ok(r) => dst.push(r),
                Err(e) => problems.push(format!("{kind}: {e}")),
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(settings_error(path, problems))
    }
}
