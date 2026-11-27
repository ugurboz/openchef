//! The task API (§12.3) over the service, ported from Go `internal/daemon/tasks.go`:
//! run, list, get, cancel, retry, and watches that forward a task's live events to the
//! connection as notifications.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use openchef_api::{
    CODE_APPROVAL_SETTLED, CODE_INVALID_PARAMS, CODE_TASK_INVALID, ListParams, METHOD_TASK_CANCEL,
    METHOD_TASK_GET, METHOD_TASK_LIST, METHOD_TASK_RETRY, METHOD_TASK_RUN, METHOD_TASK_UNWATCH,
    METHOD_TASK_WATCH, NOTIFY_EVENT, NOTIFY_WATCH_ENDED, RunParams, TaskDetail, TaskParams,
    TaskWire, WatchEnded, WatchParams, WatchResult, finished, rpc_error,
};
use openchef_core::{PermissionMode, TaskId};
use openchef_engine::service::{Error as ServiceError, RunRequest, Service, Subscription};
use openchef_runtime::linerpc::RpcError;
use openchef_store::TaskFilter;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{Conn, Method};

/// Bounds the events returned by one watch, so a replay fits one JSON-RPC message.
pub const MAX_REPLAY: usize = 5000;

/// One live watch: a connection following a task.
type WatchKey = (u64, TaskId);

/// The watches in progress. A small leaf value, never held across `.await` (D-030).
#[derive(Default)]
struct Watches {
    next: u64,
    /// Each watch's generation and the token that ends its forwarder.
    live: HashMap<WatchKey, (u64, CancellationToken)>,
}

struct Handlers {
    service: Service,
    watches: Mutex<Watches>,
}

/// The task methods, served by `service`.
#[must_use]
pub fn task_methods(service: Service) -> HashMap<String, Method> {
    let h = Arc::new(Handlers {
        service,
        watches: Mutex::new(Watches::default()),
    });
    let mut methods: HashMap<String, Method> = HashMap::new();
    methods.insert(
        METHOD_TASK_RUN.into(),
        method(&h, |h, _, p| async move { h.run(p).await }),
    );
    methods.insert(
        METHOD_TASK_LIST.into(),
        method(&h, |h, _, p| async move { h.list(p).await }),
    );
    methods.insert(
        METHOD_TASK_GET.into(),
        method(&h, |h, _, p| async move { h.get(p).await }),
    );
    methods.insert(
        METHOD_TASK_CANCEL.into(),
        method(&h, |h, _, p| async move { h.cancel(p).await }),
    );
    methods.insert(
        METHOD_TASK_RETRY.into(),
        method(&h, |h, _, p| async move { h.retry(p).await }),
    );
    methods.insert(
        METHOD_TASK_WATCH.into(),
        method(&h, |h, c, p| async move { h.watch(c, p).await }),
    );
    methods.insert(
        METHOD_TASK_UNWATCH.into(),
        method(&h, |h, c, p| async move { h.unwatch(&c, p) }),
    );
    methods
}

/// Wraps a handler as a [`Method`].
fn method<F, Fut>(h: &Arc<Handlers>, f: F) -> Method
where
    F: Fn(Arc<Handlers>, Conn, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, RpcError>> + Send + 'static,
{
    let h = Arc::clone(h);
    let f = Arc::new(f);
    Arc::new(move |conn, params, _cancel| {
        let (h, f) = (Arc::clone(&h), Arc::clone(&f));
        Box::pin(async move { f(h, conn, params).await })
    })
}

pub(crate) fn decode<T: DeserializeOwned + Default>(params: Value) -> Result<T, RpcError> {
    if params.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(params)
        .map_err(|e| rpc_error(CODE_INVALID_PARAMS, format!("invalid params: {e}")))
}

pub(crate) fn decode_required<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| rpc_error(CODE_INVALID_PARAMS, format!("invalid params: {e}")))
}

pub(crate) fn encode(v: &impl Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::server(format!("encode result: {e}")))
}

pub(crate) fn task_id(s: &str) -> Result<TaskId, RpcError> {
    s.parse::<TaskId>()
        .map_err(|e| rpc_error(CODE_INVALID_PARAMS, e.to_string()))
}

/// Turns service errors into JSON-RPC errors the CLI can show as they are.
pub(crate) fn rpc_err(e: &ServiceError) -> RpcError {
    match e {
        ServiceError::Invalid(message) => rpc_error(CODE_TASK_INVALID, message.clone()),
        ServiceError::Store(s) if s.is_not_found() => rpc_error(
            CODE_TASK_INVALID,
            s.to_string().replace("not found: ", "no such "),
        ),
        ServiceError::Settled { .. } => rpc_error(CODE_APPROVAL_SETTLED, e.to_string()),
        ServiceError::Store(_)
        | ServiceError::Engine(_)
        | ServiceError::EventLog(_)
        | ServiceError::StopTimeout
        | ServiceError::Withdrawn(_)
        | ServiceError::Stopping => RpcError::server(e.to_string()),
    }
}

