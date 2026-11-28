//! The plan council's loop (§7.5 phase 2, D-039): critics of other providers read the
//! plan; the planner revises it in its own session; the last round's findings wait at
//! approval, where the user decides. The council never blocks a plan from approval.

use openchef_core::{Attempt, AttemptRole, AttemptStatus, TurnKind};
use openchef_runtime::FailureKind;
use openchef_store::NewAttempt;

use crate::council;
use crate::engine::{Error, Run};
use crate::events::EventKind;
use crate::plan::{self, Plan};
use crate::review::Finding;
use crate::reviewing::finding_rows;
use crate::router::{Agent, Avoid};
use crate::session::Live;
use crate::turn::TurnEnd;

/// One critic: its attempt, its open session and its blocking findings of the last
/// round it answered (with its own ids).
struct Critic {
    agent: Agent,
    attempt: Attempt,
    live: Option<Live>,
    open: Vec<Finding>,
}

impl Run<'_> {
    /// Runs the council on a valid `plan` while the planner's session is still open and
    /// returns the plan to store: the last valid revision.
    pub(crate) async fn council(
        &mut self,
        planner_attempt: &Attempt,
        planner: &Agent,
        live: &mut Option<Live>,
        mut plan: Plan,
    ) -> Result<Plan, Error> {
        let importance = council::importance(self.task.importance, &plan);
        let depth = council::depth(importance);
        if depth.critics == 0 {
            return Ok(plan);
        }
        let mut critics = self
            .open_critics(planner, importance, depth.critics)
            .await?;
        if critics.is_empty() {
            return Ok(plan);
        }
        let result = self
            .council_rounds(
                planner_attempt,
                planner,
                live,
                &mut critics,
                &mut plan,
                depth.rounds,
            )
            .await;
        for c in critics {
            self.close_quietly(c.live, 0, c.attempt.id).await;
            self.store()
                .end_attempt(c.attempt.id, AttemptStatus::Succeeded, "critiqued")
                .await?;
        }
        result.map(|()| plan)
    }

    /// Routes and starts up to `wanted` critics, each of a provider neither the planner
    /// nor an earlier critic uses. None at all → the council is skipped, said so.
    async fn open_critics(
        &self,
        planner: &Agent,
        importance: openchef_core::Importance,
        wanted: usize,
    ) -> Result<Vec<Critic>, Error> {
        let mut step = self.planning_step();
        step.importance = importance;
        let mut avoid = Avoid::runtimes([planner.runtime.as_str()]);
        let mut critics = Vec::new();
        let mut why = String::new();
        while critics.len() < wanted {
            let agent = match self
                .e
                .cfg
                .router
                .route_independent(&self.task, &step, &avoid)
            {
                Ok(a) => a,
                Err(no) => {
                    why = no.reason;
                    break;
                }
            };
            avoid.runtimes.push(agent.runtime.clone());
            if let Some(c) = self.open_critic(agent).await? {
                critics.push(c);
            }
        }
        if critics.is_empty() {
            self.council_finished("skipped", &why);
        } else if critics.len() < wanted {
            self.notice(
                0,
                None,
                format!("{} of {wanted} critics: {why}", critics.len()),
            );
        }
        Ok(critics)
    }

    /// Records a critic's attempt and opens its session; `None` (recorded) when it
    /// cannot start.
    async fn open_critic(&self, agent: Agent) -> Result<Option<Critic>, Error> {
        let attempt = self
            .store()
            .start_attempt(NewAttempt {
                task_id: self.task.id.clone(),
                step_index: 0,
                role: AttemptRole::Council,
                agent: agent.name.clone(),
                runtime: agent.runtime.clone(),
                model: agent.model.clone().unwrap_or_default(),
                pool: agent.pool.clone(),
            })
            .await?;
        match self.open_session(&attempt, &agent, None, None, 1).await {
            Ok(l) => Ok(Some(Critic {
                agent,
                attempt,
                live: Some(l),
                open: Vec::new(),
            })),
            Err(e) if e.stopped() || self.shutdown.is_cancelled() => Err(e),
            Err(e) => {
                let why = format!("critic {} could not start: {e}", agent.name);
                self.store()
                    .end_attempt(attempt.id, AttemptStatus::Failed, &why)
                    .await?;
                self.notice(0, Some(attempt.id), why);
                Ok(None)
            }
        }
    }

    /// The critique rounds: each critic reviews, then the planner revises, until the
    /// plan is clean, a revision fails or the rounds are used up.
    async fn council_rounds(
        &mut self,
        planner_attempt: &Attempt,
        planner: &Agent,
        live: &mut Option<Live>,
        critics: &mut Vec<Critic>,
        plan: &mut Plan,
        rounds: u32,
    ) -> Result<(), Error> {
        for round in 1..=rounds {
            let blocking = self.critique_round(critics, plan, round).await?;
            if critics.is_empty() {
                self.council_finished("skipped", "no critic gave a valid critique");
                return Ok(());
            }
            if blocking.is_empty() {
                self.council_finished("locked", &format!("round {round}"));
                return Ok(());
            }
            if round == rounds {
                let msg = format!(
                    "{} blocking finding(s) left for approval after {round} round(s)",
                    blocking.len()
                );
                self.council_finished("findings", &msg);
                return Ok(());
            }
            let Some(revised) = self
                .revise(planner_attempt, planner, live, &blocking)
                .await?
            else {
                let msg = format!(
                    "the planner's revision failed; {} blocking finding(s) left for approval",
                    blocking.len()
                );
                self.council_finished("findings", &msg);
                return Ok(());
            };
            *plan = revised;
        }
        Ok(())
    }

    /// One round of every critic. Returns the round's blocking findings with the critic's
    /// name; a critic that cannot answer is dropped (recorded).
    async fn critique_round(
        &mut self,
        critics: &mut Vec<Critic>,
        plan: &Plan,
        round: u32,
    ) -> Result<Vec<(String, Finding)>, Error> {
        let mut blocking = Vec::new();
        let mut dropped = Vec::new();
        for (n, c) in critics.iter_mut().enumerate() {
            let mut ev = self.event(EventKind::CouncilStarted);
            ev.step = Some(0);
            ev.attempt = Some(c.attempt.id);
            ev.agent = Some(c.agent.name.clone());
            ev.message = Some(format!("round {round}"));
            self.publish(ev);
            let prompt = council::critic_prompt(&self.task.prompt, plan, &c.open, round);
            let answered = self
                .review_turns(0, &c.attempt, &c.agent, &prompt, &mut c.live)
                .await?;
            let found = match answered {
                Ok(r) => council::locate(r, plan),
                Err(why) => {
                    self.notice(0, Some(c.attempt.id), format!("critic dropped: {why}"));
                    dropped.push(n);
                    continue;
                }
            };
            let again: Vec<&str> = found.findings.iter().map(|f| f.id.as_str()).collect();
            let fixed: Vec<(String, Option<String>)> = c
                .open
                .iter()
                .filter(|f| !again.contains(&f.id.as_str()))
                .map(|f| (format!("{}/{}", c.agent.name, f.id), None))
                .collect();
            if round > 1 && !fixed.is_empty() {
                self.store()
                    .settle_findings(&self.task.id, 0, round - 1, fixed, "fixed")
                    .await?;
            }
            let rows = finding_rows(
                &self.task.id,
                0,
                round,
                c.attempt.id,
                &found.findings,
                Some(&c.agent.name),
            );
            self.store().record_findings(rows).await?;
            c.open = found
                .findings
                .into_iter()
                .filter(|f| f.severity.blocking())
                .collect();
            blocking.extend(c.open.iter().map(|f| {
                let mut f = f.clone();
                f.id = format!("{}/{}", c.agent.name, f.id);
                (c.agent.name.clone(), f)
            }));
        }
        for n in dropped.into_iter().rev() {
            let c = critics.remove(n);
            self.close_quietly(c.live, 0, c.attempt.id).await;
            self.store()
                .end_attempt(c.attempt.id, AttemptStatus::Failed, "no valid critique")
                .await?;
        }
        Ok(blocking)
    }

    /// The planner's revision and its one repair turn; `None` (with a notice) when no
    /// valid revision came, so the last valid plan stands.
    async fn revise(
        &self,
        a: &Attempt,
        planner: &Agent,
        live: &mut Option<Live>,
        blocking: &[(String, Finding)],
    ) -> Result<Option<Plan>, Error> {
        let schema = plan::schema();
        let mut msg = council::revision_prompt(blocking);
        for turn in 0..2 {
            let Some(l) = live.as_mut() else {
                return Ok(None);
            };
            let result = match self
                .run_turn(l, a, planner, TurnKind::Retry, &msg, Some(&schema))
                .await?
            {
                TurnEnd::Completed(r) => r,
                TurnEnd::Failed(f) => {
                    if matches!(
                        f.kind,
                        FailureKind::QuotaExhausted | FailureKind::RateLimited
                    ) && let Some(cb) = &self.e.cfg.pool_exhausted
                    {
                        cb(&planner.pool, f.resets_at);
                    }
                    self.notice(
                        0,
                        Some(a.id),
                        format!("the revision failed ({}): {}", f.kind.as_str(), f.message),
                    );
                    return Ok(None);
                }
                TurnEnd::Loop | TurnEnd::Stall(_) => {
                    self.notice(0, Some(a.id), "the revision stopped making progress");
                    return Ok(None);
                }
            };
            match plan::parse(result.structured.as_ref(), &result.final_text) {
                Ok(p) => return Ok(Some(p)),
                Err(problems) if turn == 0 => msg = plan::repair_prompt(&problems),
                Err(problems) => {
                    self.notice(
                        0,
                        Some(a.id),
                        format!(
                            "the revised plan is still not valid; keeping the last valid plan: {}",
                            problems.join("; ")
                        ),
                    );
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    /// The council's outcome for the plan.
    fn council_finished(&self, status: &str, message: &str) {
        let mut ev = self.event(EventKind::CouncilFinished);
        ev.step = Some(0);
        ev.status = Some(status.to_owned());
        ev.message = Some(message.to_owned());
        self.publish(ev);
    }
}
