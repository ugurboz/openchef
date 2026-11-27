//! Quota tracking (§10, D-025, D-026), ported from Go `internal/service/quota.go` as
//! two actors (D-030):
//!
//! - the **monitor** owns the merged state of every pool, the per-step cost history and
//!   the API pools' spending. It folds L1 observations from agent streams, L2 probe
//!   results and L3 budget windows, persists them through a bounded writer, and
//!   publishes a [`View`] the router reads without waiting;
//! - the **probe runner** owns the L2 probes and their schedule, so a slow provider
//!   (up to 10 s per probe) never delays routing or the monitor.
//!
//! Every sender that the engine calls (`observe`, `attempt_*`, `spend`, `exhausted`)
//! uses `try_send`: the engine's sink must never block. A full queue loses the record
//! and says so in the daemon log.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use openchef_quota::{
    Health, Observation, Prober, Source, State, Tracker, Window, estimate, health_of, reset_at,
    spent,
};
use openchef_store::{ObservationRow, Spend, Store};
use time::{Date, Month, OffsetDateTime, Time, UtcOffset};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The stored kill switch (`openchef quota probes off`, §10.2).
pub const PROBES_FLAG: &str = "quota.probes";
/// Scheduled L2 probes are this far apart; the prober itself enforces 30 s.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_mins(5);
/// How much history a restarted daemon replays: a weekly window.
const HISTORY: Duration = Duration::from_hours(8 * 24);
/// How many step costs per pool estimates look at.
const COST_HISTORY: usize = 20;
/// Commands waiting for the monitor.
const COMMAND_BUFFER: usize = 1024;
/// Records waiting to be stored.
const WRITE_BUFFER: usize = 256;
/// How long a pool that ran out without a reset time stays exhausted.
const UNKNOWN_RESET: Duration = Duration::from_hours(1);

/// Returns the current time; tests replace it.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// An API pool's spending limits; `None` is unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Budget {
    /// The most a day may spend.
    pub daily_usd: Option<f64>,
    /// The most a month may spend.
    pub monthly_usd: Option<f64>,
}

impl Budget {
    fn limited(self) -> bool {
        self.daily_usd.is_some() || self.monthly_usd.is_some()
    }
}

/// What the monitor is built from.
pub struct Settings {
    /// Where history and switches are kept.
    pub store: Store,
    /// The L2 probes the configuration allows.
    pub probes: Vec<Prober>,
    /// How far apart scheduled probes are.
    pub interval: Duration,
    /// Now; tests replace it.
    pub clock: Clock,
    /// Every API pool with its limits; only these pools' spending is recorded
    /// (subscriptions report notional costs that bill nothing).
    pub budgets: BTreeMap<String, Budget>,
    /// When a budget's day and month begin.
    pub offset: UtcOffset,
}

/// What routing reads: a copy of the monitor's state, replaced after every change.
#[derive(Debug, Clone, Default)]
pub struct View {
    tracker: Tracker,
    costs: BTreeMap<String, Vec<f64>>,
}

impl View {
    /// A view of fixed state, for tests of its readers.
    #[cfg(test)]
    pub fn of(observations: &[Observation], costs: BTreeMap<String, Vec<f64>>) -> Self {
        let mut tracker = Tracker::new();
        for o in observations {
            tracker.observe(o);
        }
        Self { tracker, costs }
    }

    /// A pool's merged state at `now`; empty before the first observation.
    pub fn state(&self, pool: &str, now: OffsetDateTime) -> State {
        self.tracker.state(pool, now).unwrap_or_else(|| State {
            pool: pool.to_owned(),
            ..State::default()
        })
    }

    /// A pool's health at `now` and when an exhausted pool takes work again.
    pub fn health(&self, pool: &str, now: OffsetDateTime) -> (Health, Option<OffsetDateTime>) {
        let s = self.state(pool, now);
        (health_of(&s, now), reset_at(&s))
    }

    /// The expected cost of the next step on `pool` (§10.3), once three are known.
    pub fn estimate(&self, pool: &str) -> Option<f64> {
        self.costs.get(pool).and_then(|c| estimate(c))
    }
}

pub use openchef_api::{PoolQuota, ProbeStatus, QuotaResult as Report};

enum Command {
    Observe(Observation),
    AttemptStarted {
        attempt: i64,
        pool: String,
    },
    AttemptEnded {
        attempt: i64,
    },
    Spend {
        pool: String,
        usd: f64,
        at: OffsetDateTime,
    },
    Exhausted {
        pool: String,
        until: Option<OffsetDateTime>,
    },
    Pools(oneshot::Sender<Vec<PoolQuota>>),
    Stop(oneshot::Sender<()>),
}

enum ProbeCommand {
    /// Runs every probe now, also for pools observed recently; replies each failure.
    Refresh(oneshot::Sender<BTreeMap<String, String>>),
    SetEnabled(bool),
    Status(oneshot::Sender<Vec<ProbeStatus>>),
}

enum Record {
    Observation(ObservationRow),
    Cost {
        pool: String,
        attempt: i64,
        share: f64,
        at: OffsetDateTime,
    },
    Spend {
        pool: String,
        usd: f64,
        at: OffsetDateTime,
    },
}

/// The monitor's handle. Cheap to clone.
#[derive(Clone)]
pub struct Quota {
    commands: mpsc::Sender<Command>,
    probes: mpsc::Sender<ProbeCommand>,
    view: watch::Receiver<Arc<View>>,
    store: Store,
    clock: Clock,
    tasks: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
    life: CancellationToken,
}

