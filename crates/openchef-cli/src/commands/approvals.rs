//! `openchef approvals|approve|deny|answer`, ported from Go `internal/cli/approvals.go`.

use std::io::Write;

use openchef_api::{ApprovalListParams, DecideParams, Verdict, friendly};
use openchef_config::Paths;
use time::OffsetDateTime;

use crate::cli::ApprovalsArgs;
use crate::commands::daemon::{CLIENT_NAME, client_runtime, report};
use crate::exit;
use crate::output::table;
use crate::render::{first_line, since, what};

async fn dial(paths: &Paths) -> anyhow::Result<openchef_api::Client> {
    Ok(openchef_api::Client::dial(&paths.socket, CLIENT_NAME).await?)
}

pub(crate) fn list(
    out: &mut dyn Write,
    paths: &Paths,
    args: &ApprovalsArgs,
) -> anyhow::Result<i32> {
    client_runtime()?.block_on(async {
        let c = dial(paths).await?;
        let params = ApprovalListParams {
            task: args.task.clone(),
            all: args.all,
            limit: Some(args.limit),
        };
        let result = c.approvals(&params).await;
        c.close();
        let list = result.map_err(|e| anyhow::anyhow!(friendly(&e)))?;
        if args.json {
            report(out, true, &list, "")?;
            return Ok(exit::OK);
        }
        if list.is_empty() {
            let msg = if args.all {
                "no requests yet"
            } else {
                "nothing is waiting for you"
            };
            writeln!(out, "{msg}")?;
            return Ok(exit::OK);
        }
        let now = OffsetDateTime::now_utc();
        let mut rows = vec![
            ["ID", "TASK", "AGENT", "STATUS", "ASKED", "REQUEST"]
                .map(str::to_owned)
                .to_vec(),
        ];
        for a in &list {
            rows.push(vec![
                a.id.to_string(),
                a.task_id.to_string(),
                a.agent.clone(),
                a.status.to_string(),
                since(a.created_at, now),
                first_line(&what(a), 70),
            ]);
        }
        out.write_all(table(&rows).as_bytes())?;
        Ok(exit::OK)
    })
}

/// A request id as typed: `12` or `#12`.
fn parse_id(raw: &str) -> anyhow::Result<i64> {
    match raw.trim_start_matches('#').parse::<i64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => anyhow::bail!("request id {raw:?}: want the number shown by `openchef approvals`"),
    }
}

pub(crate) fn decide(
    out: &mut dyn Write,
    paths: &Paths,
    id: &str,
    verdict: Verdict,
    words: &[String],
    json: bool,
) -> anyhow::Result<i32> {
    let id = parse_id(id)?;
    let text = (!words.is_empty()).then(|| words.join(" "));
    client_runtime()?.block_on(async {
        let c = dial(paths).await?;
        let result = c.decide(&DecideParams { id, verdict, text }).await;
        c.close();
        let a = result.map_err(|e| anyhow::anyhow!(friendly(&e)))?;
        report(out, json, &a, &format!("request {} {}", a.id, a.status))?;
        Ok(exit::OK)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_id() {
        assert_eq!(super::parse_id("12").ok(), Some(12));
        assert_eq!(super::parse_id("#7").ok(), Some(7));
        assert!(super::parse_id("0").is_err());
        assert!(super::parse_id("x").is_err());
    }
}
