//! `openchef doctor`: installed agent CLIs, logins and configuration.

use std::io::Write;

use openchef_config::{Config, Error as ConfigError, Login, Paths, RuntimeStatus};
use serde::Serialize;

use crate::output::{or_dash, table};
use crate::{Deps, exit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfigStatus {
    Missing,
    Valid,
    Invalid,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConfigReport {
    pub(crate) status: ConfigStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) problems: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) paths: Paths,
    pub(crate) config: ConfigReport,
    pub(crate) runtimes: Vec<RuntimeStatus>,
    pub(crate) healthy: bool,
}

pub(crate) fn report(deps: &Deps, paths: &Paths) -> Report {
    let config = match Config::load(&paths.config) {
        Ok(_) => ConfigReport {
            status: ConfigStatus::Valid,
            problems: Vec::new(),
        },
        Err(ConfigError::NotFound { .. }) => ConfigReport {
            status: ConfigStatus::Missing,
            problems: Vec::new(),
        },
        Err(ConfigError::Invalid { problems, .. }) => ConfigReport {
            status: ConfigStatus::Invalid,
            problems,
        },
        Err(other) => ConfigReport {
            status: ConfigStatus::Invalid,
            problems: vec![other.to_string()],
        },
    };
    let runtimes = deps.detector.detect();
    let healthy =
        runtimes.iter().any(RuntimeStatus::usable) && config.status != ConfigStatus::Invalid;
    Report {
        paths: paths.clone(),
        config,
        runtimes,
        healthy,
    }
}

pub(crate) fn run(
    out: &mut dyn Write,
    deps: &Deps,
    paths: &Paths,
    json: bool,
) -> anyhow::Result<i32> {
    let r = report(deps, paths);
    if json {
        serde_json::to_writer_pretty(&mut *out, &r)?;
        writeln!(out)?;
    } else {
        render(out, &r)?;
    }
    Ok(if r.healthy { exit::OK } else { exit::FAILURE })
}

/// How a runtime looks in the table.
pub(crate) fn state(s: &RuntimeStatus) -> String {
    let with = |text: &str| match s.login_detail.as_deref() {
        Some(d) if !d.is_empty() => format!("{text} ({d})"),
        _ => text.to_owned(),
    };
    match (&s.problem, s.login) {
        (Some(problem), _) => problem.clone(),
        (None, Login::LoggedIn) => with("logged in"),
        (None, Login::LoggedOut) => with("NOT logged in"),
        (None, Login::Unknown) => with("login unknown"),
    }
}

fn render(out: &mut dyn Write, r: &Report) -> anyhow::Result<()> {
    let config = match r.config.status {
        ConfigStatus::Valid => format!("valid ({})", r.paths.config.display()),
        ConfigStatus::Missing => "not found — run `openchef init`".to_owned(),
        ConfigStatus::Invalid => format!("invalid ({})", r.paths.config.display()),
    };
    write!(
        out,
        "{}",
        table(&[
            vec![
                "OpenChef home".to_owned(),
                r.paths.home.display().to_string()
            ],
            vec!["Config".to_owned(), config]
        ])
    )?;
    for problem in &r.config.problems {
        writeln!(out, "  - {problem}")?;
    }
    let mut rows = vec![vec![
        "Runtime".to_owned(),
        "Version".to_owned(),
        "Status".to_owned(),
        "Path".to_owned(),
    ]];
    for rt in &r.runtimes {
        rows.push(vec![
            rt.runtime.to_string(),
            or_dash(rt.version.as_deref()),
            state(rt),
            or_dash(rt.path.as_ref().and_then(|p| p.to_str())),
        ]);
    }
    write!(out, "\n{}", table(&rows))?;
    if !r.healthy {
        if r.config.status == ConfigStatus::Invalid {
            writeln!(
                out,
                "\nOpenChef cannot run yet: fix the configuration above."
            )?;
        } else {
            writeln!(
                out,
                "\nOpenChef cannot run yet: install and log in to at least one of claude, codex or agy."
            )?;
        }
    }
    Ok(())
}