impl std::fmt::Debug for Quota {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Quota").finish_non_exhaustive()
    }
}

fn log(message: &str) {
    use std::io::Write as _;
    // The daemon log is best effort.
    let _logged = writeln!(std::io::stderr(), "quota: {message}");
}

impl Quota {
    /// Replays recent history, applies the stored kill switch and starts the actors.
    pub async fn start(s: Settings) -> Result<Self, String> {
        let now = (s.clock)();
        let since = now - HISTORY;
        s.store
            .prune_observations(since)
            .await
            .map_err(|e| e.to_string())?;
        let mut tracker = Tracker::new();
        for row in s
            .store
            .observations(since)
            .await
            .map_err(|e| e.to_string())?
        {
            match observation(&row) {
                Ok(o) => tracker.observe(&o),
                Err(e) => log(&format!(
                    "skipped a stored observation of {}: {e}",
                    row.pool
                )),
            }
        }
        let costs = s
            .store
            .step_costs(u32::try_from(COST_HISTORY).unwrap_or(u32::MAX))
            .await
            .map_err(|e| e.to_string())?;
        let month = month_start(now, s.offset);
        let spent = s
            .store
            .spend_since(month)
            .await
            .map_err(|e| e.to_string())?;
        let probes_on = s
            .store
            .flag(PROBES_FLAG)
            .await
            .map_err(|e| e.to_string())?
            .as_deref()
            != Some("off");

        let (writes, write_rx) = mpsc::channel(WRITE_BUFFER);
        let (commands, command_rx) = mpsc::channel(COMMAND_BUFFER);
        let mut monitor = Monitor {
            tracker,
            costs: costs.into_iter().collect(),
            started: BTreeMap::new(),
            spent,
            budgets: s.budgets,
            offset: s.offset,
            clock: Arc::clone(&s.clock),
            writes,
            view: watch::channel(Arc::new(View::default())).0,
        };
        let pools: Vec<String> = monitor
            .budgets
            .iter()
            .filter(|(_, b)| b.limited())
            .map(|(p, _)| p.clone())
            .collect();
        for pool in pools {
            let o = monitor.budget_observation(&pool, now);
            monitor.tracker.observe(&o);
        }
        let view = monitor.view.subscribe();
        monitor.publish();

        let life = CancellationToken::new();
        let (probes, probe_rx) = mpsc::channel(16);
        let mut probe_list = s.probes;
        for p in &mut probe_list {
            p.set_enabled(probes_on);
        }
        let runner = Runner {
            probers: probe_list,
            interval: s.interval,
            clock: Arc::clone(&s.clock),
            view: view.clone(),
            monitor: commands.clone(),
            life: life.clone(),
        };
        let tasks = vec![
            tokio::spawn(write(s.store.clone(), write_rx)),
            tokio::spawn(monitor.run(command_rx)),
            tokio::spawn(runner.run(probe_rx)),
        ];
        Ok(Self {
            commands,
            probes,
            view,
            store: s.store,
            clock: s.clock,
            tasks: Arc::new(std::sync::Mutex::new(tasks)),
            life,
        })
    }

    fn send(&self, c: Command, what: &str) {
        if let Err(e) = self.commands.try_send(c) {
            log(&format!("{what} dropped: {e}"));
        }
    }

    /// Records an observation (never blocks).
    pub fn observe(&self, o: Observation) {
        self.send(Command::Observe(o), "observation");
    }

    /// Notes the pool's state when an attempt begins, to measure its cost (L3).
    pub fn attempt_started(&self, attempt: i64, pool: &str) {
        let pool = pool.to_owned();
        self.send(Command::AttemptStarted { attempt, pool }, "attempt start");
    }

    /// Records the share of its pool's window the attempt used, when the observations
    /// made during it can tell.
    pub fn attempt_ended(&self, attempt: i64) {
        self.send(Command::AttemptEnded { attempt }, "attempt end");
    }

    /// Records money an API pool spent (a turn's reported cost).
    pub fn spend(&self, pool: &str, usd: f64, at: OffsetDateTime) {
        let pool = pool.to_owned();
        self.send(Command::Spend { pool, usd, at }, "spend");
    }

    /// A turn failed for quota: the pool is out until `until`, or for an hour.
    pub fn exhausted(&self, pool: &str, until: Option<OffsetDateTime>) {
        let pool = pool.to_owned();
        self.send(Command::Exhausted { pool, until }, "exhaustion");
    }

    /// What routing reads, now.
    pub fn view(&self) -> Arc<View> {
        Arc::clone(&self.view.borrow())
    }

    /// The current time by the monitor's clock.
    pub fn now(&self) -> OffsetDateTime {
        (self.clock)()
    }

