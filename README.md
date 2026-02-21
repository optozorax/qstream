# QStream MVP

MVP for streamer Q&A:
- `backend`: Rust + Axum API with SQLite storage.
- `frontend`: Svelte single-page app with nickname form + hCaptcha.

## Implemented backend API

### Auth / login
- `POST /api/register`
  - body: `{ "nickname": "...", "hcaptcha_token": "..." }`
  - validates hCaptcha
  - creates/updates user
  - creates auth session
  - returns: `{ user, auth_token, session }`

Use returned token in header:
- `Authorization: Bearer <auth_token>`

### Sessions
- `POST /api/sessions`
  - auth required
  - creates one stream session per user (or returns existing)
  - returns: `{ session, created, public_url }`
- `GET /api/sessions/:code/events`
  - public SSE endpoint for real-time session updates
  - emits JSON events in `data:` with `kind`:
    - `question_created`
    - `question_changed`
    - `question_deleted`
    - `resync`

### Questions
- `GET /api/sessions/:code/questions?sort=top|new|answered`
  - public endpoint
  - returns ordered question list with score and vote count
  - `top/new` exclude answered questions
  - `answer in progress` questions are pinned to top in `top/new`
  - `answered` returns only answered questions
- `POST /api/sessions/:code/questions`
  - auth required
  - body: `{ "text": "..." }`
  - max 300 chars
  - one question per minute per user per session
  - session owner cannot create questions

### Votes
- `POST /api/questions/:id/vote`
  - auth required
  - body: `{ "value": -1 | 1 }`
  - one vote per user per question (upsert)
  - can change vote anytime
  - voting is disabled for answered/in-progress questions
  - session owner cannot vote

### Admin moderation
- `POST /api/questions/:id/moderate`
  - auth required
  - only session owner can call it
  - body: `{ "action": "answer" | "finish_answering" | "delete" }`
  - `answer` sets `is_answering=1`
  - `finish_answering` sets `is_answered=1` and moves question to answered tab

### Health
- `GET /api/health`

## Database schema
- Full schema is in `backend/src/schema.sql`.
- Reset script is in `backend/src/schema_reset.sql`.

## Backend setup

```bash
cd backend
cp .env.example .env
cargo run
```

Default env values:
- `APP_ADDR=0.0.0.0:3000`
- `FRONTEND_ORIGIN=http://localhost:5173`
- `PUBLIC_BASE_URL=http://localhost:5173`
- `DATABASE_URL=sqlite://qstream.db?mode=rwc`
- `HCAPTCHA_SITE_KEY=...`
- `HCAPTCHA_SECRET=...`
- `HCAPTCHA_SKIP_VERIFY=false`
- `RESET_DB_ON_BOOT=false`

If `RESET_DB_ON_BOOT=true`, backend drops and recreates all tables at startup.

## Schema changes in MVP

For this MVP we do not maintain SQL migrations.  
When schema changes are needed, reset the DB intentionally:

1. Set `RESET_DB_ON_BOOT=true`.
2. Restart backend once (it will rebuild schema from `backend/src/schema.sql`).
3. Set `RESET_DB_ON_BOOT=false` again.

## Frontend setup

```bash
cd frontend
cp .env.example .env
npm install
npm run dev
```

Frontend env:
- `VITE_API_BASE_URL=http://localhost:3000`
- `VITE_HCAPTCHA_SITE_KEY=...`

## Frontend behavior

- `/` (main page):
  - login with nickname + hCaptcha
  - create one session (`Create` button)
  - open current session link
- `/s/:code` (public session page):
  - public question list with real-time updates and sorting tabs (`top` / `new` / `answered`)
  - answered questions are removed from `top/new` and shown only in `answered`
  - `answer in progress` questions are pinned at the top in `top/new`
  - update mode switch: `Manual` (via `Update now` button) or `Auto (live)`
  - in `manual` mode, `Update now` shows pending new-question count from SSE notifications
  - logged-in non-owner viewers can toggle `Hide interacted` to filter out questions they already voted on or asked
  - default update mode: stream owner -> auto, non-logged guest -> auto, logged non-owner viewer -> manual
  - `Login` button (nickname + hCaptcha)
  - after login (non-owner): can submit question and vote `Like/Dislike` (voting disabled for answered/in-progress)
  - stream owner cannot vote
  - stream owner cannot submit questions
  - if logged in user is session owner: can `Answer`, `Finish answering`, or `Delete`

Local storage keys:
- `qstream_auth_token`
- `qstream_user`
- `qstream_current_session_code`
