CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    nickname TEXT NOT NULL,
    google_sub TEXT UNIQUE,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_login_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS auth_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen_at INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_auth_sessions_user_id
    ON auth_sessions(user_id);

CREATE TABLE IF NOT EXISTS oauth_login_states (
    state TEXT PRIMARY KEY,
    return_to TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_oauth_login_states_expires_at
    ON oauth_login_states(expires_at);

CREATE TABLE IF NOT EXISTS stream_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    public_code TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    name TEXT,
    description TEXT,
    stream_link TEXT,
    stopped_at INTEGER,
    downvote_threshold INTEGER NOT NULL DEFAULT 5,
    donations_enabled INTEGER NOT NULL DEFAULT 1 CHECK (donations_enabled IN (0, 1)),
    donations_enabled_at INTEGER,
    donations_min_external_id INTEGER
);

CREATE INDEX IF NOT EXISTS idx_stream_sessions_owner_created_at
    ON stream_sessions(owner_user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS da_oauth_states (
    state TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    return_to TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_da_oauth_states_expires_at
    ON da_oauth_states(expires_at);

CREATE TABLE IF NOT EXISTS da_integrations (
    owner_user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    da_user_id INTEGER NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    token_expires_at INTEGER NOT NULL,
    scope TEXT NOT NULL,
    last_seen_external_id INTEGER,
    last_sync_at INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS donations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES stream_sessions(id) ON DELETE CASCADE,
    owner_user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    external_donation_id INTEGER NOT NULL,
    donor_name TEXT NOT NULL,
    body TEXT NOT NULL CHECK (length(body) <= 300),
    amount_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    usd_cents INTEGER NOT NULL,
    provider_created_at TEXT,
    status TEXT NOT NULL DEFAULT 'new'
        CHECK (status IN ('new', 'answering', 'answered', 'rejected', 'deleted')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    answering_started_at INTEGER,
    answered_at INTEGER,
    UNIQUE(owner_user_id, external_donation_id)
);

CREATE INDEX IF NOT EXISTS idx_donations_session_status_created
    ON donations(session_id, status, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_donations_owner_external
    ON donations(owner_user_id, external_donation_id DESC);

CREATE TABLE IF NOT EXISTS questions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES stream_sessions(id) ON DELETE CASCADE,
    author_user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 300),
    status TEXT NOT NULL DEFAULT 'new'
        CHECK (status IN ('new', 'answering', 'answered', 'rejected', 'deleted')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    answering_started_at INTEGER,
    answered_at INTEGER,
    score INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_questions_new
    ON questions(session_id, status, created_at DESC);

-- Used by API-side "one question per minute" checks.
CREATE INDEX IF NOT EXISTS idx_questions_rate_limit
    ON questions(session_id, author_user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS votes (
    question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    value INTEGER NOT NULL CHECK (value IN (-1, 1)),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (question_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_votes_user_rate_limit
    ON votes(user_id, updated_at);

CREATE TABLE IF NOT EXISTS vote_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
    value INTEGER NOT NULL CHECK (value IN (-1, 0, 1)),
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_vote_actions_user_created_at
    ON vote_actions(user_id, created_at);

CREATE INDEX IF NOT EXISTS idx_vote_actions_created_at
    ON vote_actions(created_at);

CREATE TABLE IF NOT EXISTS bans (
    owner_user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    message TEXT,
    session_name TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (owner_user_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_bans_owner_created_at
    ON bans(owner_user_id, created_at DESC);