    /// Every known pool's quota and every probe's state; with `refresh`, the probes run
    /// first (each at most every 30 s).
    pub async fn report(&self, refresh: bool) -> Result<Report, String> {
        let stopped = || "the quota monitor stopped".to_owned();
        let mut failures = BTreeMap::new();
        if refresh {
            let (tx, rx) = oneshot::channel();
            self.probes
                .send(ProbeCommand::Refresh(tx))
                .await
                .map_err(|_| stopped())?;
            failures = rx.await.map_err(|_| stopped())?;
        }
        // The runner handed its observations to the monitor before replying, and the
        // monitor's queue is ordered, so they are merged before this answer.
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Pools(tx))
            .await
            .map_err(|_| stopped())?;
        let pools = rx.await.map_err(|_| stopped())?;
        let (tx, rx) = oneshot::channel();
        self.probes
            .send(ProbeCommand::Status(tx))
            .await
            .map_err(|_| stopped())?;
        let mut probes = rx.await.map_err(|_| stopped())?;
        for p in &mut probes {
            if p.problem.is_none() {
                p.problem = failures.remove(&p.pool);
            }
        }
        Ok(Report { pools, probes })
    }

    /// The kill switch: stored, so it outlives the daemon, and applied at once.
    pub async fn set_probes(&self, on: bool) -> Result<(), String> {
        self.store
            .set_flag(PROBES_FLAG, if on { "on" } else { "off" })
            .await
            .map_err(|e| e.to_string())?;
        self.probes
            .send(ProbeCommand::SetEnabled(on))
            .await
            .map_err(|_| "the quota monitor stopped".to_owned())
    }

    /// Ends the schedule and stores what is still queued.
    pub async fn stop(&self) {
        self.life.cancel();
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::Stop(tx)).await.is_ok() {
            // A closed reply means the monitor already ended; either way the writer
            // drains before its task, awaited below, ends.
            let _stopped = rx.await;
        }
        let tasks: Vec<JoinHandle<()>> = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for t in tasks {
            if let Err(e) = t.await {
                log(&format!("a quota task failed: {e}"));
            }
        }
    }
}

/// An observation from its stored row.
fn observation(row: &ObservationRow) -> Result<Observation, String> {
    let source = match row.source.as_str() {
        "L1" => Source::Stream,
        "L2" => Source::Probe,
        "L3" => Source::Estimate,
        other => return Err(format!("unknown source {other:?}")),
    };
    let windows: Vec<Window> = serde_json::from_str(&row.windows).map_err(|e| e.to_string())?;
    Ok(Observation {
        pool: row.pool.clone(),
        source,
        at: row.at,
        windows,
        exhausted: row.exhausted,
    })
}

fn row(o: &Observation) -> ObservationRow {
    ObservationRow {
        pool: o.pool.clone(),
        source: o.source.as_str().to_owned(),
        at: o.at,
        exhausted: o.exhausted,
        // Plain data always serializes; an empty array keeps the row valid regardless.
        windows: serde_json::to_string(&o.windows).unwrap_or_else(|_| "[]".to_owned()),
    }
}

/// The monitor's state.
struct Monitor {
    tracker: Tracker,
    costs: BTreeMap<String, Vec<f64>>,
    /// Attempts in flight: their pool and its state when they began.
    started: BTreeMap<i64, (String, State)>,
    /// API pools' spending this month.
    spent: BTreeMap<String, Vec<Spend>>,
    budgets: BTreeMap<String, Budget>,
    offset: UtcOffset,
    clock: Clock,
    writes: mpsc::Sender<Record>,
    view: watch::Sender<Arc<View>>,
}

