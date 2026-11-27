//! Whether a path an agent names is inside a workspace and outside the directories
//! only OpenChef may write (§8.2, §9.2), ported from Go `internal/pathguard`. Symlinks
//! are resolved (a committed `vendor -> /` must not let writes escape), names compare
//! case-insensitively where the file system usually is (macOS, Windows), and Windows
//! name tricks (trailing dots or spaces, alternate data streams, 8.3 short names) that
//! would alias a protected directory are refused.

use std::io;
use std::path::{Component, Path, PathBuf};

/// The workspace directories only OpenChef writes.
pub const PROTECTED: &[&str] = &[".git", ".openchef", ".claude"];

/// Where the default file systems are case-insensitive.
const FOLD_CASE: bool = cfg!(any(target_os = "macos", windows));

/// Why a path could not be checked.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A relative path came without a base to resolve it against.
    #[error("pathguard: relative path {} without a base", .0.display())]
    RelativeWithoutBase(PathBuf),
    /// Resolving symlinks failed for a reason other than a missing file.
    #[error("pathguard: resolve {}: {source}", path.display())]
    Resolve {
        /// The path being resolved.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
}

/// `p` made absolute (relative to `base`) and lexically cleaned, with every symlink in
/// its existing part resolved. The not-yet-existing tail is appended as is: a file the
/// agent is about to create lives where its nearest existing parent really is.
///
/// # Errors
///
/// A relative `p` without `base`, or a resolution failure other than "not found".
pub fn resolve(base: Option<&Path>, p: &Path) -> Result<PathBuf, Error> {
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        match base {
            Some(b) if !b.as_os_str().is_empty() => b.join(p),
            _ => return Err(Error::RelativeWithoutBase(p.to_path_buf())),
        }
    };
    let cleaned = clean(&absolute);
    let mut existing = cleaned.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(mut resolved) => {
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Resolve {
                    path: existing,
                    source,
                });
            }
        }
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
            return Ok(cleaned); // nothing exists; nothing to resolve
        };
        tail.push(name.to_os_string());
        existing = parent.to_path_buf();
    }
}

/// Why an agent may not write `p` in `workspace`, or `None` if it may. Both paths are
/// resolved first.
///
/// # Errors
///
/// When a path cannot be resolved.
pub fn check(workspace: &Path, p: &Path) -> Result<Option<String>, Error> {
    if workspace.as_os_str().is_empty() || !workspace.is_absolute() {
        return Ok(Some(
            "no workspace is known, so no file may be changed".to_owned(),
        ));
    }
    let shown = p.display().to_string();
    // Names first, on the path as given: a Windows alias of .git
    // (".git::$INDEX_ALLOCATION") may not even resolve, and must be refused for what it
    // names, not for an I/O error.
    let lexical = clean(&workspace.join(p));
    if let Some(rel) = within(&clean(workspace), &lexical)
        && let Some(why) = protected_in(&rel, &shown)
    {
        return Ok(Some(why));
    }
    let root = resolve(None, workspace)?;
    let resolved = resolve(Some(workspace), p)?;
    match within(&root, &resolved) {
        None => Ok(Some(format!(
            "only files inside the task's workspace may be changed: {shown}"
        ))),
        Some(rel) => Ok(protected_in(&rel, &shown)),
    }
}

/// Whether `p` (resolved) is inside `workspace` (resolved), for read checks.
///
/// # Errors
///
/// When a path cannot be resolved.
pub fn is_within(workspace: &Path, p: &Path) -> Result<bool, Error> {
    let root = resolve(None, workspace)?;
    let resolved = resolve(Some(workspace), p)?;
    Ok(within(&root, &resolved).is_some())
}

/// Removes `.` and resolves `..` lexically (like Go's `filepath.Clean`).
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // At the root, `..` stays at the root.
                let _popped = out.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => out.push(c),
        }
    }
    out
}

fn fold(s: &str) -> String {
    if FOLD_CASE {
        s.to_lowercase()
    } else {
        s.to_owned()
    }
}

/// The path elements of `p` below `root`, or `None` when `p` is not `root` or below it.
fn within(root: &Path, p: &Path) -> Option<Vec<String>> {
    let names = |x: &Path| -> Vec<String> {
        x.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect()
    };
    let (root, p) = (names(root), names(p));
    if p.len() < root.len() {
        return None;
    }
    let prefix_matches = root.iter().zip(&p).all(|(a, b)| fold(a) == fold(b));
    prefix_matches.then(|| p[root.len()..].to_vec())
}

/// Why `rel` (relative to the workspace) is under a protected directory, or `None`.
fn protected_in(rel: &[String], shown: &str) -> Option<String> {
    rel.iter().find_map(|part| {
        protected(part).map(|name| {
            format!("files under {name} are managed by OpenChef and may not be changed: {shown}")
        })
    })
}

/// The protected directory one path element names (or may alias).
fn protected(part: &str) -> Option<String> {
    let mut name = part;
    if cfg!(windows) {
        if name.contains([':', '~']) {
            // Alternate data streams (.git::$INDEX_ALLOCATION), short names (GIT~1).
            return Some(name.to_owned());
        }
        name = name.trim_end_matches(['.', ' ']); // ".git." and ".git " open ".git"
    }
    PROTECTED
        .iter()
        .find(|d| name == **d || (FOLD_CASE && name.eq_ignore_ascii_case(d)))
        .map(|d| (*d).to_owned())
}
