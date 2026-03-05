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
- `GET /api/sessions`
  - auth required
  - returns all sessions owned by the authenticated user
- `POST /api/sessions`
  - auth required
  - body: `{ name, description?, stream_link?, downvote_threshold? }`
  - creates a new session (multiple sessions per user supported)
  - returns: `{ session, public_url }`
- `GET /api/sessions/:code/events`
  - public SSE endpoint for real-time session updates
  - per-IP connection limit enforced
  - emits JSON events in `data:` with `kind`:
    - `question_created`
    - `question_changed`
    - `question_deleted`
    - `resync`
- `DELETE /api/sessions/:code`
  - auth required, owner only
  - hard-deletes the session and all its questions

### Questions
- `GET /api/sessions/:code/questions?sort=top|new|answered|downvoted`
  - public endpoint
  - returns ordered question list with score and vote count
  - `top/new` exclude answered questions and questions at or below the downvote threshold
  - `answer in progress` questions are pinned to top in `top/new` regardless of score
  - `answered` returns only answered questions, ordered by `answered_at DESC`
  - `downvoted` returns questions at or below the session's downvote threshold (excludes in-progress)
  - response includes `viewer_is_banned: bool` for the authenticated user
- `POST /api/sessions/:code/questions`
  - auth required
  - body: `{ "text": "..." }`
  - max 300 chars
  - one question per minute per user per session
  - session owner cannot create questions
  - banned users cannot create questions

### Votes
- `POST /api/questions/:id/vote`
  - auth required
  - body: `{ "value": -1 | 0 | 1 }` (0 removes the vote)
  - one vote per user per question (upsert — can change or retract anytime)
  - voting disabled for answered/in-progress questions
  - session owner cannot vote
  - banned users cannot vote
  - rate limit: 200 votes per minute per user

### Admin moderation
- `POST /api/questions/:id/moderate`
  - auth required
  - only session owner can call it
  - body: `{ "action": "answer" | "finish_answering" | "reopen" | "reject" | "delete" | "ban" }`
  - only one question per session can be in `answering` state at a time
  - `answer` sets `is_answering=1`, records `answering_started_at`
  - `finish_answering` sets `is_answered=1`, records `answered_at`
  - `reopen` clears `is_answering` and `is_answered` (moves back to active queue)
  - `reject` marks question as rejected
  - `delete` hard-deletes the question
  - `ban` bans the question's author (owner-scoped, cross-session), deletes all their questions in the session, records the triggering question

### Sessions (extended)
- `PUT /api/sessions/:code`
  - auth required, owner only
  - body: `{ name, description?, stream_link?, downvote_threshold? }`
  - `downvote_threshold` (integer 1–1000, default 5): questions at or below `-threshold` move to the Downvoted tab
- `POST /api/sessions/:code/stop`
  - auth required, owner only
  - ends the session; questions can no longer be submitted

### Bans
- `GET /api/bans`
  - auth required
  - returns all bans created by the authenticated user (across all their sessions)
  - each entry includes: `user_id`, `nickname`, `banned_at`, `question_body` (the triggering question), `session_name`
- `DELETE /api/bans/:user_id`
  - auth required
  - removes the ban; user can interact with the owner's sessions again

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

## Schema changes

The backend applies `backend/src/schema.sql` at startup and does not run in-place `ALTER TABLE` migrations.

To start completely fresh:

1. Set `RESET_DB_ON_BOOT=true`.
2. Restart backend once (it drops and recreates all tables from `backend/src/schema.sql`).
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

Put sensitive values into separate local env files (ignored by git):

```bash
# .env.tunnel
SSH_TARGET=<ssh_user@tunnel_host>
SSH_KEY_PATH=<path_to_private_key>
PUBLIC_HOST=<public_https_host>
TUNNEL_UPSTREAM_HOST=<tunnel_host_or_ip>
REMOTE_TUNNEL_BIND_ADDRESS=127.0.0.1
REMOTE_FRONTEND_INTERNAL_PORT=45173
REMOTE_BACKEND_INTERNAL_PORT=43000
GOOGLE_CLIENT_ID=<google_client_id>
GOOGLE_CLIENT_SECRET=<google_client_secret>
GOOGLE_REDIRECT_URI=https://<public_https_host>/api/google_oauth2

# .env.install
DEPLOY_SSH_TARGET=<ssh_user@production_host>
DEPLOY_PUBLIC_HOST=<production_public_host>
DEPLOY_SSH_KEY_PATH=<path_to_private_key>
DEPLOY_GOOGLE_CLIENT_ID=<google_client_id>
DEPLOY_GOOGLE_CLIENT_SECRET=<google_client_secret>
DEPLOY_GOOGLE_REDIRECT_URI=https://<production_public_host>/api/google_oauth2
```

Start from `.env.tunnel.example` and `.env.install.example`.

One-time remote reverse-proxy setup:

```bash
./scripts/install-remote-caddy.sh
```

This script renders and installs `deploy/templates/caddy.tunnel.Caddyfile`.
By default it reads `.env.tunnel`.

Daily run (starts backend+frontend locally and opens tunnel):

```bash
./scripts/run-dev-tunnel.sh
```

Open:
- `https://<PUBLIC_HOST>`