impl Handlers {
    fn watches(&self) -> MutexGuard<'_, Watches> {
        self.watches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn run(&self, params: Value) -> Result<Value, RpcError> {
        let p: RunParams = decode_required(params)?;
        let dir = PathBuf::from(&p.dir);
        if !dir.is_absolute() {
            return Err(rpc_error(
                CODE_INVALID_PARAMS,
                "dir must be an absolute path",
            ));
        }
        let permissions = p
            .permission_mode
            .as_deref()
            .filter(|m| !m.is_empty())
            .map(|m| {
                m.parse::<PermissionMode>()
                    .map_err(|e| rpc_error(CODE_TASK_INVALID, e.to_string()))
            })
            .transpose()?;
        let req = RunRequest {
            prompt: p.prompt,
            dir,
            verify: p.verify,
            agent: p.agent,
            importance: None,
            permissions,
            sandbox: p.sandbox,
        };
        let t = self.service.run(req).await.map_err(|e| rpc_err(&e))?;
        encode(&TaskWire::from(&t))
    }

    async fn list(&self, params: Value) -> Result<Value, RpcError> {
        let p: ListParams = decode(params)?;
        let tasks = self
            .service
            .list(TaskFilter {
                active: p.active,
                limit: p.limit,
            })
            .await
            .map_err(|e| rpc_err(&e))?;
        encode(&tasks.iter().map(TaskWire::from).collect::<Vec<_>>())
    }

    async fn get(&self, params: Value) -> Result<Value, RpcError> {
        let p: TaskParams = decode_required(params)?;
        let d = self
            .service
            .get(&task_id(&p.id)?)
            .await
            .map_err(|e| rpc_err(&e))?;
        encode(&TaskDetail {
            task: TaskWire::from(&d.task),
            steps: d.steps.iter().map(Into::into).collect(),
            attempts: d.attempts.iter().map(Into::into).collect(),
            driving: d.driving,
        })
    }

    async fn cancel(&self, params: Value) -> Result<Value, RpcError> {
        let p: TaskParams = decode_required(params)?;
        let t = self
            .service
            .cancel(&task_id(&p.id)?)
            .await
            .map_err(|e| rpc_err(&e))?;
        encode(&TaskWire::from(&t))
    }

    async fn retry(&self, params: Value) -> Result<Value, RpcError> {
        let p: TaskParams = decode_required(params)?;
        let t = self
            .service
            .retry(&task_id(&p.id)?)
            .await
            .map_err(|e| rpc_err(&e))?;
        encode(&TaskWire::from(&t))
    }

    /// Subscribes the connection to a task's events. Live events are forwarded as
    /// notifications by a task that ends with the subscription, an unwatch or the
    /// connection.
    async fn watch(self: Arc<Self>, conn: Conn, params: Value) -> Result<Value, RpcError> {
        let p: WatchParams = decode_required(params)?;
        let id = task_id(&p.id)?;
        let (mut past, sub) = self
            .service
            .watch(&id, p.replay)
            .await
            .map_err(|e| rpc_err(&e))?;
        let detail = match self.service.get(&id).await {
            Ok(d) => d,
            Err(e) => {
                self.service.unwatch(&sub);
                return Err(rpc_err(&e));
            }
        };
        let key: WatchKey = (conn.id(), id);
        let (generation, stop) = {
            let mut w = self.watches();
            w.next += 1;
            let generation = w.next;
            let stop = CancellationToken::new();
            if let Some((_, old)) = w.live.insert(key.clone(), (generation, stop.clone())) {
                old.cancel(); // a second watch of the same task replaces the first
            }
            (generation, stop)
        };
        let status = detail.task.status.to_string();
        // The newest events matter most.
        let omitted = past.len().saturating_sub(MAX_REPLAY);
        drop(past.drain(..omitted));
        let result = WatchResult {
            events: past,
            finished: finished(&status),
            status,
            omitted,
        };
        let handlers = Arc::clone(&self);
        drop(tokio::spawn(async move {
            handlers.forward(conn, key, generation, sub, stop).await;
        }));
        encode(&result)
    }

    async fn forward(
        &self,
        conn: Conn,
        key: WatchKey,
        generation: u64,
        mut sub: Subscription,
        stop: CancellationToken,
    ) {
        let closed = conn.closed();
        loop {
            let ev = tokio::select! {
                () = stop.cancelled() => break,
                () = closed.cancelled() => break,
                ev = sub.rx.recv() => ev,
            };
            let Some(ev) = ev else {
                if sub.lagged() {
                    self.ended(
                        &conn,
                        &key.1,
                        "the viewer fell behind; watch again to catch up",
                    )
                    .await;
                }
                break;
            };
            let Ok(params) = serde_json::to_value(&ev) else {
                break;
            };
            if conn.notify(NOTIFY_EVENT, params).await.is_err() {
                break; // the client is gone
            }
        }
        {
            let mut w = self.watches();
            if w.live.get(&key).is_some_and(|(g, _)| *g == generation) {
                drop(w.live.remove(&key));
            }
        }
        self.service.unwatch(&sub);
    }

    async fn ended(&self, conn: &Conn, id: &TaskId, reason: &str) {
        let ended = WatchEnded {
            id: id.to_string(),
            reason: reason.to_owned(),
        };
        if let Ok(params) = serde_json::to_value(&ended) {
            // Best effort: the client may be gone already.
            let _sent = conn.notify(NOTIFY_WATCH_ENDED, params).await;
        }
    }

    fn unwatch(&self, conn: &Conn, params: Value) -> Result<Value, RpcError> {
        let p: TaskParams = decode_required(params)?;
        let key: WatchKey = (conn.id(), task_id(&p.id)?);
        if let Some((_, stop)) = self.watches().live.remove(&key) {
            stop.cancel();
        }
        Ok(Value::Object(serde_json::Map::new()))
    }
}
