CREATE TABLE goal_supervisor_continuity (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    action_kind TEXT NOT NULL CHECK (
        action_kind IN ('followup', 'snooze', 'compact')
    ),
    action_at_ms INTEGER NOT NULL,
    snooze_delay_seconds INTEGER,
    snooze_reason TEXT,
    CHECK (
        (
            action_kind = 'snooze'
            AND snooze_delay_seconds IS NOT NULL
            AND snooze_delay_seconds > 0
            AND snooze_reason IS NOT NULL
            AND length(trim(snooze_reason)) > 0
        ) OR (
            action_kind != 'snooze'
            AND snooze_delay_seconds IS NULL
            AND snooze_reason IS NULL
        )
    )
);
