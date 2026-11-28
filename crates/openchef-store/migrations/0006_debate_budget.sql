-- The debate budget (§7.5, D-040): review, council, arbiter, fix and escalation turns a
-- task has spent, kept on the task so a daemon restart never refills it.
ALTER TABLE tasks ADD COLUMN debate_turns INTEGER NOT NULL DEFAULT 0 CHECK (debate_turns >= 0);
