-- Review findings (§7.5, D-038): what reviewers reported per step and round, and what
-- became of it, for `openchef status` and the report. Step 0 holds the plan council's
-- findings and `dismissed` an arbiter's verdict (R5.3), so neither needs a table rebuild.
CREATE TABLE findings (
    id          INTEGER PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    step_idx    INTEGER NOT NULL CHECK (step_idx BETWEEN 0 AND 7),
    round       INTEGER NOT NULL CHECK (round >= 1),
    attempt_id  INTEGER REFERENCES attempts (id) ON DELETE SET NULL,
    finding     TEXT NOT NULL, -- the reviewer's id (F1, …)
    severity    TEXT NOT NULL CHECK (severity IN ('blocker', 'major', 'nit')),
    file        TEXT,
    line        INTEGER,
    contract    TEXT,
    claim       TEXT NOT NULL,
    evidence    TEXT NOT NULL,
    fix         TEXT NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('open', 'fixed', 'disputed', 'dismissed', 'nit')),
    note        TEXT, -- a rebuttal's evidence
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
) STRICT;

CREATE INDEX findings_step ON findings (task_id, step_idx, round);
