//! The review → fix loop of a step (§7.5 phase 4, D-038): after verification passes,
//! a reviewer of another provider reads the diff; blockers and majors go back to the
//! implementer's open session; every fix is re-verified; the step is committed when
//! nothing blocking is open or the rounds are used up.

use openchef_core::{Attempt, AttemptRole, AttemptStatus, TurnKind};
use openchef_runtime::{Failure, FailureKind};
use openchef_store::{FindingRow, NewAttempt};

use crate::arbiter::Dispute;
use crate::arbitrating::Ruled;
use crate::breakers::Progress;
use crate::debate::Level;
use crate::engine::{Error, Run};
use crate::escalating::Stagnation;
use crate::events::EventKind;
use crate::prompts::retry_prompt;
use crate::review::{self, Finding, Review, Severity};
use crate::router::{Agent, Avoid, NoAgent};
use crate::session::Live;
use crate::step::AttemptEnd;
use crate::turn::TurnEnd;

/// Validates a judging agent's answer: its structured output, else its final text.
pub(crate) type Parse<'p, T> =
    &'p (dyn Fn(Option<&serde_json::Value>, &str) -> Result<T, Vec<String>> + Sync);

/// How a judging agent's answer is asked for and checked.
pub(crate) struct Judge<'p, T> {
    /// What the answer is called in notices (`review`, `verdict`).
    pub what: &'static str,
    /// The structured output's JSON Schema.
    pub schema: serde_json::Value,
    /// Validates an answer (structured output, else the final text).
    pub parse: Parse<'p, T>,
    /// The repair turn's instructions for the problems found.
    pub repair: fn(&[String]) -> String,
}

/// What one review round produced.
enum Round {
    /// The reviewer's findings (located-or-nit already applied).
    Reviewed {
        reviewer: Agent,
        attempt: i64,
        review: Review,
    },
    /// No review this round: no other provider, quota, the budget, or an invalid
    /// answer twice.
    Skipped,
}

/// How a fix turn went.
pub(crate) enum Fix {
    /// The implementer answered (or the turn ended otherwise): its answer and the
    /// findings it rebutted.
    Answered(String, Vec<Dispute>),
    /// The debate budget is used up: the turn did not run, the findings stay open.
    Unbudgeted,
    /// The implementer's pool ran out: fail over like an implement turn (§7.2).
    Exhausted(Failure),
}

/// How the verification after a fix went.
pub(crate) enum Reverified {
    /// It passes (or the step has none).
    Passed,
    /// It still fails with the retries used up.
    Exhausted,
    /// It keeps failing the same way, or the change went back to an earlier state.
    Stagnated(Stagnation),
}

/// What earlier attempts at this step left of its review (D-040): it continues after
/// the last recorded round instead of repeating rounds already paid for.
#[derive(Default)]
struct Earlier {
    last_round: u32,
    /// The last round's open blocking findings.
    open: Vec<Finding>,
    /// Disputed findings by round, for the arbiter.
    raised: Vec<(u32, Dispute)>,
    /// Findings settled `fixed`.
    fixed: Vec<Finding>,
    /// The reviewers' runtimes and pools.
    reviewers: Vec<(String, String)>,
}

