# Remote Server Operations

This document describes how the production/tunnel server is configured and how to debug it.

## Configuration source files (in git)

Templates live in `deploy/templates/`:

- `caddy.tunnel.Caddyfile` (used by `scripts/install-remote-caddy.sh`)
- `caddy.production.Caddyfile` (used by `scripts/deploy-production.sh`)
- `qstream-backend.service` (systemd unit template)
- `qstream-journald.conf` (journald retention config)

The scripts render these templates with runtime values and install them on the server.

## Remote filesystem layout

- App releases: `/opt/qstream/releases/<release_id>/`
- Current release symlink: `/opt/qstream/current`
- Backend binary: `/opt/qstream/current/bin/qstream-backend`
- Frontend static files: `/opt/qstream/current/frontend`
- Rendered backend env: `/etc/qstream/backend.env`
- Installed backend unit: `/etc/systemd/system/qstream-backend.service`
- Installed Caddy config: `/etc/caddy/Caddyfile`
- Journald policy: `/etc/systemd/journald.conf.d/qstream.conf`
- Database/data dir: `/var/lib/qstream`

## Systemd services

- `qstream-backend.service` (Rust API on `127.0.0.1:<DEPLOY_BACKEND_PORT>`, default `3000`)
- `caddy.service` (public `80/443`, serves frontend + proxies `/api/*` to backend)

## Logs and status

Status:

```bash
sudo systemctl --no-pager --full status qstream-backend
sudo systemctl --no-pager --full status caddy
```

Follow logs:

```bash
sudo journalctl -u qstream-backend -f
sudo journalctl -u caddy -f
```

Recent logs:

```bash
sudo journalctl -u qstream-backend -n 200 --no-pager
sudo journalctl -u caddy -n 200 --no-pager
```

Ports/listeners:

```bash
sudo ss -ltnp | sed -n '1,120p'
```

## Common issues and fixes

### `https://<host>` does not open

1. Check DNS points to the server.
2. Check `caddy` is active:
   - `sudo systemctl is-active caddy`
3. Check ports `80`/`443` are listening:
   - `sudo ss -ltnp | grep -E ':(80|443)\\b'`
4. Ensure nginx is not taking `:80`/`:443`:
   - `sudo systemctl stop nginx && sudo systemctl disable nginx` (if present)

### Backend fails to start with `Address in use`

Check who owns backend port (default `3000`):

```bash
sudo ss -ltnp | grep ':3000'
```

Known conflict: `pm2-root` running an old Node app. Disable it:

```bash
sudo systemctl stop pm2-root
sudo systemctl disable pm2-root
```

Then restart backend:

```bash
sudo systemctl restart qstream-backend
```

### OAuth opens `:3000` URL in browser

This means old frontend JS is still deployed/cached.

1. Redeploy frontend (`scripts/deploy-production.sh`).
2. Hard refresh browser (`Ctrl+Shift+R`).

## Re-apply configuration

Tunnel reverse-proxy setup (for local dev via SSH reverse tunnel):

```bash
./scripts/install-remote-caddy.sh <ssh_user@host>
```

Production deploy (binary + static bundle + service configs):

```bash
./scripts/deploy-production.sh <ssh_user@host>
```
