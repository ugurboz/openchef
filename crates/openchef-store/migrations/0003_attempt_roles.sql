-- Attempts gain a role (D-037): planners, critics, reviewers, arbiters and auditors are
-- recorded and recovered like implementers. Task-level roles use step 0, so the foreign
-- key to steps goes (a plan does not exist before it is made); the step is checked in
-- code. One running attempt per (task, step, role). Rebuilt with foreign keys off by the
-- migration runner, which then checks them.
CREATE TABLE attempts_new (
    id         INTEGER PRIMARY KEY,
    task_id    TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    step_idx   INTEGER NOT NULL CHECK (step_idx BETWEEN 0 AND 7),
    role       TEXT NOT NULL DEFAULT 'implement' CHECK (role IN (
                   'implement', 'plan', 'review', 'council', 'arbiter', 'audit')),
    agent      TEXT NOT NULL,
    runtime    TEXT NOT NULL,
    model      TEXT,
    pool       TEXT NOT NULL,
    status     TEXT NOT NULL CHECK (status IN (
                   'running', 'succeeded', 'failed_over', 'failed', 'interrupted', 'cancelled')),
    outcome    TEXT,
    started_at TEXT NOT NULL,
    ended_at   TEXT,
    CHECK ((role IN ('plan', 'council', 'audit')) = (step_idx = 0))
) STRICT;

INSERT INTO attempts_new (id, task_id, step_idx, role, agent, runtime, model, pool, status,
                          outcome, started_at, ended_at)
SELECT id, task_id, step_idx, 'implement', agent, runtime, model, pool, status, outcome,
       started_at, ended_at
FROM attempts;

DROP TABLE attempts;
ALTER TABLE attempts_new RENAME TO attempts;

CREATE INDEX attempts_step ON attempts (task_id, step_idx);
CREATE UNIQUE INDEX attempts_one_running ON attempts (task_id, step_idx, role)
    WHERE status = 'running';
