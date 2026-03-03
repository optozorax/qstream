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
```

You can start from `.env.local.example`.

### One-time remote reverse proxy setup

```bash
./scripts/install-remote-caddy.sh
```

### Daily tunnel run

```bash
INSTALL_FRONTEND_DEPS=0 ./scripts/run-dev-tunnel.sh
```

Keep that process running while testing.

### Quick verification

In another terminal:

```bash
curl -sv "https://${PUBLIC_HOST}/"
curl -sv "https://${PUBLIC_HOST}/api/health"
```

Expected:
- `/` returns `HTTP 200` and frontend HTML
- `/api/health` returns `HTTP 200` with `{"ok":true}`
