-- Schema-only fixture for supported native thread storage. No user rows.
CREATE TABLE guardian_review_feedback (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    record BLOB NOT NULL
);
CREATE TABLE project_roots (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (project_id, position)
);
CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    metadata TEXT NOT NULL DEFAULT '{}',
    position INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE TABLE "thread_attachments" (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    attachment_type TEXT NOT NULL,
    identity_key TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (thread_id, attachment_type, identity_key)
);
CREATE TABLE thread_dynamic_tools (
    thread_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    input_schema TEXT NOT NULL, defer_loading INTEGER NOT NULL DEFAULT 0, namespace TEXT,
    PRIMARY KEY(thread_id, position),
    FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE
);
CREATE TABLE thread_read_receipts (
    thread_id TEXT PRIMARY KEY NOT NULL,
    first_unread_turn TEXT,
    revision TEXT NOT NULL,
    last_published_turn TEXT
);
CREATE TABLE thread_sections (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
, appearance TEXT);
CREATE TABLE thread_spawn_edges (
    parent_thread_id TEXT NOT NULL,
    child_thread_id TEXT NOT NULL PRIMARY KEY,
    status TEXT NOT NULL
);
CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    rollout_path TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    source TEXT NOT NULL,
    model_provider TEXT NOT NULL,
    cwd TEXT NOT NULL,
    title TEXT NOT NULL,
    sandbox_policy TEXT NOT NULL,
    approval_mode TEXT NOT NULL,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    has_user_event INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    archived_at INTEGER,
    git_sha TEXT,
    git_branch TEXT,
    git_origin_url TEXT
, cli_version TEXT NOT NULL DEFAULT '', first_user_message TEXT NOT NULL DEFAULT '', agent_nickname TEXT, agent_role TEXT, memory_mode TEXT NOT NULL DEFAULT 'enabled', model TEXT, reasoning_effort TEXT, agent_path TEXT, created_at_ms INTEGER, updated_at_ms INTEGER, thread_source TEXT, preview TEXT NOT NULL DEFAULT '', recency_at INTEGER NOT NULL DEFAULT 0, recency_at_ms INTEGER NOT NULL DEFAULT 0, history_mode TEXT NOT NULL DEFAULT 'legacy', name TEXT, is_pinned INTEGER NOT NULL DEFAULT 0, thread_section_id TEXT
    REFERENCES thread_sections(id) ON DELETE SET NULL, section_position INTEGER, section_entered_at_ms INTEGER, project_id TEXT
    REFERENCES projects(id) ON DELETE SET NULL, originator TEXT, daybreak_enabled BOOLEAN, creator_user_id TEXT, creator_account_id TEXT);
CREATE TRIGGER threads_created_at_ms_after_insert
AFTER INSERT ON threads
WHEN NEW.created_at_ms IS NULL
BEGIN
    UPDATE threads
    SET created_at_ms = NEW.created_at * 1000
    WHERE id = NEW.id;
END;
CREATE TRIGGER threads_created_at_ms_after_update
AFTER UPDATE OF created_at ON threads
WHEN NEW.created_at != OLD.created_at
 AND NEW.created_at_ms IS OLD.created_at_ms
BEGIN
    UPDATE threads
    SET created_at_ms = NEW.created_at * 1000
    WHERE id = NEW.id;
END;
CREATE TRIGGER threads_recency_at_after_insert
AFTER INSERT ON threads
WHEN NEW.recency_at_ms = 0
BEGIN
    UPDATE threads
    SET recency_at = NEW.updated_at,
        recency_at_ms = COALESCE(NEW.updated_at_ms, NEW.updated_at * 1000)
    WHERE id = NEW.id;
END;
CREATE TRIGGER threads_updated_at_ms_after_insert
AFTER INSERT ON threads
WHEN NEW.updated_at_ms IS NULL
BEGIN
    UPDATE threads
    SET updated_at_ms = NEW.updated_at * 1000
    WHERE id = NEW.id;
END;
CREATE TRIGGER threads_updated_at_ms_after_update
AFTER UPDATE OF updated_at ON threads
WHEN NEW.updated_at != OLD.updated_at
 AND NEW.updated_at_ms IS OLD.updated_at_ms
BEGIN
    UPDATE threads
    SET updated_at_ms = NEW.updated_at * 1000
    WHERE id = NEW.id;
END;
