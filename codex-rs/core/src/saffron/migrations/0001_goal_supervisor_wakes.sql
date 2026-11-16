CREATE TABLE goal_supervisor_wakes (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    goal_objective TEXT NOT NULL,
    goal_updated_at_ms INTEGER NOT NULL,
    wake_at_ms INTEGER NOT NULL
);