How it works:
- Reverse proxy on VPS listens on `80`/`443` for `PUBLIC_HOST`.
- Proxy forwards `/api/*` to `${TUNNEL_UPSTREAM_HOST}:${REMOTE_BACKEND_INTERNAL_PORT}`.
- Proxy forwards all other paths to `${TUNNEL_UPSTREAM_HOST}:${REMOTE_FRONTEND_INTERNAL_PORT}`.
- Local script keeps SSH reverse forwards from `${REMOTE_TUNNEL_BIND_ADDRESS}:${REMOTE_*_INTERNAL_PORT}` on `SSH_TARGET` to your local backend/frontend.

If `PUBLIC_HOST` and tunnel `SSH_TARGET` are different servers:
- install Caddy on the `PUBLIC_HOST` server:

```bash
./scripts/install-remote-caddy.sh <ssh_user@public_host_server>
```

- run the tunnel to `SSH_TARGET` with non-loopback bind:

```bash
REMOTE_TUNNEL_BIND_ADDRESS=0.0.0.0 ./scripts/run-dev-tunnel.sh <ssh_user@ssh_target_host>
```

- ensure SSH server on tunnel `SSH_TARGET` allows non-loopback reverse binds:
  set `GatewayPorts clientspecified` in `/etc/ssh/sshd_config` and restart sshd.

Tunnel scripts support `LOCAL_ENV_FILE=<path>` and fallback to legacy `.env.tunnel.local` then `.env.local`.

## Production deploy (weak VPS friendly)

For low-resource VPS (for example `1 CPU / 512MB / 10GB`), avoid building Rust or installing `node_modules` on the server.

Use local build + artifact deploy:

```bash
./scripts/deploy-production.sh <ssh_user@prod_vps_host>
```

To wipe SQLite data during deploy:

```bash
./scripts/deploy-production.sh --remove-database <ssh_user@prod_vps_host>
```

By default, deploy script reads `.env.install` (fallback: `.env.install.local`, then `.env.local`).

What it does:
- builds backend release binary locally (`cargo build --release`)
- builds frontend static files locally (`npm run build`)
- uploads only binary + static assets + env to server
- installs/updates:
  - `qstream-backend.service` (systemd)
  - Caddy config for `https://<DEPLOY_PUBLIC_HOST>`
  - persistent journald logs (`/var/log/journal`)

Config templates used by deploy:
- `deploy/templates/qstream-backend.service`
- `deploy/templates/caddy.production.Caddyfile`
- `deploy/templates/qstream-journald.conf`

Frontend runtime choice:
- Best for this setup: **no runtime on server** (no Node/Bun/Deno).
- Build frontend on your local machine and serve static `dist` via Caddy.

Useful log commands:

```bash
ssh <ssh_user@prod_vps_host> 'sudo journalctl -u qstream-backend -f'
ssh <ssh_user@prod_vps_host> 'sudo journalctl -u caddy -f'
```

Server layout, operations, and debugging checklist:
- `docs/remote-server-operations.md`

## Frontend behavior

- `/` (main page):
  - login via `Continue with Google`
  - list of all your sessions with Active/Stopped badges
  - create a new session (`New session` button)
  - delete stopped sessions (with confirmation)
  - **Banned users** section (collapsed by default, loads on demand):
    - shows all users banned across all your sessions
    - each entry shows: nickname, the question that triggered the ban (in italics), session name, time of ban
    - `Unban` button per entry; unbanned users can interact with your sessions again
- `/s/:code` (public session page):
  - public question list with real-time updates and sorting tabs (`Top` / `New` / `Answered` / `Downvoted`)
  - answered questions are removed from `Top`/`New` and shown only in `Answered`
  - questions at or below the session's downvote threshold move to `Downvoted` (everyone can still vote them back)
  - `answer in progress` questions are pinned at the top in `Top`/`New` regardless of score
  - update mode switch: `Manual` or `Auto`
  - in `Manual` mode, `Refresh` button shows a badge with pending new-question count from SSE notifications
  - logged-in non-owner viewers can toggle `Hide voted` to filter out questions they already voted on or asked
  - default update mode: stream owner → auto, non-logged guest → auto, logged non-owner viewer → manual
  - `Log in` button opens `Continue with Google`
  - after login (non-owner): can submit question and vote upvote/downvote (voting disabled for answered/in-progress)
  - stream owner cannot vote or submit questions
  - **banned users** see an error banner and cannot submit questions or vote
  - session owner controls per question: `Answer`, `Done` (finish answering), `Undo` (reopen), `Reject`, `Delete` (with confirmation), `Ban` (with confirmation)
  - **Session settings panel** (owner only, `Settings` button):
    - edit name, description, stream link
    - **Downvote threshold**: configurable score at which questions move to the Downvoted tab (default: 5, range: 1–1000)
    - `Stop session` button with confirmation (ends session, no new questions accepted)
  - **YouTube timecodes panel** (owner only, shown after session ends):
    - displays all answered questions as YouTube chapter lines (`M:SS Question text` or `H:MM:SS Question text`)
    - sorted by answer time ascending (chronological chapter order)
    - stream start time input: `DD - MM - YYYY HH : MM : SS` (locale-independent European format)
    - defaults to session creation time; adjust to match actual stream start
    - timecodes update instantly as you change the start time
    - `Reset` button restores start time to session creation time
    - `Copy` button copies the timecode block to clipboard (shows `Copied!` for 2 seconds)
    - paste directly into YouTube stream description for automatic chapters

Local storage keys:
- `qstream_auth_token`
- `qstream_user`
