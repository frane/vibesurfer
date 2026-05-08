-- vibesurfer initial schema
-- See docs/PRIMITIVES.md and docs/ARCHITECTURE.md for the data model.

CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    policy_id   TEXT,
    status      TEXT NOT NULL,                  -- open|closed
    closed_at   INTEGER
);

CREATE TABLE pages (
    id            TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES sessions(id),
    url           TEXT NOT NULL,
    title         TEXT,
    last_token    TEXT,
    last_dom_hash TEXT,
    last_seen_at  INTEGER NOT NULL,
    closed_at     INTEGER
);

CREATE TABLE refs (
    session_id   TEXT NOT NULL,
    page_id      TEXT NOT NULL,
    ref          INTEGER NOT NULL,
    dom_path     TEXT NOT NULL,
    role         TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    retired_at   INTEGER,
    PRIMARY KEY (session_id, page_id, ref)
);

CREATE TABLE marks (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    page_id         TEXT NOT NULL,
    name            TEXT NOT NULL,
    dom_path        TEXT NOT NULL,
    role            TEXT,
    content_excerpt TEXT,
    created_at      INTEGER NOT NULL,
    UNIQUE (session_id, name)
);

CREATE TABLE annotations (
    id          TEXT PRIMARY KEY,
    target_kind TEXT NOT NULL,             -- ref|mark|page
    target_id   TEXT NOT NULL,
    key         TEXT NOT NULL,
    value       TEXT,
    created_at  INTEGER NOT NULL
);

CREATE TABLE actions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    page_id         TEXT,
    primitive       TEXT NOT NULL,
    args_redacted   TEXT NOT NULL,
    args_hash       TEXT NOT NULL,
    before_token    TEXT,
    after_token     TEXT,
    idempotency_hit INTEGER NOT NULL DEFAULT 0,
    result_summary  TEXT,
    latency_ms      INTEGER NOT NULL,
    group_label     TEXT,
    started_at      INTEGER NOT NULL,
    finished_at     INTEGER NOT NULL,
    error_code      TEXT
);

CREATE INDEX idx_actions_session ON actions(session_id, started_at);
CREATE INDEX idx_actions_page    ON actions(page_id, started_at);
CREATE INDEX idx_actions_idem    ON actions(page_id, before_token, args_hash);

CREATE TABLE auth_blobs (
    name         TEXT PRIMARY KEY,
    ciphertext   BLOB NOT NULL,
    nonce        BLOB NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);

CREATE TABLE skill_cache (
    name         TEXT PRIMARY KEY,
    version      TEXT NOT NULL,
    sha          TEXT NOT NULL,
    manifest     TEXT NOT NULL,
    last_used_at INTEGER
);
