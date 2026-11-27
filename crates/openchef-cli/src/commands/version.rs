//! `openchef version`.

use std::io::Write;

use serde::Serialize;

use crate::exit;

/// Build metadata stamped by `build.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
    /// The crate version.
    pub version: &'static str,
    /// The git commit, if known.
    pub commit: &'static str,
    /// The commit date, if known.
    pub date: &'static str,
    /// Whether the tree had uncommitted changes.
    pub modified: bool,
    /// The compiler.
    pub rustc: &'static str,
    /// `os/arch`.
    pub platform: String,
}

impl BuildInfo {
    /// The running binary's metadata.
    #[must_use]
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            commit: env!("OPENCHEF_COMMIT"),
            date: env!("OPENCHEF_COMMIT_DATE"),
            modified: env!("OPENCHEF_MODIFIED") == "true",
            rustc: env!("OPENCHEF_RUSTC"),
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        }
    }
}

impl std::fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "openchef {}", self.version)?;
        if !self.commit.is_empty() {
            write!(
                f,
                " ({}{}",
                self.commit,
                if self.modified { ", modified" } else { "" }
            )?;
            if !self.date.is_empty() {
                write!(f, ", {}", self.date)?;
            }
            write!(f, ")")?;
        }
        write!(f, " {}", self.platform)
    }
}

pub(crate) fn run(out: &mut dyn Write, json: bool) -> anyhow::Result<i32> {
    let info = BuildInfo::current();
    if json {
        serde_json::to_writer_pretty(&mut *out, &info)?;
        writeln!(out)?;
    } else {
        writeln!(out, "{info}")?;
    }
    Ok(exit::OK)
}