impl Monitor {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        while let Some(c) = commands.recv().await {
            match c {
                Command::Observe(o) => self.observe(&o),
                Command::AttemptStarted { attempt, pool } => {
                    let state = self.state(&pool);
                    let _previous = self.started.insert(attempt, (pool, state));
                }
                Command::AttemptEnded { attempt } => self.attempt_ended(attempt),
                Command::Spend { pool, usd, at } => self.spend(&pool, usd, at),
                Command::Exhausted { pool, until } => self.exhausted(pool, until),
                Command::Pools(reply) => {
                    // The caller may have gone away; nothing to do then.
                    let _sent = reply.send(self.pools());
                }
                Command::Stop(reply) => {
                    drop(self.writes);
                    let _sent = reply.send(());
                    return;
                }
            }
        }
    }

    fn state(&self, pool: &str) -> State {
        let now = (self.clock)();
        self.tracker.state(pool, now).unwrap_or_else(|| State {
            pool: pool.to_owned(),
            ..State::default()
        })
    }

    fn publish(&self) {
        let view = View {
            tracker: self.tracker.clone(),
            costs: self.costs.clone(),
        };
        // `send_replace` stores the value even while no one is subscribed.
        let _old = self.view.send_replace(Arc::new(view));
    }

    fn store(&self, r: Record) {
        if let Err(e) = self.writes.try_send(r) {
            log(&format!("record not stored, the writer is behind: {e}"));
        }
    }

    fn observe(&mut self, o: &Observation) {
        if o.pool.is_empty() {
            return;
        }
        self.tracker.observe(o);
        self.publish();
        self.store(Record::Observation(row(o)));
    }

    fn attempt_ended(&mut self, attempt: i64) {
        let Some((pool, before)) = self.started.remove(&attempt) else {
            return;
        };
        let after = self.state(&pool);
        let Some(share) = spent(&before, &after) else {
            return;
        };
        let share = share.clamp(0.0, 1.0);
        let costs = self.costs.entry(pool.clone()).or_default();
        costs.push(share);
        if costs.len() > COST_HISTORY {
            let extra = costs.len() - COST_HISTORY;
            let _old: Vec<f64> = costs.drain(..extra).collect();
        }
        self.publish();
        let at = (self.clock)();
        self.store(Record::Cost {
            pool,
            attempt,
            share,
            at,
        });
    }

    /// A pool with a budget gets its spending as windows (`daily_usd`, `monthly_usd`:
    /// utilization = spent / budget, reset at the next day or month), so health,
    /// admission and routing treat budgets like any quota window (§10.3).
    fn spend(&mut self, pool: &str, usd: f64, at: OffsetDateTime) {
        let Some(budget) = self.budgets.get(pool).copied() else {
            return;
        };
        if !(usd.is_finite() && usd > 0.0) {
            return;
        }
        self.store(Record::Spend {
            pool: pool.to_owned(),
            usd,
            at,
        });
        if !budget.limited() {
            return; // unlimited: recorded, nothing to route by
        }
        self.spent
            .entry(pool.to_owned())
            .or_default()
            .push(Spend { usd, at });
        let o = self.budget_observation(pool, at);
        self.observe(&o);
    }

    fn budget_observation(&self, pool: &str, at: OffsetDateTime) -> Observation {
        let budget = self.budgets.get(pool).copied().unwrap_or_default();
        let day = day_start(at, self.offset);
        let month = month_start(at, self.offset);
        let (mut today, mut this_month) = (0.0, 0.0);
        for s in self.spent.get(pool).map_or(&[][..], Vec::as_slice) {
            if s.at >= month {
                this_month += s.usd;
            }
            if s.at >= day {
                today += s.usd;
            }
        }
        let mut windows = Vec::new();
        if let Some(limit) = budget.daily_usd.filter(|l| *l > 0.0) {
            windows.push(Window {
                name: "daily_usd".into(),
                utilization: (today / limit).min(1.0),
                resets_at: Some(day + Duration::from_hours(24)),
                duration: Some(Duration::from_hours(24)),
            });
        }
        if let Some(limit) = budget.monthly_usd.filter(|l| *l > 0.0) {
            windows.push(Window {
                name: "monthly_usd".into(),
                utilization: (this_month / limit).min(1.0),
                resets_at: Some(next_month(month)),
                duration: None,
            });
        }
        Observation {
            pool: pool.to_owned(),
            source: Source::Estimate,
            at,
            windows,
            exhausted: false,
        }
    }

    fn exhausted(&mut self, pool: String, until: Option<OffsetDateTime>) {
        let now = (self.clock)();
        let until = until.filter(|u| *u > now).unwrap_or(now + UNKNOWN_RESET);
        self.observe(&Observation {
            pool,
            source: Source::Stream,
            at: now,
            windows: vec![Window {
                name: "reported".into(),
                utilization: 1.0,
                resets_at: Some(until),
                duration: None,
            }],
            exhausted: true,
        });
    }

    fn pools(&self) -> Vec<PoolQuota> {
        let now = (self.clock)();
        self.tracker
            .states(now)
            .into_iter()
            .map(|s| PoolQuota {
                health: health_of(&s, now),
                reset_at: reset_at(&s),
                state: s,
            })
            .collect()
    }
}

/// Stores records in order until every sender is gone.
async fn write(store: Store, mut records: mpsc::Receiver<Record>) {
    while let Some(r) = records.recv().await {
        let (pool, result) = match r {
            Record::Observation(o) => (o.pool.clone(), store.record_observation(o).await),
            Record::Cost {
                pool,
                attempt,
                share,
                at,
            } => {
                let r = store
                    .record_step_cost(&pool, Some(attempt), share, at)
                    .await;
                (pool, r)
            }
            Record::Spend { pool, usd, at } => {
                let r = store.record_spend(&pool, usd, at).await;
                (pool, r)
            }
        };
        if let Err(e) = result {
            log(&format!("store a record of {pool}: {e}"));
        }
    }
}

/// The probe runner's state.
struct Runner {
    probers: Vec<Prober>,
    interval: Duration,
    clock: Clock,
    view: watch::Receiver<Arc<View>>,
    monitor: mpsc::Sender<Command>,
    life: CancellationToken,
}

