//! The review → fix loop of a step (§7.5 phase 4, D-038): after verification passes,
//! a reviewer of another provider reads the diff; blockers and majors go back to the
//! implementer's open session; every fix is re-verified; the step is committed when
//! nothing blocking is open or the rounds are used up.

use openchef_core::{Attempt, AttemptRole, AttemptStatus, TurnKind};
use openchef_store::{FindingRow, NewAttempt};

use crate::engine::{Error, Run};
use crate::events::EventKind;
use crate::prompts::retry_prompt;
use crate::review::{self, Finding, Review, Severity};
use crate::router::Agent;
use crate::session::Live;
use crate::step::AttemptEnd;
use crate::turn::TurnEnd;

/// What one review round produced.
enum Round {
    /// The reviewer's findings (located-or-nit already applied).
    Reviewed {
        reviewer: Agent,
        attempt: i64,
        review: Review,
    },
    /// No review this round: no other provider, or an invalid answer twice.
    Skipped,
}

impl Run<'_> {
    /// Reviews step `i` after its verification passed, runs fix turns, then commits.
    pub(crate) async fn reviewed_commit(
        &mut self,
        i: usize,
        a: &Attempt,
        agent: &Agent,
        live: &mut Option<Live>,
        retries: &mut u32,
    ) -> Result<AttemptEnd, Error> {
        let importance = self.steps[i].importance.at_least(self.task.importance);
        // Review belongs to the pipeline (§7.5); a single-step `run` stays the fast path.
        let total = if self.task.kind == openchef_core::Kind::Pipeline {
            review::rounds(importance)
        } else {
            0
        };
        let index = self.steps[i].index;
        let mut open: Vec<Finding> = Vec::new();
        for round in 1..=total {
            let (reviewer, review_attempt, found) =
                match self.review_round(i, agent, round, &open).await? {
                    Round::Skipped => break,
                    Round::Reviewed {
                        reviewer,
                        attempt,
                        review,
                    } => (reviewer, attempt, review),
                };
            // Findings of the last round that are not reported again were fixed.
            let again: Vec<String> = found.findings.iter().map(|f| f.id.clone()).collect();
            let fixed: Vec<(String, Option<String>)> = open
                .iter()
                .filter(|f| !again.contains(&f.id))
                .map(|f| (f.id.clone(), None))
                .collect();
            if round > 1 {
                self.store()
                    .settle_findings(&self.task.id, index, round - 1, fixed, "fixed")
                    .await?;
            }
            self.record(i, round, review_attempt, &found).await?;
            let blocking: Vec<Finding> = found
                .findings
                .into_iter()
                .filter(|f| f.severity.blocking())
                .collect();
            if blocking.is_empty() {
                break;
            }
            let Some(l) = live.as_mut() else {
                return Err(Error::Session("the attempt has no session".into()));
            };
            let msg = review::fix_prompt(&reviewer.name, &blocking);
            let answer = match self
                .run_turn(l, a, agent, TurnKind::Retry, &msg, None)
                .await?
            {
                TurnEnd::Completed(r) => r.final_text,
                TurnEnd::Failed(_) | TurnEnd::Loop | TurnEnd::Stall(_) => {
                    // The fix turn did not finish: whatever is in the worktree must still
                    // pass verification before it is committed.
                    self.notice(
                        index,
                        Some(a.id),
                        "the fix turn did not complete; re-verifying",
                    );
                    String::new()
                }
            };
            let rebutted = review::rebuttals(&answer);
            let disputed: Vec<(String, Option<String>)> = rebutted
                .iter()
                .filter(|(id, _)| blocking.iter().any(|f| &f.id == id))
                .map(|(id, why)| (id.clone(), Some(why.clone())))
                .collect();
            if !disputed.is_empty() {
                self.store()
                    .settle_findings(&self.task.id, index, round, disputed.clone(), "disputed")
                    .await?;
            }
            open = blocking
                .into_iter()
                .filter(|f| !disputed.iter().any(|(id, _)| id == &f.id))
                .collect();
            if !self.reverify(i, a, agent, live, retries).await? {
                let reason = format!("step {index}: verification failed after review fixes");
                return self.give_up(i, a, live.take(), &reason).await;
            }
            if round == total {
                // No review is left to confirm the last fixes: take the implementer's word
                // for those it reports fixed; the rest stay open for the report.
                let claimed: Vec<(String, Option<String>)> = review::fixed_claims(&answer)
                    .into_iter()
                    .filter(|id| open.iter().any(|f| &f.id == id))
                    .map(|id| (id, None))
                    .collect();
                self.store()
                    .settle_findings(&self.task.id, index, round, claimed, "fixed")
                    .await?;
                break;
            }
            if open.is_empty() {
                break; // everything blocking was rebutted: nothing left to review
            }
        }
        self.commit(i, a, agent).await
    }

    /// Runs verification after a fix; failures get the usual retry turns (§7.2).
    /// `Ok(false)` when the retries are used up.
    async fn reverify(
        &mut self,
        i: usize,
        a: &Attempt,
        agent: &Agent,
        live: &mut Option<Live>,
        retries: &mut u32,
    ) -> Result<bool, Error> {
        loop {
            let outcome = match self.verify(i).await? {
                None => return Ok(true),
                Some(o) if o.passed => return Ok(true),
                Some(o) => o,
            };
            if !self
                .retry(
                    i,
                    a,
                    retries,
                    &format!("verification {} after a review fix", outcome.summary()),
                )
                .await?
            {
                return Ok(false);
            }
            let Some(l) = live.as_mut() else {
                return Err(Error::Session("the attempt has no session".into()));
            };
            let msg = retry_prompt(&self.task, &self.steps[i], self.total(), &outcome);
            if !matches!(
                self.run_turn(l, a, agent, TurnKind::Retry, &msg, None)
                    .await?,
                TurnEnd::Completed(_)
            ) {
                return Ok(false);
            }
        }
    }

    /// One review round: route a reviewer of another provider, ask for findings in
    /// `plan` mode, repair once.
    async fn review_round(
        &mut self,
        i: usize,
        implementer: &Agent,
        round: u32,
        open: &[Finding],
    ) -> Result<Round, Error> {
        let index = self.steps[i].index;
        let reviewer =
            match self
                .e
                .cfg
                .router
                .route_reviewer(&self.task, &self.steps[i], &implementer.runtime)
            {
                Ok(r) => r,
                Err(no) => {
                    self.review_finished(index, None, None, "skipped", &no.reason);
                    return Ok(Round::Skipped);
                }
            };
        let a = self
            .store()
            .start_attempt(NewAttempt {
                task_id: self.task.id.clone(),
                step_index: index,
                role: AttemptRole::Review,
                agent: reviewer.name.clone(),
                runtime: reviewer.runtime.clone(),
                model: reviewer.model.clone().unwrap_or_default(),
                pool: reviewer.pool.clone(),
            })
            .await?;
        let mut ev = self.event(EventKind::ReviewStarted);
        ev.step = Some(index);
        ev.attempt = Some(a.id);
        ev.agent = Some(reviewer.name.clone());
        ev.message = Some(format!("round {round}"));
        self.publish(ev);
        let diff = self.worktree()?.diff(review::MAX_DIFF + 1).await?;
        let prompt =
            review::reviewer_prompt(&self.steps[i], &review::bounded_diff(&diff), open, round);
        let mut live = match self.open_session(&a, &reviewer, None, None, 1).await {
            Ok(l) => Some(l),
            Err(e) if e.stopped() || self.shutdown.is_cancelled() => return Err(e),
            Err(e) => {
                let why = format!("reviewer {} could not start: {e}", reviewer.name);
                self.store()
                    .end_attempt(a.id, AttemptStatus::Failed, &why)
                    .await?;
                self.review_finished(index, Some(a.id), Some(&reviewer.name), "skipped", &why);
                return Ok(Round::Skipped);
            }
        };
        let answered = self
            .review_turns(index, &a, &reviewer, &prompt, &mut live)
            .await;
        self.close_quietly(live, index, a.id).await;
        match answered? {
            Ok(review) => {
                self.store()
                    .end_attempt(a.id, AttemptStatus::Succeeded, "reviewed")
                    .await?;
                self.report_review(index, a.id, &reviewer.name, &review);
                Ok(Round::Reviewed {
                    reviewer,
                    attempt: a.id,
                    review,
                })
            }
            Err(why) => {
                self.store()
                    .end_attempt(a.id, AttemptStatus::Failed, &why)
                    .await?;
                self.review_finished(index, Some(a.id), Some(&reviewer.name), "skipped", &why);
                Ok(Round::Skipped)
            }
        }
    }

    /// The review turn and its one repair turn. `Ok(Err(why))` when the review cannot be
    /// used; the step is never blocked by a reviewer that does not answer properly.
    async fn review_turns(
        &self,
        index: u32,
        a: &Attempt,
        reviewer: &Agent,
        prompt: &str,
        live: &mut Option<Live>,
    ) -> Result<Result<Review, String>, Error> {
        let schema = review::schema();
        let (mut kind, mut msg) = (TurnKind::Prompt, prompt.to_owned());
        for turn in 0..2 {
            let Some(l) = live.as_mut() else {
                return Err(Error::Session("the review has no session".into()));
            };
            let result = match self
                .run_turn(l, a, reviewer, kind, &msg, Some(&schema))
                .await?
            {
                TurnEnd::Completed(r) => r,
                TurnEnd::Failed(f) => {
                    if matches!(f.kind, openchef_runtime::FailureKind::QuotaExhausted)
                        && let Some(cb) = &self.e.cfg.pool_exhausted
                    {
                        cb(&reviewer.pool, f.resets_at);
                    }
                    return Ok(Err(format!(
                        "reviewer {} failed ({}): {}",
                        reviewer.name,
                        f.kind.as_str(),
                        f.message
                    )));
                }
                TurnEnd::Loop | TurnEnd::Stall(_) => {
                    return Ok(Err(format!(
                        "reviewer {} stopped making progress",
                        reviewer.name
                    )));
                }
            };
            match review::parse(result.structured.as_ref(), &result.final_text) {
                Ok(r) => return Ok(Ok(r)),
                Err(problems) if turn == 0 => {
                    self.notice(
                        index,
                        Some(a.id),
                        format!(
                            "the review is not valid; asking once to repair it: {}",
                            problems.join("; ")
                        ),
                    );
                    (kind, msg) = (TurnKind::Retry, review::repair_prompt(&problems));
                }
                Err(problems) => {
                    return Ok(Err(format!(
                        "the review is still not valid: {}",
                        problems.join("; ")
                    )));
                }
            }
        }
        Ok(Err("the review ended without an answer".into()))
    }

    /// Publishes a review's outcome: clean, or the first blocking findings.
    fn report_review(&self, index: u32, attempt: i64, reviewer: &str, review: &Review) {
        let blocking: Vec<&Finding> = review
            .findings
            .iter()
            .filter(|f| f.severity.blocking())
            .collect();
        let nits = review.findings.len() - blocking.len();
        if blocking.is_empty() {
            self.review_finished(
                index,
                Some(attempt),
                Some(reviewer),
                "clean",
                &format!("{nits} nit(s)"),
            );
            return;
        }
        let first: Vec<String> = blocking
            .iter()
            .take(3)
            .map(|f| format!("{} {}", f.severity.as_str(), f.claim))
            .collect();
        let msg = format!(
            "{} blocking, {nits} nit(s): {}",
            blocking.len(),
            first.join("; ")
        );
        self.review_finished(index, Some(attempt), Some(reviewer), "findings", &msg);
    }

    /// Stores a round's findings: blockers and majors open, nits as nits.
    async fn record(&self, i: usize, round: u32, attempt: i64, r: &Review) -> Result<(), Error> {
        let rows = r
            .findings
            .iter()
            .map(|f| FindingRow {
                id: 0,
                task_id: self.task.id.clone(),
                step_index: self.steps[i].index,
                round,
                attempt_id: Some(attempt),
                finding: f.id.clone(),
                severity: f.severity.as_str().to_owned(),
                file: f.file.clone(),
                line: f.line,
                contract: f.contract.clone(),
                claim: f.claim.clone(),
                evidence: f.evidence.clone(),
                fix: f.fix.clone(),
                status: if f.severity == Severity::Nit {
                    "nit"
                } else {
                    "open"
                }
                .to_owned(),
                note: None,
            })
            .collect();
        Ok(self.store().record_findings(rows).await?)
    }

    fn review_finished(
        &self,
        step: u32,
        attempt: Option<i64>,
        agent: Option<&str>,
        status: &str,
        message: &str,
    ) {
        let mut ev = self.event(EventKind::ReviewFinished);
        ev.step = Some(step);
        ev.attempt = attempt;
        ev.agent = agent.map(str::to_owned);
        ev.status = Some(status.to_owned());
        ev.message = Some(message.to_owned());
        self.publish(ev);
    }
}
