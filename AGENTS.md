# AGENTS

## Real HTTPS Domain Testing

Use this workflow to test features that require a real HTTPS origin (for example OAuth callbacks).

### Private config location

Keep project-specific server details in `AGENTS.private.md` (not tracked by git).

Keep sensitive runtime values in `.env.local` (not tracked by git):

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

You can start from `.env.local.example`.

### One-time remote reverse proxy setup

```bash
./scripts/install-remote-caddy.sh
```

This renders and installs `deploy/templates/caddy.tunnel.Caddyfile`.

### Daily tunnel run

```bash
INSTALL_FRONTEND_DEPS=0 ./scripts/run-dev-tunnel.sh
```

Keep that process running while testing.

### Google OAuth prerequisites

Set OAuth envs in `.env.local` (or `backend/.env`, both are local-only):

```bash
GOOGLE_CLIENT_ID=...
GOOGLE_CLIENT_SECRET=...
GOOGLE_REDIRECT_URI=https://<PUBLIC_HOST>/api/google_oauth2
```

Google OAuth app should allow:
- Authorized JavaScript origin: `https://<PUBLIC_HOST>`
- Authorized redirect URI: `https://<PUBLIC_HOST>/api/google_oauth2`

### Quick verification

In another terminal:

```bash
curl -sv "https://${PUBLIC_HOST}/"
curl -sv "https://${PUBLIC_HOST}/api/health"
```

Expected:
- `/` returns `HTTP 200` and frontend HTML
- `/api/health` returns `HTTP 200` with `{"ok":true}`

## Production Deploy

For weak VPS, deploy prebuilt artifacts (no Rust build / no node_modules on server):

```bash
./scripts/deploy-production.sh <ssh_user@prod_vps_host>
```

This script configures:
- `qstream-backend.service` (systemd)
- Caddy site config (static frontend + `/api` reverse proxy)
- persistent journald logs

Templates:
- `deploy/templates/qstream-backend.service`
- `deploy/templates/caddy.production.Caddyfile`
- `deploy/templates/qstream-journald.conf`

Ops/debug guide:
- `docs/remote-server-operations.md`
