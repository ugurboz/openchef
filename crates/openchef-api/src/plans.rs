//! Pipelines and their plans (§7.5, D-037): `task/pipeline` and `plan/*`.

use openchef_engine::plan::Plan;
use serde::{Deserialize, Serialize};

use crate::{Client, Error, RunParams, TaskParams, TaskWire};

/// Starts a pipeline task: plan, approve, implement.
pub const METHOD_TASK_PIPELINE: &str = "task/pipeline";
/// A pipeline's plan.
pub const METHOD_PLAN_GET: &str = "plan/get";
/// Approves a plan.
pub const METHOD_PLAN_APPROVE: &str = "plan/approve";
/// Rejects a plan, cancelling the task.
pub const METHOD_PLAN_REJECT: &str = "plan/reject";
/// Replaces a plan before approval.
pub const METHOD_PLAN_EDIT: &str = "plan/edit";

/// `task/pipeline`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineParams {
    /// The goal and options, as for `task/run`.
    #[serde(flatten)]
    pub run: RunParams,
    /// Run a valid plan without waiting for approval.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub yes: bool,
}

/// `plan/reject`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectParams {
    /// The task id.
    pub id: String,
    /// Why, for the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `plan/edit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditParams {
    /// The task id.
    pub id: String,
    /// The plan to store.
    pub plan: Plan,
}

/// A task with its plan (`plan/get`, `plan/edit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanResult {
    /// The task.
    pub task: TaskWire,
    /// Its plan; empty steps while it is still being planned.
    pub plan: Plan,
    /// The plan council's findings (step 0), every round; older daemons send none.
    #[serde(default)]
    pub findings: Vec<crate::FindingWire>,
}

impl Client {
    /// Starts a pipeline task.
    ///
    /// # Errors
    ///
    /// When the call fails; a refusal carries [`crate::CODE_TASK_INVALID`].
    pub async fn pipeline(&self, p: &PipelineParams) -> Result<TaskWire, Error> {
        self.call(METHOD_TASK_PIPELINE, p).await
    }

    /// A pipeline's plan.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn plan(&self, id: &str) -> Result<PlanResult, Error> {
        self.call(METHOD_PLAN_GET, TaskParams { id: id.to_owned() })
            .await
    }

    /// Approves a plan.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn approve_plan(&self, id: &str) -> Result<TaskWire, Error> {
        self.call(METHOD_PLAN_APPROVE, TaskParams { id: id.to_owned() })
            .await
    }

    /// Rejects a plan.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn reject_plan(&self, p: &RejectParams) -> Result<TaskWire, Error> {
        self.call(METHOD_PLAN_REJECT, p).await
    }

    /// Replaces a plan.
    ///
    /// # Errors
    ///
    /// When the call fails; an invalid plan carries [`crate::CODE_TASK_INVALID`].
    pub async fn edit_plan(&self, p: &EditParams) -> Result<PlanResult, Error> {
        self.call(METHOD_PLAN_EDIT, p).await
    }
}
