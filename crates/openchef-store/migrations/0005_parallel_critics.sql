-- Plan critics run side by side (D-039): one running attempt per (task, step, role)
-- still holds for every role but `council`, whose critics are kept apart by agent.
-- Only the index changes; the table is not rebuilt.
DROP INDEX attempts_one_running;

CREATE UNIQUE INDEX attempts_one_running ON attempts (task_id, step_idx, role)
    WHERE status = 'running' AND role <> 'council';

CREATE UNIQUE INDEX attempts_one_critic_running ON attempts (task_id, step_idx, agent)
    WHERE status = 'running' AND role = 'council';
