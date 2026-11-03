ALTER TABLE goal_supervisor_continuity
ADD COLUMN delivered_followup_message TEXT;

ALTER TABLE goal_supervisor_continuity
ADD COLUMN snooze_count INTEGER NOT NULL DEFAULT 0 CHECK (snooze_count >= 0);

ALTER TABLE goal_supervisor_continuity
ADD COLUMN snoozed_seconds INTEGER NOT NULL DEFAULT 0 CHECK (snoozed_seconds >= 0);
