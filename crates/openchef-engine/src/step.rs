//! Working on one step: attempts, retries, failover, and picking up an attempt a
//! previous daemon left running (§5.3, §7.2).

use std::time::Duration;

use openchef_core::{
    Attempt, AttemptStatus, Backend, SessionState, StepStatus, TaskStatus, TurnKind, TurnStatus,
};
use openchef_gitws::{CONTROL_DIR, StepCommit};
use openchef_runtime::{Failure, FailureKind, SessionRef};
use openchef_store::{NewAttempt, StepChange};
use time::OffsetDateTime;

use crate::engine::{Error, Run};
use crate::events::EventKind;
use crate::prompts::{
    handoff_prompt, loop_prompt, recovery_prompt, resume_prompt, retry_prompt, stall_prompt,
    step_prompt,
};
use crate::router::{Agent, NoAgent};
use crate::session::Live;
use crate::turn::TurnEnd;
use crate::verify::{self, Outcome};

/// What the step loop does after an attempt.
enum AttemptEnd {
    /// Next step.
    Verified,
    /// Route to another pool and try again.
    Failover {
        pool: String,
        resets_at: Option<OffsetDateTime>,
        from: String,
        patch: Option<String>,
    },
    /// The task left running (`needs_attention`, `waiting_quota`).
    Parked,
    /// A carried-over attempt could not resume: start afresh.
    Restart,
}

/// An attempt left running by a daemon that stopped mid-step (§5.3).
struct CarryOver {
    attempt: Attempt,
    agent: Agent,
    reference: SessionRef,
    /// The closed session row whose conversation continues.
    from: i64,
    seq: u32,
}

