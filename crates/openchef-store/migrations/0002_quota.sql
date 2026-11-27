-- Quota (docs/architecture.md §10, §11; D-025, D-026), ported from Go migrations 0005
-- and 0006.

-- What each source said about a pool, kept so a restarted daemon routes by what it
-- knew, and for `openchef quota`.
CREATE TABLE quota_observations (
    id        INTEGER PRIMARY KEY,
    pool      TEXT NOT NULL,
    source    TEXT NOT NULL CHECK (source IN ('L1', 'L2', 'L3')),
    at        TEXT NOT NULL,
    exhausted INTEGER NOT NULL DEFAULT 0 CHECK (exhausted IN (0, 1)),
    windows   TEXT NOT NULL DEFAULT '[]' -- JSON array of openchef-quota windows
) STRICT;

CREATE INDEX quota_observations_at ON quota_observations (at);

-- Switches the user flips from the CLI (`openchef quota probes off`), so they outlive
-- the daemon without OpenChef rewriting the user's config.toml.
CREATE TABLE flags (
    name       TEXT NOT NULL PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

-- Per-step cost history (L3, §10.1, §10.3): the share of its pool's tightest window one
-- attempt used, the input to admission estimates.
CREATE TABLE usage (
    id         INTEGER PRIMARY KEY,
    pool       TEXT NOT NULL,
    attempt_id INTEGER, -- for audit only: costs outlive the attempts they came from
    share      REAL NOT NULL CHECK (share >= 0 AND share <= 1),
    at         TEXT NOT NULL
) STRICT;

CREATE INDEX usage_pool ON usage (pool, id);

-- What API pools spent (§10.3: daily and monthly USD budgets).
CREATE TABLE spend (
    id   INTEGER PRIMARY KEY,
    pool TEXT NOT NULL,
    usd  REAL NOT NULL CHECK (usd >= 0),
    at   TEXT NOT NULL
) STRICT;

CREATE INDEX spend_pool_at ON spend (pool, at);
