CREATE TABLE executor_exclusions (
    thread_id TEXT NOT NULL,
    environment_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    selection_json TEXT NOT NULL,
    PRIMARY KEY (thread_id, environment_id)
);