impl Runner {
    async fn run(mut self, mut commands: mpsc::Receiver<ProbeCommand>) {
        // Without probes the runner still answers status and switch commands.
        let scheduled = !self.probers.is_empty();
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = self.life.cancelled() => return,
                c = commands.recv() => match c {
                    Some(c) => self.command(c).await,
                    None => return,
                },
                _ = tick.tick(), if scheduled => {
                    let _failures = self.probe_all(true).await;
                }
            }
        }
    }

    async fn command(&mut self, c: ProbeCommand) {
        match c {
            ProbeCommand::Refresh(reply) => {
                let failures = self.probe_all(false).await;
                let _sent = reply.send(failures);
            }
            ProbeCommand::SetEnabled(on) => {
                for p in &mut self.probers {
                    p.set_enabled(on);
                }
            }
            ProbeCommand::Status(reply) => {
                let now = (self.clock)();
                let status = self
                    .probers
                    .iter()
                    .map(|p| ProbeStatus {
                        name: p.name().to_owned(),
                        pool: p.pool().to_owned(),
                        problem: p.status(now).err().map(|e| e.to_string()),
                    })
                    .collect();
                let _sent = reply.send(status);
            }
        }
    }

    /// Runs every probe once and returns each failure by pool. On the schedule
    /// (`skip_fresh`), a pool observed within the interval is skipped: the agents' own
    /// streams are preferred over undocumented endpoints (§10.2).
    async fn probe_all(&mut self, skip_fresh: bool) -> BTreeMap<String, String> {
        let mut failures = BTreeMap::new();
        for i in 0..self.probers.len() {
            let now = (self.clock)();
            let pool = self.probers[i].pool().to_owned();
            if skip_fresh {
                let updated = self.view.borrow().state(&pool, now).updated;
                if updated.is_some_and(|u| now - u < self.interval) {
                    continue;
                }
            }
            match self.probers[i].run(now, self.life.clone()).await {
                Ok(o) => {
                    if self.monitor.send(Command::Observe(o)).await.is_err() {
                        return failures; // the monitor stopped
                    }
                }
                Err(e) => {
                    match &e {
                        openchef_quota::Error::Disabled | openchef_quota::Error::Muted { .. } => {}
                        openchef_quota::Error::ShapeChanged { .. } => {
                            log(&format!("probe of {pool} disabled: {e}"));
                        }
                        openchef_quota::Error::NoCredentials { .. }
                        | openchef_quota::Error::ReadCredentials { .. }
                        | openchef_quota::Error::Status { .. }
                        | openchef_quota::Error::Request { .. }
                        | openchef_quota::Error::InvalidUrl { .. }
                        | openchef_quota::Error::Cancelled { .. } => {
                            log(&format!("probe of {pool} failed: {e}"));
                        }
                    }
                    let _previous = failures.insert(pool, e.to_string());
                }
            }
        }
        failures
    }
}

fn day_start(at: OffsetDateTime, offset: UtcOffset) -> OffsetDateTime {
    at.to_offset(offset).replace_time(Time::MIDNIGHT)
}

fn month_start(at: OffsetDateTime, offset: UtcOffset) -> OffsetDateTime {
    let local = at.to_offset(offset);
    let first = Date::from_calendar_date(local.year(), local.month(), 1).unwrap_or(local.date());
    local.replace_date(first).replace_time(Time::MIDNIGHT)
}

fn next_month(month: OffsetDateTime) -> OffsetDateTime {
    let (year, m) = if month.month() == Month::December {
        (month.year() + 1, Month::January)
    } else {
        (month.year(), month.month().next())
    };
    Date::from_calendar_date(year, m, 1).map_or(month + Duration::from_hours(31 * 24), |d| {
        month.replace_date(d)
    })
}

/// Records the quota signals in agent streams (L1, §10.1), spending, and attempt
/// boundaries (L3) from the engine's events. It never blocks.
pub struct QuotaTap {
    /// The monitor.
    pub quota: Quota,
    /// Each agent's pool.
    pub pool_of: BTreeMap<String, String>,
}

impl openchef_engine::Sink for QuotaTap {
    fn publish(&self, ev: openchef_engine::Event) {
        use openchef_engine::EventKind;
        use openchef_runtime::EventBody;
        let Some(pool) = ev.agent.as_ref().and_then(|a| self.pool_of.get(a)) else {
            return;
        };
        match ev.kind {
            EventKind::Agent => match ev.runtime.as_ref().map(|r| &r.body) {
                Some(EventBody::Quota(signal)) => {
                    self.quota
                        .observe(openchef_quota::from_signal(pool, ev.at, signal));
                }
                Some(EventBody::TurnCompleted(r)) => {
                    // Only API pools record spending; the monitor drops the rest.
                    if let Some(usd) = r.usage.cost_usd {
                        self.quota.spend(pool, usd, ev.at);
                    }
                }
                Some(
                    EventBody::Text(_)
                    | EventBody::ToolCall(_)
                    | EventBody::ToolResult(_)
                    | EventBody::TurnFailed(_),
                )
                | None => {}
            },
            // The attempt's cost is measured from its start to its end.
            EventKind::AttemptStarted => {
                if let Some(attempt) = ev.attempt {
                    self.quota.attempt_started(attempt, pool);
                }
            }
            EventKind::AttemptEnded => {
                if let Some(attempt) = ev.attempt {
                    self.quota.attempt_ended(attempt);
                }
            }
            EventKind::TaskStatus
            | EventKind::StepStarted
            | EventKind::VerifyStarted
            | EventKind::VerifyFinished
            | EventKind::StepFinished
            | EventKind::Notice
            | EventKind::ApprovalRequested
            | EventKind::ApprovalDecided => {}
        }
    }
}

/// The L2 probes the configuration allows (§10.2): one per subscription pool whose
/// provider has a probe and is not switched off.
pub fn probes_for(cfg: &openchef_config::Config, home: &std::path::Path) -> Vec<Prober> {
    use openchef_config::{Billing, Provider};
    let env = |k: &str| std::env::var(k).ok();
    let mut out = Vec::new();
    for (name, pool) in &cfg.pools {
        if pool.billing != Billing::Subscription || !cfg.quota.probes_enabled(pool.provider) {
            continue;
        }
        let probe = match pool.provider {
            Provider::Anthropic => openchef_quota::claude_probe(
                name.clone(),
                Box::new(openchef_quota::claude_credentials(home, env)),
            ),
            Provider::OpenAi => openchef_quota::chatgpt_probe(
                name.clone(),
                Box::new(openchef_quota::codex_credentials(home, env)),
            ),
            Provider::Google => continue, // no usage endpoint known (§10.1)
        };
        match probe {
            Ok(p) => out.push(p),
            Err(e) => log(&format!("no probe for {name}: {e}")),
        }
    }
    out
}

