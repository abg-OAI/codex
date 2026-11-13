CREATE TABLE subagent_promotions (
    source_id TEXT PRIMARY KEY NOT NULL,
    root_id TEXT NOT NULL,
    destination_id TEXT UNIQUE NOT NULL,
    members_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('preparing', 'ready', 'retired'))
);
