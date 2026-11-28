//! What a stagnating step does (§7.5 circuit breakers, D-040): one escalation turn by a
//! frontier agent on another pool, on the worktree as the implementer left it, verified
//! once; otherwise the task needs attention.

use openchef_core::{AttemptRole, AttemptStatus, Importance, TurnKind};
use openchef_store::NewAttempt;

use crate::engine::{Error, Run};
use crate::events::EventKind;
use crate::prompts::escalation_prompt;
use crate::review::{self, Finding};
use crate::router::{Agent, Avoid};
use crate::session::Live;
use crate::step::AttemptEnd;
use crate::turn::TurnEnd;

/// Why a step stopped making progress.
pub(crate) struct Stagnation {
    /// The signal, for the event and the reason.
    pub(crate) why: String,
    /// What kept failing (verification output, findings), for the escalation turn.
    pub(crate) evidence: String,
    /// Reopened findings with the round they were recorded in, settled on a `FIXED`.
    pub(crate) findings: Vec<(u32, Finding)>,
}

impl Stagnation {
    /// A stagnation without findings.
    pub(crate) fn plain(why: String, evidence: impl Into<String>) -> Self {
        Self {
            why,
            evidence: evidence.into(),
            findings: Vec::new(),
        }
    }
}

impl Run<'_> {
    /// Stops the implementer's loop on step `i` and escalates once; the step fails and
    /// the task needs attention when that is not possible or does not resolve it.
    pub(crate) async fn stagnated(
        &mut self,
        i: usize,
        a: &openchef_core::Attempt,
        implementer: &Agent,
        live: Option<Live>,
        s: Stagnation,
    ) -> Result<AttemptEnd, Error> {
        let index = self.steps[i].index;
        let mut ev = self.event(EventKind::Stagnated);
        ev.step = Some(index);
        ev.attempt = Some(a.id);
        ev.agent = Some(implementer.name.clone());
        ev.message = Some(s.why.clone());
        self.publish(ev);
        let reason = format!("step {index} stagnated: {}", s.why);
        let escalation = match self.escalation_agent(i, implementer).await? {
            Ok(agent) => agent,
            Err(why) => {
                return self
                    .give_up(i, a, live, &format!("{reason}; no escalation: {why}"))
                    .await;
            }
        };
        self.close_quietly(live, index, a.id).await;
        let ended = format!("stagnated: {}", s.why);
        self.store()
            .end_attempt(a.id, AttemptStatus::Failed, &ended)
            .await?;
        self.attempt_ended(index, a, implementer, AttemptStatus::Failed, &ended);
        self.escalate(i, &escalation, &implementer.name, &reason, s)
            .await
    }

    /// The escalation's agent: a frontier agent (routed as `critical`) on a pool other
    /// than the implementer's. Why there is none: a pin, the budget, the router.
    async fn escalation_agent(
        &self,
        i: usize,
        implementer: &Agent,
    ) -> Result<Result<Agent, String>, Error> {
        if let Some(pin) = &self.task.agent_pin {
            return Ok(Err(format!("the task is pinned to {pin}")));
        }
        if !self.debate_left(self.steps[i].index).await? {
            return Ok(Err(self.budget_reason()));
        }
        let mut step = self.steps[i].clone();
        step.importance = Importance::Critical;
        let avoid = Avoid {
            runtimes: Vec::new(),
            pools: vec![implementer.pool.clone()],
        };
        Ok(self
            .e
            .cfg
            .router
            .route_independent(&self.task, &step, &avoid)
            .map_err(|no| no.reason))
    }

    /// The escalation attempt: one turn, verification once, commit or attention.
    async fn escalate(
        &mut self,
        i: usize,
        agent: &Agent,
        from: &str,
        reason: &str,
        stuck: Stagnation,
    ) -> Result<AttemptEnd, Error> {
        let index = self.steps[i].index;
        let a = self
            .store()
            .start_attempt(NewAttempt {
                task_id: self.task.id.clone(),
                step_index: index,
                role: AttemptRole::Implement,
                agent: agent.name.clone(),
                runtime: agent.runtime.clone(),
                model: agent.model.clone().unwrap_or_default(),
                pool: agent.pool.clone(),
            })
            .await?;
        let mut ev = self.event(EventKind::AttemptStarted);
        ev.step = Some(index);
        ev.attempt = Some(a.id);
        ev.agent = Some(agent.name.clone());
        ev.message = Some("escalation".into());
        self.publish(ev);
        let failed =
            |detail: String| format!("{reason}; the escalation to {} {detail}", agent.name);
        if !self.spend_debate_turn(index).await? {
            let why = failed(format!("did not run: {}", self.budget_reason()));
            return self.give_up(i, &a, None, &why).await;
        }
        let mut live = match self.open_session(&a, agent, None, None, 1).await {
            Ok(l) => Some(l),
            Err(e) if e.stopped() || self.shutdown.is_cancelled() => return Err(e),
            Err(e) => {
                let why = failed(format!("could not start: {e}"));
                return self.give_up(i, &a, None, &why).await;
            }
        };
        let msg = escalation_prompt(
            &self.task,
            &self.steps[i],
            self.total(),
            from,
            &stuck.why,
            &stuck.evidence,
        );
        let Some(l) = live.as_mut() else {
            return Err(Error::Session("the attempt has no session".into()));
        };
        let answer = match self
            .run_turn(l, &a, agent, TurnKind::Handoff, &msg, None)
            .await?
        {
            TurnEnd::Completed(r) => r.final_text,
            TurnEnd::Failed(f) => {
                let why = failed(format!("failed ({}): {}", f.kind.as_str(), f.message));
                return self.give_up(i, &a, live.take(), &why).await;
            }
            TurnEnd::Loop | TurnEnd::Stall(_) => {
                let why = failed("stopped making progress".to_owned());
                return self.give_up(i, &a, live.take(), &why).await;
            }
        };
        if let Some(o) = self.verify(i).await?
            && !o.passed
        {
            let why = failed(format!("did not resolve it: verification {}", o.summary()));
            return self.give_up(i, &a, live.take(), &why).await;
        }
        self.close_quietly(live, index, a.id).await;
        let claimed = review::fixed_claims(&answer);
        for (round, f) in stuck
            .findings
            .iter()
            .filter(|(_, f)| claimed.contains(&f.id))
        {
            self.store()
                .settle_findings(
                    &self.task.id,
                    index,
                    *round,
                    vec![(f.id.clone(), None)],
                    "fixed",
                )
                .await?;
        }
        self.commit(i, &a, agent).await
    }
}

/// The end of a long output, where test runners put their summary.
const EVIDENCE_TAIL: usize = 8 * 1024;

impl Run<'_> {
    /// Records a failed verification with the worktree's state; the
    /// stagnation when this is the same failure as the last or the change returned to an
    /// earlier state. Pipeline steps only: a `run` keeps §7.2's retries (D-040).
    pub(crate) async fn failure_stagnates(
        &self,
        o: &crate::verify::Outcome,
        p: &mut crate::breakers::Progress,
    ) -> Result<Option<Stagnation>, Error> {
        if self.task.kind != openchef_core::Kind::Pipeline {
            return Ok(None);
        }
        let wt = self.worktree()?;
        let cycle = p.state(wt.state().await?);
        let same = p.failed(o, &wt.path);
        Ok(same
            .or(cycle)
            .map(|why| Stagnation::plain(why, tail(&o.output, EVIDENCE_TAIL))))
    }
}

/// The last `max` bytes of `s`, cut at a character boundary.
fn tail(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s.get(start..).unwrap_or(s)
}
