//! The composition root, ported from Go `internal/app/app.go`: it builds the daemon's
//! object graph (store, runtime adapters, router, engine, service, task API) from the
//! configuration. Nothing else constructs these pieces, so every other crate depends
//! only on traits and plain values.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openchef_config::{Config, Paths, Phase};
use openchef_core::{Mode, PermissionMode, Task};
use openchef_engine::service::{Deps, Service};
use openchef_engine::{Agent, Config as EngineConfig, DynRuntime, Ordered, Runtimes, SpecFor};
use openchef_runtime::agy::{AgyOptions, AgyRuntime};
use openchef_runtime::claude::{ClaudeOptions, ClaudeRuntime};
use openchef_runtime::codex::{CodexOptions, CodexRuntime};
use openchef_runtime::{INVARIANT_DENY_RULES, SessionSpec};
use openchef_store::Store;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::approver::WorkspaceApprover;
use crate::env::session_env;
use crate::{Init, OnStop, Options, STOP_TIMEOUT, Services, service_methods};

/// How long a pool cools down when it ran out of quota without saying until when.
const UNKNOWN_RESET: Duration = Duration::from_hours(1);

/// The service's share of [`STOP_TIMEOUT`], leaving time to close the store.
const SERVICE_STOP: Duration = Duration::from_secs(STOP_TIMEOUT.as_secs().saturating_sub(1));

/// The daemon's options: its [`Init`] (run while the daemon holds the lock) loads the
/// configuration, opens the store, recovers work left by a previous daemon and exposes
/// the task API.
#[must_use]
pub fn daemon_options(paths: Paths, version: String, shutdown: CancellationToken) -> Options {
    let init: Init = {
        let (paths, version) = (paths.clone(), version.clone());
        Box::new(move || Box::pin(async move { services(paths, version).await }))
    };
    Options {
        paths,
        version,
        methods: HashMap::new(),
        on_stop: None,
        init: Some(init),
        shutdown,
    }
}

async fn services(paths: Paths, version: String) -> Result<Services, String> {
    let cfg = match Config::load(&paths.config) {
        Ok(cfg) => cfg,
        Err(e @ openchef_config::Error::NotFound { .. }) => {
            return Err(format!("{e} (run `openchef init` first)"));
        }
        Err(e) => return Err(e.to_string()),
    };
    paths.ensure_dirs().map_err(|e| e.to_string())?;
    let store = Store::open(&paths.state).await.map_err(|e| e.to_string())?;
    let service = match build(&cfg, &paths, &version, &store) {
        Ok(s) => s,
        Err(e) => return Err(close_after(store, e).await),
    };
    match service.start().await {
        Ok(0) => {}
        Ok(killed) => {
            use std::io::Write as _;
            // The daemon log is best effort.
            let _logged = writeln!(
                std::io::stderr(),
                "killed processes left by a previous daemon: {killed}"
            );
        }
        Err(e) => return Err(close_after(store, e.to_string()).await),
    }
    let on_stop: OnStop = {
        let service = service.clone();
        Box::new(move || {
            Box::pin(async move {
                // Runs that did not stop still use the store; the process is exiting,
                // so leave the database to the OS rather than closing it under them.
                service
                    .stop(SERVICE_STOP)
                    .await
                    .map_err(|e| e.to_string())?;
                store.close().await.map_err(|e| e.to_string())
            })
        })
    };
    Ok(Services {
        methods: service_methods(service),
        on_stop: Some(on_stop),
    })
}

async fn close_after(store: Store, error: String) -> String {
    match store.close().await {
        Ok(()) => error,
        Err(e) => format!("{error}; close store: {e}"),
    }
}

fn build(cfg: &Config, paths: &Paths, version: &str, store: &Store) -> Result<Service, String> {
    let mut runtimes: Runtimes = HashMap::new();
    let claude = ClaudeRuntime::new(ClaudeOptions::default());
    let codex = CodexRuntime::new(CodexOptions {
        client_version: version.to_owned(),
        ..CodexOptions::default()
    });
    let agy = AgyRuntime::new(AgyOptions {
        client_version: version.to_owned(),
        ..AgyOptions::default()
    });
    runtimes.insert("claude".into(), Arc::new(claude) as Arc<dyn DynRuntime>);
    runtimes.insert("codex".into(), Arc::new(codex) as Arc<dyn DynRuntime>);
    runtimes.insert("agy".into(), Arc::new(agy) as Arc<dyn DynRuntime>);

    let cooldowns = Arc::new(Cooldowns::default());
    let router = Arc::new(Ordered {
        agents: agents(cfg),
        cooldown: Some({
            let c = Arc::clone(&cooldowns);
            Arc::new(move |pool: &str| c.get(pool, OffsetDateTime::now_utc()))
        }),
    });
    let env: Vec<(String, String)> = std::env::vars().collect();
    let pools = cfg.pools.clone();
    let spec_for: SpecFor = Arc::new(move |t: &Task, a: &Agent| spec(t, a, &env, &pools));
    let mut engine = EngineConfig::new(store.clone(), runtimes, router, spec_for, &paths.home);
    engine.verify_env = openchef_engine::verify::minimal_env(std::env::vars_os(), &[]);
    engine.pool_exhausted = Some({
        let c = Arc::clone(&cooldowns);
        Arc::new(move |pool: &str, until: Option<OffsetDateTime>| {
            c.set(pool, until, OffsetDateTime::now_utc());
        })
    });
    Service::new(Deps {
        engine,
        event_dir: paths.sessions.clone(),
        default_permissions: None,
        on_fail: Some(Arc::new(|message| {
            use std::io::Write as _;
            // The daemon log is best effort.
            let _logged = writeln!(std::io::stderr(), "event log: {message}");
        })),
        approval_timeout: None,
        notify: None,
    })
    .map_err(|e| e.to_string())
}