impl Run<'_> {
    /// Reviews step `i` after its verification passed, runs fix turns, then commits.
    #[allow(
        clippy::too_many_lines,
        reason = "the review loop of D-038 with its breakers in one place"
    )]
    pub(crate) async fn reviewed_commit(
        &mut self,
        i: usize,
        a: &Attempt,
        agent: &Agent,
        live: &mut Option<Live>,
        progress: &mut Progress,
    ) -> Result<AttemptEnd, Error> {
        // Review belongs to the pipeline (§7.5); a single-step `run` stays the fast path.
        if self.task.kind != openchef_core::Kind::Pipeline {
            return self.commit(i, a, agent).await;
        }
        let index = self.steps[i].index;
        let mut level = Level::new(self.steps[i].importance.at_least(self.task.importance));
        self.lower_for(&mut level, index, &agent.pool);
        let earlier = self.earlier_review(index).await?;
        progress.fixed(&earlier.fixed);
        let mut open = earlier.open;
        // Rebutted findings by round, for the arbiter; and what the arbiter must not share
        // with the work it judges: the implementer and every reviewer.
        let mut raised = earlier.raised;
        let mut judged = Avoid {
            runtimes: vec![agent.runtime.clone()],
            pools: vec![agent.pool.clone()],
        };
        for (runtime, pool) in earlier.reviewers {
            judged.runtimes.push(runtime);
            judged.pools.push(pool);
        }
        let mut round = earlier.last_round;
        while round < self.depth(level).review_rounds {
            round += 1;
            // The content under review: a fix that undid an earlier one is a cycle.
            if let Some(why) = progress.state(self.worktree()?.state().await?) {
                let s = Stagnation::plain(why, String::new());
                return self.stagnated(i, a, agent, live.take(), s).await;
            }
            let (reviewer, review_attempt, found) = match self
                .review_round(i, agent, round, &open, &mut level)
                .await?
            {
                Round::Skipped => break,
                Round::Reviewed {
                    reviewer,
                    attempt,
                    review,
                } => (reviewer, attempt, review),
            };
            judged.runtimes.push(reviewer.runtime.clone());
            judged.pools.push(reviewer.pool.clone());
            // Findings of the last round that are not reported again were fixed.
            let again: Vec<String> = found.findings.iter().map(|f| f.id.clone()).collect();
            let fixed: Vec<&Finding> = open.iter().filter(|f| !again.contains(&f.id)).collect();
            if round > 1 && !fixed.is_empty() {
                let ids = fixed.iter().map(|f| (f.id.clone(), None)).collect();
                self.store()
                    .settle_findings(&self.task.id, index, round - 1, ids, "fixed")
                    .await?;
            }
            progress.fixed(fixed);
            self.record(i, round, review_attempt, &found).await?;
            let blocking: Vec<Finding> = found
                .findings
                .into_iter()
                .filter(|f| f.severity.blocking())
                .collect();
            let reopened = progress.reopened(&blocking);
            if !reopened.is_empty() {
                let s = reopened_stagnation(round, &reopened);
                return self.stagnated(i, a, agent, live.take(), s).await;
            }
            if blocking.is_empty() {
                break;
            }
            let (answer, rebutted) = match self
                .fix_turn(i, a, agent, live, &reviewer.name, round, &blocking)
                .await?
            {
                Fix::Answered(answer, rebutted) => (answer, rebutted),
                Fix::Unbudgeted => break, // the findings stay open for the report
                Fix::Exhausted(f) => return self.exhausted(i, a, live.take(), agent, &f).await,
            };
            open = blocking
                .into_iter()
                .filter(|f| !rebutted.iter().any(|d| d.finding.id == f.id))
                .collect();
            raised.extend(rebutted.into_iter().map(|d| (round, d)));
            match self.reverify(i, a, agent, live, progress).await? {
                Reverified::Passed => {}
                Reverified::Exhausted => {
                    let reason = format!("step {index}: verification failed after review fixes");
                    return self.give_up(i, a, live.take(), &reason).await;
                }
                Reverified::Stagnated(s) => {
                    return self.stagnated(i, a, agent, live.take(), s).await;
                }
            }
            if round >= self.depth(level).review_rounds {
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
        if !raised.is_empty() {
            match self
                .arbitrate(i, a, agent, live, progress, &raised, &judged)
                .await?
            {
                Ruled::Done => {}
                Ruled::Failed => {
                    let reason =
                        format!("step {index}: verification failed after the arbiter's fixes");
                    return self.give_up(i, a, live.take(), &reason).await;
                }
                Ruled::Stagnated(s) => return self.stagnated(i, a, agent, live.take(), s).await,
                Ruled::Exhausted(f) => return self.exhausted(i, a, live.take(), agent, &f).await,
            }
        }
        self.commit(i, a, agent).await
    }

    /// What earlier attempts at step `index` left of its review.
    async fn earlier_review(&self, index: u32) -> Result<Earlier, Error> {
        let rows: Vec<FindingRow> = self
            .store()
            .findings(&self.task.id)
            .await?
            .into_iter()
            .filter(|r| r.step_index == index)
            .collect();
        let attempts = self.store().attempts(&self.task.id).await?;
        let reviewer_of = |id: Option<i64>| {
            attempts
                .iter()
                .find(|a| Some(a.id) == id)
                .map_or_else(|| "an earlier reviewer".to_owned(), |a| a.agent.clone())
        };
        let mut e = Earlier {
            last_round: rows.iter().map(|r| r.round).max().unwrap_or(0),
            reviewers: attempts
                .iter()
                .filter(|a| a.step_index == index && a.role == AttemptRole::Review)
                .map(|a| (a.runtime.clone(), a.pool.clone()))
                .collect(),
            ..Earlier::default()
        };
        for r in &rows {
            let Some(f) = stored_finding(r) else {
                continue;
            };
            match r.status.as_str() {
                "open" if r.round == e.last_round && f.severity.blocking() => e.open.push(f),
                "disputed" => e.raised.push((
                    r.round,
                    Dispute {
                        finding: f,
                        reviewer: reviewer_of(r.attempt_id),
                        rebuttal: r.note.clone().unwrap_or_default(),
                    },
                )),
                "fixed" => e.fixed.push(f),
                _ => {} // open in an earlier round, dismissed, nits: nothing to carry on
            }
        }
        Ok(e)
    }

    /// The implementer's fix turn for `blocking`: records the findings it rebuts as
    /// `disputed` and returns its answer with those disputes.
    #[allow(clippy::too_many_arguments, reason = "the step's attempt context")]
    async fn fix_turn(
        &self,
        i: usize,
        a: &Attempt,
        agent: &Agent,
        live: &mut Option<Live>,
        reviewer: &str,
        round: u32,
        blocking: &[Finding],
    ) -> Result<Fix, Error> {
        let index = self.steps[i].index;
        if !self.spend_debate_turn(index).await? {
            return Ok(Fix::Unbudgeted);
        }
        let Some(l) = live.as_mut() else {
            return Err(Error::Session("the attempt has no session".into()));
        };
        let msg = review::fix_prompt(reviewer, blocking);
        let answer = match self
            .run_turn(l, a, agent, TurnKind::Retry, &msg, None)
            .await?
        {
            TurnEnd::Completed(r) => r.final_text,
            TurnEnd::Failed(f) if out_of_quota(&f) => return Ok(Fix::Exhausted(f)),
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
        let rebutted: Vec<Dispute> = review::rebuttals(&answer)
            .into_iter()
            .filter_map(|(id, why)| {
                let finding = blocking.iter().find(|f| f.id == id)?.clone();
                Some(Dispute {
                    finding,
                    reviewer: reviewer.to_owned(),
                    rebuttal: why,
                })
            })
            .collect();
        if !rebutted.is_empty() {
            let ids = rebutted
                .iter()
                .map(|d| (d.finding.id.clone(), Some(d.rebuttal.clone())))
                .collect();
            self.store()
                .settle_findings(&self.task.id, index, round, ids, "disputed")
                .await?;
        }
        Ok(Fix::Answered(answer, rebutted))
    }

    /// Runs verification after a fix; failures get the usual retry turns (§7.2) until
    /// they stagnate.
    pub(crate) async fn reverify(
        &mut self,
        i: usize,
        a: &Attempt,
        agent: &Agent,
        live: &mut Option<Live>,
        progress: &mut Progress,
    ) -> Result<Reverified, Error> {
        loop {
            let outcome = match self.verify(i).await? {
                None => return Ok(Reverified::Passed),
                Some(o) if o.passed => {
                    progress.passed();
                    return Ok(Reverified::Passed);
                }
                Some(o) => o,
            };
            if let Some(s) = self.failure_stagnates(&outcome, progress).await? {
                return Ok(Reverified::Stagnated(s));
            }
            if !self
                .retry(
                    i,
                    a,
                    &mut progress.retries,
                    &format!("verification {} after a review fix", outcome.summary()),
                )
                .await?
            {
                return Ok(Reverified::Exhausted);
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
                return Ok(Reverified::Exhausted);
            }
        }
    }

    /// One review round: route a reviewer of another provider, ask for findings in
    /// `plan` mode, repair once. The reviewer's pool may lower `level`.
    async fn review_round(
        &mut self,
        i: usize,
        implementer: &Agent,
        round: u32,
        open: &[Finding],
        level: &mut Level,
    ) -> Result<Round, Error> {
        let index = self.steps[i].index;
        if !self.debate_left(index).await? {
            self.review_finished(index, None, None, "skipped", &self.budget_reason());
            return Ok(Round::Skipped);
        }
        let reviewer = match self.e.cfg.router.route_independent(
            &self.task,
            &self.steps[i],
            &Avoid::runtimes([implementer.runtime.as_str()]),
        ) {
            Ok(r) => r,
            Err(no) => {
                self.review_finished(index, None, None, "skipped", &skip_reason(&no));
                return Ok(Round::Skipped);
            }
        };
        if self.lower_for(level, index, &reviewer.pool) && round > self.depth(*level).review_rounds
        {
            let why = format!(
                "the lowered debate has {} round(s)",
                self.depth(*level).review_rounds
            );
            self.review_finished(index, None, None, "skipped", &why);
            return Ok(Round::Skipped);
        }
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
    pub(crate) async fn review_turns(
        &self,
        index: u32,
        a: &Attempt,
        reviewer: &Agent,
        prompt: &str,
        live: &mut Option<Live>,
    ) -> Result<Result<Review, String>, Error> {
        let judge = Judge {
            what: "review",
            schema: review::schema(),
            parse: &review::parse,
            repair: review::repair_prompt,
        };
        self.judge_turns(index, a, reviewer, prompt, live, &judge)
            .await
    }

    /// A judging agent's turn (reviewer, critic, arbiter) and its one repair turn, for
    /// structured output validated by `judge.parse`. `Ok(Err(why))` when the answer
    /// cannot be used; a judge never blocks the work it judges.
    pub(crate) async fn judge_turns<T>(
        &self,
        index: u32,
        a: &Attempt,
        judge_agent: &Agent,
        prompt: &str,
        live: &mut Option<Live>,
        judge: &Judge<'_, T>,
    ) -> Result<Result<T, String>, Error> {
        let (mut kind, mut msg) = (TurnKind::Prompt, prompt.to_owned());
        for turn in 0..2 {
            if !self.spend_debate_turn(index).await? {
                return Ok(Err(self.budget_reason()));
            }
            let Some(l) = live.as_mut() else {
                return Err(Error::Session(format!("the {} has no session", judge.what)));
            };
            let result = match self
                .run_turn(l, a, judge_agent, kind, &msg, Some(&judge.schema))
                .await?
            {
                TurnEnd::Completed(r) => r,
                TurnEnd::Failed(f) => {
                    let quota = out_of_quota(&f);
                    if quota && let Some(cb) = &self.e.cfg.pool_exhausted {
                        cb(&judge_agent.pool, f.resets_at);
                    }
                    let why = format!(
                        "{} {} failed ({}): {}",
                        judge.what,
                        judge_agent.name,
                        f.kind.as_str(),
                        f.message
                    );
                    return Ok(Err(if quota {
                        format!("skipped for quota: {why}")
                    } else {
                        why
                    }));
                }
                TurnEnd::Loop | TurnEnd::Stall(_) => {
                    return Ok(Err(format!(
                        "{} {} stopped making progress",
                        judge.what, judge_agent.name
                    )));
                }
            };
            match (judge.parse)(result.structured.as_ref(), &result.final_text) {
                Ok(r) => return Ok(Ok(r)),
                Err(problems) if turn == 0 => {
                    self.notice(
                        index,
                        Some(a.id),
                        format!(
                            "the {} is not valid; asking once to repair it: {}",
                            judge.what,
                            problems.join("; ")
                        ),
                    );
                    (kind, msg) = (TurnKind::Retry, (judge.repair)(&problems));
                }
                Err(problems) => {
                    return Ok(Err(format!(
                        "the {} is still not valid: {}",
                        judge.what,
                        problems.join("; ")
                    )));
                }
            }
        }
        Ok(Err(format!("the {} ended without an answer", judge.what)))
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
        let rows = finding_rows(
            &self.task.id,
            self.steps[i].index,
            round,
            attempt,
            &r.findings,
            None,
        );
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

/// A round's findings as store rows: blockers and majors open, nits as nits. `prefix`
/// keeps the ids of several critics in one round apart (`gpt/F1`).
pub(crate) fn finding_rows(
    task: &openchef_core::TaskId,
    step: u32,
    round: u32,
    attempt: i64,
    findings: &[Finding],
    prefix: Option<&str>,
) -> Vec<FindingRow> {
    findings
        .iter()
        .map(|f| FindingRow {
            id: 0,
            task_id: task.clone(),
            step_index: step,
            round,
            attempt_id: Some(attempt),
            finding: prefix.map_or_else(|| f.id.clone(), |p| format!("{p}/{}", f.id)),
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
        .collect()
}

/// Whether a turn failed because its pool ran out (or keeps rate limiting).
pub(crate) const fn out_of_quota(f: &Failure) -> bool {
    matches!(
        f.kind,
        FailureKind::QuotaExhausted | FailureKind::RateLimited
    )
}

/// Why no judge was routed: quota or none independent.
pub(crate) fn skip_reason(no: &NoAgent) -> String {
    if no.quota {
        format!("skipped for quota: {}", no.reason)
    } else {
        no.reason.clone()
    }
}

/// A stored finding back in the review's vocabulary; `None` for a row it never wrote.
fn stored_finding(r: &FindingRow) -> Option<Finding> {
    let severity = match r.severity.as_str() {
        "blocker" => Severity::Blocker,
        "major" => Severity::Major,
        "nit" => Severity::Nit,
        _ => return None,
    };
    Some(Finding {
        id: r.finding.clone(),
        severity,
        file: r.file.clone(),
        line: r.line,
        contract: r.contract.clone(),
        claim: r.claim.clone(),
        evidence: r.evidence.clone(),
        fix: r.fix.clone(),
    })
}

/// The stagnation of findings reported again after they were fixed.
fn reopened_stagnation(round: u32, reopened: &[&Finding]) -> Stagnation {
    let ids: Vec<&str> = reopened.iter().map(|f| f.id.as_str()).collect();
    let evidence: Vec<String> = reopened
        .iter()
        .map(|f| {
            format!(
                "{} [{}] {}: {}\n  evidence: {}",
                f.id,
                f.severity.as_str(),
                f.file.as_deref().unwrap_or("unlocated"),
                f.claim,
                f.evidence
            )
        })
        .collect();
    Stagnation {
        why: format!("finding(s) {} reported again after a fix", ids.join(", ")),
        evidence: evidence.join("\n"),
        findings: reopened.iter().map(|f| (round, (*f).clone())).collect(),
    }
}
