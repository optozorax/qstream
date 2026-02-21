# QStream Vision

## Project Essence
QStream is a lightweight live-stream companion tool that helps streamers collect, rank, and moderate audience questions in real time.

A streamer opens a session, shares a public link with viewers, and receives a dynamic question feed where the most relevant questions rise to the top through community voting.

## Problem Statement
During live streams, chat is noisy and fast. Good questions get lost quickly, and streamers cannot efficiently prioritize what to answer next.

## Product Goals
- Make question intake simple for viewers.
- Surface the most useful questions using likes/dislikes.
- Give the streamer clear moderation controls.
- Keep UX fast and real-time.
- Keep infrastructure simple for a single streamer with a small audience.

## Target Scope
- Single streamer deployment.
- Small/medium audience.
- Non-enterprise setup.
- Fast MVP iteration over heavy production complexity.

## User Roles
- Streamer (admin): creates sessions, moderates questions, controls sorting.
- Viewer: passes captcha, sets nickname, submits questions, votes.

## Core User Flow
1. Streamer opens the app and starts a stream session.
2. System creates an admin view and a shareable viewer link.
3. Streamer posts viewer link in chat/description.
4. Viewer opens link, passes hCaptcha, enters nickname.
5. Viewer submits questions.
6. All viewers see questions and vote (upvote/downvote).
7. Streamer sorts feed by latest/top/hot.
8. Streamer marks questions as answered, hides/rejects, or bans abusive users.

## MVP Features
- hCaptcha verification for anti-bot protection.
- Nickname registration saved in SQLite.
- Question submission.
- Upvote/downvote per question.
- Real-time feed updates.
- Basic moderation: answered, hidden/rejected, ban.
- Sorting modes: `new`, `top`, `hot`.

## Suggested Data Model
- `stream_sessions`: stream room and state.
- `users`: viewer identity (nickname and auth metadata).
- `questions`: question text, status, timestamps.
- `votes`: one vote per user per question.
- `bans`: moderation actions per session/user.

## Question Statuses
- `visible`: default state.
- `answered`: already handled on stream.
- `hidden`: removed from public list.
- `rejected`: explicitly declined by moderator.

## Real-Time Strategy
Use Server-Sent Events (SSE) for fan-out updates (new question, vote changes, moderation events). Use regular HTTP POST for writes.

## Security and Abuse Controls
- hCaptcha on sensitive actions.
- Rate limits (question creation and voting).
- Input validation and length limits.
- One vote per user per question (DB unique constraint).
- Admin-only moderation endpoints.

## Technical Direction
- Backend: Rust + Axum + SQLx + SQLite.
- Frontend: Svelte.
- Realtime: SSE.
- Reverse proxy/TLS for deployment: Caddy or Nginx.

## Near-Term Roadmap
1. Session creation and shareable links.
2. Question CRUD + voting.
3. SSE event stream.
4. Admin moderation panel.
5. Sorting policies and answered queue.
6. Optional OAuth providers later (if needed).

## Non-Goals (for MVP)
- Multi-tenant platform.
- Complex role hierarchy.
- Enterprise analytics.
- Heavy distributed infrastructure.

## Definition of Success
During a live stream, the streamer can reliably see and answer the highest-value audience questions without chat chaos.
