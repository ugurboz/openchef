//! `openchef quota` and `openchef quota probes on|off`, ported from Go
//! `internal/cli/quota.go`.

use std::fmt::Write as _;
use std::io::Write;

use openchef_api::{ProbesParams, QuotaParams, QuotaResult, friendly};
use openchef_config::Paths;
use time::OffsetDateTime;

use crate::cli::{OnOff, QuotaArgs, QuotaCommand};
use crate::commands::daemon::{CLIENT_NAME, client_runtime, report};
use crate::exit;
use crate::output::table;
use crate::render::since;

pub(crate) fn run(out: &mut dyn Write, paths: &Paths, args: &QuotaArgs) -> anyhow::Result<i32> {
    client_runtime()?.block_on(async {
        let c = openchef_api::Client::dial(&paths.socket, CLIENT_NAME).await?;
        let shown = match &args.command {
            Some(QuotaCommand::Probes { state }) => {
                let on = *state == OnOff::On;
                c.set_probes(&ProbesParams { on })
                    .await
                    .map(|()| Ok(format!("usage probes {}\n", if on { "on" } else { "off" })))
            }
            None => c
                .quota(&QuotaParams {
                    refresh: args.refresh,
                })
                .await
                .map(|r| shown(&r, args.json, OffsetDateTime::now_utc())),
        };
        c.close();
        let shown = shown.map_err(|e| anyhow::anyhow!(friendly(&e)))??;
        out.write_all(shown.as_bytes())?;
        Ok(exit::OK)
    })
}

fn shown(r: &QuotaResult, json: bool, now: OffsetDateTime) -> anyhow::Result<String> {
    if json {
        let mut buf = Vec::new();
        report(&mut buf, true, r, "")?;
        return Ok(String::from_utf8_lossy(&buf).into_owned());
    }
    Ok(text(r, now))
}

/// The report as people read it: one row per window, then each probe's state.
fn text(r: &QuotaResult, now: OffsetDateTime) -> String {
    let mut out = String::new();
    if r.pools.is_empty() {
        out.push_str(
            "no quota data yet: it arrives with the first agent turn or probe \
             (openchef quota --refresh)\n",
        );
    } else {
        let mut rows = vec![
            ["POOL", "HEALTH", "WINDOW", "USED", "RESETS", "UPDATED"]
                .map(str::to_owned)
                .to_vec(),
        ];
        for p in &r.pools {
            let mut health = p.health.as_str().to_owned();
            if let (true, Some(at)) = (
                p.state.exhausted || p.health.as_str() == "exhausted",
                p.reset_at,
            ) {
                health = format!("{health} until {}", short_time(at));
            }
            let updated = p
                .state
                .updated
                .map_or_else(|| "-".to_owned(), |u| since(u, now));
            if p.state.windows.is_empty() {
                rows.push(vec![
                    p.state.pool.clone(),
                    health.clone(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    updated.clone(),
                ]);
            }
            for (i, w) in p.state.windows.iter().enumerate() {
                let (pool, h) = if i == 0 {
                    (p.state.pool.clone(), health.clone())
                } else {
                    (String::new(), String::new())
                };
                let resets = w
                    .resets_at
                    .map_or_else(|| "-".to_owned(), |at| format!("in {}", until(at, now)));
                rows.push(vec![
                    pool,
                    h,
                    w.name.clone(),
                    format!("{:.0}%", w.utilization * 100.0),
                    resets,
                    updated.clone(),
                ]);
            }
        }
        out.push_str(&table(&rows));
    }
    for p in &r.probes {
        let state = p.problem.as_deref().unwrap_or("ok");
        // Writing to a String cannot fail.
        let _written = writeln!(out, "probe {} ({}): {state}", p.name, p.pool);
    }
    out
}

/// A time as `15:04 UTC`.
fn short_time(at: OffsetDateTime) -> String {
    format!(
        "{:02}:{:02} UTC {} {}",
        at.hour(),
        at.minute(),
        at.month(),
        at.day()
    )
}

/// How long until `at`, rounded to minutes (`2h05m`, `7m`).
fn until(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let minutes = ((at - now).whole_seconds().max(0) + 30) / 60;
    let (d, h, m) = (minutes / 1440, minutes % 1440 / 60, minutes % 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h{m:02}m"),
        _ => format!("{d}d{h}h"),
    }
}

#[cfg(test)]
mod tests {
    use openchef_api::{PoolQuota, ProbeStatus, QuotaResult};
    use openchef_quota::{Health, State, Window};
    use time::macros::datetime;

    use super::{text, until};

    #[test]
    fn quota_text() {
        let now = datetime!(2026-11-26 12:00 UTC);
        assert!(text(&QuotaResult::default(), now).starts_with("no quota data yet"));
        let r = QuotaResult {
            pools: vec![PoolQuota {
                state: State {
                    pool: "claude-max".into(),
                    windows: vec![
                        Window {
                            name: "five_hour".into(),
                            utilization: 0.7,
                            resets_at: Some(now + std::time::Duration::from_mins(125)),
                            duration: None,
                        },
                        Window {
                            name: "weekly".into(),
                            utilization: 0.1,
                            resets_at: None,
                            duration: None,
                        },
                    ],
                    updated: Some(now),
                    ..State::default()
                },
                health: Health::Yellow,
                reset_at: None,
            }],
            probes: vec![ProbeStatus {
                name: "claude".into(),
                pool: "claude-max".into(),
                problem: Some("quota: probe is disabled".into()),
            }],
        };
        let out = text(&r, now);
        for want in [
            "POOL",
            "claude-max  yellow",
            "five_hour",
            "70%",
            "in 2h05m",
            "weekly",
            "probe claude (claude-max): quota: probe is disabled",
        ] {
            assert!(out.contains(want), "lacks {want:?}:\n{out}");
        }
    }

    #[test]
    fn until_rounds_to_minutes() {
        let now = datetime!(2026-11-26 12:00 UTC);
        assert_eq!(until(now + std::time::Duration::from_secs(400), now), "7m");
        assert_eq!(
            until(now + std::time::Duration::from_hours(50), now),
            "2d2h"
        );
        assert_eq!(until(now - std::time::Duration::from_hours(1), now), "0m");
    }
}
