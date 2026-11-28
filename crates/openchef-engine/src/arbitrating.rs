//! The arbiter's turn (§7.5 phase 4.4, D-039): once per step, after its review loop,
//! an agent independent of the implementer and every reviewer rules on the findings the
//! implementer rebutted; upheld ones get one last fix turn, dismissed ones are dropped.

use std::collections::BTreeMap;

use openchef_core::{Attempt, AttemptRole, AttemptStatus, TurnKind};
use openchef_runtime::Failure;
use openchef_store::NewAttempt;

use crate::arbiter::{self, Dispute, Ruling, Verdicts};
use crate::breakers::Progress;
use crate::engine::{Error, Run};
use crate::escalating::Stagnation;
use crate::events::EventKind;
use crate::review;
use crate::reviewing::{Judge, Reverified, out_of_quota, skip_reason};
use crate::router::{Agent, Avoid};
use crate::session::Live;
use crate::turn::TurnEnd;

/// How a step's arbitration ended.
pub(crate) enum Ruled {
    /// Ruled (or skipped: the findings stay `disputed`); the step may commit.
    Done,
    /// The upheld fix broke verification and the retries ran out.
    Failed,
    /// The upheld fix's verification stagnated.
    Stagnated(Stagnation),
    /// The implementer's pool ran out during the upheld fix: fail over (§7.2).
    Exhausted(Failure),
}