impl Run<'_> {
    fn total(&self) -> usize {
        self.steps.len()
    }

    /// Works on step `i` until it is verified (`true`) or the task is parked (`false`).
    pub(crate) async fn run_step(&mut self, i: usize) -> Result<bool, Error> {
        let index = self.steps[i].index;
        let left = self.running_attempt(index).await?;
        // A step commit is only made after verification passed, so a commit for a step
        // that is not verified yet means a crash came between the two: finish it.
        if self.adopt_commit(i, left.as_ref()).await? {
            return Ok(true);
        }
        if self.steps[i].status == StepStatus::Failed {
            self.step_to(i, StepStatus::Pending, StepChange::default())
                .await?;
        }
        if self.steps[i].status == StepStatus::Pending {
            self.step_to(i, StepStatus::Running, StepChange::default())
                .await?;
            let mut ev = self.event(EventKind::StepStarted);
            ev.step = Some(index);
            ev.message = Some(self.steps[i].title.clone());
            self.publish(ev);
        }
        let mut exclude: Vec<String> = Vec::new();
        let mut retries = 0_u32; // feedback turns on this step, across attempts
        let mut earliest_reset: Option<OffsetDateTime> = None;
        let (mut kind, mut msg) = (
            TurnKind::Prompt,
            step_prompt(&self.task, &self.steps[i], self.total()),
        );
        let mut carry = None;
        if let Some(a) = left {
            let (c, k, m) = self.carry_over(i, a).await?;
            (carry, kind, msg) = (c, k, m);
        }
        loop {
            let agent = match &carry {
                Some(c) => c.agent.clone(),
                None => match self
                    .e
                    .cfg
                    .router
                    .route(&self.task, &self.steps[i], &exclude)
                {
                    Ok(a) => a,
                    Err(no) => {
                        self.no_agent(no, earliest_reset).await?;
                        return Ok(false);
                    }
                },
            };
            match self
                .attempt(i, &agent, kind, &msg, &mut retries, carry.take())
                .await?
            {
                AttemptEnd::Verified => return Ok(true),
                AttemptEnd::Parked => return Ok(false),
                AttemptEnd::Restart => {
                    (kind, msg) = (
                        TurnKind::Prompt,
                        step_prompt(&self.task, &self.steps[i], self.total()),
                    );
                }
                AttemptEnd::Failover {
                    pool,
                    resets_at,
                    from,
                    patch,
                } => {
                    exclude.push(pool.clone());
                    if let Some(r) = resets_at {
                        earliest_reset = Some(earliest_reset.map_or(r, |e| e.min(r)));
                    }
                    kind = TurnKind::Handoff;
                    msg = handoff_prompt(
                        &self.task,
                        &self.steps[i],
                        self.total(),
                        &from,
                        &self.last_verified(i),
                        patch.as_deref(),
                    );
                    self.notice(
                        index,
                        None,
                        format!("{from} ran out of quota on pool {pool}; failing over"),
                    );
                }
            }
        }
    }

    /// The attempt at a step still recorded as running (left by a daemon that stopped).
    async fn running_attempt(&self, step: u32) -> Result<Option<Attempt>, Error> {
        let attempts = self.store().attempts(&self.task.id).await?;
        Ok(attempts
            .into_iter()
            .find(|a| a.step_index == step && a.status == AttemptStatus::Running))
    }

    /// Stops whatever an earlier daemon left of an attempt: kills a process that is
    /// somehow still alive, closes its sessions and ends turns left in flight. Returns
    /// the last session and the message of the interrupted turn, if any.
    async fn settle(
        &self,
        a: &Attempt,
    ) -> Result<(Option<openchef_core::Session>, Option<String>), Error> {
        let sessions = self.store().sessions(a.id).await?;
        let mut interrupted = None;
        for s in &sessions {
            if s.state != SessionState::Closed {
                crate::recover::kill_leftover(s)?;
                self.store().close_session(s.id).await?;
            }
            match self.store().in_flight_turn(s.id).await {
                Ok(turn) => {
                    interrupted = Some(turn.message.clone());
                    self.store()
                        .end_turn(turn.id, TurnStatus::Interrupted, "daemon restart")
                        .await?;
                }
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok((sessions.into_iter().last(), interrupted))
    }

    /// Prepares to continue an attempt left running by a previous daemon: its
    /// conversation resumes with the recovery note and the interrupted message. An
    /// attempt whose session never reported a native id cannot resume; it ends and the
    /// step starts a fresh attempt.
    async fn carry_over(
        &self,
        i: usize,
        a: Attempt,
    ) -> Result<(Option<CarryOver>, TurnKind, String), Error> {
        let fresh = step_prompt(&self.task, &self.steps[i], self.total());
        let (last, interrupted) = self.settle(&a).await?;
        let Some((last, native)) = last.and_then(|s| s.native_id.clone().map(|n| (s, n))) else {
            self.store()
                .end_attempt(
                    a.id,
                    AttemptStatus::Interrupted,
                    "daemon restart before the session started",
                )
                .await?;
            return Ok((None, TurnKind::Prompt, fresh));
        };
        let agent = self
            .e
            .cfg
            .router
            .agent_named(&a.agent)
            .unwrap_or_else(|| Agent {
                name: a.agent.clone(),
                runtime: a.runtime.clone(),
                model: (!a.model.is_empty()).then(|| a.model.clone()),
                pool: a.pool.clone(),
                effort: None,
            });
        let seq = u32::try_from(self.store().sessions(a.id).await?.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let mut ev = self.event(EventKind::Notice);
        ev.step = Some(self.steps[i].index);
        ev.attempt = Some(a.id);
        ev.agent = Some(agent.name.clone());
        ev.message = Some("resuming the session interrupted by a daemon restart".into());
        self.publish(ev);
        let prompt = recovery_prompt(&self.steps[i], interrupted.as_deref().unwrap_or(&fresh));
        let reference = SessionRef {
            runtime: last.runtime.clone(),
            id: native,
        };
        Ok((
            Some(CarryOver {
                attempt: a,
                agent,
                reference,
                from: last.id,
                seq,
            }),
            TurnKind::Recovery,
            prompt,
        ))
    }

    /// Marks step `i` verified if the branch already has its commit.
    async fn adopt_commit(&mut self, i: usize, left: Option<&Attempt>) -> Result<bool, Error> {
        let index = self.steps[i].index;
        let Some(sha) = self
            .worktree()?
            .find_step_commit(self.base_commit(), self.task.id.as_str(), index)
            .await?
        else {
            return Ok(false);
        };
        if self.steps[i].status != StepStatus::Running {
            if self.steps[i].status == StepStatus::Failed {
                self.step_to(i, StepStatus::Pending, StepChange::default())
                    .await?;
            }
            self.step_to(i, StepStatus::Running, StepChange::default())
                .await?;
        }
        if let Some(a) = left {
            drop(self.settle(a).await?);
        }
        self.step_to(
            i,
            StepStatus::Verified,
            StepChange {
                verified_commit: Some(sha.clone()),
                count_retry: false,
            },
        )
        .await?;
        if let Some(a) = left {
            self.store()
                .end_attempt(a.id, AttemptStatus::Succeeded, "verified before a restart")
                .await?;
        }
        let mut ev = self.event(EventKind::StepFinished);
        ev.step = Some(index);
        ev.status = Some(StepStatus::Verified.to_string());
        ev.message = Some(format!("{sha} (found after a restart)"));
        self.publish(ev);
        Ok(true)
    }

    /// Parks the task: `waiting_quota` when a pool resets at a known time,
    /// `needs_attention` otherwise.
    async fn no_agent(
        &mut self,
        no: NoAgent,
        earliest_reset: Option<OffsetDateTime>,
    ) -> Result<(), Error> {
        match no.resume_at.or(earliest_reset) {
            Some(at) => {
                self.park(TaskStatus::WaitingQuota, &no.to_string(), Some(at))
                    .await
            }
            None => {
                self.park(TaskStatus::NeedsAttention, &no.to_string(), None)
                    .await
            }
        }
    }

    async fn step_to(&mut self, i: usize, to: StepStatus, change: StepChange) -> Result<(), Error> {
        let s = &self.steps[i];
        self.steps[i] = self
            .store()
            .transition_step(&self.task.id, s.index, s.status, to, change)
            .await?;
        Ok(())
    }

    /// One agent's work on step `i`: one session (resumed once after a crash) and as
    /// many turns as the retry budget allows.
    async fn attempt(
        &mut self,
        i: usize,
        agent: &Agent,
        kind: TurnKind,
        msg: &str,
        retries: &mut u32,
        carry: Option<CarryOver>,
    ) -> Result<AttemptEnd, Error> {
        let index = self.steps[i].index;
        let (a, seq, live) = if let Some(c) = carry {
            match self
                .open_session(
                    &c.attempt,
                    agent,
                    Some(c.reference.clone()),
                    Some(c.from),
                    c.seq,
                )
                .await
            {
                Ok(live) => (c.attempt, c.seq, live),
                Err(e) if e.stopped() || self.shutdown.is_cancelled() => return Err(e),
                Err(e) => {
                    let reason = format!("resume after restart failed: {e}");
                    self.store()
                        .end_attempt(c.attempt.id, AttemptStatus::Interrupted, &reason)
                        .await?;
                    self.attempt_ended(
                        index,
                        &c.attempt,
                        agent,
                        AttemptStatus::Interrupted,
                        &reason,
                    );
                    return Ok(AttemptEnd::Restart);
                }
            }
        } else {
            let a = self
                .store()
                .start_attempt(NewAttempt {
                    task_id: self.task.id.clone(),
                    step_index: index,
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
            self.publish(ev);
            match self.open_session(&a, agent, None, None, 1).await {
                Ok(live) => (a, 1, live),
                Err(e) if e.stopped() || self.shutdown.is_cancelled() => return Err(e),
                Err(e) => {
                    return self
                        .give_up(
                            i,
                            &a,
                            None,
                            &format!("agent {} could not start: {e}", agent.name),
                        )
                        .await;
                }
            }
        };
        let mut live = Some(live);
        let result = self
            .attempt_turns(i, &a, agent, kind, msg, retries, seq, &mut live)
            .await;
        self.close_quietly(live, index, a.id).await;
        result
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::many_single_char_names,
        reason = "the attempt loop of §7.2 in one place"
    )]
    async fn attempt_turns(
        &mut self,
        i: usize,
        a: &Attempt,
        agent: &Agent,
        kind: TurnKind,
        msg: &str,
        retries: &mut u32,
        mut seq: u32,
        live: &mut Option<Live>,
    ) -> Result<AttemptEnd, Error> {
        let lim = self.e.cfg.limits;
        let index = self.steps[i].index;
        let (mut kind, mut msg) = (kind, msg.to_owned());
        let (mut rate_limits, mut resumed) = (0_u32, false);
        loop {
            let Some(l) = live.as_mut() else {
                return Err(Error::Session("the attempt has no session".into()));
            };
            match self.run_turn(l, a, agent, kind, &msg).await? {
                TurnEnd::Completed => {
                    let outcome = self.verify(i).await?;
                    match outcome {
                        None => return self.commit(i, a, agent).await,
                        Some(o) if o.passed => return self.commit(i, a, agent).await,
                        Some(o) => {
                            if !self
                                .retry(i, a, retries, &format!("verification {}", o.summary()))
                                .await?
                            {
                                let reason = format!(
                                    "step {index}: verification {} after {} retries",
                                    o.summary(),
                                    lim.max_retries
                                );
                                return self.give_up(i, a, live.take(), &reason).await;
                            }
                            (kind, msg) = (
                                TurnKind::Retry,
                                retry_prompt(&self.task, &self.steps[i], self.total(), &o),
                            );
                        }
                    }
                }
                end @ (TurnEnd::Loop | TurnEnd::Stall(_)) => {
                    let (why, next) = match &end {
                        TurnEnd::Loop => (
                            format!("the same tool call was repeated {} times", lim.loop_repeats),
                            loop_prompt(&self.task, &self.steps[i], self.total(), lim.loop_repeats),
                        ),
                        TurnEnd::Stall(why) => (
                            why.clone(),
                            stall_prompt(&self.task, &self.steps[i], self.total(), why),
                        ),
                        TurnEnd::Completed | TurnEnd::Failed(_) => (String::new(), String::new()),
                    };
                    if !self.retry(i, a, retries, &why).await? {
                        return self
                            .give_up(
                                i,
                                a,
                                live.take(),
                                &format!("step {index}: {why}, retries exhausted"),
                            )
                            .await;
                    }
                    (kind, msg) = (TurnKind::Nudge, next);
                }
                TurnEnd::Failed(f) => match f.kind {
                    FailureKind::RateLimited if rate_limits < lim.rate_limit_retries => {
                        rate_limits += 1;
                        self.backoff(index, a, &f, rate_limits).await?;
                    }
                    FailureKind::RateLimited | FailureKind::QuotaExhausted => {
                        return self.exhausted(i, a, live.take(), agent, &f).await;
                    }
                    FailureKind::Crashed
                    | FailureKind::Protocol
                    | FailureKind::Interrupted
                    | FailureKind::Timeout => {
                        if resumed {
                            let reason = format!(
                                "agent {} failed twice ({}): {}",
                                agent.name,
                                f.kind.as_str(),
                                f.message
                            );
                            return self.give_up(i, a, live.take(), &reason).await;
                        }
                        resumed = true;
                        seq += 1;
                        let Some(old) = live.take() else {
                            return Err(Error::Session("the attempt has no session".into()));
                        };
                        match self.resume(a, agent, old, seq).await {
                            Ok(l) => *live = Some(l),
                            Err(e) if e.stopped() || self.shutdown.is_cancelled() => return Err(e),
                            Err(e) => {
                                return self
                                    .give_up(
                                        i,
                                        a,
                                        None,
                                        &format!("agent {} could not resume: {e}", agent.name),
                                    )
                                    .await;
                            }
                        }
                        (kind, msg) = (
                            TurnKind::Recovery,
                            resume_prompt(&self.task, &self.steps[i], self.total()),
                        );
                    }
                    FailureKind::Auth => {
                        let reason = format!(
                            "agent {} ({}) is not logged in; run `{}` to log in, then `openchef retry {}`: {}",
                            agent.name, agent.runtime, agent.runtime, self.task.id, f.message
                        );
                        return self.give_up(i, a, live.take(), &reason).await;
                    }
                    FailureKind::ContextOverflow | FailureKind::Unknown => {
                        let reason = format!(
                            "agent {} failed ({}): {}",
                            agent.name,
                            f.kind.as_str(),
                            f.message
                        );
                        return self.give_up(i, a, live.take(), &reason).await;
                    }
                },
            }
        }
    }

    /// Spends one retry of the step's budget; `false` when none is left.
    async fn retry(
        &mut self,
        i: usize,
        a: &Attempt,
        retries: &mut u32,
        why: &str,
    ) -> Result<bool, Error> {
        let max = self.e.cfg.limits.max_retries;
        if *retries >= max {
            return Ok(false);
        }
        *retries += 1;
        self.step_to(
            i,
            StepStatus::Running,
            StepChange {
                verified_commit: None,
                count_retry: true,
            },
        )
        .await?;
        self.notice(
            self.steps[i].index,
            Some(a.id),
            format!("retry {retries} of {max}: {why}"),
        );
        Ok(true)
    }

    /// Closes the crashed session and continues its conversation in a new process.
    async fn resume(&self, a: &Attempt, agent: &Agent, old: Live, seq: u32) -> Result<Live, Error> {
        let reference = old.session.session_ref().clone();
        let from = old.row;
        if let Err(e) = self.close_session(old).await {
            self.notice(a.step_index, Some(a.id), e.to_string());
        }
        let mut ev = self.event(EventKind::Notice);
        ev.step = Some(a.step_index);
        ev.attempt = Some(a.id);
        ev.agent = Some(agent.name.clone());
        ev.message = Some("resuming the session after a failure".into());
        self.publish(ev);
        self.open_session(a, agent, Some(reference), Some(from), seq)
            .await
    }

    async fn backoff(&self, step: u32, a: &Attempt, f: &Failure, n: u32) -> Result<(), Error> {
        let mut wait = Duration::from_secs(30).saturating_mul(1 << (n - 1).min(10));
        if let Some(reset) = f.resets_at {
            let until = reset - self.e.now();
            if until.is_positive() {
                wait = Duration::try_from(until).unwrap_or(wait);
            }
        }
        let wait = wait.min(self.e.cfg.limits.max_backoff);
        self.notice(
            step,
            Some(a.id),
            format!("rate limited; waiting {}s", wait.as_secs()),
        );
        self.e.sleep(wait, self.shutdown).await
    }

    /// Runs the step's verification command; `None` when the step has none.
    async fn verify(&self, i: usize) -> Result<Option<Outcome>, Error> {
        let s = &self.steps[i];
        let Some(command) = s.verify.clone() else {
            return Ok(None);
        };
        let mut ev = self.event(EventKind::VerifyStarted);
        ev.step = Some(s.index);
        ev.message = Some(command.clone());
        self.publish(ev);
        let mut spec = verify::Spec {
            command,
            dir: self.worktree()?.path.clone(),
            env: self.e.cfg.verify_env.clone(),
            timeout: Some(self.e.cfg.limits.verify_timeout),
            max_output: None,
            confine: None,
        };
        if self.task.backend == Backend::Sandbox {
            let Some(confine_for) = &self.e.cfg.confine_verify else {
                // Fail closed: a sandboxed task never verifies unconfined.
                return Err(Error::Config(
                    "this daemon has no sandbox for verification".into(),
                ));
            };
            let (confine, extra) = confine_for(&self.task).map_err(Error::Config)?;
            spec.env.retain(|(k, _)| !extra.iter().any(|(e, _)| e == k));
            spec.env.extend(extra);
            spec.confine = Some(confine);
        }
        let outcome = match verify::run(&spec, self.shutdown).await {
            Ok(o) => o,
            Err(verify::Error::Cancelled { .. }) => return Err(Error::Shutdown),
            Err(e) => return Err(e.into()),
        };
        let mut ev = self.event(EventKind::VerifyFinished);
        ev.step = Some(s.index);
        ev.verify = Some(outcome.clone());
        self.publish(ev);
        Ok(Some(outcome))
    }

    /// Records a verified step: git commit, then step verified, then attempt
    /// succeeded. A replay after a crash finds the existing commit by its trailers.
    async fn commit(&mut self, i: usize, a: &Attempt, agent: &Agent) -> Result<AttemptEnd, Error> {
        let index = self.steps[i].index;
        self.ensure_active().await?;
        let step = StepCommit {
            task: self.task.id.to_string(),
            step: index,
            agent: agent.name.clone(),
            pool: agent.pool.clone(),
            subject: self.steps[i].title.clone(),
        };
        let sha = self
            .worktree()?
            .commit_step(self.base_commit(), &step)
            .await?;
        self.step_to(
            i,
            StepStatus::Verified,
            StepChange {
                verified_commit: Some(sha.clone()),
                count_retry: false,
            },
        )
        .await?;
        self.store()
            .end_attempt(a.id, AttemptStatus::Succeeded, "verified")
            .await?;
        self.attempt_ended(index, a, agent, AttemptStatus::Succeeded, "verified");
        let mut ev = self.event(EventKind::StepFinished);
        ev.step = Some(index);
        ev.status = Some(StepStatus::Verified.to_string());
        ev.message = Some(sha);
        self.publish(ev);
        Ok(AttemptEnd::Verified)
    }

    /// Saves the attempt's partial work as a patch, resets the workspace to the last
    /// verified commit and ends the attempt so the step can fail over (§7.2).
    async fn exhausted(
        &mut self,
        i: usize,
        a: &Attempt,
        live: Option<Live>,
        agent: &Agent,
        f: &Failure,
    ) -> Result<AttemptEnd, Error> {
        let index = self.steps[i].index;
        self.close_quietly(live, index, a.id).await;
        self.ensure_active().await?;
        // The attempt id keeps patches of several failovers apart, and lets a replay of
        // this attempt (a crash after the reset) find the patch it saved.
        let rel = format!(
            "{CONTROL_DIR}/patches/step-{index}-{}-{}.patch",
            agent.name, a.id
        );
        let wt = self.worktree()?;
        let abs = wt.path.join(&rel);
        let saved = wt.capture_patch(&abs).await? || abs.exists();
        wt.reset_to(&self.last_verified(i)).await?;
        self.store()
            .end_attempt(a.id, AttemptStatus::FailedOver, f.kind.as_str())
            .await?;
        if let Some(cb) = &self.e.cfg.pool_exhausted {
            cb(&agent.pool, f.resets_at);
        }
        self.attempt_ended(index, a, agent, AttemptStatus::FailedOver, &f.message);
        Ok(AttemptEnd::Failover {
            pool: agent.pool.clone(),
            resets_at: f.resets_at,
            from: agent.name.clone(),
            patch: saved.then_some(rel),
        })
    }

    /// Fails the attempt and the step and parks the task for a human.
    async fn give_up(
        &mut self,
        i: usize,
        a: &Attempt,
        live: Option<Live>,
        reason: &str,
    ) -> Result<AttemptEnd, Error> {
        let index = self.steps[i].index;
        self.close_quietly(live, index, a.id).await;
        self.store()
            .end_attempt(a.id, AttemptStatus::Failed, reason)
            .await?;
        let mut ev = self.event(EventKind::AttemptEnded);
        ev.step = Some(index);
        ev.attempt = Some(a.id);
        ev.agent = Some(a.agent.clone());
        ev.status = Some(AttemptStatus::Failed.to_string());
        ev.message = Some(reason.to_owned());
        self.publish(ev);
        self.step_to(i, StepStatus::Failed, StepChange::default())
            .await?;
        let mut ev = self.event(EventKind::StepFinished);
        ev.step = Some(index);
        ev.status = Some(StepStatus::Failed.to_string());
        ev.message = Some(reason.to_owned());
        self.publish(ev);
        self.park(TaskStatus::NeedsAttention, reason, None).await?;
        Ok(AttemptEnd::Parked)
    }

    fn attempt_ended(
        &self,
        step: u32,
        a: &Attempt,
        agent: &Agent,
        status: AttemptStatus,
        message: &str,
    ) {
        let mut ev = self.event(EventKind::AttemptEnded);
        ev.step = Some(step);
        ev.attempt = Some(a.id);
        ev.agent = Some(agent.name.clone());
        ev.status = Some(status.to_string());
        ev.message = Some(message.to_owned());
        self.publish(ev);
    }
}
