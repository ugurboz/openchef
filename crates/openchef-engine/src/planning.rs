//! The plan phase of a pipeline task (§7.5 phase 1, D-037): a planner agent, in `plan`
//! mode, answers with a plan; OpenChef validates it, allows one repair turn, stores it
//! and waits for the user's approval.

use openchef_core::{Attempt, AttemptRole, AttemptStatus, Step, StepStatus, TaskStatus, TurnKind};
use openchef_runtime::FailureKind;
use openchef_store::NewAttempt;

use crate::engine::{Error, Run};
use crate::events::EventKind;
use crate::plan::{self, Plan};
use crate::router::Agent;
use crate::session::Live;
use crate::turn::TurnEnd;

/// How a planning attempt ended.
enum Planned {
    /// A valid plan.
    Plan(Plan),
    /// The task was parked (quota, failure); the engine run ends.
    Parked,
}

impl Run<'_> {
    /// Plans the task; `Ok(false)` when it stopped (awaiting approval or parked).
    pub(crate) async fn plan(&mut self) -> Result<bool, Error> {
        self.settle_planning().await?;
        let agent = match self
            .e
            .cfg
            .router
            .route(&self.task, &self.planning_step(), &[])
        {
            Ok(a) => a,
            Err(no) => {
                let status = if no.resume_at.is_some() {
                    TaskStatus::WaitingQuota
                } else {
                    TaskStatus::NeedsAttention
                };
                self.park(status, &no.to_string(), no.resume_at).await?;
                return Ok(false);
            }
        };
        let a = self
            .store()
            .start_attempt(NewAttempt {
                task_id: self.task.id.clone(),
                step_index: 0,
                role: AttemptRole::Plan,
                agent: agent.name.clone(),
                runtime: agent.runtime.clone(),
                model: agent.model.clone().unwrap_or_default(),
                pool: agent.pool.clone(),
            })
            .await?;
        let mut ev = self.event(EventKind::AttemptStarted);
        ev.step = Some(0);
        ev.attempt = Some(a.id);
        ev.agent = Some(agent.name.clone());
        ev.message = Some("planning".into());
        self.publish(ev);
        let mut live = match self.open_session(&a, &agent, None, None, 1).await {
            Ok(live) => Some(live),
            Err(e) if e.stopped() || self.shutdown.is_cancelled() => return Err(e),
            Err(e) => {
                let why = format!("planner {} could not start: {e}", agent.name);
                return self.planning_failed(&a, None, &why).await.map(|()| false);
            }
        };
        let planned = match self.planning_turns(&a, &agent, &mut live).await {
            // The council runs while the planner's session is open: it revises there.
            Ok(Planned::Plan(p)) => self
                .council(&a, &agent, &mut live, p)
                .await
                .map(Planned::Plan),
            other => other,
        };
        self.close_quietly(live, 0, a.id).await;
        match planned? {
            Planned::Parked => Ok(false),
            Planned::Plan(p) => self.store_plan(&a, &p).await.map(|()| false),
        }
    }

    /// The routing request for the planner: the task's importance, no files.
    pub(crate) fn planning_step(&self) -> Step {
        Step {
            task_id: self.task.id.clone(),
            index: 0,
            title: "plan".into(),
            prompt: self.task.prompt.clone(),
            files: Vec::new(),
            verify: None,
            importance: self.task.importance,
            status: StepStatus::Pending,
            retries: 0,
            verified_commit: None,
            updated_at: self.e.now(),
        }
    }

    /// Ends whatever a previous daemon left of planning (the planner's and the critics'
    /// attempts at step 0); planning starts again in a new conversation (a plan is cheap
    /// to redo and has no partial work to keep).
    async fn settle_planning(&self) -> Result<(), Error> {
        let left: Vec<Attempt> = self
            .store()
            .attempts(&self.task.id)
            .await?
            .into_iter()
            .filter(|a| a.step_index == 0 && a.status == AttemptStatus::Running)
            .collect();
        for a in left {
            drop(self.settle(&a).await?);
            self.store()
                .end_attempt(
                    a.id,
                    AttemptStatus::Interrupted,
                    &format!("daemon restart while planning ({})", a.role),
                )
                .await?;
        }
        Ok(())
    }

    /// The planning turn and, on an invalid answer, the one repair turn.
    async fn planning_turns(
        &mut self,
        a: &Attempt,
        agent: &Agent,
        live: &mut Option<Live>,
    ) -> Result<Planned, Error> {
        let schema = plan::schema();
        let (mut kind, mut msg) = (
            TurnKind::Prompt,
            plan::planner_prompt(&self.task.prompt, None),
        );
        for turn in 0..2 {
            let Some(l) = live.as_mut() else {
                return Err(Error::Session("the planning attempt has no session".into()));
            };
            let end = self
                .run_turn(l, a, agent, kind, &msg, Some(&schema))
                .await?;
            let result = match end {
                TurnEnd::Completed(r) => r,
                TurnEnd::Failed(f)
                    if matches!(
                        f.kind,
                        FailureKind::QuotaExhausted | FailureKind::RateLimited
                    ) =>
                {
                    if let Some(cb) = &self.e.cfg.pool_exhausted {
                        cb(&agent.pool, f.resets_at);
                    }
                    self.store()
                        .end_attempt(a.id, AttemptStatus::FailedOver, f.kind.as_str())
                        .await?;
                    self.attempt_ended(0, a, agent, AttemptStatus::FailedOver, &f.message);
                    let why = format!("planner {} ran out of quota on {}", agent.name, agent.pool);
                    let status = if f.resets_at.is_some() {
                        TaskStatus::WaitingQuota
                    } else {
                        TaskStatus::NeedsAttention
                    };
                    self.park(status, &why, f.resets_at).await?;
                    return Ok(Planned::Parked);
                }
                TurnEnd::Failed(f) => {
                    let why = format!(
                        "planner {} failed ({}): {}",
                        agent.name,
                        f.kind.as_str(),
                        f.message
                    );
                    self.planning_failed(a, live.take(), &why).await?;
                    return Ok(Planned::Parked);
                }
                TurnEnd::Loop | TurnEnd::Stall(_) => {
                    let why = format!("planner {} stopped making progress", agent.name);
                    self.planning_failed(a, live.take(), &why).await?;
                    return Ok(Planned::Parked);
                }
            };
            match plan::parse(result.structured.as_ref(), &result.final_text) {
                Ok(p) => return Ok(Planned::Plan(p)),
                Err(problems) if turn == 0 => {
                    self.notice(
                        0,
                        Some(a.id),
                        format!(
                            "the plan is not valid; asking once to repair it: {}",
                            problems.join("; ")
                        ),
                    );
                    (kind, msg) = (TurnKind::Retry, plan::repair_prompt(&problems));
                }
                Err(problems) => {
                    let raw = result
                        .structured
                        .as_ref()
                        .map_or_else(|| result.final_text.clone(), ToString::to_string);
                    self.notice(0, Some(a.id), format!("the planner's last answer:\n{raw}"));
                    let why = format!(
                        "the plan is still not valid after a repair turn: {}",
                        problems.join("; ")
                    );
                    self.planning_failed(a, live.take(), &why).await?;
                    return Ok(Planned::Parked);
                }
            }
        }
        Err(Error::Session("planning ended without an answer".into()))
    }

    /// Ends the attempt as failed and parks the task for the user.
    async fn planning_failed(
        &mut self,
        a: &Attempt,
        live: Option<Live>,
        why: &str,
    ) -> Result<(), Error> {
        self.close_quietly(live, 0, a.id).await;
        self.store()
            .end_attempt(a.id, AttemptStatus::Failed, why)
            .await?;
        let mut ev = self.event(EventKind::AttemptEnded);
        ev.step = Some(0);
        ev.attempt = Some(a.id);
        ev.agent = Some(a.agent.clone());
        ev.status = Some(AttemptStatus::Failed.to_string());
        ev.message = Some(why.to_owned());
        self.publish(ev);
        self.park(TaskStatus::NeedsAttention, why, None).await
    }

    /// Stores a valid plan and hands the task to the user.
    async fn store_plan(&mut self, a: &Attempt, p: &Plan) -> Result<(), Error> {
        self.steps = self
            .store()
            .set_plan(&self.task.id, p.steps(&self.task.id, self.e.now()))
            .await?;
        self.store()
            .end_attempt(a.id, AttemptStatus::Succeeded, "planned")
            .await?;
        let mut ev = self.event(EventKind::AttemptEnded);
        ev.step = Some(0);
        ev.attempt = Some(a.id);
        ev.agent = Some(a.agent.clone());
        ev.status = Some(AttemptStatus::Succeeded.to_string());
        ev.message = Some(format!("planned {} step(s)", p.steps.len()));
        self.publish(ev);
        self.set_status(
            TaskStatus::AwaitingApproval,
            &format!(
                "plan ready: {} ({} step{})",
                p.title,
                p.steps.len(),
                if p.steps.len() == 1 { "" } else { "s" }
            ),
            None,
        )
        .await
    }
}