/// The session settings for an agent working on a task (the engine sets dir, model,
/// effort, log path and `on_spawn`). `bypassPermissions` runs only in an isolated
/// worktree (§9.3).
fn spec(
    t: &Task,
    a: &Agent,
    env: &[(String, String)],
    pools: &BTreeMap<String, openchef_config::Pool>,
) -> Result<SessionSpec, String> {
    if t.permissions == PermissionMode::BypassPermissions
        && (t.worktree.is_none() || t.mode != Mode::Isolated)
    {
        return Err("bypassPermissions is only allowed in an isolated worktree".into());
    }
    let dir = t.worktree.clone().unwrap_or_default();
    Ok(SessionSpec {
        dir: dir.clone(),
        model: None,
        effort: None,
        mode: t.permissions,
        deny_rules: INVARIANT_DENY_RULES
            .iter()
            .map(|r| (*r).to_owned())
            .collect(),
        ask_rules: Vec::new(),
        web_access: false,
        env: session_env(env, pools.get(&a.pool)),
        approver: Arc::new(WorkspaceApprover {
            dir,
            mode: t.permissions,
        }),
        ask_user: None,
        log_path: None,
        on_spawn: None,
    })
}

/// The configured agents in routing order: the implement phase's list first, then the
/// remaining agents by name.
#[must_use]
pub fn agents(cfg: &Config) -> Vec<Agent> {
    let mut order: Vec<String> = cfg
        .routing
        .get(&Phase::Implement)
        .map_or_else(Vec::new, |r| r.agents.clone());
    let rest: Vec<String> = cfg
        .agents
        .keys()
        .filter(|name| !order.contains(name))
        .cloned()
        .collect(); // a BTreeMap: already sorted by name
    order.extend(rest);
    order
        .into_iter()
        .filter_map(|name| {
            cfg.agents.get(&name).map(|a| Agent {
                runtime: a.runtime.to_string(),
                model: a.model.clone(),
                pool: a.pool.clone(),
                effort: a.effort,
                name,
            })
        })
        .collect()
}

/// Pools that ran out of quota, until their reset: the stand-in for quota tracking
/// (§10, R4.3). A small leaf value, never held across `.await` (D-030).
#[derive(Debug, Default)]
struct Cooldowns {
    until: Mutex<HashMap<String, OffsetDateTime>>,
}

impl Cooldowns {
    fn set(&self, pool: &str, until: Option<OffsetDateTime>, now: OffsetDateTime) {
        let until = until.unwrap_or(now + UNKNOWN_RESET);
        let _previous = self
            .until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pool.to_owned(), until);
    }

    fn get(&self, pool: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
        let mut map = self.until.lock().unwrap_or_else(PoisonError::into_inner);
        match map.get(pool).copied() {
            Some(u) if now >= u => {
                let _expired = map.remove(pool);
                None
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use openchef_config::{Agent, Config, PhaseRouting, Quota, Runtime, Strategy};
    use openchef_core::{Effort, Tier};
    use time::OffsetDateTime;

    use super::{Cooldowns, agents};

    fn agent(runtime: Runtime, pool: &str, effort: Option<Effort>) -> Agent {
        Agent {
            runtime,
            model: None,
            pool: pool.into(),
            tier: Tier::Standard,
            effort,
        }
    }

    #[test]
    fn agents_order() {
        let mut routing = BTreeMap::new();
        routing.insert(
            openchef_config::Phase::Implement,
            PhaseRouting {
                strategy: Strategy::Ordered,
                agents: vec!["c".into(), "ghost".into()],
                weights: BTreeMap::new(),
            },
        );
        let cfg = Config {
            pools: BTreeMap::new(),
            agents: BTreeMap::from([
                ("b".to_owned(), agent(Runtime::Codex, "p2", None)),
                (
                    "a".to_owned(),
                    agent(Runtime::Claude, "p1", Some(Effort::High)),
                ),
                ("c".to_owned(), agent(Runtime::Agy, "p3", None)),
            ]),
            routing,
            quota: Quota::default(),
        };
        let got = agents(&cfg);
        let names: Vec<&str> = got.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["c", "a", "b"]);
        assert_eq!(got[1].effort, Some(Effort::High));
        assert_eq!(got[0].runtime, "agy");
    }

    #[test]
    fn cooldowns_expire() {
        let c = Cooldowns::default();
        let now = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(c.get("p", now), None);
        c.set("p", None, now);
        assert_eq!(c.get("p", now), Some(now + super::UNKNOWN_RESET));
        assert_eq!(c.get("p", now + super::UNKNOWN_RESET), None, "expired");
        assert_eq!(c.get("p", now), None, "removed once expired");
    }
}
