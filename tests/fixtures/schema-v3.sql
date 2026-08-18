CREATE TABLE IF NOT EXISTS schema_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS team_missions (
    mission_id TEXT PRIMARY KEY,
    brief TEXT NOT NULL DEFAULT '',
    template TEXT NOT NULL DEFAULT 'general',
    agent_profile_id TEXT NOT NULL DEFAULT 'codex-default-v1',
    agent_profile_version INTEGER NOT NULL DEFAULT 1,
    context_rev INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS team_roles (
    mission_id TEXT NOT NULL,
    role TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL DEFAULT '',
    thinking TEXT NOT NULL DEFAULT '',
    permission_policy TEXT NOT NULL DEFAULT '',
    profile_id TEXT NOT NULL DEFAULT 'codex-default-v1',
    profile_version INTEGER NOT NULL DEFAULT 1,
    config_digest TEXT NOT NULL DEFAULT '',
    pane_id TEXT NOT NULL DEFAULT '',
    terminal_id TEXT NOT NULL DEFAULT '',
    session_json TEXT,
    launch_generation TEXT NOT NULL DEFAULT '',
    health TEXT NOT NULL DEFAULT 'unknown',
    last_seen_rev INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (mission_id, role),
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS assignments (
    id TEXT PRIMARY KEY,
    mission_id TEXT NOT NULL,
    source_role TEXT NOT NULL,
    target_role TEXT NOT NULL,
    kind TEXT NOT NULL,
    summary TEXT NOT NULL,
    state TEXT NOT NULL,
    parent_id TEXT,
    review_round INTEGER NOT NULL DEFAULT 0,
    skills_json TEXT NOT NULL DEFAULT '[]',
    replace_skills INTEGER NOT NULL DEFAULT 0,
    review_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    mission_id TEXT NOT NULL,
    assignment_id TEXT,
    source_role TEXT NOT NULL,
    target_role TEXT NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    context_rev INTEGER NOT NULL,
    in_reply_to TEXT,
    review_id TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL UNIQUE,
    mission_id TEXT NOT NULL,
    target_role TEXT NOT NULL,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    claimed_by TEXT NOT NULL DEFAULT '',
    claimed_at INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    delivered_at TEXT,
    FOREIGN KEY (message_id) REFERENCES messages(id)
        ON DELETE CASCADE,
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS context_ledger (
    mission_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    kind TEXT NOT NULL,
    source_role TEXT NOT NULL,
    summary TEXT NOT NULL,
    refs_json TEXT NOT NULL DEFAULT '[]',
    assignment_id TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (mission_id, revision),
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS processed_events (
    event_key TEXT PRIMARY KEY,
    mission_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS expert_instances (
    mission_id TEXT NOT NULL,
    role TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL DEFAULT '',
    thinking TEXT NOT NULL DEFAULT '',
    permission_policy TEXT NOT NULL DEFAULT '',
    profile_id TEXT NOT NULL DEFAULT 'codex-default-v1',
    profile_version INTEGER NOT NULL DEFAULT 1,
    config_digest TEXT NOT NULL DEFAULT '',
    pane_id TEXT NOT NULL DEFAULT '',
    terminal_id TEXT NOT NULL DEFAULT '',
    session_json TEXT,
    launch_generation TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'missing',
    current_assignment_id TEXT,
    close_policy TEXT NOT NULL,
    role_skill TEXT NOT NULL DEFAULT '',
    capability_skills_json TEXT NOT NULL DEFAULT '[]',
    skill_hash TEXT NOT NULL DEFAULT '',
    prompt_path TEXT NOT NULL DEFAULT '',
    last_active_at TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (mission_id, role),
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS role_launch_leases (
    mission_id TEXT NOT NULL,
    role TEXT NOT NULL,
    owner TEXT NOT NULL,
    generation TEXT NOT NULL,
    acquired_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (mission_id, role),
    FOREIGN KEY (mission_id, role)
        REFERENCES team_roles(mission_id, role) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS review_revisions (
    id TEXT PRIMARY KEY,
    mission_id TEXT NOT NULL,
    reviewer_assignment_id TEXT NOT NULL UNIQUE,
    worker_assignment_id TEXT NOT NULL,
    verdict TEXT NOT NULL,
    summary TEXT NOT NULL,
    refs_json TEXT NOT NULL DEFAULT '[]',
    context_rev INTEGER NOT NULL,
    acknowledged_by_pm INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    acknowledged_at TEXT,
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS tool_jobs (
    job_id TEXT PRIMARY KEY,
    mission_id TEXT NOT NULL,
    assignment_id TEXT NOT NULL,
    source_role TEXT NOT NULL,
    mode TEXT NOT NULL,
    label TEXT NOT NULL,
    argv_json TEXT NOT NULL,
    cwd TEXT NOT NULL,
    env_json TEXT NOT NULL DEFAULT '{}',
    timeout_seconds REAL NOT NULL,
    parallel INTEGER NOT NULL DEFAULT 0,
    max_output_bytes INTEGER NOT NULL,
    request_json TEXT NOT NULL,
    state TEXT NOT NULL,
    pane_id TEXT NOT NULL DEFAULT '',
    coordination_dir TEXT NOT NULL DEFAULT '',
    request_path TEXT NOT NULL DEFAULT '',
    stdout_path TEXT NOT NULL DEFAULT '',
    stderr_path TEXT NOT NULL DEFAULT '',
    result_path TEXT NOT NULL DEFAULT '',
    stdout_bytes INTEGER NOT NULL DEFAULT 0,
    stderr_bytes INTEGER NOT NULL DEFAULT 0,
    stdout_truncated INTEGER NOT NULL DEFAULT 0,
    stderr_truncated INTEGER NOT NULL DEFAULT 0,
    stdout_checksum TEXT NOT NULL DEFAULT '',
    stderr_checksum TEXT NOT NULL DEFAULT '',
    exit_code INTEGER,
    error TEXT NOT NULL DEFAULT '',
    result_notified INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    cancelled_at TEXT,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (mission_id) REFERENCES team_missions(mission_id)
        ON DELETE CASCADE,
    FOREIGN KEY (assignment_id) REFERENCES assignments(id)
        ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS outbox_target_status
    ON outbox(mission_id, target_role, status, created_at);
CREATE INDEX IF NOT EXISTS assignments_target_state
    ON assignments(mission_id, target_role, state, created_at);
CREATE INDEX IF NOT EXISTS ledger_mission_revision
    ON context_ledger(mission_id, revision);
CREATE INDEX IF NOT EXISTS expert_instance_state
    ON expert_instances(mission_id, state, role);
CREATE INDEX IF NOT EXISTS review_pending
    ON review_revisions(mission_id, acknowledged_by_pm);
CREATE INDEX IF NOT EXISTS tool_jobs_mission_state
    ON tool_jobs(mission_id, state, created_at);
CREATE INDEX IF NOT EXISTS tool_jobs_role_state
    ON tool_jobs(mission_id, source_role, state, created_at);

INSERT INTO schema_meta(key, value)
VALUES('schema_version', '3')
ON CONFLICT(key) DO UPDATE SET value = excluded.value;