impl Run<'_> {
    /// Rules on step `i`'s disputes (with the round each was raised in) and runs the
    /// fix turn for upheld ones. A missing or failing arbiter, or a used-up budget,
    /// leaves the findings `disputed`.
    #[allow(
        clippy::too_many_arguments,
        reason = "the step's attempt context, as reviewed_commit"
    )]
    pub(crate) async fn arbitrate(
        &mut self,
        i: usize,
        a: &Attempt,
        implementer: &Agent,
        live: &mut Option<Live>,
        progress: &mut Progress,
        disputes: &[(u32, Dispute)],
        judged: &Avoid,
    ) -> Result<Ruled, Error> {
        let index = self.steps[i].index;
        if !self.debate_left(index).await? {
            self.arbitration_finished(index, None, None, "skipped", &self.budget_reason());
            return Ok(Ruled::Done);
        }
        let arbiter = match self.route_arbiter(i, judged) {
            Ok(agent) => agent,
            Err(why) => {
                self.arbitration_finished(index, None, None, "skipped", &why);
                return Ok(Ruled::Done);
            }
        };
        let Some(verdicts) = self.rulings(i, &arbiter, disputes).await? else {
            return Ok(Ruled::Done);
        };
        let upheld = self.apply_rulings(index, disputes, &verdicts).await?;
        if upheld.is_empty() || !self.spend_debate_turn(index).await? {
            return Ok(Ruled::Done); // upheld findings without a fix turn stay open
        }
        let Some(l) = live.as_mut() else {
            return Err(Error::Session("the attempt has no session".into()));
        };
        let fix: Vec<(review::Finding, String)> = upheld
            .iter()
            .map(|(_, d, reason)| (d.finding.clone(), reason.clone()))
            .collect();
        let msg = arbiter::upheld_prompt(&arbiter.name, &fix);
        let answer = match self
            .run_turn(l, a, implementer, TurnKind::Retry, &msg, None)
            .await?
        {
            TurnEnd::Completed(r) => r.final_text,
            TurnEnd::Failed(f) if out_of_quota(&f) => return Ok(Ruled::Exhausted(f)),
            TurnEnd::Failed(_) | TurnEnd::Loop | TurnEnd::Stall(_) => {
                self.notice(
                    index,
                    Some(a.id),
                    "the fix turn did not complete; re-verifying",
                );
                String::new()
            }
        };
        match self.reverify(i, a, implementer, live, progress).await? {
            Reverified::Passed => {}
            Reverified::Exhausted => return Ok(Ruled::Failed),
            Reverified::Stagnated(s) => return Ok(Ruled::Stagnated(s)),
        }
        // No review is left to confirm these fixes: the implementer's FIXED claims stand.
        let claimed = review::fixed_claims(&answer);
        let mut by_round: BTreeMap<u32, Vec<(String, Option<String>)>> = BTreeMap::new();
        for (round, d, _) in upheld
            .iter()
            .filter(|(_, d, _)| claimed.contains(&d.finding.id))
        {
            by_round
                .entry(*round)
                .or_default()
                .push((d.finding.id.clone(), None));
        }
        for (round, ids) in by_round {
            self.store()
                .settle_findings(&self.task.id, index, round, ids, "fixed")
                .await?;
        }
        Ok(Ruled::Done)
    }

    /// An arbiter on a pool the implementer and the reviewers did not use, preferring a
    /// provider none of them is; the reason when there is none.
    fn route_arbiter(&self, i: usize, judged: &Avoid) -> Result<Agent, String> {
        let router = &self.e.cfg.router;
        router
            .route_independent(&self.task, &self.steps[i], judged)
            .or_else(|_| {
                let pools_only = Avoid {
                    runtimes: Vec::new(),
                    pools: judged.pools.clone(),
                };
                router.route_independent(&self.task, &self.steps[i], &pools_only)
            })
            .map_err(|no| format!("no arbiter on a third pool: {}", skip_reason(&no)))
    }

    /// The arbiter's attempt: its session, the batched turn and its repair. `None`
    /// (recorded) when it gave no valid verdicts.
    async fn rulings(
        &self,
        i: usize,
        arbiter: &Agent,
        disputes: &[(u32, Dispute)],
    ) -> Result<Option<Verdicts>, Error> {
        let index = self.steps[i].index;
        let att = self
            .store()
            .start_attempt(NewAttempt {
                task_id: self.task.id.clone(),
                step_index: index,
                role: AttemptRole::Arbiter,
                agent: arbiter.name.clone(),
                runtime: arbiter.runtime.clone(),
                model: arbiter.model.clone().unwrap_or_default(),
                pool: arbiter.pool.clone(),
            })
            .await?;
        let mut ev = self.event(EventKind::ArbitrationStarted);
        ev.step = Some(index);
        ev.attempt = Some(att.id);
        ev.agent = Some(arbiter.name.clone());
        ev.message = Some(format!("{} disputed finding(s)", disputes.len()));
        self.publish(ev);
        let diff = self.worktree()?.diff(review::MAX_DIFF + 1).await?;
        let list: Vec<Dispute> = disputes.iter().map(|(_, d)| d.clone()).collect();
        let prompt = arbiter::prompt(&self.steps[i].title, &review::bounded_diff(&diff), &list);
        let mut live = match self.open_session(&att, arbiter, None, None, 1).await {
            Ok(l) => Some(l),
            Err(e) if e.stopped() || self.shutdown.is_cancelled() => return Err(e),
            Err(e) => {
                let why = format!("arbiter {} could not start: {e}", arbiter.name);
                self.store()
                    .end_attempt(att.id, AttemptStatus::Failed, &why)
                    .await?;
                self.arbitration_finished(
                    index,
                    Some(att.id),
                    Some(&arbiter.name),
                    "skipped",
                    &why,
                );
                return Ok(None);
            }
        };
        let ids: Vec<String> = list.iter().map(|d| d.finding.id.clone()).collect();
        let parse = move |structured: Option<&serde_json::Value>, text: &str| {
            arbiter::parse(structured, text, &ids)
        };
        let judge = Judge {
            what: "verdict",
            schema: arbiter::schema(),
            parse: &parse,
            repair: arbiter::repair_prompt,
        };
        let answered = self
            .judge_turns(index, &att, arbiter, &prompt, &mut live, &judge)
            .await;
        self.close_quietly(live, index, att.id).await;
        match answered? {
            Ok(v) => {
                self.store()
                    .end_attempt(att.id, AttemptStatus::Succeeded, "ruled")
                    .await?;
                let upheld = v
                    .verdicts
                    .iter()
                    .filter(|v| v.verdict == Ruling::Uphold)
                    .count();
                let msg = format!("{upheld} upheld, {} dismissed", v.verdicts.len() - upheld);
                self.arbitration_finished(index, Some(att.id), Some(&arbiter.name), "ruled", &msg);
                Ok(Some(v))
            }
            Err(why) => {
                self.store()
                    .end_attempt(att.id, AttemptStatus::Failed, &why)
                    .await?;
                self.arbitration_finished(
                    index,
                    Some(att.id),
                    Some(&arbiter.name),
                    "skipped",
                    &why,
                );
                Ok(None)
            }
        }
    }

    /// Records the verdicts: dismissed findings are `dismissed`, upheld ones `open`
    /// again, both noting the rebuttal and the arbiter's reason. Returns the upheld
    /// disputes with their round and the reason.
    async fn apply_rulings<'d>(
        &self,
        index: u32,
        disputes: &'d [(u32, Dispute)],
        verdicts: &Verdicts,
    ) -> Result<Vec<(u32, &'d Dispute, String)>, Error> {
        let mut upheld = Vec::new();
        for v in &verdicts.verdicts {
            let Some((round, d)) = disputes.iter().find(|(_, d)| d.finding.id == v.id) else {
                continue; // parse accepts only the disputed ids
            };
            let to = match v.verdict {
                Ruling::Uphold => "open",
                Ruling::Dismiss => "dismissed",
            };
            let note = format!("rebuttal: {}; arbiter: {}", d.rebuttal, v.reason);
            self.store()
                .resettle_findings(
                    &self.task.id,
                    index,
                    *round,
                    vec![(v.id.clone(), Some(note))],
                    "disputed",
                    to,
                )
                .await?;
            if v.verdict == Ruling::Uphold {
                upheld.push((*round, d, v.reason.clone()));
            }
        }
        Ok(upheld)
    }

    fn arbitration_finished(
        &self,
        step: u32,
        attempt: Option<i64>,
        agent: Option<&str>,
        status: &str,
        message: &str,
    ) {
        let mut ev = self.event(EventKind::ArbitrationFinished);
        ev.step = Some(step);
        ev.attempt = attempt;
        ev.agent = agent.map(str::to_owned);
        ev.status = Some(status.to_owned());
        ev.message = Some(message.to_owned());
        self.publish(ev);
    }
}
