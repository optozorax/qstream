#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

LOCAL_ENV_FILE="${LOCAL_ENV_FILE:-${ROOT_DIR}/.env.local}"

load_local_env() {
  if [[ -f "${LOCAL_ENV_FILE}" ]]; then
    set -a
    # shellcheck disable=SC1090
    source "${LOCAL_ENV_FILE}"
    set +a
  fi
}

load_local_env

SSH_TARGET="${1:-${SSH_TARGET:-}}"
if [[ -z "${SSH_TARGET}" ]]; then
  cat <<'USAGE'
Usage:
  ./scripts/install-remote-caddy.sh [ssh_user@vps_host]

Environment variables:
  SSH_TARGET=<ssh_user@vps_host>
  SSH_KEY_PATH=<path_to_private_key>
  PUBLIC_HOST=<public_https_host>
  REMOTE_FRONTEND_INTERNAL_PORT=45173
  REMOTE_BACKEND_INTERNAL_PORT=43000

Optional local env file:
  .env.local (or LOCAL_ENV_FILE=<path>)
USAGE
  exit 1
fi

SSH_KEY_PATH="${SSH_KEY_PATH:-}"
PUBLIC_HOST="${PUBLIC_HOST:-}"
REMOTE_FRONTEND_INTERNAL_PORT="${REMOTE_FRONTEND_INTERNAL_PORT:-45173}"
REMOTE_BACKEND_INTERNAL_PORT="${REMOTE_BACKEND_INTERNAL_PORT:-43000}"
SSH_KEY_TEMP=""

require_non_empty() {
  local value="$1"
  local name="$2"
  if [[ -z "${value}" ]]; then
    echo "[remote-setup] ERROR: ${name} is required (set it in env/.env.local or pass SSH target as arg)" >&2
    exit 1
  fi
}

require_non_empty "${SSH_TARGET}" "SSH_TARGET"
require_non_empty "${SSH_KEY_PATH}" "SSH_KEY_PATH"
require_non_empty "${PUBLIC_HOST}" "PUBLIC_HOST"

[[ -f "${SSH_KEY_PATH}" ]] || {
  echo "[remote-setup] ERROR: SSH key not found at ${SSH_KEY_PATH}" >&2
  exit 1
}

cleanup() {
  if [[ -n "${SSH_KEY_TEMP}" && -f "${SSH_KEY_TEMP}" ]]; then
    rm -f "${SSH_KEY_TEMP}"
  fi
}
trap cleanup EXIT INT TERM

SSH_KEY_TEMP="$(mktemp)"
cp "${SSH_KEY_PATH}" "${SSH_KEY_TEMP}"
chmod 600 "${SSH_KEY_TEMP}"

ssh \
  -i "${SSH_KEY_TEMP}" \
  -o ConnectTimeout=15 \
  -o StrictHostKeyChecking=accept-new \
  "${SSH_TARGET}" \
  "PUBLIC_HOST='${PUBLIC_HOST}' REMOTE_FRONTEND_INTERNAL_PORT='${REMOTE_FRONTEND_INTERNAL_PORT}' REMOTE_BACKEND_INTERNAL_PORT='${REMOTE_BACKEND_INTERNAL_PORT}' bash -s" <<'REMOTE_SCRIPT'
set -Eeuo pipefail

if ! command -v sudo >/dev/null 2>&1; then
  echo "[remote-setup] sudo is required on remote host" >&2
  exit 1
fi

if ! command -v caddy >/dev/null 2>&1; then
  sudo apt-get update
  sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gnupg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
  sudo apt-get update
  sudo apt-get install -y caddy
fi

sudo tee /etc/caddy/Caddyfile >/dev/null <<CADDY
${PUBLIC_HOST} {
    encode zstd gzip

    # API from local machine via SSH reverse tunnel
    reverse_proxy /api/* 127.0.0.1:${REMOTE_BACKEND_INTERNAL_PORT}

    # Frontend from local machine via SSH reverse tunnel
    reverse_proxy 127.0.0.1:${REMOTE_FRONTEND_INTERNAL_PORT}
}
CADDY

sudo caddy fmt --overwrite /etc/caddy/Caddyfile
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl enable caddy
sudo systemctl restart caddy
sudo systemctl --no-pager --full status caddy | sed -n '1,40p'

echo "[remote-setup] Caddy configured for https://${PUBLIC_HOST}"
REMOTE_SCRIPT