/// Every API pool with its USD limits.
pub fn budgets_for(cfg: &openchef_config::Config) -> BTreeMap<String, Budget> {
    cfg.pools
        .iter()
        .filter(|(_, p)| p.billing == openchef_config::Billing::Api)
        .map(|(name, p)| {
            let budget = Budget {
                daily_usd: p.daily_budget_usd.filter(|b| *b > 0.0),
                monthly_usd: p.monthly_budget_usd.filter(|b| *b > 0.0),
            };
            (name.clone(), budget)
        })
        .collect()
}

/// The quota methods, served by `quota`.
pub fn quota_methods(quota: Quota) -> std::collections::HashMap<String, crate::Method> {
    use openchef_api::{METHOD_QUOTA_PROBES, METHOD_QUOTA_REPORT, ProbesParams, QuotaParams};
    use openchef_runtime::linerpc::RpcError;

    use crate::tasks::{decode, decode_required, encode};

    let mut methods: std::collections::HashMap<String, crate::Method> =
        std::collections::HashMap::new();
    let q = quota.clone();
    methods.insert(
        METHOD_QUOTA_REPORT.into(),
        Arc::new(move |_conn, params, _cancel| {
            let q = q.clone();
            Box::pin(async move {
                let p: QuotaParams = decode(params)?;
                encode(&q.report(p.refresh).await.map_err(RpcError::server)?)
            })
        }),
    );
    methods.insert(
        METHOD_QUOTA_PROBES.into(),
        Arc::new(move |_conn, params, _cancel| {
            let q = quota.clone();
            Box::pin(async move {
                let p: ProbesParams = decode_required(params)?;
                q.set_probes(p.on).await.map_err(RpcError::server)?;
                Ok(serde_json::json!({}))
            })
        }),
    );
    methods
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use openchef_quota::{
        BoxFuture, Error, Fetcher, Health, Observation, Prober, Source, Window, parse_claude,
    };
    use openchef_store::{ObservationRow, Store};
    use time::macros::{datetime, offset};
    use time::{OffsetDateTime, UtcOffset};
    use tokio_util::sync::CancellationToken;

    use super::{Budget, Clock, Quota, Settings};

    const USAGE: &str = r#"{"five_hour": {"utilization": 70.0, "resets_at": "2026-11-26T14:00:00+00:00"},
        "seven_day": {"utilization": 10.0, "resets_at": "2026-11-30T09:00:00+00:00"}}"#;

    /// Answers every request with `body`, counting calls.
    struct FakeFetcher {
        body: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl Fetcher for FakeFetcher {
        fn fetch(&self, _cancel: CancellationToken) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
            let _before = self.calls.fetch_add(1, Ordering::SeqCst);
            let body = self.body.as_bytes().to_vec();
            Box::pin(async move { Ok(body) })
        }
    }

    fn claude_probe(body: &'static str, calls: &Arc<AtomicUsize>) -> Prober {
        let fetcher = FakeFetcher {
            body,
            calls: Arc::clone(calls),
        };
        Prober::new("claude-max", "claude", Box::new(fetcher), parse_claude)
    }

    /// A clock tests can move.
    fn clock(start: OffsetDateTime) -> (Clock, Arc<Mutex<OffsetDateTime>>) {
        let now = Arc::new(Mutex::new(start));
        let c = Arc::clone(&now);
        (
            Arc::new(move || *c.lock().unwrap_or_else(std::sync::PoisonError::into_inner)),
            now,
        )
    }

    async fn store(dir: &tempfile::TempDir) -> Store {
        Store::open(&dir.path().join("state.db"))
            .await
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn settings(store: &Store, clock: &Clock, probes: Vec<Prober>) -> Settings {
        Settings {
            store: store.clone(),
            probes,
            interval: Duration::from_hours(1),
            clock: Arc::clone(clock),
            budgets: BTreeMap::new(),
            offset: UtcOffset::UTC,
        }
    }

    async fn start(s: Settings) -> Quota {
        Quota::start(s).await.unwrap_or_else(|e| panic!("{e}"))
    }

    /// Waits for `f` (events, not timing margins).
    async fn wait_for(mut f: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "condition never held"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits until the monitor processed everything sent so far.
    async fn settle(q: &Quota) {
        let _report = q.report(false).await.unwrap_or_else(|e| panic!("{e}"));
    }

    fn five_hour(pool: &str, at: OffsetDateTime, u: f64, reset: OffsetDateTime) -> Observation {
        Observation {
            pool: pool.into(),
            source: Source::Stream,
            at,
            windows: vec![Window {
                name: "five_hour".into(),
                utilization: u,
                resets_at: Some(reset),
                duration: None,
            }],
            exhausted: false,
        }
    }

    #[tokio::test]
    async fn quota_observe_persist_and_restore() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let db = store(&dir).await;
        let now = datetime!(2026-11-26 12:00 UTC);
        let (c, _) = clock(now);
        let q = start(settings(&db, &c, Vec::new())).await;
        q.observe(five_hour(
            "claude-max",
            now,
            0.9,
            now + Duration::from_hours(1),
        ));
        settle(&q).await;
        assert_eq!(q.view().health("claude-max", now).0, Health::Red);
        q.stop().await; // flushes the writer

        let again = start(settings(&db, &c, Vec::new())).await;
        let r = again.report(false).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(r.pools.len(), 1);
        assert_eq!(r.pools[0].health, Health::Red);
        assert!((r.pools[0].state.windows[0].utilization - 0.9).abs() < f64::EPSILON);
        again.stop().await;
    }

    #[tokio::test]
    async fn quota_probes_and_kill_switch() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let db = store(&dir).await;
        let now = datetime!(2026-11-26 12:00 UTC);
        let (c, moved) = clock(now);
        let calls = Arc::new(AtomicUsize::new(0));
        let q = start(settings(&db, &c, vec![claude_probe(USAGE, &calls)])).await;
        // The schedule probes once at start.
        wait_for(|| q.view().health("claude-max", now).0 == Health::Yellow).await;
        q.set_probes(false).await.unwrap_or_else(|e| panic!("{e}"));
        let before = calls.load(Ordering::SeqCst);
        let r = q.report(true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(calls.load(Ordering::SeqCst), before, "probes off but ran");
        assert!(
            r.probes[0]
                .problem
                .as_deref()
                .is_some_and(|p| p.contains("disabled")),
            "{:?}",
            r.probes
        );
        q.stop().await;

        // The switch outlives the daemon.
        let q = start(settings(&db, &c, vec![claude_probe(USAGE, &calls)])).await;
        let r = q.report(true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before,
            "switch lost on restart"
        );
        assert!(r.probes[0].problem.is_some());
        q.set_probes(true).await.unwrap_or_else(|e| panic!("{e}"));
        *moved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now + Duration::from_mins(1);
        let r = q.report(true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(calls.load(Ordering::SeqCst), before + 1, "probes back on");
        assert_eq!(r.probes[0].problem, None);
        q.stop().await;
    }

    #[tokio::test]
    async fn quota_shape_change_disables_probe() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let db = store(&dir).await;
        let (c, _) = clock(datetime!(2026-11-26 12:00 UTC));
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = claude_probe(r#"{"five_hour": {"used": 3}}"#, &calls);
        let q = start(settings(&db, &c, vec![probe])).await;
        wait_for(|| calls.load(Ordering::SeqCst) == 1).await;
        let r = q.report(false).await.unwrap_or_else(|e| panic!("{e}"));
        let problem = r.probes[0].problem.clone().unwrap_or_default();
        assert!(problem.contains("changed shape"), "problem {problem:?}");
        q.stop().await;
    }

    /// A pool the agents' streams reported on recently is not probed on the schedule.
    #[tokio::test]
    async fn quota_schedule_skips_fresh_pools() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let db = store(&dir).await;
        let now = datetime!(2026-11-26 12:00 UTC);
        db.record_observation(ObservationRow {
            pool: "claude-max".into(),
            source: "L1".into(),
            at: now - Duration::from_mins(1),
            exhausted: false,
            windows: r#"[{"name":"five_hour","utilization":0.2}]"#.into(),
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let (c, _) = clock(now);
        let calls = Arc::new(AtomicUsize::new(0));
        let q = start(settings(&db, &c, vec![claude_probe(USAGE, &calls)])).await;
        // A report goes through the runner after its first scheduled tick.
        settle(&q).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "probed a pool with fresh stream data"
        );
        let _r = q.report(true).await; // a manual refresh always probes
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        q.stop().await;
    }

    /// Each attempt's cost is the window share it used; three make an estimate, and the
    /// history survives a restart.
    #[tokio::test]
    async fn quota_step_costs() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let db = store(&dir).await;
        let start_at = datetime!(2026-11-26 12:00 UTC);
        let (c, moved) = clock(start_at);
        let tick = || {
            let mut t = moved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *t += Duration::from_mins(1);
            *t
        };
        let q = start(settings(&db, &c, Vec::new())).await;
        let reset = start_at + Duration::from_hours(4);
        let mut util = 0.10;
        q.observe(five_hour("p", start_at, util, reset));
        for (i, spend) in [0.02, 0.04, 0.03].into_iter().enumerate() {
            let attempt = i64::try_from(i).unwrap_or(0) + 1;
            let _t = tick();
            q.attempt_started(attempt, "p");
            util += spend;
            let at = tick();
            q.observe(five_hour("p", at, util, reset));
            q.attempt_ended(attempt);
        }
        q.attempt_started(9, "p"); // nothing observed during it: no cost recorded
        q.attempt_ended(9);
        settle(&q).await;
        let est = q
            .view()
            .estimate("p")
            .unwrap_or_else(|| panic!("no estimate"));
        assert!((est - 0.04).abs() < 1e-4, "estimate {est}");
        q.stop().await;

        let again = start(settings(&db, &c, Vec::new())).await;
        assert_eq!(again.view().estimate("p"), Some(est), "after restart");
        again.exhausted("p", None);
        settle(&again).await;
        let now = c();
        let (h, ends) = again.view().health("p", now);
        assert_eq!(h, Health::Exhausted);
        assert_eq!(ends, Some(now + Duration::from_hours(1)));
        again.stop().await;
    }

    /// An API pool's spending is a quota window: past its daily budget it is exhausted
    /// until midnight, and the spending survives a restart. Subscriptions record
    /// nothing.
    #[tokio::test]
    async fn quota_budgets() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let db = store(&dir).await;
        let local = offset!(+3);
        let now = datetime!(2026-11-26 20:00 +3);
        let (c, _) = clock(now);
        let mut s = settings(&db, &c, Vec::new());
        s.offset = local;
        s.budgets = BTreeMap::from([
            (
                "api".to_owned(),
                Budget {
                    daily_usd: Some(2.0),
                    monthly_usd: Some(100.0),
                },
            ),
            ("free-api".to_owned(), Budget::default()),
        ]);
        let budgets = s.budgets.clone();
        let q = start(s).await;
        q.spend("api", 1.2, now);
        settle(&q).await;
        assert_eq!(
            q.view().health("api", now).0,
            Health::Yellow,
            "60% of the day"
        );
        q.spend("api", 0.9, now);
        settle(&q).await;
        let (h, until) = q.view().health("api", now);
        assert_eq!(h, Health::Exhausted);
        assert_eq!(until, Some(datetime!(2026-11-27 00:00 +3)));
        q.spend("claude-max", 5.0, now); // a subscription's notional cost
        q.spend("free-api", 3.0, now); // unlimited: recorded only
        settle(&q).await;
        assert_eq!(q.view().health("free-api", now).0, Health::Unknown);
        q.stop().await;
        let spent = db
            .spend_since(OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(spent.get("api").map(Vec::len), Some(2));
        assert_eq!(spent.get("free-api").map(Vec::len), Some(1));
        assert_eq!(spent.get("claude-max"), None);

        let mut s = settings(&db, &c, Vec::new());
        s.offset = local;
        s.budgets = budgets;
        let again = start(s).await;
        assert_eq!(
            again.view().health("api", now).0,
            Health::Exhausted,
            "after restart"
        );
        again.stop().await;
    }

    /// L1: the quota signals in agent streams reach the monitor through the tap, by the
    /// agent's pool; events of unknown agents are ignored.
    #[tokio::test]
    async fn quota_tap_records_stream_signals() {
        use openchef_engine::{Event, EventKind, Sink as _};
        use openchef_runtime::{EventBody, QuotaSignal, QuotaWindow};

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let db = store(&dir).await;
        let now = datetime!(2026-11-26 12:00 UTC);
        let (c, _) = clock(now);
        let q = start(settings(&db, &c, Vec::new())).await;
        let tap = super::QuotaTap {
            quota: q.clone(),
            pool_of: BTreeMap::from([("opus".to_owned(), "claude-max".to_owned())]),
        };
        let task = openchef_core::TaskId::generate().unwrap_or_else(|e| panic!("{e}"));
        let signal = |agent: &str| {
            let mut ev = Event::new(&task, now, EventKind::Agent);
            ev.agent = Some(agent.into());
            ev.runtime = Some(openchef_runtime::Event {
                at: now,
                body: EventBody::Quota(QuotaSignal {
                    windows: vec![QuotaWindow {
                        name: "5h".into(),
                        utilization: 0.7,
                        resets_at: Some(now + Duration::from_hours(2)),
                        duration: None,
                    }],
                    exhausted: false,
                }),
            });
            ev
        };
        tap.publish(signal("opus"));
        tap.publish(signal("stranger"));
        settle(&q).await;
        let r = q.report(false).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(r.pools.len(), 1, "{:?}", r.pools);
        assert_eq!(r.pools[0].state.pool, "claude-max");
        assert_eq!(r.pools[0].state.windows[0].name, "five_hour");
        assert_eq!(r.pools[0].health, Health::Yellow);
        q.stop().await;
    }

    #[test]
    fn probes_for_config() {
        use openchef_config::{Billing, Config, EffortProfile, Pool, Provider, Quota as Switches};
        let pool = |provider, billing| Pool {
            provider,
            billing,
            plan_tier: None,
            max_concurrent: 1,
            effort_profile: EffortProfile::Auto,
            api_key_env: None,
            daily_budget_usd: Some(5.0),
            monthly_budget_usd: None,
        };
        let mut cfg = Config {
            pools: BTreeMap::from([
                (
                    "claude-max".to_owned(),
                    pool(Provider::Anthropic, Billing::Subscription),
                ),
                (
                    "chatgpt".to_owned(),
                    pool(Provider::OpenAi, Billing::Subscription),
                ),
                (
                    "gemini".to_owned(),
                    pool(Provider::Google, Billing::Subscription),
                ),
                ("api".to_owned(), pool(Provider::Anthropic, Billing::Api)),
            ]),
            agents: BTreeMap::new(),
            routing: BTreeMap::new(),
            quota: Switches::default(),
        };
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let names = |cfg: &Config| -> Vec<(String, String)> {
            super::probes_for(cfg, dir.path())
                .iter()
                .map(|p| (p.pool().to_owned(), p.name().to_owned()))
                .collect()
        };
        assert_eq!(
            names(&cfg),
            [
                ("chatgpt".to_owned(), "chatgpt".to_owned()),
                ("claude-max".to_owned(), "claude".to_owned())
            ],
            "one probe per subscription pool with a known endpoint"
        );
        cfg.quota.providers.insert(Provider::OpenAi, false);
        assert_eq!(names(&cfg).len(), 1, "a provider switched off");
        cfg.quota.probes = Some(false);
        assert!(names(&cfg).is_empty(), "the global switch wins");
        let budgets = super::budgets_for(&cfg);
        assert_eq!(budgets.keys().collect::<Vec<_>>(), ["api"]);
        assert_eq!(budgets["api"].daily_usd, Some(5.0));
    }
}
