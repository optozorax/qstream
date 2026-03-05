PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS bans;
DROP TABLE IF EXISTS vote_actions;
DROP TABLE IF EXISTS votes;
DROP TABLE IF EXISTS donations;
DROP TABLE IF EXISTS da_integrations;
DROP TABLE IF EXISTS da_oauth_states;
DROP TABLE IF EXISTS questions;
DROP TABLE IF EXISTS stream_sessions;
DROP TABLE IF EXISTS auth_sessions;
DROP TABLE IF EXISTS oauth_login_states;
DROP TABLE IF EXISTS users;
DROP TABLE IF EXISTS schema_migrations;

PRAGMA foreign_keys = ON;
