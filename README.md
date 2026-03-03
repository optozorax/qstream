# QStream MVP

MVP for streamer Q&A:
- `backend`: Rust + Axum API with SQLite storage.
- `frontend`: Svelte single-page app with Google OAuth login.

## Implemented backend API

### Auth / login
- `GET /api/google_oauth2/start?return_to=/optional/path`
  - starts Google OAuth flow
  - stores one-time oauth state
  - redirects to Google consent page with `openid profile`
- `GET /api/google_oauth2`
  - Google callback endpoint
  - exchanges code for token, loads Google profile, creates/updates user
  - creates auth session
  - redirects back to `return_to` with `#auth_token=...`

Use returned token in header (`auth_token` from URL fragment after OAuth callback):
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
- `GOOGLE_CLIENT_ID=...`
- `GOOGLE_CLIENT_SECRET=...`
- `GOOGLE_REDIRECT_URI=http://localhost:3000/api/google_oauth2`
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
- `VITE_ALLOWED_HOSTS=`

## Run on a real public HTTPS domain through SSH tunnel

This workflow uses a root-level reverse proxy on your VPS (for example Caddy) and a local reverse SSH tunnel.

Put sensitive deployment values into `.env.local` (ignored by git):

```bash
SSH_TARGET=<ssh_user@vps_host>
SSH_KEY_PATH=<path_to_private_key>
PUBLIC_HOST=<public_https_host>
REMOTE_FRONTEND_INTERNAL_PORT=45173
REMOTE_BACKEND_INTERNAL_PORT=43000
GOOGLE_CLIENT_ID=<google_client_id>
GOOGLE_CLIENT_SECRET=<google_client_secret>
GOOGLE_REDIRECT_URI=https://<public_https_host>/api/google_oauth2
```

Start from `.env.local.example` and fill your local values.

One-time remote reverse-proxy setup:

```bash
./scripts/install-remote-caddy.sh
```

Daily run (starts backend+frontend locally and opens tunnel):

```bash
./scripts/run-dev-tunnel.sh
```

Open:
- `https://<PUBLIC_HOST>`

How it works:
- Reverse proxy on VPS listens on `80`/`443` for `PUBLIC_HOST`.
- Proxy forwards `/api/*` to `127.0.0.1:${REMOTE_BACKEND_INTERNAL_PORT}` on VPS.
- Proxy forwards all other paths to `127.0.0.1:${REMOTE_FRONTEND_INTERNAL_PORT}` on VPS.
- Local script keeps SSH reverse forwards from those VPS loopback ports to your local backend/frontend.

## Frontend behavior

- `/` (main page):
  - login via `Continue with Google`
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
  - `Log in` button opens `Continue with Google`
  - after login (non-owner): can submit question and vote `Like/Dislike` (voting disabled for answered/in-progress)
  - stream owner cannot vote
  - stream owner cannot submit questions
  - if logged in user is session owner: can `Answer`, `Finish answering`, or `Delete`

Local storage keys:
- `qstream_auth_token`
- `qstream_user`
- `qstream_current_session_code`
