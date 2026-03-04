# QStream Vision

## Project Essence
QStream is a lightweight live-stream companion tool that helps streamers collect, rank, and moderate audience questions in real time.

A streamer opens a session, shares a public link with viewers, and receives a dynamic question feed where the most relevant questions rise to the top through community voting.

## Problem Statement
During live streams, chat is noisy and fast. Good questions get lost quickly, and streamers cannot efficiently prioritize what to answer next.

## Product Goals
- Make question intake simple for viewers.
- Surface the most useful questions using upvotes/downvotes.
- Give the streamer clear moderation controls.
- Keep UX fast and real-time.
- Keep infrastructure simple for a single streamer with a small audience.
- Generate useful post-stream artifacts (YouTube timecodes).

## Target Scope
- Single streamer deployment.
- Small/medium audience.
- Non-enterprise setup.
- Fast iteration over heavy production complexity.

## User Roles
- **Streamer (admin)**: creates and manages sessions, moderates questions, bans abusive users, controls sorting and thresholds.
- **Viewer**: logs in with Google, submits questions, votes on others' questions.

## Core User Flow
1. Streamer logs in with Google and creates a session.
2. System generates a shareable public link (`/s/:code`).
3. Streamer posts the link in chat or stream description.
4. Viewer opens the link and logs in with Google.
5. Viewer submits questions (max 300 chars, one per minute per session).
6. All viewers see questions and vote (upvote/downvote).
7. Streamer sorts feed by Top / New / Answered / Downvoted.
8. Streamer marks questions as answering → answered, or rejects / deletes / bans.
9. After stream ends, streamer copies YouTube chapter timecodes for the video description.

## Implemented Features

### Authentication
- Google OAuth 2.0 (requests only `openid profile` — name only, not email).
- Auth token stored in localStorage, sent as `Authorization: Bearer`.
- Auth sessions tracked in DB with expiry support.

### Sessions
- A streamer can create multiple named sessions.
- Each session has: name, description, stream link, downvote threshold, active/stopped state.
- Sessions are stopped explicitly; stopped sessions are read-only (no new questions or votes).
- Sessions can be deleted (hard delete).

### Questions
- Viewers submit questions up to 300 characters.
- Rate limit: one question per minute per user per session.
- Session owner cannot submit questions.
- Banned users cannot submit questions.

### Voting
- Upvote (+1) or downvote (−1) per user per question (upsert — can change anytime).
- Voting disabled for answered or in-progress questions.
- Session owner cannot vote.
- Banned users cannot vote.
- Vote rate limit: 200 votes per minute per user.

### Question Statuses
- **active**: default state, visible in Top and New tabs.
- **answering**: currently being answered on stream; pinned to top of Top/New regardless of score.
- **answered**: moved to Answered tab with timestamp.
- **rejected**: explicitly declined; hidden from all tabs.
- **deleted**: hard-deleted from DB.
- **downvoted**: questions at or below `−downvote_threshold` move to the Downvoted tab (still voteable).

### Sorting Tabs
- **Top**: active questions sorted by score descending; in-progress pinned first.
- **New**: active questions sorted by creation time descending; in-progress pinned first.
- **Answered**: answered questions sorted by answer time descending.
- **Downvoted**: questions at or below `−threshold`, sorted by score ascending; excludes in-progress.

### Moderation
- **Answer / Done**: marks question as answering → answered with timestamps.
- **Undo**: reopens an answering or answered question back to active.
- **Reject**: hides question from all public tabs.
- **Delete**: hard-deletes the question (with confirmation).
- **Ban**: bans the question's author across all of the streamer's sessions; deletes all their questions in the current session; records the triggering question for audit trail.
- **Unban**: owner can unban from the home page; user regains full access.

### Downvote Threshold
- Configurable per session (default: 5, range: 1–1000).
- Questions scoring ≤ `−threshold` move to the Downvoted tab automatically.
- In-progress questions are never moved to Downvoted regardless of score.

### Ban System
- Bans are **per owner** (not per session): banning a user applies across all sessions owned by the same streamer.
- Ban record stores: banned user, triggering question body, session name, timestamp.
- Streamer's home page shows all banned users (collapsed by default, loads on demand).

### Real-Time Updates
- Server-Sent Events (SSE) stream per session (`/api/sessions/:code/events`).
- Events: `question_created`, `question_changed`, `question_deleted`, `resync`.
- Per-IP SSE connection limit to prevent abuse.
- Two update modes for viewers: **Auto** (live) or **Manual** (refresh button with pending count badge).

### Post-Stream: YouTube Timecodes
- After a session ends, the streamer sees a timecode panel.
- Lists all answered questions as YouTube chapter lines: `M:SS Question text` (or `H:MM:SS` for sessions over an hour).
- Sorted by answer time ascending (chronological chapter order).
- Stream start time input in European format (`DD - MM - YYYY HH : MM : SS`), defaults to session creation time, adjustable with Reset button.
- Timecodes recalculate instantly as start time changes.
- Copy button for pasting into YouTube video description.

## Data Model
- `users`: Google identity (`google_sub`), nickname, login timestamps.
- `auth_sessions`: bearer tokens with expiry.
- `oauth_login_states`: one-time CSRF tokens for OAuth flow.
- `stream_sessions`: session room, state, metadata, `downvote_threshold`.
- `questions`: text, statuses (`is_answering`, `is_answered`, `is_rejected`, `is_deleted`), timestamps (`created_at`, `answering_started_at`, `answered_at`).
- `votes`: one row per (question, user) pair; value ∈ {−1, +1}.
- `bans`: per `(owner_user_id, user_id)`; references triggering question.

## Real-Time Strategy
Server-Sent Events (SSE) for fan-out updates. Regular HTTP for all writes. Debounced auto-refresh on the client (300 ms) to batch rapid SSE events into a single API call.

## Security and Abuse Controls
- Google OAuth for identity (no passwords, no email stored).
- Rate limits: one question per minute per user per session; 200 votes per minute per user.
- Per-IP SSE connection limit.
- Input validation and length limits (questions: 1–300 chars).
- One vote per user per question (DB unique constraint).
- Admin-only moderation endpoints (owner check enforced server-side).
- Ban system for persistent abusive users.

## Technical Stack
- **Backend**: Rust + Axum + SQLx + SQLite.
- **Frontend**: Svelte 4 SPA, served as static files.
- **Realtime**: SSE.
- **Deployment**: Caddy as reverse proxy/TLS termination.
- **Auth**: Google OAuth 2.0 (`openid profile`).

## Non-Goals
- Multi-tenant platform (one owner, one deployment).
- Complex role hierarchy beyond owner/viewer.
- Enterprise analytics or dashboards.
- Heavy distributed infrastructure.
- Pagination (question lists are fully loaded per request).
- Email or password authentication.
- Mobile app.

## Definition of Success
During a live stream, the streamer can reliably see and answer the highest-value audience questions without chat chaos. After the stream, they can instantly generate YouTube chapter markers from the answered question history.
