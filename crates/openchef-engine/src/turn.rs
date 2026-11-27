//! One turn: record it, send it, stream its events and enforce the turn limits.

use std::pin::pin;

use openchef_core::{Attempt, TurnKind, TurnStatus};
use openchef_runtime::{BoxFuture, EventBody, Failure, FailureKind, TurnInput};
use tokio_util::sync::CancellationToken;

use crate::engine::{Error, Run};
use crate::events::EventKind;
use crate::router::Agent;
use crate::session::Live;

/// Awaits an optional future; never completes when there is none.
async fn poll_optional<F: Future + Unpin>(f: &mut Option<F>) -> F::Output {
    match f {
        Some(f) => f.await,
        None => std::future::pending().await,
    }
}

/// How a turn ended from the engine's point of view.
#[derive(Debug)]
pub(crate) enum TurnEnd {
    Completed,
    /// The CLI reported a failure.
    Failed(Failure),
    /// OpenChef interrupted a tool-call loop.
    Loop,
    /// OpenChef interrupted an idle or overlong turn.
    Stall(String),
}

impl Run<'_> {
    /// Records a turn, sends it, streams its events and enforces a turn timeout, an
    /// idle timeout and loop detection, each of which interrupts the turn. A shutdown
    /// interrupts the turn and returns [`Error::Shutdown`], leaving the turn recorded as
    /// sent so recovery retries it.
    pub(crate) async fn run_turn(
        &self,
        live: &mut Live,
        a: &Attempt,
        agent: &Agent,
        kind: TurnKind,
        msg: &str,
    ) -> Result<TurnEnd, Error> {
        let lim = self.e.cfg.limits;
        let row = self.store().begin_turn(live.row, kind, msg).await?;
        // The turn's deadline: adapters report a cancelled turn as `timeout`.
        let deadline = CancellationToken::new();
        let timer = {
            let (deadline, timeout) = (deadline.clone(), lim.turn_timeout);
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                deadline.cancel();
            })
        };
        let result = self.stream_turn(live, a, agent, msg, &deadline).await;
        timer.abort();
        let end = result?;
        let (status, outcome) = match &end {
            TurnEnd::Completed => (TurnStatus::Completed, "completed".to_owned()),
            TurnEnd::Failed(f) => (TurnStatus::Failed, f.kind.as_str().to_owned()),
            TurnEnd::Loop => (TurnStatus::Interrupted, "loop".to_owned()),
            TurnEnd::Stall(why) => (TurnStatus::Interrupted, format!("stall: {why}")),
        };
        self.store().end_turn(row.id, status, &outcome).await?;
        Ok(end)
    }

    #[allow(clippy::too_many_lines, reason = "one select loop with its exits")]
    async fn stream_turn(
        &self,
        live: &mut Live,
        a: &Attempt,
        agent: &Agent,
        msg: &str,
        deadline: &CancellationToken,
    ) -> Result<TurnEnd, Error> {
        let lim = self.e.cfg.limits;
        let input = TurnInput {
            text: msg.to_owned(),
            output_schema: None,
        };
        let mut rx = match live.session.turn(input, deadline.clone()).await {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(TurnEnd::Failed(Failure::new(
                    FailureKind::Crashed,
                    e.to_string(),
                )));
            }
        };
        let mut idle = pin!(tokio::time::sleep(lim.idle_timeout));
        let mut stop_by = pin!(tokio::time::sleep(lim.stop_wait));
        // Set once OpenChef interrupts: why, and the interrupt call, which runs beside
        // the event stream (some adapters return only once the turn has ended).
        let mut stopped: Option<TurnEnd> = None;
        let mut interrupting: Option<BoxFuture<'_, Result<(), openchef_runtime::Error>>> = None;
        let mut shutting_down = false;
        let (mut last_signature, mut repeats) = (String::new(), 0_u32);
        let mut terminal: Option<EventBody> = None;
        let session = &mut live.session;
        let mut session_slot = Some(session);
        let mut interrupt =
            |why: TurnEnd,
             stopped: &mut Option<TurnEnd>,
             stop_by: &mut std::pin::Pin<&mut tokio::time::Sleep>| {
                if stopped.is_some() {
                    return None;
                }
                *stopped = Some(why);
                stop_by
                    .as_mut()
                    .reset(tokio::time::Instant::now() + lim.stop_wait);
                session_slot.take().map(|s| s.interrupt())
            };
        loop {
            tokio::select! {
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    let mut ev = self.event(EventKind::Agent);
                    ev.step = Some(a.step_index);
                    ev.attempt = Some(a.id);
                    ev.agent = Some(agent.name.clone());
                    ev.runtime = Some(event.clone());
                    self.publish(ev);
                    if event.is_terminal() {
                        terminal = Some(event.body);
                        break;
                    }
                    idle.as_mut().reset(tokio::time::Instant::now() + lim.idle_timeout);
                    if let EventBody::ToolCall(call) = &event.body {
                        if !call.signature.is_empty() && call.signature == last_signature {
                            repeats += 1;
                        } else {
                            last_signature.clone_from(&call.signature);
                            repeats = 1;
                        }
                        if repeats >= lim.loop_repeats
                            && let Some(f) = interrupt(TurnEnd::Loop, &mut stopped, &mut stop_by)
                        {
                            interrupting = Some(f);
                        }
                    }
                }
                () = &mut idle, if stopped.is_none() => {
                    let why = TurnEnd::Stall(format!("no activity for {:?}", lim.idle_timeout));
                    if let Some(f) = interrupt(why, &mut stopped, &mut stop_by) {
                        interrupting = Some(f);
                    }
                }
                () = self.shutdown.cancelled(), if !shutting_down => {
                    shutting_down = true;
                    let why = TurnEnd::Stall("OpenChef is shutting down".into());
                    if let Some(f) = interrupt(why, &mut stopped, &mut stop_by) {
                        interrupting = Some(f);
                    }
                }
                result = poll_optional(&mut interrupting), if interrupting.is_some() => {
                    interrupting = None;
                    if let Err(e) = result
                        && !matches!(e, openchef_runtime::Error::SessionClosed)
                    {
                        self.notice(a.step_index, Some(a.id), format!("interrupt failed: {e}"));
                    }
                }
                () = &mut stop_by, if stopped.is_some() => {
                    if shutting_down {
                        return Err(Error::Shutdown);
                    }
                    // The CLI ignored the interrupt; the caller closes (kills) the session.
                    return Ok(TurnEnd::Failed(Failure::new(FailureKind::Crashed, "the agent did not stop after an interrupt")));
                }
            }
        }
        drop(interrupting);
        if shutting_down {
            return Err(Error::Shutdown);
        }
        Ok(match (terminal, stopped) {
            (None, _) => TurnEnd::Failed(Failure::new(
                FailureKind::Protocol,
                "the turn ended without a result",
            )),
            // Completed despite an interrupt: the work is done; verification decides.
            (Some(EventBody::TurnCompleted(_)), _) => TurnEnd::Completed,
            (Some(_), Some(why)) => why,
            (Some(EventBody::TurnFailed(f)), None)
                if f.kind == FailureKind::Timeout && deadline.is_cancelled() =>
            {
                TurnEnd::Stall(format!("the turn took longer than {:?}", lim.turn_timeout))
            }
            (Some(EventBody::TurnFailed(f)), None) => TurnEnd::Failed(f),
            (
                Some(
                    EventBody::Text(_)
                    | EventBody::ToolCall(_)
                    | EventBody::ToolResult(_)
                    | EventBody::Quota(_),
                ),
                None,
            ) => TurnEnd::Failed(Failure::new(
                FailureKind::Unknown,
                "the turn failed without details",
            )),
        })
    }
}
